mod audio;
mod index;
mod prefetch;
mod video;
use std::hash::Hasher;
use std::sync::atomic::Ordering;

use anyhow::Context;
use audio::AudioDecoderState;
use prefetch::{PrefetchConfig, PrefetchHandle};
use video::VideoDecoderState;

#[aviutl2::plugin(InputPlugin)]
struct FfmpegAui2 {}

struct FfmpegAui2InputHandle {
    path: std::path::PathBuf,
    index: index::IndexContentFile,
    video_index: Vec<index::VideoEntry>,
    audio_index: Vec<index::AudioEntry>,
    current_video_track: Option<index::VideoTrackInfo>,
    current_audio_track: Option<index::AudioTrackInfo>,
    video_decoder: std::sync::Mutex<Option<VideoDecoderState>>,
    audio_decoder: std::sync::Mutex<Option<AudioDecoderState>>,
    prefetch: PrefetchHandle,
}
unsafe impl Send for FfmpegAui2InputHandle {}
unsafe impl Sync for FfmpegAui2InputHandle {}

fn index_dir() -> std::path::PathBuf {
    process_path::get_dylib_path()
        .unwrap()
        .with_file_name("index")
}

impl aviutl2::input::InputPlugin for FfmpegAui2 {
    type InputHandle = FfmpegAui2InputHandle;

    fn new(info: aviutl2::AviUtl2Info) -> aviutl2::AnyResult<Self> {
        ffmpeg_next::init()?;

        let nonce = std::fs::metadata(
            process_path::get_dylib_path()
                .context("Failed to get plugin file metadata for version nonce")?,
        )?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
        let mut hasher = xxhash_rust::xxh3::Xxh3Default::new();
        hasher.write_u64(nonce);
        let nonce = hasher.finish();

        aviutl2::tracing_subscriber::fmt()
            // .with_max_level(tracing::Level::DEBUG)
            .with_max_level(if cfg!(debug_assertions) {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            })
            .event_format(aviutl2::logger::AviUtl2Formatter)
            .with_writer(aviutl2::logger::AviUtl2LogWriter)
            .init();
        tracing::info!("ffmpeg.aui2 plugin initialized");
        tracing::info!("AviUtl2 version: {}", info.version);
        tracing::info!("Index directory: {:?}", index_dir());
        tracing::info!(
            "Version nonce: {:016x}",
            index::VERSION_NONCE.get_or_init(|| { nonce })
        );
        tracing::info!(
            "ffmpeg codec configuration: {:?}",
            ffmpeg_next::codec::configuration()
        );
        tracing::info!(
            "ffmpeg device configuration: {:?}",
            ffmpeg_next::device::configuration()
        );
        tracing::info!(
            "ffmpeg filter configuration: {:?}",
            ffmpeg_next::filter::configuration()
        );
        tracing::info!(
            "ffmpeg format configuration: {:?}",
            ffmpeg_next::format::configuration()
        );
        tracing::info!(
            "ffmpeg util configuration: {:?}",
            ffmpeg_next::util::configuration()
        );
        tracing::info!(
            "ffmpeg swscale configuration: {:?}",
            ffmpeg_next::software::scaling::configuration()
        );
        tracing::info!(
            "ffmpeg swresample configuration: {:?}",
            ffmpeg_next::software::resampling::configuration()
        );
        tracing::info!(
            "ffmpeg codec version: {}",
            format_ffmpeg_version(ffmpeg_next::codec::version())
        );
        tracing::info!(
            "ffmpeg device version: {}",
            format_ffmpeg_version(ffmpeg_next::device::version())
        );
        tracing::info!(
            "ffmpeg filter version: {}",
            format_ffmpeg_version(ffmpeg_next::filter::version())
        );
        tracing::info!(
            "ffmpeg format version: {}",
            format_ffmpeg_version(ffmpeg_next::format::version())
        );
        tracing::info!(
            "ffmpeg util version: {}",
            format_ffmpeg_version(ffmpeg_next::util::version())
        );
        tracing::info!(
            "ffmpeg swscale version: {}",
            format_ffmpeg_version(ffmpeg_next::software::scaling::version())
        );
        tracing::info!(
            "ffmpeg swresample version: {}",
            format_ffmpeg_version(ffmpeg_next::software::resampling::version())
        );
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
        ffmpeg_next::format::input(&file)?;
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
            match rkyv::from_bytes::<index::IndexHeaderFile, rkyv::rancor::Error>(
                &index_header_file,
            ) {
                Ok(header) => {
                    if header.filehash == hash
                        && header.version_nonce == *index::VERSION_NONCE.get().unwrap()
                    {
                        tracing::info!("Index header valid for file: {:?}. Loading index.", file);
                        false
                    } else {
                        tracing::warn!(
                            "Index header mismatch for file: {:?}. Expected hash: {:016x}, version nonce: {:016x}. Found hash: {:016x}, version nonce: {:016x}. Recreating index.",
                            file,
                            hash,
                            *index::VERSION_NONCE.get().unwrap(),
                            header.filehash,
                            header.version_nonce,
                        );
                        true
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to deserialize index header for file: {:?}. Error: {}. Recreating index.",
                        file,
                        e
                    );
                    true
                }
            }
        } else {
            true
        };

