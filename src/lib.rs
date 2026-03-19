mod index;
use std::hash::Hasher;

use anyhow::Context;

#[aviutl2::plugin(InputPlugin)]
struct FfmpegAui2 {}

struct FfmpegAui2InputHandle {
    path: std::path::PathBuf,
    format_context: ffmpeg_next::format::context::Input,
    index: index::IndexContentFile,
    video_index: Vec<index::VideoEntry>,
    audio_index: Vec<index::AudioEntry>,
}
unsafe impl Send for FfmpegAui2InputHandle {}
unsafe impl Sync for FfmpegAui2InputHandle {}

fn index_dir() -> std::path::PathBuf {
    process_path::get_dylib_path()
        .unwrap()
        .with_file_name("ffmpeg.aui2.index")
}

impl aviutl2::input::InputPlugin for FfmpegAui2 {
    type InputHandle = FfmpegAui2InputHandle;

    fn new(_info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        ffmpeg_next::init()?;
        aviutl2::tracing_subscriber::fmt()
            .with_max_level(if cfg!(debug_assertions) {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            })
            .event_format(aviutl2::logger::AviUtl2Formatter)
            .with_writer(aviutl2::logger::AviUtl2LogWriter)
            .init();
        Ok(Self {})
    }

    fn plugin_info(&self) -> aviutl2::input::InputPluginTable {
        aviutl2::input::InputPluginTable {
            name: "ffmpeg.aui2".into(),
            information: "FFMpeg-based input plugin for AviUtl2".into(),
            input_type: aviutl2::input::InputType::Both,
            concurrent: true,
            file_filters: aviutl2::file_filters! {
                "Video Files" => ["mp4", "mkv", "avi", "mov", "flv"],
            },
            can_config: false,
        }
    }

    fn open(&self, file: std::path::PathBuf) -> aviutl2::AnyResult<Self::InputHandle> {
        let opened = ffmpeg_next::format::input(&file)?;
        tracing::info!("Opened file: {:?}", file);
        let mut hash = xxhash_rust::xxh3::Xxh3Default::new();
        let mut reader = std::fs::File::open(&file)?;
        std::io::copy(&mut reader, &mut hash)?;
        let hash = hash.finish();
        tracing::info!("File {:?} has XXH3 hash: {:016x}", file, hash);

        let index_header_path = index_dir().join(format!("{:016x}.header.index", hash));
        let index_path = index_dir().join(format!("{:016x}.index", hash));
        let should_create_index = if index_header_path.exists() && index_path.exists() {
            let index_header_file = std::fs::read(&index_header_path).with_context(|| {
                format!("Failed to open index header file: {:?}", index_header_path)
            })?;
            // Header presence and successful deserialization = index is complete
            rkyv::from_bytes::<index::IndexHeaderFile, rkyv::rancor::Error>(&index_header_file)
                .is_err()
        } else {
            true
        };

        let index = if should_create_index {
            tracing::info!("Creating index for file: {:?}", file);
            index::create_index(&file, &index_header_path, &index_path, hash)
                .with_context(|| format!("Failed to create index for file: {:?}", file))?
        } else {
            tracing::info!("Loading existing index for file: {:?}", file);
            let index_file = std::fs::read(&index_path)
                .with_context(|| format!("Failed to open index file: {:?}", index_path))?;
            rkyv::from_bytes::<index::IndexContentFile, rkyv::rancor::Error>(&index_file)
                .with_context(|| format!("Failed to deserialize index file: {:?}", index_path))?
        };

        tracing::info!(
            "Index loaded: {} video tracks, {} audio tracks, {} total entries",
            index
                .tracks
                .iter()
                .filter(|t| matches!(t, index::TrackInfo::Video { .. }))
                .count(),
            index
                .tracks
                .iter()
                .filter(|t| matches!(t, index::TrackInfo::Audio { .. }))
                .count(),
            index.entries.len()
        );

        Ok(FfmpegAui2InputHandle {
            path: file,
            format_context: opened,
            index,
            video_index: vec![],
            audio_index: vec![],
        })
    }

    fn close(&self, handle: Self::InputHandle) -> aviutl2::AnyResult<()> {
        tracing::info!("Closing file: {:?}", handle.path);
        Ok(())
    }

    fn get_input_info(
        &self,
        handle: &mut Self::InputHandle,
        video_track: u32,
        audio_track: u32,
    ) -> aviutl2::AnyResult<aviutl2::input::InputInfo> {
        let video = if let Some(index::TrackInfo::Video {
            stream_index,
            width,
            height,
            frames,
            duration,
        }) = handle.index.tracks.get(video_track as usize)
        {
            if *duration <= 0.0 || *frames == 0 {
                tracing::warn!(
                    "Video track {} has invalid duration ({}) or frame count ({}), skipping video info",
                    video_track,
                    duration,
                    frames
                );
                None
            } else {
                handle.video_index = handle
                    .index
                    .entries
                    .iter()
                    .filter(|e| e.stream_index() == *stream_index)
                    .filter_map(|e| e.as_video().cloned())
                    .collect();

                static FPS_ACCURACY: i32 = 1000;
                Some(aviutl2::input::VideoInputInfo {
                    width: *width,
                    height: *height,
                    fps: aviutl2::Rational32::new(
                        *frames as i32 * FPS_ACCURACY,
                        (duration * FPS_ACCURACY as f64) as i32,
                    ),
                    num_frames: *frames as _,
                    manual_frame_index: true,
                    format: aviutl2::input::InputPixelFormat::Hf64,
                })
            }
        } else {
            None
        };
        let audio = if let Some(index::TrackInfo::Audio {
            stream_index,
            sample_rate,
            channels,
            samples,
            duration,
        }) = handle.index.tracks.get(audio_track as usize)
        {
            if *duration <= 0.0 || *samples == 0 {
                tracing::warn!(
                    "Audio track {} has invalid duration ({}) or frame count ({}), skipping audio info",
                    audio_track,
                    duration,
                    samples
                );
                None
            } else {
                handle.audio_index = handle
                    .index
                    .entries
                    .iter()
                    .filter(|e| e.stream_index() == *stream_index)
                    .filter_map(|e| e.as_audio().cloned())
                    .collect();
                Some(aviutl2::input::AudioInputInfo {
                    sample_rate: *sample_rate,
                    channels: *channels,
                    num_samples: *samples as _,
                    format: aviutl2::input::AudioFormat::IeeeFloat32,
                })
            }
        } else {
            None
        };
        Ok(aviutl2::input::InputInfo { video, audio })
    }

    fn get_track_count(&self, handle: &mut Self::InputHandle) -> aviutl2::AnyResult<(u32, u32)> {
        let video_count = handle
            .index
            .tracks
            .iter()
            .filter(|t| matches!(t, index::TrackInfo::Video { .. }))
            .count() as u32;
        let audio_count = handle
            .index
            .tracks
            .iter()
            .filter(|t| matches!(t, index::TrackInfo::Audio { .. }))
            .count() as u32;
        Ok((video_count, audio_count))
    }
}

aviutl2::register_input_plugin!(FfmpegAui2);
