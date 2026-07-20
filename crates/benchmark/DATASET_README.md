---
license: cc-by-4.0
tags:
  - video
  - benchmark
  - aviutl2
  - ffmpeg
pretty_name: ffmpeg.aui2 benchmark videos
---

# ffmpeg.aui2 benchmark videos

A reproducible set of eight Minecraft capture videos used to benchmark AviUtl2 input plugins.

## Contents

- `video-00.mp4` through `video-07.mp4`: the original, unmodified capture files
- `16vid.aup2`: a portable AviUtl2 project with relative video paths
- `manifest.csv`: layer order, file sizes, SHA-256 hashes, and video metadata

The project places all eight videos at frame 0 on separate layers. Its timeline is 1920×1080 at 60 fps and spans frames 0 through 3389.

## Benchmark usage

Download the dataset into `crates/benchmark/videos` in the ffmpeg.aui2 repository, then run:

```powershell
cargo run --release -p ffmpeg-aui2-benchmark -- <INPUT_PLUGIN_DLL>
```

The default benchmark performs 30 warm-up frames followed by 300 measured frames in both sequential and parallel modes. Index generation and file opening are excluded from frame timings.

## License

The recordings and accompanying metadata are provided under the Creative Commons Attribution 4.0 International license. Minecraft is a trademark of Microsoft Corporation. This dataset is not affiliated with or endorsed by Microsoft or Mojang.
