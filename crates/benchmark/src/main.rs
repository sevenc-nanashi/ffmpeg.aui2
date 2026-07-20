mod benchmark;
mod cli;
mod host;
mod manifest;
mod plugin;

use clap::Parser;

use benchmark::BenchmarkMode;
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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    args.validate()?;

    let videos_dir = args.manifest_dir();
    let videos = manifest::resolve_videos(&args.videos, &videos_dir)?;
    println!("videos={} dll={}", videos.len(), args.dll.display());

    let plugin = plugin::LoadedPlugin::load(&args.dll, &videos_dir)?;
    let preparation_time = benchmark::prepare_inputs(&plugin, &videos)?;
    println!(
        "preparation={:.3}s (excluded from frame timings)",
        preparation_time.as_secs_f64()
    );

    let modes = modes_for_plugin(args.mode, plugin.is_concurrent())?;
    if args.mode == ExecutionMode::Both && !plugin.is_concurrent() {
        eprintln!(
            "skipping parallel mode: plugin does not advertise INPUT_PLUGIN_TABLE::FLAG_CONCURRENT"
        );
    }

    let mut all_samples = Vec::with_capacity(args.frames as usize * modes.len());
    for mode in modes {
        println!(
            "running mode={} warmup={} frames={}",
            mode.as_str(),
            args.warmup,
            args.frames
        );
        let samples = benchmark::run(&plugin, &videos, args.warmup, args.frames, mode)?;
        let summary = benchmark::summarize(&samples)?;
        benchmark::print_summary(&summary);
        all_samples.extend(samples);
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
