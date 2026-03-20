use anyhow::Context;

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub struct IndexHeaderFile {
    pub filename: String,
    pub filehash: u64,
}

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
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

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub enum TrackInfo {
    Video(VideoTrackInfo),
    Audio(AudioTrackInfo),
}

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub struct VideoTrackInfo {
    pub stream_index: usize,
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub duration: f64,
    pub is_yuv422: bool,
}
#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub struct AudioTrackInfo {
    pub stream_index: usize,
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: u64,
    pub duration: f64,
}

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub struct VideoEntry {
    pub stream_index: usize,
    pub keyframe: bool,
    pub position: u64,
    pub timestamp: f64,
    pub duration: i64,
    pub last_keyframe_timestamp: f64,
}

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub struct AudioEntry {
    pub stream_index: usize,
    pub position: u64,
    pub timestamp: f64,
}

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
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
) -> aviutl2::AnyResult<IndexContentFile> {
    fn timestamp_to_seconds(timestamp: i64, time_base: ffmpeg_next::Rational) -> f64 {
        (timestamp as f64) * f64::from(time_base)
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
                tracing::info!(
                    "Found video stream {}: {}x{}, frames={}, duration={:.2}s, format={:?}",
                    stream.index(),
                    video.width(),
                    video.height(),
                    stream.frames(),
                    (stream.duration() as f64) * f64::from(stream.time_base()),
                    video.format()
                );
                Some(TrackInfo::Video(VideoTrackInfo {
                    stream_index: stream.index(),
                    width: video.width(),
                    height: video.height(),
                    frames: stream.frames().max(0) as u64,
                    duration: (stream.duration() as f64) * f64::from(stream.time_base()),
                    is_yuv422: matches!(
                        video.format(),
                        ffmpeg_next::format::Pixel::YUV422P | ffmpeg_next::format::Pixel::YUYV422
                    ),
                }))
            }
            ffmpeg_next::media::Type::Audio => {
                let codec =
                    ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
                        .ok()?;
                let audio = codec.decoder().audio().ok()?;
                tracing::info!(
                    "Found audio stream {}: {} Hz, {} channels, frames={}, duration={:.2}s, format={:?}",
                    stream.index(),
                    audio.rate(),
                    audio.channels(),
                    stream.frames(),
                    (stream.duration() as f64) * f64::from(stream.time_base()),
                    audio.format()
                );
                Some(TrackInfo::Audio(AudioTrackInfo {
                    stream_index: stream.index(),
                    sample_rate: audio.rate(),
                    channels: audio.channels(),
                    samples: stream.frames().max(0) as u64,
                    duration: (stream.duration() as f64) * f64::from(stream.time_base()),
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
    // Build sample_rate map for audio streams to accumulate sample counts
    let audio_sample_rates: std::collections::HashMap<usize, u32> = tracks
        .iter()
        .filter_map(|t| {
            if let TrackInfo::Audio(a) = t {
                Some((a.stream_index, a.sample_rate))
            } else {
                None
            }
        })
        .collect();
    let mut audio_sample_counts: std::collections::HashMap<usize, u64> =
        std::collections::HashMap::new();

    let mut last_keyframe_timestamp = 0.0f64;

    for (stream, packet) in input.packets() {
        let time_base = stream.time_base();
        match stream.parameters().medium() {
            ffmpeg_next::media::Type::Video => {
                let timestamp = timestamp_to_seconds(packet.pts().unwrap_or(0), time_base);
                if packet.is_key() {
                    last_keyframe_timestamp = timestamp;
                }
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
                if let Some(&sr) = audio_sample_rates.get(&stream.index()) {
                    let samples =
                        (packet.duration() as f64 * f64::from(time_base) * sr as f64) as u64;
                    *audio_sample_counts.entry(stream.index()).or_insert(0) += samples;
                }
                entries.push(IndexEntry::Audio(AudioEntry {
                    stream_index: stream.index(),
                    position: packet.position() as u64,
                    timestamp: timestamp_to_seconds(packet.pts().unwrap_or(0), time_base),
                }));
            }
            _ => {}
        }
    }

    // Update samples field with actual counts from packet durations
    for track in &mut tracks {
        if let TrackInfo::Audio(a) = track
            && let Some(&count) = audio_sample_counts.get(&a.stream_index)
        {
            a.samples = count;
        }
    }

    let index_content = IndexContentFile { tracks, entries };
    let index_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&index_content)
        .context("Failed to serialize index content")?;
    std::fs::write(content_path, &*index_bytes).context("Failed to write index file")?;

    // Update header with done=true
    let final_header = IndexHeaderFile { filename, filehash };
    let header_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&final_header)
        .context("Failed to serialize final index header")?;
    std::fs::write(header_path, &*header_bytes).context("Failed to write final index header")?;

    Ok(index_content)
}