        let index = if should_create_index {
            tracing::info!("Creating index for file: {:?}", file);
            let start_time = std::time::Instant::now();
            let index = index::create_index(&file, &index_header_path, &index_path, hash)
                .with_context(|| format!("Failed to create index for file: {:?}", file))?;
            let elapsed = start_time.elapsed();
            tracing::info!("Index created in {:.2?}", elapsed,);
            index
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
            prefetch: PrefetchHandle::new(file.clone()),
            path: file,
            index,
            video_index: vec![],
            audio_index: vec![],
            current_video_track: None,
            current_audio_track: None,
            video_decoder: std::sync::Mutex::new(None),
            audio_decoder: std::sync::Mutex::new(None),
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
        let video = if let Some(v) = handle.index.video_tracks().nth(video_track as usize) {
            if v.duration <= 0.0 || v.frames == 0 {
                tracing::warn!(
                    "Video track {} has invalid duration ({}) or frame count ({}), skipping video info",
                    video_track,
                    v.duration,
                    v.frames
                );
                handle.current_video_track = None;
                None
            } else {
                handle.video_index = handle
                    .index
                    .entries
                    .iter()
                    .filter(|e| e.stream_index() == v.stream_index)
                    .filter_map(|e| e.as_video().cloned())
                    .collect();
                // Sort by PTS (display order) — packet order is DTS which differs for B-frames
                handle
                    .video_index
                    .sort_by(|a, b| a.timestamp.partial_cmp(&b.timestamp).unwrap());

                // Invalidate cached decoder if stream changed
                let mut vd = handle.video_decoder.lock().unwrap();
                if vd
                    .as_ref()
                    .is_some_and(|s| s.stream_index != v.stream_index)
                {
                    *vd = None;
                }

                handle.current_video_track = Some(v.clone());

                static FPS_ACCURACY: i32 = 1000;
                Some(aviutl2::input::VideoInputInfo {
                    width: v.width,
                    height: v.height,
                    fps: aviutl2::Rational32::new(
                        v.frames as i32 * FPS_ACCURACY,
                        (v.duration * FPS_ACCURACY as f64) as i32,
                    ),
                    num_frames: v.frames as _,
                    manual_frame_index: true,
                    format: if v.convert_to_yuv422 {
                        aviutl2::input::InputPixelFormat::Yuy2
                    } else {
                        aviutl2::input::InputPixelFormat::Bgra
                    },
                })
            }
        } else {
            None
        };

        // Update prefetch config whenever video track changes
        *handle.prefetch.config.write().unwrap() =
            handle.current_video_track.as_ref().map(|v| PrefetchConfig {
                video_index: std::sync::Arc::new(handle.video_index.clone()),
                convert_to_yuv422: v.convert_to_yuv422,
            });
        handle.prefetch.cache.clear();

        let audio = if let Some(a) = handle.index.audio_tracks().nth(audio_track as usize) {
            if a.duration <= 0.0 || a.samples == 0 {
                tracing::warn!(
                    "Audio track {} has invalid duration ({}) or sample count ({}), skipping audio info",
                    audio_track,
                    a.duration,
                    a.samples,
                );
                handle.current_audio_track = None;
                None
            } else {
                handle.audio_index = handle
                    .index
                    .entries
                    .iter()
                    .filter(|e| e.stream_index() == a.stream_index)
                    .filter_map(|e| e.as_audio().cloned())
                    .collect();

                // Invalidate cached decoder if stream changed
                let mut ad = handle.audio_decoder.lock().unwrap();
                if ad
                    .as_ref()
                    .is_some_and(|s| s.stream_index != a.stream_index)
                {
                    *ad = None;
                }

                handle.current_audio_track = Some(a.clone());

                Some(aviutl2::input::AudioInputInfo {
                    sample_rate: a.sample_rate,
                    channels: a.channels,
                    num_samples: a.samples as _,
                    format: aviutl2::input::AudioFormat::IeeeFloat32,
                })
            }
        } else {
            None
        };
        tracing::info!(
            "Video track {} info: {:?}, Audio track {} info: {:?}",
            video_track,
            video,
            audio_track,
            audio
        );
        Ok(aviutl2::input::InputInfo { video, audio })
    }

    fn get_track_count(&self, handle: &mut Self::InputHandle) -> aviutl2::AnyResult<(u32, u32)> {
        let video_count = handle.index.video_tracks().count() as u32;
        let audio_count = handle.index.audio_tracks().count() as u32;
        Ok((video_count, audio_count))
    }

