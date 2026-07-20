use std::path::Path;
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::Serialize;

use crate::manifest::VideoSource;
use crate::memory::ProcessMemory;
use crate::plugin::{InputHandle, LoadedPlugin};
use crate::priority::ThreadPriority;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchmarkMode {
    Sequential,
    Parallel,
}

impl BenchmarkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Parallel => "parallel",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDirection {
    Forward,
    Reverse,
}

impl FrameDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Reverse => "reverse",
        }
    }

    fn frame_at(self, step: u32, total_frames: u32) -> u32 {
        match self {
            Self::Forward => step,
            Self::Reverse => total_frames
                .checked_sub(step + 1)
                .expect("reverse frame step exceeds total frames"),
        }
    }
}

#[derive(Debug)]
pub struct FrameSample {
    pub mode: BenchmarkMode,
    pub direction: FrameDirection,
    pub frame: u32,
    pub wall: Duration,
    pub memory: ProcessMemory,
    pub calls: Vec<CallSample>,
}

#[derive(Debug)]
pub struct CallSample {
    pub input_index: usize,
    pub duration: Duration,
    pub bytes: usize,
}

#[derive(Debug)]
struct WorkerResult {
    input_index: usize,
    measurement: Result<crate::plugin::ReadMeasurement, String>,
}

#[derive(Debug)]
pub struct Summary {
    pub mode: BenchmarkMode,
    pub direction: FrameDirection,
    pub frames: usize,
    pub inputs: usize,
    pub total: Duration,
    pub fps: f64,
    pub average_ms: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub average_working_set_mib: f64,
    pub max_working_set_mib: f64,
    pub average_private_mib: f64,
    pub max_private_mib: f64,
}

#[derive(Serialize)]
struct CsvRow<'a> {
    mode: &'static str,
    direction: &'static str,
    frame: u32,
    input_index: usize,
    file: &'a str,
    duration_ns: u64,
    bytes: usize,
    frame_wall_ns: u64,
    process_working_set_bytes: u64,
    process_private_bytes: u64,
}

pub fn prepare_inputs(plugin: &LoadedPlugin, videos: &[VideoSource]) -> anyhow::Result<Duration> {
    let started = Instant::now();
    let handles = open_inputs(plugin, videos)?;
    print_input_info(&handles);
    drop(handles);
    Ok(started.elapsed())
}

pub fn verify_frames(
    plugin: &LoadedPlugin,
    videos: &[VideoSource],
    frames: &[u32],
) -> anyhow::Result<()> {
    if frames.is_empty() {
        return Ok(());
    }

    let mut handles = open_inputs(plugin, videos)?;
    for &frame in frames {
        let frame = i32::try_from(frame).context("verification frame exceeds i32")?;
        for (input_index, handle) in handles.iter_mut().enumerate() {
            handle.read_frame(frame)?;
            println!(
                "verify input={input_index} frame={frame} digest={:016x}",
                handle.frame_digest()
            );
        }
    }
    Ok(())
}

pub fn run(
    plugin: &LoadedPlugin,
    videos: &[VideoSource],
    warmup: u32,
    frames: u32,
    mode: BenchmarkMode,
    direction: FrameDirection,
    thread_priority: ThreadPriority,
) -> anyhow::Result<Vec<FrameSample>> {
    let handles = open_inputs(plugin, videos)?;
    let required_frames = warmup
        .checked_add(frames)
        .context("warmup + frames overflowed")?;
    for handle in &handles {
        anyhow::ensure!(
            handle.total_frames() >= i32::try_from(required_frames)?,
            "{} has only {} frames, but {required_frames} are required",
            handle.path().display(),
            handle.total_frames()
        );
    }

    match mode {
        BenchmarkMode::Sequential => run_sequential(handles, warmup, frames, direction),
        BenchmarkMode::Parallel => {
            anyhow::ensure!(
                plugin.is_concurrent(),
                "Parallel mode requires INPUT_PLUGIN_TABLE::FLAG_CONCURRENT"
            );
            run_parallel(handles, warmup, frames, direction, thread_priority)
        }
    }
}

fn open_inputs(plugin: &LoadedPlugin, videos: &[VideoSource]) -> anyhow::Result<Vec<InputHandle>> {
    anyhow::ensure!(!videos.is_empty(), "No benchmark videos were specified");
    videos
        .iter()
        .map(|video| {
            plugin
                .open(&video.path)
                .with_context(|| format!("Failed to open input {}", video.input_index))
        })
        .collect()
}

fn print_input_info(handles: &[InputHandle]) {
    for (index, handle) in handles.iter().enumerate() {
        let (width, height) = handle.dimensions();
        let (rate, scale) = handle.frame_rate();
        println!(
            "input={index} frames={} size={}x{} fps={:.3} file={}",
            handle.total_frames(),
            width,
            height,
            f64::from(rate) / f64::from(scale),
            handle.path().display()
        );
    }
}

