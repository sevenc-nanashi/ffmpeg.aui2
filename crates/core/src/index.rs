use anyhow::Context;

pub static VERSION_NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexHeaderFile {
    pub filename: String,
    pub filehash: u64,
    pub version_nonce: u64,
}

#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct IndexContentFile {
    pub tracks: Vec<TrackInfo>,
    pub entries: Vec<IndexEntry>,
}

impl IndexContentFile {
    pub fn video_tracks(&self) -> impl Iterator<Item = &VideoTrackInfo> {
        self.tracks.iter().filter_map(|t| {
            if let TrackInfo::Video(v) = t {
                Some(v)
            } else {
                None
            }
        })
    }
    pub fn audio_tracks(&self) -> impl Iterator<Item = &AudioTrackInfo> {
        self.tracks.iter().filter_map(|t| {
            if let TrackInfo::Audio(a) = t {
                Some(a)
            } else {
                None
            }
        })
    }
}

#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum TrackInfo {
    Video(VideoTrackInfo),
    Audio(AudioTrackInfo),
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum VideoOutputFormat {
    Bgra,
    Yuy2,
    Hf64,
}

#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct VideoTrackInfo {
    pub stream_index: usize,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub duration: f64,
    pub output_format: VideoOutputFormat,
}
#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct AudioTrackInfo {
    pub stream_index: usize,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: u64,
    pub duration: f64,
}

#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct VideoEntry {
    pub stream_index: usize,
    pub keyframe: bool,
    pub position: u64,
    pub timestamp: f64,
    pub duration: i64,
    pub last_keyframe_timestamp: f64,
}

#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct AudioEntry {
    pub stream_index: usize,
    pub position: u64,
    pub timestamp: f64,
    pub start_sample: u64,
}

#[derive(
    Debug,
    Clone,
    rkyv::Serialize,
    rkyv::Deserialize,
    rkyv::Archive,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum IndexEntry {
    Video(VideoEntry),
    Audio(AudioEntry),
}

impl IndexEntry {
    pub fn stream_index(&self) -> usize {
        match self {
            IndexEntry::Video(entry) => entry.stream_index,
            IndexEntry::Audio(entry) => entry.stream_index,
        }
    }
    pub fn as_video(&self) -> Option<&VideoEntry> {
        if let IndexEntry::Video(entry) = self {
            Some(entry)
        } else {
            None
        }
    }
    pub fn as_audio(&self) -> Option<&AudioEntry> {
        if let IndexEntry::Audio(entry) = self {
            Some(entry)
        } else {
            None
        }
    }
}

