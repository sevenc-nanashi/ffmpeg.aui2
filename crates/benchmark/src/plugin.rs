use std::ffi::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::Context;
use aviutl2_sys::{
    cache2::CACHE_HANDLE,
    config2::CONFIG_HANDLE,
    input2::{INPUT_HANDLE, INPUT_INFO, INPUT_PLUGIN_TABLE},
    logger2::LOG_HANDLE,
};
use libloading::Library;

use crate::host::HostEnvironment;

type RequiredVersion = unsafe extern "C" fn() -> u32;
type InitializeLogger = unsafe extern "C" fn(*mut LOG_HANDLE);
type InitializeConfig = unsafe extern "C" fn(*mut CONFIG_HANDLE);
type InitializeCache = unsafe extern "C" fn(*mut CACHE_HANDLE);
type InitializePlugin = unsafe extern "C" fn(u32) -> bool;
type UninitializePlugin = unsafe extern "C" fn();
type GetInputPluginTable = unsafe extern "C" fn() -> *mut INPUT_PLUGIN_TABLE;

const HOST_VERSION: u32 = 2_010_100;

pub struct LoadedPlugin {
    table: NonNull<INPUT_PLUGIN_TABLE>,
    uninitialize: Option<UninitializePlugin>,
    _host: HostEnvironment,
    _library: Library,
}

impl LoadedPlugin {
    pub fn load(dll: &Path, app_data_path: &Path) -> anyhow::Result<Self> {
        let dll = dll
            .canonicalize()
            .with_context(|| format!("Failed to resolve DLL: {}", dll.display()))?;
        let dll = normal_path(&dll);
        let library = unsafe { Library::new(&dll) }
            .with_context(|| format!("Failed to load DLL: {}", dll.display()))?;
        let get_table = unsafe {
            *library
                .get::<GetInputPluginTable>(b"GetInputPluginTable\0")
                .context("DLL does not export GetInputPluginTable")?
        };

        let mut host = HostEnvironment::new(app_data_path);
        unsafe {
            if let Ok(required_version) = library.get::<RequiredVersion>(b"RequiredVersion\0") {
                let required_version = required_version();
                anyhow::ensure!(
                    HOST_VERSION >= required_version,
                    "Plugin requires AviUtl2 version {required_version}, host is {HOST_VERSION}"
                );
            }
            if let Ok(initialize_logger) = library.get::<InitializeLogger>(b"InitializeLogger\0") {
                initialize_logger(host.logger_ptr());
            }
            if let Ok(initialize_config) = library.get::<InitializeConfig>(b"InitializeConfig\0") {
                initialize_config(host.config_ptr());
            }
            if let Ok(initialize_cache) = library.get::<InitializeCache>(b"InitializeCache\0") {
                initialize_cache(host.cache_ptr());
            }
        }

        if let Ok(initialize_plugin) =
            unsafe { library.get::<InitializePlugin>(b"InitializePlugin\0") }
        {
            anyhow::ensure!(
                unsafe { initialize_plugin(HOST_VERSION) },
                "InitializePlugin({HOST_VERSION}) failed"
            );
        }

        let uninitialize = unsafe {
            library
                .get::<UninitializePlugin>(b"UninitializePlugin\0")
                .ok()
                .map(|function| *function)
        };

        let table = NonNull::new(unsafe { get_table() });
        let Some(table) = table else {
            if let Some(uninitialize) = uninitialize {
                unsafe { uninitialize() };
            }
            anyhow::bail!("GetInputPluginTable returned null");
        };

        Ok(Self {
            table,
            uninitialize,
            _host: host,
            _library: library,
        })
    }

    fn table(&self) -> &INPUT_PLUGIN_TABLE {
        unsafe { self.table.as_ref() }
    }

    pub fn is_concurrent(&self) -> bool {
        self.table().flag & INPUT_PLUGIN_TABLE::FLAG_CONCURRENT != 0
    }

    pub fn open(&self, path: &Path) -> anyhow::Result<InputHandle> {
        let table = self.table();
        let open = table.func_open.context("Input plugin has no func_open")?;
        let close = table.func_close.context("Input plugin has no func_close")?;
        let info_get = table
            .func_info_get
            .context("Input plugin has no func_info_get")?;
        let read_video = table
            .func_read_video
            .context("Input plugin has no func_read_video")?;

        let wide_path = wide_path(path);
        let raw = unsafe { open(wide_path.as_ptr()) };
        anyhow::ensure!(!raw.is_null(), "func_open failed: {}", path.display());

        let result = initialize_input_handle(table, raw, info_get, read_video, close, path);
        if result.is_err() {
            anyhow::ensure!(close(raw), "func_close failed after initialization error");
        }
        result
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        if let Some(uninitialize) = self.uninitialize {
            unsafe { uninitialize() };
        }
    }
}

pub struct InputHandle {
    raw: INPUT_HANDLE,
    close: extern "C" fn(INPUT_HANDLE) -> bool,
    read_video: extern "C" fn(INPUT_HANDLE, i32, *mut c_void) -> i32,
    path: PathBuf,
    total_frames: i32,
    width: i32,
    height: i32,
    rate: i32,
    scale: i32,
    buffer: Vec<u8>,
}

