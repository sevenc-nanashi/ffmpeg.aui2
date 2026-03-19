use anyhow::Context;

pub struct VideoDecoderState {
    pub input: ffmpeg_next::format::context::Input,
    pub decoder: ffmpeg_next::decoder::Video,
    pub scaler: Option<ffmpeg_next::software::scaling::Context>,
    pub stream_index: usize,
    pub time_base: ffmpeg_next::Rational,
    pub current_ts: f64,
}

impl VideoDecoderState {
    pub fn new(path: &std::path::Path, stream_index: usize) -> anyhow::Result<Self> {
        let input = ffmpeg_next::format::input(path)?;
        let time_base = input
            .stream(stream_index)
            .ok_or_else(|| anyhow::anyhow!("Video stream {} not found", stream_index))?
            .time_base();
        let codec_params = input.stream(stream_index).unwrap().parameters();
        let mut codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(codec_params)?;
        let threading_kind = ffmpeg_next::codec::decoder::find(codec_ctx.id())
            .map(|codec| {
                let caps = codec.capabilities();
                if caps.contains(ffmpeg_next::codec::capabilities::Capabilities::FRAME_THREADS) {
                    ffmpeg_next::codec::threading::Type::Frame
                } else if caps
                    .contains(ffmpeg_next::codec::capabilities::Capabilities::SLICE_THREADS)
                {
                    ffmpeg_next::codec::threading::Type::Slice
                } else {
                    ffmpeg_next::codec::threading::Type::None
                }
            })
            .unwrap_or(ffmpeg_next::codec::threading::Type::None);
        tracing::info!(
            "Using {:?} threading for video stream {}",
            threading_kind,
            stream_index
        );
        codec_ctx.set_threading(ffmpeg_next::codec::threading::Config {
            kind: threading_kind,
            count: 0,
        });
        let decoder = codec_ctx.decoder().video()?;
        Ok(Self {
            input,
            decoder,
            scaler: None,
            stream_index,
            time_base,
            current_ts: f64::NEG_INFINITY,
        })
    }

    pub fn seek(&mut self, timestamp: f64) {
        let ts = (timestamp / f64::from(self.time_base)) as i64;
        unsafe {
            ffmpeg_next::ffi::avformat_seek_file(
                self.input.as_mut_ptr(),
                self.stream_index as i32,
                i64::MIN,
                ts,
                ts,
                0,
            );
        }
        self.decoder.flush();
        self.current_ts = f64::NEG_INFINITY;
    }

    pub fn decode_to(&mut self, target_ts: f64) -> anyhow::Result<ffmpeg_next::frame::Video> {
        let stream_index = self.stream_index;
        let time_base = self.time_base;
        let mut frame = ffmpeg_next::frame::Video::empty();

        let input = &mut self.input;
        let decoder = &mut self.decoder;
        let current_ts = &mut self.current_ts;

        'outer: for (stream, packet) in input.packets() {
            if stream.index() != stream_index {
                continue;
            }
            if decoder.send_packet(&packet).is_err() {
                continue;
            }
            while decoder.receive_frame(&mut frame).is_ok() {
                let pts = frame.pts().unwrap_or(0);
                let frame_ts = pts as f64 * f64::from(time_base);
                *current_ts = frame_ts;
                if frame_ts >= target_ts - 1e-6 {
                    break 'outer;
                }
            }
        }

        if *current_ts < target_ts - 1e-6 {
            let _ = decoder.send_eof();
            if decoder.receive_frame(&mut frame).is_ok() {
                *current_ts = target_ts;
            } else {
                anyhow::bail!("Frame at timestamp {} not found", target_ts);
            }
        }

        Ok(frame)
    }

    pub fn ensure_scaler(&mut self) -> anyhow::Result<()> {
        if self.scaler.is_none() {
            let width = self.decoder.width();
            let height = self.decoder.height();
            self.scaler = Some(
                ffmpeg_next::software::scaling::Context::get(
                    self.decoder.format(),
                    width,
                    height,
                    ffmpeg_next::format::Pixel::BGRA,
                    width,
                    height,
                    ffmpeg_next::software::scaling::Flags::BILINEAR,
                )
                .context("Failed to create scaler")?,
            );
        }
        Ok(())
    }
}
