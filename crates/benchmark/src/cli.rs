use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExecutionMode {
    Sequential,
    Parallel,
    Both,
}

#[derive(Debug, Parser)]
#[command(about = "Benchmark an AviUtl2 input plugin DLL")]
pub struct Args {
    /// AviUtl2 input plugin DLL to load.
    pub dll: PathBuf,

    /// Execution mode for multiple input handles.
    #[arg(long, value_enum, default_value_t = ExecutionMode::Both)]
    pub mode: ExecutionMode,

    /// Sequential frames executed before measurement.
    #[arg(long, default_value_t = 30)]
    pub warmup: u32,

    /// Number of sequential frames to measure.
    #[arg(long, default_value_t = 300)]
    pub frames: u32,

    /// CSV output path.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Directory containing manifest.csv and the benchmark videos.
    #[arg(long)]
    pub videos_dir: Option<PathBuf>,

    /// Video path. Repeat this option to override manifest.csv.
    #[arg(long = "video")]
    pub videos: Vec<PathBuf>,
}

impl Args {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.dll.is_file(),
            "DLL does not exist: {}",
            self.dll.display()
        );
        anyhow::ensure!(self.frames > 0, "--frames must be greater than zero");
        let end_frame = self
            .warmup
            .checked_add(self.frames)
            .context("--warmup + --frames overflowed")?;
        anyhow::ensure!(
            i32::try_from(end_frame).is_ok(),
            "--warmup + --frames exceeds the plugin API frame range"
        );
        for video in &self.videos {
            anyhow::ensure!(video.is_file(), "Video does not exist: {}", video.display());
        }
        Ok(())
    }

    pub fn manifest_dir(&self) -> PathBuf {
        self.videos_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("videos"))
    }

    pub fn output_path(&self) -> anyhow::Result<PathBuf> {
        if let Some(path) = &self.output {
            return Ok(path.clone());
        }

        let stem = self
            .dll
            .file_stem()
            .and_then(|value| value.to_str())
            .context("DLL file name is not valid UTF-8")?;
        Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("results")
            .join(format!("{stem}.csv")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_manifest_dir_is_inside_the_crate() {
        let args = Args {
            dll: PathBuf::from("plugin.dll"),
            mode: ExecutionMode::Both,
            warmup: 30,
            frames: 300,
            output: None,
            videos_dir: None,
            videos: Vec::new(),
        };

        assert_eq!(
            args.manifest_dir(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("videos")
        );
    }
}
