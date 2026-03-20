use anyhow::Context;

pub struct AudioDecoderState {
    pub input: ffmpeg_next::format::context::Input,
    pub decoder: ffmpeg_next::decoder::Audio,
    pub resampler: ffmpeg_next::software::resampling::Context,
    pub stream_index: usize,
    pub time_base: ffmpeg_next::Rational,
    pub sample_rate: u32,
    pub channels: usize,
    // Decoded packed f32 samples (interleaved by channel)
    pub buffer: Vec<f32>,
    // Global sample index of buffer[0]
    pub buffer_start: usize,
}

impl AudioDecoderState {
    pub fn new(
        path: &std::path::Path,
        stream_index: usize,
        sample_rate: u32,
        channels: usize,
    ) -> anyhow::Result<Self> {
        let input = ffmpeg_next::format::input(path)?;
        let time_base = input
            .stream(stream_index)
            .ok_or_else(|| anyhow::anyhow!("Audio stream {} not found", stream_index))?
            .time_base();
        let codec_params = input.stream(stream_index).unwrap().parameters();
        let codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(codec_params)?;
        let decoder = codec_ctx.decoder().audio()?;
        let resampler = ffmpeg_next::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Packed),
            decoder.channel_layout(),
            sample_rate,
        )?;
        Ok(Self {
            input,
            decoder,
            resampler,
            stream_index,
            time_base,
            sample_rate,
            channels,
            buffer: Vec::new(),
            buffer_start: 0,
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
        self.buffer.clear();
        self.buffer_start = (timestamp * self.sample_rate as f64) as usize;
    }

    pub fn fill_until(&mut self, end_sample: usize) -> anyhow::Result<()> {
        let stream_index = self.stream_index;
        let time_base = self.time_base;
        let sample_rate = self.sample_rate;
        let channels = self.channels;
        let mut frame = ffmpeg_next::frame::Audio::empty();
        let mut resampled = ffmpeg_next::frame::Audio::empty();

        while self.buffer_start + self.buffer.len() / channels < end_sample {
            // Send one relevant packet to the decoder
            let got_packet = {
                let input = &mut self.input;
                let decoder = &mut self.decoder;
                let mut found = false;
                for (stream, packet) in input.packets() {
                    if stream.index() != stream_index {
                        continue;
                    }
                    if decoder.send_packet(&packet).is_ok() {
                        found = true;
                        break;
                    }
                }
                found
            };

            if !got_packet {
                let _ = self.decoder.send_eof();
            }

            // Drain decoded frames
            loop {
                if self.decoder.receive_frame(&mut frame).is_err() {
                    break;
                }
                let frame_ts = frame.pts().unwrap_or(0) as f64 * f64::from(time_base);
                let frame_start = (frame_ts * sample_rate as f64) as usize;

                self.resampler
                    .run(&frame, &mut resampled)
                    .context("Failed to resample audio")?;

                let n_out = resampled.samples();
                let data = resampled.data(0);

                let skip = self.buffer_start.saturating_sub(frame_start);
                for i in skip..n_out {
                    for ch in 0..channels {
                        let in_off = (i * channels + ch) * 4;
                        if in_off + 4 <= data.len() {
                            self.buffer.push(f32::from_le_bytes([
                                data[in_off],
                                data[in_off + 1],
                                data[in_off + 2],
                                data[in_off + 3],
                            ]));
                        } else {
                            self.buffer.push(0.0);
                        }
                    }
                }
            }

            if !got_packet {
                break;
            }
        }

        Ok(())
    }
}
