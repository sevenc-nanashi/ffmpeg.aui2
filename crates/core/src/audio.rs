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
                        for sample_index in 0..num_samples {
                            for channel in 0..channels {
                                self.buffer
                                    .samples
                                    .push_back(frame.plane::<f32>(channel)[sample_index]);
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
                        let mut samples = vec![0f32; num_samples * channels];
                        for i in 0..num_samples * channels {
                            samples[i] =
                                f32::from_le_bytes(data[i * 4..(i + 1) * 4].try_into().unwrap());
                        }
                        self.buffer.samples.extend(samples);
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

    pub fn fill_all(&mut self) -> anyhow::Result<()> {
        self.fill_until(usize::MAX)
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
