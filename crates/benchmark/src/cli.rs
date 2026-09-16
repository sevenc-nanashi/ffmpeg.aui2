use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, ValueEnum};

use crate::priority::{ProcessPriority, ThreadPriority};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExecutionMode {
    Sequential,
    Parallel,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FrameDirection {
    Forward,
    Reverse,
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

    /// Frame access direction.
    #[arg(long, value_enum, default_value_t = FrameDirection::Forward)]
    pub direction: FrameDirection,

    /// Windows process priority class.
    #[arg(long, value_enum, default_value_t = ProcessPriority::High)]
    pub process_priority: ProcessPriority,

    /// Priority for benchmark threads that call the input plugin.
    #[arg(long, value_enum, default_value_t = ThreadPriority::Highest)]
    pub thread_priority: ThreadPriority,

    /// Frame reads executed before measurement.
    #[arg(long, default_value_t = 30)]
    pub warmup: u32,

    /// Number of frame reads to measure.
    #[arg(long, default_value_t = 300)]
    pub frames: u32,

    /// Delay after each frame batch before requesting the next frame.
    #[arg(long, default_value_t = 0)]
    pub read_delay_ms: u64,

    /// Minimum number of frames skipped between requests.
    #[arg(long, default_value_t = 0)]
    pub frame_skip_min: u32,

    /// Maximum number of frames skipped between requests.
    #[arg(long, default_value_t = 0)]
    pub frame_skip_max: u32,

    /// CSV output path.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Directory containing manifest.csv and the benchmark videos.
    #[arg(long)]
    pub videos_dir: Option<PathBuf>,

    /// Video path. Repeat this option to override manifest.csv.
    #[arg(long = "video")]
    pub videos: Vec<PathBuf>,

    /// Read and hash a frame before benchmarking. Repeat to verify an access sequence.
    #[arg(long = "verify-frame")]
    pub verify_frames: Vec<u32>,
}

impl Args {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.dll.is_file(),
            "DLL does not exist: {}",
            self.dll.display()
        );
        anyhow::ensure!(self.frames > 0, "--frames must be greater than zero");
        anyhow::ensure!(
            self.frame_skip_min <= self.frame_skip_max,
            "--frame-skip-min must not exceed --frame-skip-max"
        );
        let end_frame = self
            .warmup
            .checked_add(self.frames)
            .context("--warmup + --frames overflowed")?;
        let maximum_frame_interval = u64::from(self.frame_skip_max) + 1;
        let required_frame_upper_bound = u64::from(end_frame)
            .checked_mul(maximum_frame_interval)
            .context("required frame range overflowed")?;
        anyhow::ensure!(
            i32::try_from(required_frame_upper_bound).is_ok(),
            "requested frames exceed the plugin API frame range"
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
    fn priority_defaults_are_high_and_highest() {
        let args = Args::try_parse_from(["benchmark", "plugin.dll"]).unwrap();

        assert_eq!(args.process_priority, ProcessPriority::High);
        assert_eq!(args.thread_priority, ThreadPriority::Highest);
    }

    #[test]
    fn default_manifest_dir_is_inside_the_crate() {
        let args = Args {
            dll: PathBuf::from("plugin.dll"),
            mode: ExecutionMode::Both,
            direction: FrameDirection::Both,
            process_priority: ProcessPriority::High,
            thread_priority: ThreadPriority::Highest,
            warmup: 30,
            frames: 300,
            read_delay_ms: 0,
            frame_skip_min: 0,
            frame_skip_max: 0,
            output: None,
            videos_dir: None,
            videos: Vec::new(),
            verify_frames: Vec::new(),
        };

        assert_eq!(
            args.manifest_dir(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("videos")
        );
    }

    #[test]
    fn read_delay_can_be_configured_in_milliseconds() {
        let default_args = Args::try_parse_from(["benchmark", "plugin.dll"]).unwrap();
        let args =
            Args::try_parse_from(["benchmark", "plugin.dll", "--read-delay-ms", "10"]).unwrap();

        assert_eq!(default_args.read_delay_ms, 0);
        assert_eq!(args.read_delay_ms, 10);
    }

    #[test]
    fn frame_skip_range_can_be_configured() {
        let args = Args::try_parse_from([
            "benchmark",
            "plugin.dll",
            "--frame-skip-min",
            "1",
            "--frame-skip-max",
            "3",
        ])
        .unwrap();

        assert_eq!(args.frame_skip_min, 1);
        assert_eq!(args.frame_skip_max, 3);
    }
}
