use std::sync::atomic::Ordering;

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
