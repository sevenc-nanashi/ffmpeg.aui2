use std::sync::atomic::Ordering;

use crate::audio::AudioDecoderState;
use crate::index;
use crate::video::VideoDecoderState;

#[derive(Clone)]
pub struct PrefetchConfig {
    pub video_index: std::sync::Arc<Vec<index::VideoEntry>>,
    pub output_format: index::VideoOutputFormat,
}

pub struct PrefetchHandle {
    pub cache: std::sync::Arc<dashmap::DashMap<usize, Vec<u8>>>,
    pub config: std::sync::Arc<std::sync::RwLock<Option<PrefetchConfig>>>,
    pub position: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub tx: std::sync::mpsc::Sender<()>,
}

#[derive(Clone)]
pub struct AudioPrefetchRequest {
    pub audio_index: std::sync::Arc<Vec<index::AudioEntry>>,
    pub stream_index: usize,
    pub sample_rate: u32,
    pub channels: usize,
    pub start: usize,
    pub length: usize,
}

pub struct AudioPrefetchHandle {
    tx: std::sync::mpsc::Sender<(
        AudioPrefetchRequest,
        std::sync::mpsc::SyncSender<anyhow::Result<Vec<f32>>>,
    )>,
}

impl AudioPrefetchHandle {
    pub fn new(path: std::path::PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(
            AudioPrefetchRequest,
            std::sync::mpsc::SyncSender<anyhow::Result<Vec<f32>>>,
        )>();

        std::thread::spawn(move || run_audio_prefetch_thread(rx, path));

        Self { tx }
    }

    pub fn read(&self, request: AudioPrefetchRequest) -> anyhow::Result<Vec<f32>> {
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send((request, response_tx))
            .map_err(|_| anyhow::anyhow!("Audio prefetch thread has stopped"))?;
        response_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("Audio prefetch response channel disconnected"))?
    }
}

impl PrefetchHandle {
    pub fn new(path: std::path::PathBuf) -> Self {
        let cache = std::sync::Arc::new(dashmap::DashMap::new());
        let config = std::sync::Arc::new(std::sync::RwLock::new(None::<PrefetchConfig>));
        let position = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        {
            let cache_clone = std::sync::Arc::clone(&cache);
            let config_clone = std::sync::Arc::clone(&config);
            let position_clone = std::sync::Arc::clone(&position);
            std::thread::spawn(move || {
                run_prefetch_thread(rx, path, position_clone, config_clone, cache_clone);
            });
        }

        Self {
            cache,
            config,
            position,
            tx,
        }
    }
}

fn run_prefetch_thread(
    rx: std::sync::mpsc::Receiver<()>,
    path: std::path::PathBuf,
    position: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    config: std::sync::Arc<std::sync::RwLock<Option<PrefetchConfig>>>,
    cache: std::sync::Arc<dashmap::DashMap<usize, Vec<u8>>>,
) {
    let mut decoder: Option<VideoDecoderState> = None;

    loop {
        match rx.try_recv() {
            Ok(()) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }
        while rx.try_recv().is_ok() {}

        let cfg = match config.read().unwrap().clone() {
            Some(c) => c,
            None => continue,
        };

        let mut current = position.load(Ordering::Relaxed);
        let mut start_ts = cfg
            .video_index
            .get(current)
            .map(|e| e.timestamp)
            .unwrap_or(0.0);
        const PREFETCH_DURATION: f64 = 0.1;
        let mut end_ts = start_ts + PREFETCH_DURATION;
        let next_frame = current + 1;

        for (i, entry) in cfg.video_index[next_frame.min(cfg.video_index.len())..]
            .iter()
            .enumerate()
        {
            let new_current = position.load(Ordering::Relaxed);
            if new_current != current {
                let new_ts = cfg
                    .video_index
                    .get(new_current)
                    .map(|e| e.timestamp)
                    .unwrap_or(f64::MAX);
                if new_ts > end_ts {
                    break;
                }
                current = new_current;
                start_ts = new_ts;
                end_ts = start_ts + PREFETCH_DURATION;
            }
            if entry.timestamp > end_ts {
                break;
            }

            let frame_idx = next_frame + i;

            if cache.contains_key(&frame_idx) {
                continue;
            }

            if decoder
                .as_ref()
                .is_none_or(|d| d.stream_index != entry.stream_index)
            {
                match VideoDecoderState::new(&path, entry.stream_index) {
                    Ok(d) => decoder = Some(d),
                    Err(e) => {
                        tracing::warn!("Prefetch: decoder init failed: {e}");
                        break;
                    }
                }
            }
            let state = decoder.as_mut().unwrap();

            if entry.timestamp < state.current_ts - 1e-6 {
                state.seek(entry.last_keyframe_timestamp);
            }

            match state.decode_to(entry.timestamp) {
                Ok(frame) => match state.frame_to_bytes(&frame, &cfg.output_format) {
                    Ok(data) => {
                        tracing::debug!(
                            "Prefetch: cached frame {frame_idx} at timestamp {}",
                            entry.timestamp
                        );
                        cache.insert(frame_idx, data);
                    }
                    Err(e) => {
                        tracing::warn!("Prefetch: scale failed at frame {frame_idx}: {e}");
                        break;
                    }
                },
                Err(e) => {
                    tracing::warn!("Prefetch: decode failed at frame {frame_idx}: {e}");
                    break;
                }
            }
        }
    }
}

