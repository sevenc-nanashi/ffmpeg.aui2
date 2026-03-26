#[derive(Debug, Default)]
pub struct BufferedAudio {
    // Interleaved sample buffer start position in audio frames, not raw f32 elements.
    pub start_sample: Option<usize>,
    pub samples: std::collections::VecDeque<f32>,
}

pub struct AudioDecoderState {
    pub input: ffmpeg_next::format::context::Input,
    pub decoder: ffmpeg_next::decoder::Audio,
    pub resampler: ffmpeg_next::software::resampling::Context,
    pub stream_index: usize,
    pub channels: usize,
    pub buffer: BufferedAudio,
    pub decoder_eof_sent: bool,
    // // Decoded packed f32 samples (interleaved by channel)
    // pub buffer: Vec<f32>,
    // // Global sample index of buffer[0]
    // pub buffer_start: usize,
}

impl AudioDecoderState {
    fn create_resampler(
        decoder: &ffmpeg_next::decoder::Audio,
        sample_rate: u32,
    ) -> anyhow::Result<ffmpeg_next::software::resampling::Context> {
        Ok(ffmpeg_next::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Packed),
            decoder.channel_layout(),
            sample_rate,
        )?)
    }

    fn create_decoder(
        input: &ffmpeg_next::format::context::Input,
        stream_index: usize,
    ) -> anyhow::Result<ffmpeg_next::decoder::Audio> {
        let stream = input
            .stream(stream_index)
            .ok_or_else(|| anyhow::anyhow!("Audio stream {} not found", stream_index))?;
        let codec_params = stream.parameters();
        let codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(codec_params)?;
        let mut decoder = codec_ctx.decoder().audio()?;
        decoder.set_packet_time_base(stream.time_base());
        Ok(decoder)
    }

    pub fn new(
        path: &std::path::Path,
        stream_index: usize,
        sample_rate: u32,
        channels: usize,
    ) -> anyhow::Result<Self> {
        let input = ffmpeg_next::format::input(path)?;
        let decoder = Self::create_decoder(&input, stream_index)?;
        let resampler = Self::create_resampler(&decoder, sample_rate)?;
        Ok(Self {
            input,
            decoder,
            resampler,
            stream_index,
            channels,
            buffer: BufferedAudio::default(),
            decoder_eof_sent: false,
        })
    }

    pub fn fill_until(&mut self, end_sample: usize) -> anyhow::Result<()> {
        let channels = self.channels;
        let mut frame = ffmpeg_next::frame::Audio::empty();
        let mut resampled = ffmpeg_next::frame::Audio::empty();

        // while self.buffer.start_sample + self.buffer.samples.len() / self.channels < end_sample {
        while self.buffer.start_sample.map_or(true, |start| {
            start + self.buffer.samples.len() / channels < end_sample
        }) {
            match self.decoder.receive_frame(&mut frame) {
                Ok(_) => {
                    if self.buffer.samples.is_empty() && self.buffer.start_sample.is_none() {
                        self.buffer.start_sample = Some(0);
                    }
                    if frame.format()
                        == ffmpeg_next::format::Sample::F32(
                            ffmpeg_next::format::sample::Type::Planar,
                        )
                        && frame.channels() as usize == channels
                    {
                        let num_samples = frame.samples();
                        let planes: Vec<&[f32]> =
                            (0..channels).map(|c| frame.plane::<f32>(c)).collect();
                        self.buffer.samples.reserve(num_samples * channels);
                        for sample_index in 0..num_samples {
                            for plane in &planes {
                                self.buffer.samples.push_back(plane[sample_index]);
                            }
                        }
                    } else if frame.format()
                        == ffmpeg_next::format::Sample::F32(
                            ffmpeg_next::format::sample::Type::Packed,
                        )
                        && frame.channels() as usize == channels
                    {
                        self.buffer
                            .samples
                            .extend(frame.plane::<f32>(0).iter().copied());
                    } else {
                        self.resampler.run(&frame, &mut resampled)?;
                        let data = resampled.data(0);
                        let num_samples = resampled.samples();
                        self.buffer.samples.extend(
                            data[..num_samples * channels * 4]
                                .chunks_exact(4)
                                .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                        );
                    }
                }
                Err(e) => {
                    if e == ffmpeg_next::util::error::Error::Eof {
                        break;
                    } else if e
                        == (ffmpeg_next::util::error::Error::Other {
                            errno: ffmpeg_next::ffi::EAGAIN,
                        })
                    {
                        if !self.send_next_audio_packet()? {
                            break;
                        }
                    } else {
                        return Err(anyhow::anyhow!("Audio decode error: {e}"));
                    }
                }
            }
        }

        Ok(())
    }

    pub fn trim_before(&mut self, sample: usize) {
        if let Some(start) = self.buffer.start_sample {
            if sample > start {
                let drain_frames =
                    (sample - start).min(self.buffer.samples.len() / self.channels);
                let drain_count = drain_frames * self.channels;
                self.buffer.samples.drain(..drain_count);
                self.buffer.start_sample = Some(start + drain_frames);
            }
        }
    }

    pub fn seek(&mut self, timestamp: f64) {
        let time_base = self.input.stream(self.stream_index).unwrap().time_base();
        let ts = (timestamp / f64::from(time_base)) as i64;
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
        self.buffer.start_sample = None;
        self.buffer.samples.clear();
        self.decoder_eof_sent = false;
    }

    fn send_next_audio_packet(&mut self) -> anyhow::Result<bool> {
        for (stream, packet) in self.input.packets() {
            if stream.index() != self.stream_index {
                continue;
            }
            self.decoder.send_packet(&packet)?;
            return Ok(true);
        }

        if !self.decoder_eof_sent {
            self.decoder.send_eof()?;
            self.decoder_eof_sent = true;
            return Ok(true);
        }

        Ok(false)
    }
}