fn run_sequential(
    mut handles: Vec<InputHandle>,
    warmup: u32,
    frames: u32,
    direction: FrameDirection,
) -> anyhow::Result<Vec<FrameSample>> {
    let total_frames = warmup + frames;
    let mut samples = Vec::with_capacity(frames as usize);

    for step in 0..total_frames {
        let frame = direction.frame_at(step, total_frames);
        let wall_started = Instant::now();
        let calls = handles
            .iter_mut()
            .enumerate()
            .map(|(input_index, handle)| {
                let measurement = handle.read_frame(i32::try_from(frame)?)?;
                Ok(CallSample {
                    input_index,
                    duration: measurement.duration,
                    bytes: measurement.bytes,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let wall = wall_started.elapsed();

        if step >= warmup {
            samples.push(FrameSample {
                mode: BenchmarkMode::Sequential,
                direction,
                frame,
                wall,
                memory: crate::memory::current_process_memory()?,
                calls,
            });
        }
    }

    Ok(samples)
}

fn run_parallel(
    handles: Vec<InputHandle>,
    warmup: u32,
    frames: u32,
    direction: FrameDirection,
    thread_priority: ThreadPriority,
) -> anyhow::Result<Vec<FrameSample>> {
    let total_frames = warmup + frames;
    let input_count = handles.len();
    let barrier = Arc::new(Barrier::new(input_count + 1));
    let (sender, receiver) = mpsc::channel::<WorkerResult>();

    std::thread::scope(|scope| {
        for (input_index, mut handle) in handles.into_iter().enumerate() {
            let barrier = Arc::clone(&barrier);
            let sender = sender.clone();
            scope.spawn(move || {
                let priority_error = crate::priority::set_current_thread_priority(thread_priority)
                    .err()
                    .map(|error| format!("Failed to set worker thread priority: {error:#}"));
                for step in 0..total_frames {
                    let frame = direction.frame_at(step, total_frames);
                    barrier.wait();
                    let measurement = match &priority_error {
                        Some(error) => Err(error.clone()),
                        None => handle
                            .read_frame(i32::try_from(frame).expect("frame index exceeds i32"))
                            .map_err(|error| format!("{error:#}")),
                    };
                    sender
                        .send(WorkerResult {
                            input_index,
                            measurement,
                        })
                        .expect("benchmark result receiver disconnected");
                    barrier.wait();
                }
            });
        }
        drop(sender);

        let mut samples = Vec::with_capacity(frames as usize);
        let mut first_error = None;
        for step in 0..total_frames {
            let frame = direction.frame_at(step, total_frames);
            let wall_started = Instant::now();
            barrier.wait();
            barrier.wait();
            let wall = wall_started.elapsed();

            let mut calls = Vec::with_capacity(input_count);
            for _ in 0..input_count {
                let result = receiver.recv().context("Benchmark worker disconnected")?;
                match result.measurement {
                    Ok(measurement) => calls.push(CallSample {
                        input_index: result.input_index,
                        duration: measurement.duration,
                        bytes: measurement.bytes,
                    }),
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(anyhow::anyhow!(
                                "Input {} frame {frame}: {error}",
                                result.input_index
                            ));
                        }
                    }
                }
            }
            calls.sort_by_key(|call| call.input_index);

            if step >= warmup && calls.len() == input_count {
                samples.push(FrameSample {
                    mode: BenchmarkMode::Parallel,
                    direction,
                    frame,
                    wall,
                    memory: crate::memory::current_process_memory()?,
                    calls,
                });
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(samples)
    })
}

pub fn write_csv(
    path: &Path,
    samples: &[FrameSample],
    videos: &[VideoSource],
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("CSV output path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create {}", parent.display()))?;
    let mut writer = csv::Writer::from_path(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;

    for frame in samples {
        let frame_wall_ns = duration_ns(frame.wall)?;
        for call in &frame.calls {
            let video = videos
                .get(call.input_index)
                .context("Call sample has an invalid input index")?;
            let file = video.path.to_string_lossy();
            writer.serialize(CsvRow {
                mode: frame.mode.as_str(),
                direction: frame.direction.as_str(),
                frame: frame.frame,
                input_index: call.input_index,
                file: &file,
                duration_ns: duration_ns(call.duration)?,
                bytes: call.bytes,
                frame_wall_ns,
                process_working_set_bytes: frame.memory.working_set_bytes,
                process_private_bytes: frame.memory.private_bytes,
            })?;
        }
    }
    writer.flush()?;
    Ok(())
}

pub fn summarize(samples: &[FrameSample]) -> anyhow::Result<Summary> {
    let first = samples.first().context("No benchmark samples")?;
    anyhow::ensure!(
        samples.iter().all(|sample| sample.mode == first.mode),
        "Cannot summarize mixed benchmark modes"
    );
    anyhow::ensure!(
        samples
            .iter()
            .all(|sample| sample.direction == first.direction),
        "Cannot summarize mixed frame directions"
    );
    let inputs = first.calls.len();
    anyhow::ensure!(inputs > 0, "Benchmark samples contain no input calls");
    anyhow::ensure!(
        samples.iter().all(|sample| sample.calls.len() == inputs),
        "Input count changed during benchmark"
    );

    let mut walls = samples
        .iter()
        .map(|sample| duration_ns(sample.wall))
        .collect::<anyhow::Result<Vec<_>>>()?;
    walls.sort_unstable();
    let total_ns = walls.iter().map(|value| u128::from(*value)).sum::<u128>();
    let total = Duration::from_nanos(u64::try_from(total_ns).context("Total duration overflowed")?);
    let frames = samples.len();
    let average_ns = total_ns as f64 / frames as f64;
    let total_working_set_bytes = samples
        .iter()
        .map(|sample| u128::from(sample.memory.working_set_bytes))
        .sum::<u128>();
    let total_private_bytes = samples
        .iter()
        .map(|sample| u128::from(sample.memory.private_bytes))
        .sum::<u128>();
    let max_working_set_bytes = samples
        .iter()
        .map(|sample| sample.memory.working_set_bytes)
        .max()
        .expect("benchmark samples must not be empty");
    let max_private_bytes = samples
        .iter()
        .map(|sample| sample.memory.private_bytes)
        .max()
        .expect("benchmark samples must not be empty");

    Ok(Summary {
        mode: first.mode,
        direction: first.direction,
        frames,
        inputs,
        total,
        fps: frames as f64 / total.as_secs_f64(),
        average_ms: average_ns / 1_000_000.0,
        median_ms: percentile(&walls, 0.50) as f64 / 1_000_000.0,
        p95_ms: percentile(&walls, 0.95) as f64 / 1_000_000.0,
        min_ms: walls[0] as f64 / 1_000_000.0,
        max_ms: walls[walls.len() - 1] as f64 / 1_000_000.0,
        average_working_set_mib: bytes_to_mib(total_working_set_bytes as f64 / frames as f64),
        max_working_set_mib: bytes_to_mib(max_working_set_bytes as f64),
        average_private_mib: bytes_to_mib(total_private_bytes as f64 / frames as f64),
        max_private_mib: bytes_to_mib(max_private_bytes as f64),
    })
}

pub fn print_summary(plugin_name: &str, summary: &Summary) {
    println!(
        "plugin={} mode={} direction={} frames={} inputs={} total={:.3}s fps={:.3} avg={:.3}ms median={:.3}ms p95={:.3}ms min={:.3}ms max={:.3}ms working_set_avg={:.1}MiB working_set_max={:.1}MiB private_avg={:.1}MiB private_max={:.1}MiB",
        plugin_name,
        summary.mode.as_str(),
        summary.direction.as_str(),
        summary.frames,
        summary.inputs,
        summary.total.as_secs_f64(),
        summary.fps,
        summary.average_ms,
        summary.median_ms,
        summary.p95_ms,
        summary.min_ms,
        summary.max_ms,
        summary.average_working_set_mib,
        summary.max_working_set_mib,
        summary.average_private_mib,
        summary.max_private_mib,
    );
}

fn bytes_to_mib(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0)
}

fn duration_ns(duration: Duration) -> anyhow::Result<u64> {
    u64::try_from(duration.as_nanos()).context("Duration exceeds u64 nanoseconds")
}

fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    assert!(!sorted.is_empty());
    assert!((0.0..=1.0).contains(&quantile));
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(mode: BenchmarkMode, milliseconds: u64) -> FrameSample {
        FrameSample {
            mode,
            direction: FrameDirection::Forward,
            frame: 0,
            wall: Duration::from_millis(milliseconds),
            memory: ProcessMemory {
                working_set_bytes: milliseconds * 1024 * 1024,
                private_bytes: milliseconds * 2 * 1024 * 1024,
            },
            calls: vec![CallSample {
                input_index: 0,
                duration: Duration::from_millis(milliseconds),
                bytes: 4,
            }],
        }
    }

    #[test]
    fn summary_reports_nearest_rank_percentiles() {
        let samples = [1, 2, 3, 4, 100]
            .into_iter()
            .map(|milliseconds| sample(BenchmarkMode::Sequential, milliseconds))
            .collect::<Vec<_>>();

        let summary = summarize(&samples).unwrap();

        assert_eq!(summary.median_ms, 3.0);
        assert_eq!(summary.p95_ms, 100.0);
        assert_eq!(summary.min_ms, 1.0);
        assert_eq!(summary.max_ms, 100.0);
        assert_eq!(summary.average_working_set_mib, 22.0);
        assert_eq!(summary.max_working_set_mib, 100.0);
        assert_eq!(summary.average_private_mib, 44.0);
        assert_eq!(summary.max_private_mib, 200.0);
    }

    #[test]
    fn mixed_modes_are_rejected() {
        let samples = vec![
            sample(BenchmarkMode::Sequential, 1),
            sample(BenchmarkMode::Parallel, 1),
        ];

        assert!(summarize(&samples).is_err());
    }

    #[test]
    fn reverse_direction_maps_steps_to_descending_frames() {
        let frames = (0..4)
            .map(|step| FrameDirection::Reverse.frame_at(step, 4))
            .collect::<Vec<_>>();

        assert_eq!(frames, vec![3, 2, 1, 0]);
    }
}
