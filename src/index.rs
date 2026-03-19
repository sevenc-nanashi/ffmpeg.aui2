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

#[derive(Debug, Clone, rkyv::Serialize, rkyv::Deserialize, rkyv::Archive)]
pub enum TrackInfo {
    Video {
        stream_index: usize,
        width: u32,
        height: u32,
        frames: u64,
        duration: f64,
    },
    Audio {
        stream_index: usize,
        sample_rate: u32,
        channels: u16,
        samples: u64,
        duration: f64,
    },
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
                Some(TrackInfo::Video {
                    stream_index: stream.index(),
                    width: video.width(),
                    height: video.height(),
                    frames: stream.frames().max(0) as u64,
                    duration: (stream.duration() as f64) * f64::from(stream.time_base()),
                })
            }
            ffmpeg_next::media::Type::Audio => {
                let codec =
                    ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())
                        .ok()?;
                let audio = codec.decoder().audio().ok()?;
                Some(TrackInfo::Audio {
                    stream_index: stream.index(),
                    sample_rate: audio.rate(),
                    channels: audio.channels(),
                    samples: stream.frames().max(0) as u64,
                    duration: (stream.duration() as f64) * f64::from(stream.time_base()),
                })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let largest_video_size = tracks.iter().fold((0, 0), |acc, track| match track {
        TrackInfo::Video { width, height, .. } => (acc.0.max(*width), acc.1.max(*height)),
        TrackInfo::Audio { .. } => acc,
    });
    for track in &mut tracks {
        if let TrackInfo::Video { width, height, .. } = track {
            *width = largest_video_size.0;
            *height = largest_video_size.1;
        }
    }
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
                entries.push(IndexEntry::Audio(AudioEntry {
                    stream_index: stream.index(),
                    position: packet.position() as u64,
                    timestamp: timestamp_to_seconds(packet.pts().unwrap_or(0), time_base),
                }));
            }
            _ => {}
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
