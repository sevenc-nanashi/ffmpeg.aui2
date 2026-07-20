use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct VideoSource {
    pub input_index: usize,
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    input_index: usize,
    file: PathBuf,
    size_bytes: u64,
}

pub fn resolve_videos(
    explicit_videos: &[PathBuf],
    videos_dir: &Path,
) -> anyhow::Result<Vec<VideoSource>> {
    if !explicit_videos.is_empty() {
        return explicit_videos
            .iter()
            .enumerate()
            .map(|(input_index, path)| {
                let path = path
                    .canonicalize()
                    .with_context(|| format!("Failed to resolve video: {}", path.display()))?;
                Ok(VideoSource { input_index, path })
            })
            .collect();
    }

    let manifest_path = videos_dir.join("manifest.csv");
    let mut reader = csv::Reader::from_path(&manifest_path)
        .with_context(|| format!("Failed to open {}", manifest_path.display()))?;
    let mut rows = reader
        .deserialize::<ManifestRow>()
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    anyhow::ensure!(!rows.is_empty(), "Manifest contains no videos");
    rows.sort_by_key(|row| row.input_index);

    rows.into_iter()
        .enumerate()
        .map(|(expected_index, row)| {
            anyhow::ensure!(
                row.input_index == expected_index,
                "Manifest input_index must be contiguous from zero: expected {expected_index}, got {}",
                row.input_index
            );
            let path = videos_dir.join(&row.file);
            let metadata = std::fs::metadata(&path)
                .with_context(|| format!("Failed to read video metadata: {}", path.display()))?;
            anyhow::ensure!(
                metadata.len() == row.size_bytes,
                "Video size mismatch for {}: expected {}, got {}",
                path.display(),
                row.size_bytes,
                metadata.len()
            );
            Ok(VideoSource {
                input_index: row.input_index,
                path: path.canonicalize()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_videos_keep_argument_order() {
        let current_exe = std::env::current_exe().unwrap();
        let sources =
            resolve_videos(&[current_exe.clone(), current_exe], Path::new("unused")).unwrap();

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].input_index, 0);
        assert_eq!(sources[1].input_index, 1);
    }
}