unsafe impl Send for InputHandle {}

impl InputHandle {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn total_frames(&self) -> i32 {
        self.total_frames
    }

    pub fn dimensions(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub fn frame_rate(&self) -> (i32, i32) {
        (self.rate, self.scale)
    }

    pub fn read_frame(&mut self, frame: i32) -> anyhow::Result<ReadMeasurement> {
        anyhow::ensure!(
            (0..self.total_frames).contains(&frame),
            "Frame {frame} is outside 0..{} for {}",
            self.total_frames,
            self.path.display()
        );
        let started = Instant::now();
        let bytes = (self.read_video)(self.raw, frame, self.buffer.as_mut_ptr().cast());
        let elapsed = started.elapsed();
        anyhow::ensure!(
            bytes > 0,
            "func_read_video failed for frame {frame}: {}",
            self.path.display()
        );
        let bytes = usize::try_from(bytes).context("func_read_video returned a negative size")?;
        anyhow::ensure!(
            bytes == self.buffer.len(),
            "Frame size mismatch for {} frame {frame}: expected {}, got {bytes}",
            self.path.display(),
            self.buffer.len()
        );
        Ok(ReadMeasurement {
            duration: elapsed,
            bytes,
        })
    }

    pub fn frame_digest(&self) -> u64 {
        self.buffer
            .iter()
            .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
    }
}

impl Drop for InputHandle {
    fn drop(&mut self) {
        assert!(
            (self.close)(self.raw),
            "func_close failed: {}",
            self.path.display()
        );
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReadMeasurement {
    pub duration: Duration,
    pub bytes: usize,
}

fn initialize_input_handle(
    table: &INPUT_PLUGIN_TABLE,
    raw: INPUT_HANDLE,
    info_get: extern "C" fn(INPUT_HANDLE, *mut INPUT_INFO) -> bool,
    read_video: extern "C" fn(INPUT_HANDLE, i32, *mut c_void) -> i32,
    close: extern "C" fn(INPUT_HANDLE) -> bool,
    path: &Path,
) -> anyhow::Result<InputHandle> {
    if table.flag & INPUT_PLUGIN_TABLE::FLAG_MULTI_TRACK != 0 {
        let set_track = table
            .func_set_track
            .context("FLAG_MULTI_TRACK is set but func_set_track is missing")?;
        let video_tracks = set_track(raw, INPUT_PLUGIN_TABLE::TRACK_TYPE_VIDEO, -1);
        anyhow::ensure!(video_tracks > 0, "Input contains no video track");
        let audio_tracks = set_track(raw, INPUT_PLUGIN_TABLE::TRACK_TYPE_AUDIO, -1);
        anyhow::ensure!(audio_tracks >= 0, "Failed to query audio tracks");
        anyhow::ensure!(
            set_track(raw, INPUT_PLUGIN_TABLE::TRACK_TYPE_VIDEO, 0) == 0,
            "Failed to select video track 0"
        );
        if audio_tracks > 0 {
            anyhow::ensure!(
                set_track(raw, INPUT_PLUGIN_TABLE::TRACK_TYPE_AUDIO, 0) == 0,
                "Failed to select audio track 0"
            );
        }
    }

    let mut info = unsafe { std::mem::zeroed::<INPUT_INFO>() };
    anyhow::ensure!(info_get(raw, &mut info), "func_info_get failed");
    anyhow::ensure!(
        info.flag & INPUT_INFO::FLAG_VIDEO != 0,
        "func_info_get returned no video information"
    );
    let format = unsafe { info.format.as_ref() }.context("Video format pointer is null")?;
    anyhow::ensure!(format.biWidth > 0, "Video width must be positive");
    anyhow::ensure!(format.biHeight != 0, "Video height must not be zero");
    anyhow::ensure!(info.n > 0, "Video frame count must be positive");
    anyhow::ensure!(info.rate > 0 && info.scale > 0, "Invalid video frame rate");

    let image_size = image_size(format)?;
    Ok(InputHandle {
        raw,
        close,
        read_video,
        path: path.to_path_buf(),
        total_frames: info.n,
        width: format.biWidth,
        height: format.biHeight.abs(),
        rate: info.rate,
        scale: info.scale,
        buffer: vec![0; image_size],
    })
}

fn image_size(format: &aviutl2_sys::input2::BITMAPINFOHEADER) -> anyhow::Result<usize> {
    if format.biSizeImage > 0 {
        return usize::try_from(format.biSizeImage).context("biSizeImage is too large");
    }

    anyhow::ensure!(format.biBitCount > 0, "biBitCount must be positive");
    let row_bits = u64::try_from(format.biWidth)?
        .checked_mul(u64::from(format.biBitCount))
        .context("Video row size overflowed")?;
    let row_bytes = row_bits
        .checked_add(31)
        .context("Video row alignment overflowed")?
        / 32
        * 4;
    let bytes = row_bytes
        .checked_mul(u64::from(format.biHeight.unsigned_abs()))
        .context("Video image size overflowed")?;
    usize::try_from(bytes).context("Video image size is too large")
}

fn normal_path(path: &Path) -> PathBuf {
    const VERBATIM_PREFIX: [u16; 4] = [92, 92, 63, 92];
    const UNC_PREFIX: [u16; 4] = [85, 78, 67, 92];

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.starts_with(&VERBATIM_PREFIX) {
        if wide[VERBATIM_PREFIX.len()..].starts_with(&UNC_PREFIX) {
            wide.splice(..VERBATIM_PREFIX.len() + UNC_PREFIX.len(), [92_u16; 2]);
        } else {
            wide.drain(..VERBATIM_PREFIX.len());
        }
    }
    std::ffi::OsString::from_wide(&wide).into()
}

fn wide_path(path: &Path) -> Vec<u16> {
    normal_path(path)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    static FORMAT: aviutl2_sys::input2::BITMAPINFOHEADER = aviutl2_sys::input2::BITMAPINFOHEADER {
        biSize: std::mem::size_of::<aviutl2_sys::input2::BITMAPINFOHEADER>() as u32,
        biWidth: 2,
        biHeight: 2,
        biPlanes: 1,
        biBitCount: 16,
        biCompression: aviutl2_sys::common::BI_YUY2,
        biSizeImage: 8,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    extern "C" fn close(_handle: INPUT_HANDLE) -> bool {
        true
    }

    extern "C" fn info_get(_handle: INPUT_HANDLE, info: *mut INPUT_INFO) -> bool {
        unsafe {
            (*info).flag = INPUT_INFO::FLAG_VIDEO;
            (*info).rate = 60;
            (*info).scale = 1;
            (*info).n = 10;
            (*info).format = &FORMAT;
            (*info).format_size =
                std::mem::size_of::<aviutl2_sys::input2::BITMAPINFOHEADER>() as i32;
        }
        true
    }

    extern "C" fn read_video(_handle: INPUT_HANDLE, _frame: i32, buffer: *mut c_void) -> i32 {
        unsafe { std::slice::from_raw_parts_mut(buffer.cast::<u8>(), 8) }.fill(0x5a);
        8
    }

    extern "C" fn set_track(_handle: INPUT_HANDLE, track_type: i32, track: i32) -> i32 {
        if track == -1 {
            if track_type == INPUT_PLUGIN_TABLE::TRACK_TYPE_VIDEO {
                1
            } else {
                0
            }
        } else {
            0
        }
    }

    #[test]
    fn image_size_is_derived_when_header_size_is_zero() {
        let format = aviutl2_sys::input2::BITMAPINFOHEADER {
            biSizeImage: 0,
            ..FORMAT
        };

        assert_eq!(image_size(&format).unwrap(), 8);
    }

    #[test]
    fn wide_path_removes_windows_verbatim_prefixes() {
        let local = String::from_utf16(&[
            92, 92, 63, 92, 69, 58, 92, 118, 105, 100, 101, 111, 46, 109, 112, 52,
        ])
        .unwrap();
        let unc = String::from_utf16(&[
            92, 92, 63, 92, 85, 78, 67, 92, 115, 101, 114, 118, 101, 114, 92, 118, 105, 100, 101,
            111, 46, 109, 112, 52,
        ])
        .unwrap();

        assert_eq!(
            wide_path(Path::new(&local)),
            vec![69, 58, 92, 118, 105, 100, 101, 111, 46, 109, 112, 52, 0]
        );
        assert_eq!(
            wide_path(Path::new(&unc)),
            vec![
                92, 92, 115, 101, 114, 118, 101, 114, 92, 118, 105, 100, 101, 111, 46, 109, 112,
                52, 0,
            ]
        );
    }

    #[test]
    fn input_handle_uses_track_info_and_exact_image_size() {
        let table = INPUT_PLUGIN_TABLE {
            flag: INPUT_PLUGIN_TABLE::FLAG_VIDEO | INPUT_PLUGIN_TABLE::FLAG_MULTI_TRACK,
            name: std::ptr::null(),
            filefilter: std::ptr::null(),
            information: std::ptr::null(),
            func_open: None,
            func_close: Some(close),
            func_info_get: Some(info_get),
            func_read_video: Some(read_video),
            func_read_audio: None,
            func_config: None,
            func_set_track: Some(set_track),
            func_time_to_frame: None,
        };
        let raw = NonNull::<u8>::dangling().as_ptr().cast();
        let mut handle = initialize_input_handle(
            &table,
            raw,
            info_get,
            read_video,
            close,
            Path::new("mock.mp4"),
        )
        .unwrap();

        let measurement = handle.read_frame(0).unwrap();

        assert_eq!(measurement.bytes, 8);
        assert_eq!(handle.dimensions(), (2, 2));
        assert_eq!(handle.frame_rate(), (60, 1));
        assert!(handle.read_frame(10).is_err());
    }
}