pub fn create_index(
    path: &std::path::Path,
    header_path: &std::path::Path,
    content_path: &std::path::Path,
    filehash: u64,
    json_index: bool,
) -> aviutl2::AnyResult<IndexContentFile> {
    fn timestamp_to_seconds(timestamp: i64, time_base: ffmpeg_next::Rational) -> f64 {
        (timestamp as f64) * f64::from(time_base)
    }

    fn stream_duration_seconds(duration: i64, time_base: ffmpeg_next::Rational) -> f64 {
        if duration == ffmpeg_next::ffi::AV_NOPTS_VALUE || duration <= 0 {
            0.0
        } else {
            timestamp_to_seconds(duration, time_base)
        }
    }

    fn packet_timestamp(packet: &ffmpeg_next::Packet) -> i64 {
        packet.pts().or_else(|| packet.dts()).unwrap_or(0)
    }

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    if let Some(parent) = header_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create index directory")?;
    }

    let mut input = ffmpeg_next::format::input(path).context("Failed to open file for indexing")?;
    let mut entries = Vec::new();
    let mut tracks = input
        .streams()
        .filter_map(|stream| match stream.parameters().medium() {
            ffmpeg_next::media::Type::Video => {
                let codec =
                    ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
                        .ok()?;
                let video = codec.decoder().video().ok()?;
                let duration = stream_duration_seconds(stream.duration(), stream.time_base());
                tracing::info!(
                    "Found video stream {}: {}x{}, frames={}, duration={:.2}s, format={:?}",
                    stream.index(),
                    video.width(),
                    video.height(),
                    stream.frames(),
                    duration,
                    video.format()
                );
                Some(TrackInfo::Video(VideoTrackInfo {
                    stream_index: stream.index(),
                    width: video.width(),
                    height: video.height(),
                    frames: stream.frames().max(0) as u64,
                    duration,
                    output_format: {
                        let is_hdr = matches!(
                            video.color_transfer_characteristic(),
                            ffmpeg_next::color::TransferCharacteristic::SMPTE2084
                            | ffmpeg_next::color::TransferCharacteristic::ARIB_STD_B67
                        );
                        // YUVっぽいフォーマットはYUV422に変換して扱うようにする（そのほうが速い）
                        let is_yuv = matches!(
                            video.format(),
                            ffmpeg_next::format::Pixel::YUV422P
                            | ffmpeg_next::format::Pixel::YUYV422
                            | ffmpeg_next::format::Pixel::UYVY422
                            | ffmpeg_next::format::Pixel::YUV422P10
                            | ffmpeg_next::format::Pixel::YUV422P12
                            | ffmpeg_next::format::Pixel::YUV422P16
                            | ffmpeg_next::format::Pixel::YUV422P9
                            | ffmpeg_next::format::Pixel::YUV422P14
                            | ffmpeg_next::format::Pixel::YUV420P
                            | ffmpeg_next::format::Pixel::YUV420P10
                            | ffmpeg_next::format::Pixel::YUV420P12
                            | ffmpeg_next::format::Pixel::YUV420P16
                            | ffmpeg_next::format::Pixel::YUV420P9
                            | ffmpeg_next::format::Pixel::YUV420P14
                            | ffmpeg_next::format::Pixel::YUV422P16BE
                            | ffmpeg_next::format::Pixel::YUV422P16LE
                            | ffmpeg_next::format::Pixel::YUV422P10BE
                            | ffmpeg_next::format::Pixel::YUV422P10LE
                            | ffmpeg_next::format::Pixel::YUV422P12BE
                            | ffmpeg_next::format::Pixel::YUV422P12LE
                            | ffmpeg_next::format::Pixel::YUV422P14BE
                            | ffmpeg_next::format::Pixel::YUV422P14LE
                            | ffmpeg_next::format::Pixel::YUV422P9BE
                            | ffmpeg_next::format::Pixel::YUV422P9LE
                        );
                        let format = if is_hdr {
                            VideoOutputFormat::Hf64
                        } else if is_yuv {
                            VideoOutputFormat::Yuy2
                        } else {
                            VideoOutputFormat::Bgra
                        };
                        tracing::info!(
                            "Determined output format for stream {}: {:?} (is_hdr={}, is_yuv={})",
                            stream.index(),
                            format,
                            is_hdr,
                            is_yuv
                        );
                        format
                    },
                }))
            }
            ffmpeg_next::media::Type::Audio => {
                let codec =
                    ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
                        .ok()?;
                let audio = codec.decoder().audio().ok()?;
                let duration = stream_duration_seconds(stream.duration(), stream.time_base());
                tracing::info!(
                    "Found audio stream {}: {} Hz, {} channels, frames={}, duration={:.2}s, format={:?}",
                    stream.index(),
                    audio.rate(),
                    audio.channels(),
                    stream.frames(),
                    duration,
                    audio.format()
                );
                Some(TrackInfo::Audio(AudioTrackInfo {
                    stream_index: stream.index(),
                    sample_rate: audio.rate(),
                    channels: audio.channels(),
                    samples: stream.frames().max(0) as u64,
                    duration,
                }))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let largest_video_size = tracks.iter().fold((0u32, 0u32), |acc, track| {
        if let TrackInfo::Video(v) = track {
            if v.width as u64 * v.height as u64 > acc.0 as u64 * acc.1 as u64 {
                (v.width, v.height)
            } else {
                acc
            }
        } else {
            acc
        }
    });
    for track in &mut tracks {
        if let TrackInfo::Video(v) = track {
            v.width = largest_video_size.0;
            v.height = largest_video_size.1;
        }
    }
    let mut audio_decoders: std::collections::HashMap<usize, ffmpeg_next::decoder::Audio> =
        std::collections::HashMap::new();
    for stream in input.streams() {
        if stream.parameters().medium() != ffmpeg_next::media::Type::Audio {
            continue;
        }
        let codec_ctx = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?;
        let mut decoder = codec_ctx.decoder().audio()?;
        decoder.set_packet_time_base(stream.time_base());
        audio_decoders.insert(stream.index(), decoder);
    }

    let mut decoded_audio_sample_counts: std::collections::HashMap<usize, u64> =
        std::collections::HashMap::new();
    let mut video_packet_counts: std::collections::HashMap<usize, u64> =
        std::collections::HashMap::new();
    let mut video_end_timestamps: std::collections::HashMap<usize, f64> =
        std::collections::HashMap::new();
    let mut audio_end_timestamps: std::collections::HashMap<usize, f64> =
        std::collections::HashMap::new();

    let mut last_keyframe_timestamp = 0.0f64;

    for (stream, packet) in input.packets() {
        let time_base = stream.time_base();
        match stream.parameters().medium() {
            ffmpeg_next::media::Type::Video => {
                let timestamp = timestamp_to_seconds(packet_timestamp(&packet), time_base);
                let end_timestamp =
                    timestamp + timestamp_to_seconds(packet.duration().max(0), time_base);
                if packet.is_key() {
                    last_keyframe_timestamp = timestamp;
                }
                *video_packet_counts.entry(stream.index()).or_insert(0) += 1;
                video_end_timestamps
                    .entry(stream.index())
                    .and_modify(|current| *current = current.max(end_timestamp))
                    .or_insert(end_timestamp);
                entries.push(IndexEntry::Video(VideoEntry {
                    stream_index: stream.index(),
                    keyframe: packet.is_key(),
                    position: packet.position() as u64,
                    timestamp,
                    duration: packet.duration(),
                    last_keyframe_timestamp,
                }));
            }
            ffmpeg_next::media::Type::Audio => {
                let timestamp = timestamp_to_seconds(packet_timestamp(&packet), time_base);
                let end_timestamp =
                    timestamp + timestamp_to_seconds(packet.duration().max(0), time_base);
                let packet_start_sample = decoded_audio_sample_counts
                    .get(&stream.index())
                    .copied()
                    .unwrap_or(0);
                if let Some(decoder) = audio_decoders.get_mut(&stream.index()) {
                    decoder.send_packet(&packet)?;
                    let mut frame = ffmpeg_next::frame::Audio::empty();
                    loop {
                        match decoder.receive_frame(&mut frame) {
                            Ok(()) => {
                                *decoded_audio_sample_counts
                                    .entry(stream.index())
                                    .or_insert(0) += frame.samples() as u64;
                            }
                            Err(ffmpeg_next::util::error::Error::Other {
                                errno: ffmpeg_next::ffi::EAGAIN,
                            }) => break,
                            Err(ffmpeg_next::util::error::Error::Eof) => break,
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "Failed to decode audio while indexing stream {}: {}",
                                    stream.index(),
                                    e
                                ));
                            }
                        }
                    }
                }
                audio_end_timestamps
                    .entry(stream.index())
                    .and_modify(|current| *current = current.max(end_timestamp))
                    .or_insert(end_timestamp);
                entries.push(IndexEntry::Audio(AudioEntry {
                    stream_index: stream.index(),
                    position: packet.position() as u64,
                    timestamp,
                    start_sample: packet_start_sample,
                }));
            }
            _ => {}
        }
    }

    for (stream_index, decoder) in &mut audio_decoders {
        decoder.send_eof()?;
        let mut frame = ffmpeg_next::frame::Audio::empty();
        loop {
            match decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    *decoded_audio_sample_counts
                        .entry(*stream_index)
                        .or_insert(0) += frame.samples() as u64;
                }
                Err(ffmpeg_next::util::error::Error::Eof)
                | Err(ffmpeg_next::util::error::Error::Other {
                    errno: ffmpeg_next::ffi::EAGAIN,
                }) => break,
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to flush audio decoder while indexing stream {}: {}",
                        stream_index,
                        e
                    ));
                }
            }
        }
    }

    // Containers like FLV often omit stream frame counts/durations, so fill them from packet data.
    for track in &mut tracks {
        match track {
            TrackInfo::Video(v) => {
                if let Some(&count) = video_packet_counts.get(&v.stream_index)
                    && v.frames == 0
                {
                    v.frames = count;
                }
                if let Some(&duration) = video_end_timestamps.get(&v.stream_index)
                    && v.duration <= 0.0
                {
                    v.duration = duration;
                }
            }
            TrackInfo::Audio(a) => {
                if let Some(&count) = decoded_audio_sample_counts.get(&a.stream_index)
                    && count > 0
                {
                    a.samples = count;
                }
                if let Some(&duration) = audio_end_timestamps.get(&a.stream_index)
                    && a.duration <= 0.0
                {
                    a.duration = duration;
                }
            }
        }
    }

    for track in &tracks {
        match track {
            TrackInfo::Video(v) => {
                tracing::info!(
                    "Final video track {}: {}x{}, frames={}, duration={:.2}s, output_format={:?}",
                    v.stream_index,
                    v.width,
                    v.height,
                    v.frames,
                    v.duration,
                    v.output_format
                );
            }
            TrackInfo::Audio(a) => {
                tracing::info!(
                    "Final audio track {}: {} Hz, {} channels, samples={}, duration={:.2}s",
                    a.stream_index,
                    a.sample_rate,
                    a.channels,
                    a.samples,
                    a.duration
                );
            }
        }
    }

    let index_content = IndexContentFile { tracks, entries };
    let index_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&index_content)
        .context("Failed to serialize index content")?;
    std::fs::write(content_path, &*index_bytes).context("Failed to write index file")?;

    // Update header with done=true
    let final_header = IndexHeaderFile {
        filename,
        filehash,
        version_nonce: *VERSION_NONCE.get().unwrap_or(&0),
    };
    let header =
        serde_json::to_string(&final_header).context("Failed to serialize index header")?;
    std::fs::write(header_path, header).context("Failed to write index header")?;

    if json_index {
        let dumped = serde_json::to_string_pretty(&index_content)
            .unwrap_or_else(|_| "<failed to serialize for debug>".to_string());
        std::fs::write(content_path.with_extension("json"), dumped).ok();
    }

    Ok(index_content)
}
