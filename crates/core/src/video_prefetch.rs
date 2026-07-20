use crate::index;
use crate::video::VideoDecoderState;

pub(crate) struct CachedVideoFrame(pub ffmpeg_next::frame::Video);
// SAFETY: cached frames are only moved between threads or accessed through shared references.
// AVFrame clones retain their immutable data through FFmpeg's thread-safe AVBufferRef counting.
unsafe impl Send for CachedVideoFrame {}
// SAFETY: no code in this crate mutates an AVFrame after wrapping it in CachedVideoFrame.
unsafe impl Sync for CachedVideoFrame {}

impl Clone for CachedVideoFrame {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

pub(crate) enum PrefetchedFrame {
    Yuy2(CachedVideoFrame, Option<PrefetchReservation>),
    Bytes(Vec<u8>, PrefetchReservation),
}

#[derive(Clone)]
pub struct PrefetchConfig {
    pub video_index: std::sync::Arc<Vec<index::VideoEntry>>,
    pub output_format: index::VideoOutputFormat,
    pub width: u32,
    pub height: u32,
}

pub struct PrefetchHandle {
    pub cache: std::sync::Arc<dashmap::DashMap<usize, PrefetchedFrame>>,
    pub config_tx: tokio::sync::watch::Sender<Option<PrefetchConfig>>,
    pub position_tx: tokio::sync::watch::Sender<Option<usize>>,
    reusable_buffer: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    ready: std::sync::Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
    last_frame: std::sync::Mutex<Option<(usize, CachedVideoFrame)>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PrefetchHandle {
    fn drop(&mut self) {
        self.task.abort();
        self.clear_cache();
    }
}

// VideoDecoderState contains raw FFmpeg pointers (!Send), but is only
// accessed from one task at a time inside block_in_place.
struct SendDecoder(Option<VideoDecoderState>);
unsafe impl Send for SendDecoder {}

pub(crate) struct PrefetchReservation {
    bytes: usize,
}

impl PrefetchReservation {
    fn try_new(bytes: usize, limit: usize) -> Option<Self> {
        let counter = &crate::PREFETCH_TOTAL_BYTES;
        let mut current = counter.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            let next = current.checked_add(bytes)?;
            if next > limit {
                return None;
            }
            match counter.compare_exchange_weak(
                current,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self { bytes }),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for PrefetchReservation {
    fn drop(&mut self) {
        crate::PREFETCH_TOTAL_BYTES.fetch_sub(self.bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

impl PrefetchHandle {
    pub fn clear_cache(&self) {
        self.cache.clear();
        *self.reusable_buffer.lock().unwrap() = None;
        *self.last_frame.lock().unwrap() = None;
    }

    pub fn recycle_buffer(&self, buffer: Vec<u8>) {
        let mut reusable_buffer = self.reusable_buffer.lock().unwrap();
        if reusable_buffer
            .as_ref()
            .is_none_or(|existing| existing.capacity() < buffer.capacity())
        {
            *reusable_buffer = Some(buffer);
        }
    }

    pub fn take_frame(&self, frame: usize) -> Option<PrefetchedFrame> {
        if let Some((_, cached)) = self.cache.remove(&frame) {
            return Some(cached);
        }
        self.last_frame
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(cached_frame, _)| *cached_frame == frame)
            .map(|(_, cached)| PrefetchedFrame::Yuy2(cached.clone(), None))
    }

    pub fn take_frame_or_wait(
        &self,
        frame: usize,
        timeout: std::time::Duration,
    ) -> Option<PrefetchedFrame> {
        if let Some(cached) = self.take_frame(frame) {
            return Some(cached);
        }

        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .expect("Prefetch wait deadline overflow");
        let (generation, ready) = &*self.ready;
        let mut guard = generation.lock().unwrap();
        loop {
            if let Some(cached) = self.take_frame(frame) {
                return Some(cached);
            }

            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            let observed = *guard;
            let (next_guard, wait_result) = ready
                .wait_timeout_while(guard, remaining, |current| *current == observed)
                .unwrap();
            guard = next_guard;
            if wait_result.timed_out() {
                return self.take_frame(frame);
            }
        }
    }

    pub fn remember_frame(&self, frame: usize, cached: CachedVideoFrame) {
        *self.last_frame.lock().unwrap() = Some((frame, cached));
    }

    pub fn new(path: std::path::PathBuf) -> Self {
        let cache = std::sync::Arc::new(dashmap::DashMap::new());
        let reusable_buffer = std::sync::Arc::new(std::sync::Mutex::new(None));
        let ready = std::sync::Arc::new((std::sync::Mutex::new(0u64), std::sync::Condvar::new()));
        let (config_tx, config_rx) = tokio::sync::watch::channel(None::<PrefetchConfig>);
        let (position_tx, position_rx) = tokio::sync::watch::channel(None::<usize>);

        let cache_clone = std::sync::Arc::clone(&cache);
        let reusable_buffer_clone = std::sync::Arc::clone(&reusable_buffer);
        let ready_clone = std::sync::Arc::clone(&ready);
        let task = crate::runtime().spawn(run_prefetch_task(
            config_rx,
            position_rx,
            cache_clone,
            reusable_buffer_clone,
            ready_clone,
            path,
        ));

        Self {
            cache,
            config_tx,
            position_tx,
            reusable_buffer,
            ready,
            last_frame: std::sync::Mutex::new(None),
            task,
        }
    }
}

async fn run_prefetch_task(
    mut config_rx: tokio::sync::watch::Receiver<Option<PrefetchConfig>>,
    mut position_rx: tokio::sync::watch::Receiver<Option<usize>>,
    cache: std::sync::Arc<dashmap::DashMap<usize, PrefetchedFrame>>,
    reusable_buffer: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    ready: std::sync::Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
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
        let frame_bytes = (cfg.width as usize)
            .checked_mul(cfg.height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .expect("Video frame size overflow");
        assert_ne!(frame_bytes, 0, "Video frame size must not be zero");

        let cfg_ref = crate::config();
        let per_video_limit = cfg_ref.prefetch_buffer_mb as usize * 1024 * 1024;
        let total_limit = cfg_ref.prefetch_total_buffer_mb as usize * 1024 * 1024;
        let frame_count_limit = cfg_ref.prefetch_frames as usize;

        let mut prefetch_frames = per_video_limit / frame_bytes;
        if frame_count_limit > 0 {
            prefetch_frames = prefetch_frames.min(frame_count_limit);
        }

        let mut current = *position_rx.borrow_and_update();
        let next_frame = current.map_or(0, |frame| frame.saturating_add(1));
        let mut end_frame = next_frame.saturating_add(prefetch_frames);
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
                let new_next_frame = new_current.map_or(0, |frame| frame.saturating_add(1));
                if new_next_frame > end_frame {
                    break;
                }
                current = new_current;
                end_frame = new_next_frame.saturating_add(prefetch_frames);
            }

            let frame_idx = next_frame + i;
            if frame_idx >= end_frame {
                break;
            }
            if current.is_some_and(|current| frame_idx <= current) {
                continue;
            }

            if cache.contains_key(&frame_idx) {
                continue;
            }

            let Some(reservation) = PrefetchReservation::try_new(frame_bytes, total_limit) else {
                break;
            };

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
                    Ok(frame) if matches!(cfg.output_format, index::VideoOutputFormat::Yuy2) => {
                        Some((
                            frame_idx,
                            entry.timestamp,
                            PrefetchedFrame::Yuy2(CachedVideoFrame(frame), Some(reservation)),
                        ))
                    }
                    Ok(frame) => {
                        let output = reusable_buffer.lock().unwrap().take().unwrap_or_default();
                        match state.frame_to_bytes(&frame, &cfg.output_format, output) {
                            Ok(data) => Some((
                                frame_idx,
                                entry.timestamp,
                                PrefetchedFrame::Bytes(data, reservation),
                            )),
                            Err(e) => {
                                tracing::warn!("Prefetch: scale failed at frame {frame_idx}: {e}");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Prefetch: decode failed at frame {frame_idx}: {e}");
                        None
                    }
                }
            });

            match result {
                Some((idx, ts, data)) => {
                    tracing::debug!("Prefetch: cached frame {idx} at timestamp {ts}");
                    let (generation, condition) = &*ready;
                    let mut generation = generation.lock().unwrap();
                    cache.insert(idx, data);
                    *generation = generation.wrapping_add(1);
                    condition.notify_all();
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