fn audio_buffer_range(state: &AudioDecoderState, channels: usize) -> Option<(usize, usize)> {
    state
        .buffer
        .start_sample
        .map(|start| (start, start + state.buffer.samples.len() / channels))
}

fn run_audio_prefetch_thread(
    rx: std::sync::mpsc::Receiver<(
        AudioPrefetchRequest,
        std::sync::mpsc::SyncSender<anyhow::Result<Vec<f32>>>,
    )>,
    path: std::path::PathBuf,
) {
    let mut decoder: Option<AudioDecoderState> = None;
    let mut fully_decoded_stream_index: Option<usize> = None;

    while let Ok((request, response_tx)) = rx.recv() {
        let result = read_audio_range(
            &path,
            &mut decoder,
            &mut fully_decoded_stream_index,
            &request,
        );
        let _ = response_tx.send(result);
    }
}

fn read_audio_range(
    path: &std::path::Path,
    decoder: &mut Option<AudioDecoderState>,
    fully_decoded_stream_index: &mut Option<usize>,
    request: &AudioPrefetchRequest,
) -> anyhow::Result<Vec<f32>> {
    if request.audio_index.is_empty() {
        anyhow::bail!("Audio index is empty");
    }

    if decoder
        .as_ref()
        .is_none_or(|state| state.stream_index != request.stream_index)
    {
        *decoder = Some(AudioDecoderState::new(
            path,
            request.stream_index,
            request.sample_rate,
            request.channels,
        )?);
        *fully_decoded_stream_index = None;
    }
    let state = decoder.as_mut().unwrap();
    if *fully_decoded_stream_index != Some(request.stream_index) {
        tracing::info!(
            "Audio prefetch: decoding full stream {} into memory",
            request.stream_index
        );
        state.fill_all()?;
        *fully_decoded_stream_index = Some(request.stream_index);
    }
    let start_idx = request.start;
    let sample_count = request.length;
    let end_idx = start_idx + sample_count;
    let track_end = request
        .audio_index
        .last()
        .map(|entry| entry.start_sample as usize)
        .unwrap_or(0);
    let covered =
        audio_buffer_range(state, request.channels).is_some_and(|(buffer_start, buffer_end)| {
            buffer_start <= start_idx && end_idx <= buffer_end
        });

    let total_f32 = sample_count * request.channels;
    let mut samples = vec![0.0f32; total_f32];

    if let Some(buffer_start) = state.buffer.start_sample {
        let buffer_end = buffer_start + state.buffer.samples.len() / request.channels;
        let copy_start = start_idx.max(buffer_start);
        let copy_end = end_idx.min(buffer_end);

        tracing::debug!(
            "Audio buffer range: {}-{}, requested range: {}-{}",
            buffer_start,
            buffer_end,
            start_idx,
            end_idx
        );

        if copy_start < copy_end {
            let src_offset = (copy_start - buffer_start) * request.channels;
            let dst_offset = (copy_start - start_idx) * request.channels;
            let copy_len = (copy_end - copy_start) * request.channels;
            for (i, sample) in state
                .buffer
                .samples
                .iter()
                .skip(src_offset)
                .take(copy_len)
                .enumerate()
            {
                samples[dst_offset + i] = *sample;
            }
        }
    }

    if !covered && start_idx < track_end {
        tracing::warn!(
            "Audio request was not fully covered after retries: requested range {}-{}, final buffer range {:?}",
            start_idx,
            end_idx,
            audio_buffer_range(state, request.channels)
        );
    }

    Ok(samples)
}
