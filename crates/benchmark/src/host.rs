use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void};
use std::path::Path;

use aviutl2_sys::{
    cache2::{
        AUDIO_INFO, CACHE_AUDIO, CACHE_FILE_IMAGE, CACHE_HANDLE, CACHE_IMAGE, CACHE_REFERENCE,
        VIDEO_INFO,
    },
    common::LPCWSTR,
    config2::{CONFIG_HANDLE, FONT_INFO},
    filter2::INPUT_PIXEL_FORMAT,
    logger2::LOG_HANDLE,
};

struct StaticCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for StaticCell<T> {}

static DEFAULT_FONT_NAME: [u16; 10] = [
    b'S' as u16,
    b'e' as u16,
    b'g' as u16,
    b'o' as u16,
    b'e' as u16,
    b' ' as u16,
    b'U' as u16,
    b'I' as u16,
    0,
    0,
];
static DEFAULT_FONT_INFO: StaticCell<FONT_INFO> = StaticCell(UnsafeCell::new(FONT_INFO {
    name: DEFAULT_FONT_NAME.as_ptr(),
    size: 12.0,
}));

unsafe extern "C" fn discard_log(_handle: *mut LOG_HANDLE, _message: LPCWSTR) {}

unsafe extern "C" fn translate(_handle: *mut CONFIG_HANDLE, text: LPCWSTR) -> LPCWSTR {
    text
}

unsafe extern "C" fn get_language_text(
    _handle: *mut CONFIG_HANDLE,
    _section: LPCWSTR,
    text: LPCWSTR,
) -> LPCWSTR {
    text
}

unsafe extern "C" fn get_font_info(
    _handle: *mut CONFIG_HANDLE,
    _key: *const c_char,
) -> *mut FONT_INFO {
    DEFAULT_FONT_INFO.0.get()
}

unsafe extern "C" fn get_color_code(_handle: *mut CONFIG_HANDLE, _key: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn get_layout_size(_handle: *mut CONFIG_HANDLE, _key: *const c_char) -> i32 {
    0
}

unsafe extern "C" fn get_color_code_index(
    _handle: *mut CONFIG_HANDLE,
    _key: *const c_char,
    _index: i32,
) -> i32 {
    0
}

fn empty_reference() -> CACHE_REFERENCE {
    CACHE_REFERENCE {
        func_release: None,
        cache_instance: std::ptr::null_mut(),
    }
}

fn empty_image() -> CACHE_IMAGE {
    CACHE_IMAGE {
        reference: empty_reference(),
        buffer: std::ptr::null_mut(),
        width: 0,
        height: 0,
    }
}

fn empty_audio() -> CACHE_AUDIO {
    CACHE_AUDIO {
        reference: empty_reference(),
        buffer0: std::ptr::null_mut(),
        buffer1: std::ptr::null_mut(),
        sample_num: 0,
        channel_num: 0,
    }
}

fn empty_file_image() -> CACHE_FILE_IMAGE {
    CACHE_FILE_IMAGE {
        reference: empty_reference(),
        buffer: std::ptr::null(),
        width: 0,
        height: 0,
        pitch: 0,
        format: INPUT_PIXEL_FORMAT::RGBA,
    }
}

unsafe extern "C" fn get_image_cache(_identifier: *mut c_void, _name: LPCWSTR) -> CACHE_IMAGE {
    empty_image()
}

unsafe extern "C" fn create_image_cache(
    _identifier: *mut c_void,
    _name: LPCWSTR,
    _width: i32,
    _height: i32,
) -> CACHE_IMAGE {
    empty_image()
}

unsafe extern "C" fn get_audio_cache(_identifier: *mut c_void, _name: LPCWSTR) -> CACHE_AUDIO {
    empty_audio()
}

unsafe extern "C" fn create_audio_cache(
    _identifier: *mut c_void,
    _name: LPCWSTR,
    _sample_num: i32,
    _channel_num: i32,
) -> CACHE_AUDIO {
    empty_audio()
}

unsafe extern "C" fn get_image_file_cache(_file: LPCWSTR) -> CACHE_FILE_IMAGE {
    empty_file_image()
}

unsafe extern "C" fn deprecated_get_image_file_cache(_file: LPCWSTR) -> CACHE_IMAGE {
    empty_image()
}

unsafe extern "C" fn get_video_file_info(
    _file: LPCWSTR,
    _info: *mut VIDEO_INFO,
    _info_size: i32,
) -> bool {
    false
}

unsafe extern "C" fn get_audio_file_info(
    _file: LPCWSTR,
    _info: *mut AUDIO_INFO,
    _info_size: i32,
) -> bool {
    false
}

unsafe extern "C" fn get_video_file_cache(
    _file: LPCWSTR,
    _track: i32,
    _frame: i32,
) -> CACHE_FILE_IMAGE {
    empty_file_image()
}

unsafe extern "C" fn get_video_file_cache_by_time(
    _file: LPCWSTR,
    _track: i32,
    _time: f64,
) -> CACHE_FILE_IMAGE {
    empty_file_image()
}

unsafe extern "C" fn get_audio_file_data(
    _file: LPCWSTR,
    _track: i32,
    _sample_index: i64,
    _sample_num: i32,
    _buffer0: *mut f32,
    _buffer1: *mut f32,
) -> i32 {
    0
}

pub struct HostEnvironment {
    _app_data_path: Vec<u16>,
    logger: Box<LOG_HANDLE>,
    config: Box<CONFIG_HANDLE>,
    cache: Box<CACHE_HANDLE>,
}

impl HostEnvironment {
    pub fn new(app_data_path: &Path) -> Self {
        let app_data_path = app_data_path
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let logger = Box::new(LOG_HANDLE {
            log: discard_log,
            info: discard_log,
            warn: discard_log,
            error: discard_log,
            verbose: discard_log,
        });
        let config = Box::new(CONFIG_HANDLE {
            app_data_path: app_data_path.as_ptr(),
            translate,
            get_language_text,
            get_font_info,
            get_color_code,
            get_layout_size,
            get_color_code_index,
        });
        let cache = Box::new(CACHE_HANDLE {
            get_image_cache,
            create_image_cache,
            get_audio_cache,
            create_audio_cache,
            deprecated_get_image_file_cache,
            get_video_file_info,
            get_audio_file_info,
            get_image_file_cache,
            get_video_file_cache,
            get_video_file_cache_by_time,
            get_audio_file_data,
        });

        Self {
            _app_data_path: app_data_path,
            logger,
            config,
            cache,
        }
    }

    pub fn logger_ptr(&mut self) -> *mut LOG_HANDLE {
        self.logger.as_mut()
    }

    pub fn config_ptr(&mut self) -> *mut CONFIG_HANDLE {
        self.config.as_mut()
    }

    pub fn cache_ptr(&mut self) -> *mut CACHE_HANDLE {
        self.cache.as_mut()
    }
}
