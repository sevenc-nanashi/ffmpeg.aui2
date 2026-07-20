mod benchmark;
mod cli;
mod host;
mod manifest;
mod memory;
mod plugin;
mod priority;

use clap::Parser;

use benchmark::{BenchmarkMode, FrameDirection};
use cli::{Args, ExecutionMode};

fn modes_for_plugin(
    mode: ExecutionMode,
    is_concurrent: bool,
) -> anyhow::Result<Vec<BenchmarkMode>> {
    match mode {
        ExecutionMode::Sequential => Ok(vec![BenchmarkMode::Sequential]),
        ExecutionMode::Parallel => {
            anyhow::ensure!(
                is_concurrent,
                "Parallel mode requires INPUT_PLUGIN_TABLE::FLAG_CONCURRENT"
            );
            Ok(vec![BenchmarkMode::Parallel])
        }
        ExecutionMode::Both if is_concurrent => {
            Ok(vec![BenchmarkMode::Sequential, BenchmarkMode::Parallel])
        }
        ExecutionMode::Both => Ok(vec![BenchmarkMode::Sequential]),
    }
}

fn directions(direction: cli::FrameDirection) -> Vec<FrameDirection> {
    match direction {
        cli::FrameDirection::Forward => vec![FrameDirection::Forward],
        cli::FrameDirection::Reverse => vec![FrameDirection::Reverse],
        cli::FrameDirection::Both => vec![FrameDirection::Forward, FrameDirection::Reverse],
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    args.validate()?;

    priority::set_process_priority(args.process_priority)?;
    priority::set_current_thread_priority(args.thread_priority)?;
    println!(
        "priority process={} thread={}",
        args.process_priority.as_str(),
        args.thread_priority.as_str()
    );

    let videos_dir = args.manifest_dir();
    let videos = manifest::resolve_videos(&args.videos, &videos_dir)?;

    let plugin = plugin::LoadedPlugin::load(&args.dll, &videos_dir)?;
    println!(
        "plugin={} videos={} dll={}",
        plugin.name(),
        videos.len(),
        args.dll.display()
    );
    let preparation_time = benchmark::prepare_inputs(&plugin, &videos)?;
    println!(
        "preparation={:.3}s (excluded from frame timings)",
        preparation_time.as_secs_f64()
    );

    benchmark::verify_frames(&plugin, &videos, &args.verify_frames)?;

    let modes = modes_for_plugin(args.mode, plugin.is_concurrent())?;
    let directions = directions(args.direction);
    if args.mode == ExecutionMode::Both && !plugin.is_concurrent() {
        eprintln!(
            "skipping parallel mode: plugin does not advertise INPUT_PLUGIN_TABLE::FLAG_CONCURRENT"
        );
    }

    let mut all_samples = Vec::with_capacity(args.frames as usize * modes.len() * directions.len());
    for mode in modes {
        for &direction in &directions {
            println!(
                "running plugin={} mode={} direction={} warmup={} frames={}",
                plugin.name(),
                mode.as_str(),
                direction.as_str(),
                args.warmup,
                args.frames
            );
            let samples = benchmark::run(
                &plugin,
                &videos,
                args.warmup,
                args.frames,
                mode,
                direction,
                args.thread_priority,
            )?;
            let summary = benchmark::summarize(&samples)?;
            benchmark::print_summary(plugin.name(), &summary);
            all_samples.extend(samples);
        }
    }

    let output = args.output_path()?;
    benchmark::write_csv(&output, &all_samples, &videos)?;
    println!("csv={}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_skips_parallel_for_non_concurrent_plugins() {
        assert_eq!(
            modes_for_plugin(ExecutionMode::Both, false).unwrap(),
            vec![BenchmarkMode::Sequential]
        );
        assert!(modes_for_plugin(ExecutionMode::Parallel, false).is_err());
    }
}
