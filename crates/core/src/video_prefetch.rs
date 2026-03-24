use crate::index;
use crate::video::VideoDecoderState;

#[derive(Clone)]
pub struct PrefetchConfig {
    pub video_index: std::sync::Arc<Vec<index::VideoEntry>>,
    pub output_format: index::VideoOutputFormat,
    pub width: u32,
    pub height: u32,
}

pub struct PrefetchHandle {
    pub cache: std::sync::Arc<dashmap::DashMap<usize, Vec<u8>>>,
    pub config_tx: tokio::sync::watch::Sender<Option<PrefetchConfig>>,
    pub position_tx: tokio::sync::watch::Sender<usize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PrefetchHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// VideoDecoderState contains raw FFmpeg pointers (!Send), but is only
// accessed from one task at a time inside block_in_place.
struct SendDecoder(Option<VideoDecoderState>);
unsafe impl Send for SendDecoder {}

impl PrefetchHandle {
    pub fn new(path: std::path::PathBuf) -> Self {
        let cache = std::sync::Arc::new(dashmap::DashMap::new());
        let (config_tx, config_rx) = tokio::sync::watch::channel(None::<PrefetchConfig>);
        let (position_tx, position_rx) = tokio::sync::watch::channel(0usize);

        let cache_clone = std::sync::Arc::clone(&cache);
        let task = crate::runtime()
            .spawn(run_prefetch_task(config_rx, position_rx, cache_clone, path));

        Self {
            cache,
            config_tx,
            position_tx,
            task,
        }
    }
}

async fn run_prefetch_task(
    mut config_rx: tokio::sync::watch::Receiver<Option<PrefetchConfig>>,
    mut position_rx: tokio::sync::watch::Receiver<usize>,
    cache: std::sync::Arc<dashmap::DashMap<usize, Vec<u8>>>,
    path: std::path::PathBuf,
) {
    let mut decoder = SendDecoder(None);

    'outer: loop {
        // Wait for a valid config
        let cfg = loop {
            if let Some(cfg) = config_rx.borrow_and_update().clone() {
                break cfg;
            }
            match config_rx.changed().await {
                Ok(()) => {}
                Err(_) => return,
            }
        };

        let bytes_per_pixel = match cfg.output_format {
            index::VideoOutputFormat::Yuy2 => 2usize,
            index::VideoOutputFormat::Bgra => 4,
            index::VideoOutputFormat::Hf64 => 8,
        };
        let frame_bytes = cfg.width as usize * cfg.height as usize * bytes_per_pixel;
        let prefetch_limit_bytes = crate::CONFIG
            .get()
            .map_or(512, |c| c.prefetch_buffer_mb) as usize
            * 1024
            * 1024;
        let prefetch_frames = if frame_bytes > 0 {
            prefetch_limit_bytes / frame_bytes
        } else {
            0
        };

        let mut current = *position_rx.borrow_and_update();
        let mut end_frame = current + prefetch_frames;
        let next_frame = current + 1;
        let mut did_work = false;

        for (i, entry) in cfg.video_index[next_frame.min(cfg.video_index.len())..]
            .iter()
            .enumerate()
        {
            if config_rx.has_changed().unwrap_or(false) {
                continue 'outer;
            }

            let new_current = *position_rx.borrow();
            if new_current != current {
                if new_current > end_frame {
                    break;
                }
                current = new_current;
                end_frame = current + prefetch_frames;
            }

            let frame_idx = next_frame + i;
            if frame_idx > end_frame {
                break;
            }

            if cache.contains_key(&frame_idx) {
                continue;
            }

            let entry = entry.clone();
            let output_format = cfg.output_format.clone();
            let path = path.clone();

            let result = tokio::task::block_in_place(|| {
                let decoder = &mut decoder.0;

                if decoder
                    .as_ref()
                    .is_none_or(|d| d.stream_index != entry.stream_index)
                {
                    match VideoDecoderState::new(&path, entry.stream_index) {
                        Ok(d) => *decoder = Some(d),
                        Err(e) => {
                            tracing::warn!("Prefetch: decoder init failed: {e}");
                            return None;
                        }
                    }
                }
                let state = decoder.as_mut().unwrap();

                if entry.timestamp < state.current_ts - 1e-6 {
                    state.seek(entry.last_keyframe_timestamp);
                }

                match state.decode_to(entry.timestamp) {
                    Ok(frame) => match state.frame_to_bytes(&frame, &output_format) {
                        Ok(data) => Some((frame_idx, entry.timestamp, data)),
                        Err(e) => {
                            tracing::warn!("Prefetch: scale failed at frame {frame_idx}: {e}");
                            None
                        }
                    },
                    Err(e) => {
                        tracing::warn!("Prefetch: decode failed at frame {frame_idx}: {e}");
                        None
                    }
                }
            });

            match result {
                Some((idx, ts, data)) => {
                    tracing::debug!("Prefetch: cached frame {idx} at timestamp {ts}");
                    cache.insert(idx, data);
                    did_work = true;
                }
                None => break,
            }
        }

        if !did_work {
            // Nothing to prefetch — wait for position or config change
            let result = tokio::select! {
                r = config_rx.changed() => r.map(|_| true),
                r = position_rx.changed() => r.map(|_| false),
            };
            match result {
                Ok(true) => continue 'outer, // config changed
                Ok(false) => {}              // position changed
                Err(_) => return,            // senders dropped
            }
        }
    }
}
