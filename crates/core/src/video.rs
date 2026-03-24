use crate::config::HwAccel;
use crate::index::VideoOutputFormat;
use anyhow::Context;
use aviutl2::f16;

pub struct VideoDecoderState {
    pub input: ffmpeg_next::format::context::Input,
    pub decoder: ffmpeg_next::decoder::Video,
    pub scaler: Option<ffmpeg_next::software::scaling::Context>,
    pub filter_graph: Option<ffmpeg_next::filter::Graph>,
    pub hdr_filter_graph: Option<ffmpeg_next::filter::Graph>,
    pub stream_index: usize,
    pub time_base: ffmpeg_next::Rational,
    pub current_ts: f64,
    is_hw: bool,
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
        let hwaccel = crate::CONFIG
            .get()
            .map(|c| c.hwaccel.clone())
            .unwrap_or(HwAccel::None);
        let is_hw = try_setup_hwaccel(&codec, &mut codec_ctx, &hwaccel);
        let decoder = codec_ctx.decoder().video()?;
        Ok(Self {
            input,
            decoder,
            scaler: None,
            filter_graph: None,
            hdr_filter_graph: None,
            stream_index,
            time_base,
            current_ts: f64::NEG_INFINITY,
            is_hw,
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

        let frame = if self.is_hw {
            download_hw_frame(frame)?
        } else {
            frame
        };
        Ok(frame)
    }

    pub fn ensure_scaler(
        &mut self,
        output_format: &VideoOutputFormat,
        src_fmt: ffmpeg_next::format::Pixel,
    ) -> anyhow::Result<()> {
        if self.scaler.is_none() {
            let width = self.decoder.width();
            let height = self.decoder.height();
            let dst_fmt = match output_format {
                VideoOutputFormat::Yuy2 => ffmpeg_next::format::Pixel::YUYV422,
                VideoOutputFormat::Bgra => ffmpeg_next::format::Pixel::BGRA,
                VideoOutputFormat::Hf64 => ffmpeg_next::format::Pixel::GBRPF32LE,
            };
            self.scaler = Some(
                ffmpeg_next::software::scaling::Context::get(
                    src_fmt,
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

    fn configure_rgb_scaler_colorspace(
        scaler: &mut ffmpeg_next::software::scaling::Context,
        frame: &ffmpeg_next::frame::Video,
    ) -> anyhow::Result<()> {
        let colorspace = swscale_colorspace(frame);
        let range = swscale_range(frame.color_range());

        unsafe {
            let coeffs = ffmpeg_next::ffi::sws_getCoefficients(colorspace);
            anyhow::ensure!(!coeffs.is_null(), "Failed to resolve swscale coefficients");

            let result = ffmpeg_next::ffi::sws_setColorspaceDetails(
                scaler.as_mut_ptr(),
                coeffs,
                range,
                coeffs,
                1,
                0,
                1 << 16,
                1 << 16,
            );
            anyhow::ensure!(
                result >= 0,
                "Failed to configure swscale colorspace details"
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

    fn ensure_hdr_filter(&mut self, frame: &ffmpeg_next::frame::Video) -> anyhow::Result<()> {
        if self.hdr_filter_graph.is_some() {
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
        graph
            .output("in", 0)?
            .input("out", 0)?
            .parse("zscale=transfer=linear:range=full:rangein=full,format=pix_fmts=gbrpf32le")?;
        graph.validate()?;

        self.hdr_filter_graph = Some(graph);
        Ok(())
    }

    fn apply_hdr_to_hf64(
        &mut self,
        frame: &ffmpeg_next::frame::Video,
    ) -> anyhow::Result<ffmpeg_next::frame::Video> {
        self.ensure_hdr_filter(frame)?;
        let graph = self.hdr_filter_graph.as_mut().unwrap();

        graph
            .get("in")
            .unwrap()
            .source()
            .add(frame)
            .context("Failed to add HDR frame to filter graph")?;

        let mut output = ffmpeg_next::frame::Video::empty();
        graph
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut output)
            .context("Failed to get HDR frame from filter graph")?;

        Ok(output)
    }

    /// Decode frame → (optionally vflip via avfilter) → scale → return pixel bytes. Does NOT touch prefetch.
    pub fn frame_to_bytes(
        &mut self,
        frame: &ffmpeg_next::frame::Video,
        output_format: &VideoOutputFormat,
    ) -> anyhow::Result<Vec<u8>> {
        if matches!(output_format, VideoOutputFormat::Hf64) && is_hdr_transfer(frame) {
            let scaled = self.apply_hdr_to_hf64(frame)?;
            return Self::hf64_frame_to_bytes(&scaled);
        }

        self.ensure_scaler(output_format, frame.format())?;

        let needs_vflip = matches!(output_format, VideoOutputFormat::Bgra);
        let flipped;
        let frame_to_scale = if needs_vflip {
            flipped = self.apply_vflip(frame)?;
            &flipped
        } else {
            frame
        };

        let scaler = self.scaler.as_mut().unwrap();
        let is_rgb_output = matches!(
            output_format,
            VideoOutputFormat::Bgra | VideoOutputFormat::Hf64
        );
        if is_rgb_output {
            Self::configure_rgb_scaler_colorspace(scaler, frame_to_scale)?;
        }
        let mut scaled = ffmpeg_next::frame::Video::empty();
        scaler
            .run(frame_to_scale, &mut scaled)
            .context("Failed to scale frame")?;

        let w = scaled.width() as usize;
        let h = scaled.height() as usize;

        match output_format {
            VideoOutputFormat::Yuy2 => {
                let data = scaled.data(0);
                let stride = scaled.stride(0);
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
            }
            VideoOutputFormat::Bgra => {
                let data = scaled.data(0);
                let stride = scaled.stride(0);
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
            VideoOutputFormat::Hf64 => Self::hf64_frame_to_bytes(&scaled),
        }
    }

    fn hf64_frame_to_bytes(scaled: &ffmpeg_next::frame::Video) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            scaled.format() == ffmpeg_next::format::Pixel::GBRPF32LE,
            "Unexpected pixel format for Hf64 conversion: {:?}",
            scaled.format()
        );

        let w = scaled.width() as usize;
        let h = scaled.height() as usize;
        let g_data = scaled.data(0);
        let b_data = scaled.data(1);
        let r_data = scaled.data(2);
        let g_stride = scaled.stride(0);
        let b_stride = scaled.stride(1);
        let r_stride = scaled.stride(2);
        let alpha_bytes = f16::from_f32(1.0).to_le_bytes();
        let mut output = vec![0u8; h * w * 8];

        for y in 0..h {
            for x in 0..w {
                let r = f32::from_le_bytes(
                    r_data[y * r_stride + x * 4..y * r_stride + x * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                let g = f32::from_le_bytes(
                    g_data[y * g_stride + x * 4..y * g_stride + x * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                let b = f32::from_le_bytes(
                    b_data[y * b_stride + x * 4..y * b_stride + x * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                let off = (y * w + x) * 8;
                output[off..off + 2].copy_from_slice(&f16::from_f32(r).to_le_bytes());
                output[off + 2..off + 4].copy_from_slice(&f16::from_f32(g).to_le_bytes());
                output[off + 4..off + 6].copy_from_slice(&f16::from_f32(b).to_le_bytes());
                output[off + 6..off + 8].copy_from_slice(&alpha_bytes);
            }
        }

        Ok(output)
    }
}

fn codec_supports_hwaccel(
    codec: *const ffmpeg_next::ffi::AVCodec,
    hw_type: ffmpeg_next::ffi::AVHWDeviceType,
) -> bool {
    unsafe {
        let mut i = 0;
        loop {
            let config = ffmpeg_next::ffi::avcodec_get_hw_config(codec, i);
            if config.is_null() {
                return false;
            }
            if (*config).device_type == hw_type {
                return true;
            }
            i += 1;
        }
    }
}

fn try_setup_hwaccel(
    codec: &ffmpeg_next::codec::codec::Codec,
    codec_ctx: &mut ffmpeg_next::codec::context::Context,
    hwaccel: &HwAccel,
) -> bool {
    let types_to_try: &[ffmpeg_next::ffi::AVHWDeviceType] = match hwaccel {
        HwAccel::None => return false,
        HwAccel::Auto => &[
            ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2,
        ],
        HwAccel::D3d11va => &[ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA],
        HwAccel::Dxva2 => &[ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2],
        HwAccel::Cuda => &[ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA],
    };

    for &hw_type in types_to_try {
        if !codec_supports_hwaccel(unsafe { codec.as_ptr() }, hw_type) {
            tracing::debug!("Codec does not support {:?}", hw_type);
            continue;
        }

        let mut hw_device_ctx: *mut ffmpeg_next::ffi::AVBufferRef = std::ptr::null_mut();
        let ret = unsafe {
            ffmpeg_next::ffi::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                hw_type,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ret < 0 {
            tracing::warn!("Failed to create HW device context for {:?}: {}", hw_type, ret);
            continue;
        }
        unsafe {
            (*codec_ctx.as_mut_ptr()).hw_device_ctx =
                ffmpeg_next::ffi::av_buffer_ref(hw_device_ctx);
            ffmpeg_next::ffi::av_buffer_unref(&mut hw_device_ctx);
        }
        tracing::info!("Hardware acceleration enabled: {:?}", hw_type);
        return true;
    }

    false
}

fn download_hw_frame(
    hw_frame: ffmpeg_next::frame::Video,
) -> anyhow::Result<ffmpeg_next::frame::Video> {
    let mut sw_frame = ffmpeg_next::frame::Video::empty();
    let ret = unsafe {
        ffmpeg_next::ffi::av_hwframe_transfer_data(sw_frame.as_mut_ptr(), hw_frame.as_ptr(), 0)
    };
    anyhow::ensure!(ret >= 0, "Failed to transfer HW frame to SW memory: {}", ret);
    unsafe {
        (*sw_frame.as_mut_ptr()).pts = (*hw_frame.as_ptr()).pts;
    }
    Ok(sw_frame)
}

fn swscale_colorspace(frame: &ffmpeg_next::frame::Video) -> i32 {
    match frame.color_space() {
        ffmpeg_next::color::Space::BT709 => ffmpeg_next::ffi::SWS_CS_ITU709,
        ffmpeg_next::color::Space::FCC => ffmpeg_next::ffi::SWS_CS_FCC,
        ffmpeg_next::color::Space::SMPTE240M => ffmpeg_next::ffi::SWS_CS_SMPTE240M,
        ffmpeg_next::color::Space::BT2020NCL | ffmpeg_next::color::Space::BT2020CL => {
            ffmpeg_next::ffi::SWS_CS_BT2020
        }
        ffmpeg_next::color::Space::BT470BG | ffmpeg_next::color::Space::SMPTE170M => {
            ffmpeg_next::ffi::SWS_CS_ITU601
        }
        ffmpeg_next::color::Space::Unspecified => {
            if frame.width() >= 1280 || frame.height() > 576 {
                ffmpeg_next::ffi::SWS_CS_ITU709
            } else {
                ffmpeg_next::ffi::SWS_CS_ITU601
            }
        }
        _ => ffmpeg_next::ffi::SWS_CS_DEFAULT,
    }
}

fn swscale_range(range: ffmpeg_next::color::Range) -> i32 {
    match range {
        ffmpeg_next::color::Range::JPEG => 1,
        ffmpeg_next::color::Range::MPEG | ffmpeg_next::color::Range::Unspecified => 0,
    }
}

fn is_hdr_transfer(frame: &ffmpeg_next::frame::Video) -> bool {
    matches!(
        frame.color_transfer_characteristic(),
        ffmpeg_next::color::TransferCharacteristic::SMPTE2084
            | ffmpeg_next::color::TransferCharacteristic::ARIB_STD_B67
    )
}