    fn time_to_frame(
        &self,
        handle: &mut Self::InputHandle,
        _track: u32,
        time: f64,
    ) -> anyhow::Result<u32> {
        if handle.video_index.is_empty() {
            anyhow::bail!("Video index is empty");
        }
        let pos = handle.video_index.partition_point(|e| e.timestamp < time);
        Ok(pos.min(handle.video_index.len() - 1) as u32)
    }

    fn read_video(
        &self,
        handle: &Self::InputHandle,
        frame: u32,
        returner: &mut aviutl2::input::ImageReturner,
    ) -> anyhow::Result<()> {
        let frame = frame as usize;
        let entry = handle
            .video_index
            .get(frame)
            .ok_or_else(|| anyhow::anyhow!("Frame {} out of range", frame))?
            .clone();

        let video_track = handle
            .current_video_track
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Video track info not set"))?;

        let convert_to_yuv422 = video_track.convert_to_yuv422;
        let stream_index = entry.stream_index;
        let target_ts = entry.timestamp;

        // Update current position and wake prefetch thread
        handle.prefetch.position.store(frame, Ordering::Relaxed);
        let _ = handle.prefetch.tx.send(());

        // Check prefetch cache
        if let Some((_, data)) = handle.prefetch.cache.remove(&frame) {
            handle.prefetch.cache.retain(|&k, _| k > frame);
            returner.write(&data);
            return Ok(());
        }

        // Decode with the main decoder
        let mut state_guard = handle.video_decoder.lock().unwrap();
        if state_guard
            .as_ref()
            .is_none_or(|s| s.stream_index != stream_index)
        {
            *state_guard = Some(VideoDecoderState::new(&handle.path, stream_index)?);
        }
        let state = state_guard.as_mut().unwrap();

        if target_ts < state.current_ts - 1e-6
            || entry.last_keyframe_timestamp > state.current_ts + 1e-6
        {
            state.seek(entry.last_keyframe_timestamp);
        }

        let video_frame = state.decode_to(target_ts)?;
        let pixel_data = state.frame_to_bytes(&video_frame, convert_to_yuv422)?;
        drop(state_guard);

        returner.write(&pixel_data);
        Ok(())
    }

    fn read_audio(
        &self,
        handle: &Self::InputHandle,
        start: i32,
        length: i32,
        returner: &mut aviutl2::input::AudioReturner,
    ) -> anyhow::Result<()> {
        if handle.audio_index.is_empty() {
            anyhow::bail!("Audio index is empty");
        }

        let audio_track = handle
            .current_audio_track
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Audio track info not set"))?;
        let stream_index = audio_track.stream_index;
        let sample_rate = audio_track.sample_rate;
        let channels = audio_track.channels as usize;

        let start_idx = start.max(0) as usize;
        let length = length.max(0) as usize;
        let end_idx = start_idx + length;

        let mut state_guard = handle.audio_decoder.lock().unwrap();

        if state_guard
            .as_ref()
            .is_none_or(|s| s.stream_index != stream_index)
        {
            *state_guard = Some(
                AudioDecoderState::new(&handle.path, stream_index, sample_rate, channels)
                    .with_context(|| {
                        format!(
                            "Failed to initialize audio decoder for stream {}",
                            stream_index
                        )
                    })?,
            );
        }

        let state = state_guard.as_mut().unwrap();

        let buffer_end_sample = state.buffer_start + state.buffer.len() / channels;
        let seek_threshold = sample_rate as usize; // 1秒分
        if start_idx < state.buffer_start || start_idx > buffer_end_sample + seek_threshold {
            let start_time = start_idx as f64 / sample_rate as f64;
            let seek_pos = handle
                .audio_index
                .partition_point(|e| e.timestamp < start_time);
            let seek_ts = handle.audio_index[seek_pos.saturating_sub(1)].timestamp;
            state.seek(seek_ts);
        } else if start_idx > state.buffer_start {
            let trim = ((start_idx - state.buffer_start) * channels).min(state.buffer.len());
            state.buffer.drain(..trim);
            state.buffer_start = start_idx;
        }

        state.fill_until(end_idx)?;

        let buf_offset = (start_idx - state.buffer_start) * channels;
        let needed = length * channels;
        let available = state.buffer.len().saturating_sub(buf_offset);
        let copy_len = needed.min(available);

        let mut output = vec![0.0f32; needed];
        output[..copy_len].copy_from_slice(&state.buffer[buf_offset..buf_offset + copy_len]);

        returner.write(&output);
        Ok(())
    }
}

fn format_ffmpeg_version(version: u32) -> String {
    let major = version >> 16;
    let minor = (version >> 8) & 0xFF;
    let patch = version & 0xFF;
    format!("{}.{}.{}", major, minor, patch)
}

aviutl2::register_input_plugin!(FfmpegAui2);
