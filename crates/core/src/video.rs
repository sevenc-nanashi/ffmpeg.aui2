use anyhow::Context;

pub struct VideoDecoderState {
    pub input: ffmpeg_next::format::context::Input,
    pub decoder: ffmpeg_next::decoder::Video,
    pub scaler: Option<ffmpeg_next::software::scaling::Context>,
    pub filter_graph: Option<ffmpeg_next::filter::Graph>,
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
        let codec = ffmpeg_next::codec::decoder::find(codec_ctx.id()).ok_or_else(|| {
            anyhow::anyhow!("Unsupported codec for video stream {}", stream_index)
        })?;
        let caps = codec.capabilities();
        let threading_kind =
            if caps.contains(ffmpeg_next::codec::capabilities::Capabilities::FRAME_THREADS) {
                ffmpeg_next::codec::threading::Type::Frame
            } else if caps.contains(ffmpeg_next::codec::capabilities::Capabilities::SLICE_THREADS) {
                ffmpeg_next::codec::threading::Type::Slice
            } else {
                ffmpeg_next::codec::threading::Type::None
            };
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
            filter_graph: None,
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

    pub fn ensure_scaler(&mut self, is_yuv422: bool) -> anyhow::Result<()> {
        if self.scaler.is_none() {
            let width = self.decoder.width();
            let height = self.decoder.height();
            let dst_fmt = if is_yuv422 {
                ffmpeg_next::format::Pixel::YUYV422
            } else {
                ffmpeg_next::format::Pixel::BGRA
            };
            self.scaler = Some(
                ffmpeg_next::software::scaling::Context::get(
                    self.decoder.format(),
                    width,
                    height,
                    dst_fmt,
                    width,
                    height,
                    ffmpeg_next::software::scaling::Flags::BILINEAR,
                )
                .context("Failed to create scaler")?,
            );
        }
        Ok(())
    }

    fn ensure_filter(&mut self, frame: &ffmpeg_next::frame::Video) -> anyhow::Result<()> {
        if self.filter_graph.is_some() {
            return Ok(());
        }

        let args = format!(
            "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect=1/1",
            frame.width(),
            frame.height(),
            ffmpeg_next::ffi::AVPixelFormat::from(frame.format()) as i32,
            self.time_base.numerator(),
            self.time_base.denominator(),
        );

        let mut graph = ffmpeg_next::filter::Graph::new();
        graph.add(
            &ffmpeg_next::filter::find("buffer").context("buffer filter not found")?,
            "in",
            &args,
        )?;
        graph.add(
            &ffmpeg_next::filter::find("buffersink").context("buffersink filter not found")?,
            "out",
            "",
        )?;
        graph.output("in", 0)?.input("out", 0)?.parse("vflip")?;
        graph.validate()?;

        self.filter_graph = Some(graph);
        Ok(())
    }

    fn apply_vflip(
        &mut self,
        frame: &ffmpeg_next::frame::Video,
    ) -> anyhow::Result<ffmpeg_next::frame::Video> {
        self.ensure_filter(frame)?;
        let graph = self.filter_graph.as_mut().unwrap();

        graph
            .get("in")
            .unwrap()
            .source()
            .add(frame)
            .context("Failed to add frame to filter graph")?;

        let mut output = ffmpeg_next::frame::Video::empty();
        graph
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut output)
            .context("Failed to get frame from filter graph")?;

        Ok(output)
    }

    /// Decode frame → (optionally vflip via avfilter) → scale → return pixel bytes. Does NOT touch prefetch.
    pub fn frame_to_bytes(
        &mut self,
        frame: &ffmpeg_next::frame::Video,
        is_yuv422: bool,
    ) -> anyhow::Result<Vec<u8>> {
        self.ensure_scaler(is_yuv422)?;

        let flipped;
        let frame_to_scale = if !is_yuv422 {
            flipped = self.apply_vflip(frame)?;
            &flipped
        } else {
            frame
        };

        let scaler = self.scaler.as_mut().unwrap();
        let mut scaled = ffmpeg_next::frame::Video::empty();
        scaler
            .run(frame_to_scale, &mut scaled)
            .context("Failed to scale frame")?;

        let w = scaled.width() as usize;
        let h = scaled.height() as usize;
        let data = scaled.data(0);
        let stride = scaled.stride(0);

        if is_yuv422 {
            let bpr = w * 2;
            let mut packed = vec![0u8; h * bpr];
            if stride == bpr {
                packed.copy_from_slice(&data[..h * bpr]);
            } else {
                packed.chunks_mut(bpr).enumerate().for_each(|(y, dst)| {
                    let src = y * stride;
                    dst.copy_from_slice(&data[src..src + bpr]);
                });
            }
            Ok(packed)
        } else {
            let bpr = w * 4;
            let mut output = vec![0u8; h * bpr];
            if stride == bpr {
                output.copy_from_slice(&data[..h * bpr]);
            } else {
                output.chunks_mut(bpr).enumerate().for_each(|(y, dst)| {
                    let src = y * stride;
                    dst.copy_from_slice(&data[src..src + bpr]);
                });
            }
            Ok(output)
        }
    }
}
