use crate::audio::AudioDecoderState;
use crate::index;

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
    tx: tokio::sync::mpsc::Sender<(
        AudioPrefetchRequest,
        tokio::sync::oneshot::Sender<anyhow::Result<Vec<f32>>>,
    )>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for AudioPrefetchHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// AudioDecoderState contains raw FFmpeg pointers (!Send), but is only
// accessed from one task at a time inside block_in_place.
struct SendDecoder(Option<AudioDecoderState>);
unsafe impl Send for SendDecoder {}

impl AudioPrefetchHandle {
    pub fn new(path: std::path::PathBuf) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let task = crate::runtime()
            .spawn(run_audio_prefetch_task(rx, path));
        Self { tx, task }
    }

    pub fn read(&self, request: AudioPrefetchRequest) -> anyhow::Result<Vec<f32>> {
        crate::runtime().block_on(async {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            self.tx
                .send((request, response_tx))
                .await
                .map_err(|_| anyhow::anyhow!("Audio prefetch task has stopped"))?;
            response_rx
                .await
                .map_err(|_| anyhow::anyhow!("Audio prefetch response channel disconnected"))?
        })
    }
}

async fn run_audio_prefetch_task(
    mut rx: tokio::sync::mpsc::Receiver<(
        AudioPrefetchRequest,
        tokio::sync::oneshot::Sender<anyhow::Result<Vec<f32>>>,
    )>,
    path: std::path::PathBuf,
) {
    let mut decoder = SendDecoder(None);

    while let Some((request, response_tx)) = rx.recv().await {
        let result = tokio::task::block_in_place(|| {
            read_audio_range(&path, &mut decoder.0, &request)
        });
        let _ = response_tx.send(result);
    }
}

fn audio_buffer_range(state: &AudioDecoderState, channels: usize) -> Option<(usize, usize)> {
    state
        .buffer
        .start_sample
        .map(|start| (start, start + state.buffer.samples.len() / channels))
}

fn read_audio_range(
    path: &std::path::Path,
    decoder: &mut Option<AudioDecoderState>,
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
    }
    let state = decoder.as_mut().unwrap();

    let start_idx = request.start;
    let sample_count = request.length;
    let end_idx = start_idx + sample_count;

    let covered = audio_buffer_range(state, request.channels)
        .is_some_and(|(buf_start, buf_end)| buf_start <= start_idx && end_idx <= buf_end);

    if !covered {
        let needs_seek = match state.buffer.start_sample {
            None => true,
            Some(buf_start) => {
                buf_start > start_idx
                    || (state.decoder_eof_sent
                        && audio_buffer_range(state, request.channels)
                            .is_none_or(|(_, buf_end)| buf_end < end_idx))
            }
        };

        if needs_seek {
            let entry = request
                .audio_index
                .iter()
                .rev()
                .find(|e| e.stream_index == request.stream_index && e.start_sample as usize <= start_idx);

            if let Some(entry) = entry {
                tracing::debug!(
                    "seeking to timestamp {} (sample {})",
                    entry.timestamp,
                    entry.start_sample
                );
                state.seek(entry.timestamp);
                state.buffer.start_sample = Some(entry.start_sample as usize);
            } else {
                tracing::info!("Audio: seeking to beginning");
                state.seek(0.0);
                state.buffer.start_sample = Some(0);
            }
        }

        state.fill_until(end_idx)?;
    }

    let track_end = request
        .audio_index
        .last()
        .map(|entry| entry.start_sample as usize)
        .unwrap_or(0);
    let covered_after =
        audio_buffer_range(state, request.channels).is_some_and(|(buffer_start, buffer_end)| {
            buffer_start <= start_idx && end_idx <= buffer_end
        });

    let total_f32 = sample_count * request.channels;
    let mut samples = vec![0.0f32; total_f32];

    if let Some(buffer_start) = state.buffer.start_sample {
        let buffer_end = buffer_start + state.buffer.samples.len() / request.channels;
        let copy_start = start_idx.max(buffer_start);
        let copy_end = end_idx.min(buffer_end);

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

    state.trim_before(start_idx);

    if !covered_after && end_idx < track_end {
        tracing::warn!(
            "Audio request was not fully covered: requested range {}-{}, final buffer range {:?}",
            start_idx,
            end_idx,
            audio_buffer_range(state, request.channels)
        );
    }

    Ok(samples)
}
