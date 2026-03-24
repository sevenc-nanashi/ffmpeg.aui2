const DEFAULT_CONFIG: &str = include_str!("./default_config.ini");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FpsMode {
    /// Use avg_frame_rate from stream metadata as-is.
    Metadata,
    /// Compute FPS from frames / duration and round with fps_precision.
    Real,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwAccel {
    None,
    Auto,
    D3d11va,
    Dxva2,
    Cuda,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub log_level: tracing::Level,
    pub json_index: bool,
    pub hwaccel: HwAccel,
    /// Per-video prefetch buffer size in MB (based on pixel format × resolution).
    pub prefetch_buffer_mb: u32,
    /// Total prefetch budget across all open videos in MB.
    pub prefetch_total_buffer_mb: u32,
    /// Maximum number of frames to prefetch ahead (0 = no frame-count limit).
    pub prefetch_frames: u32,
    /// FPS rounding precision: 0 = round to integer, n ≥ 1 = round to 10^(-n).
    /// Only used when fps_mode = real.
    pub fps_precision: u32,
    pub fps_mode: FpsMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: tracing::Level::INFO,
            json_index: false,
            hwaccel: HwAccel::None,
            prefetch_buffer_mb: 32,
            prefetch_total_buffer_mb: 512,
            prefetch_frames: 10,
            fps_precision: 3,
            fps_mode: FpsMode::Real,
        }
    }
}

/// Extracts blocks (comment lines + key=value line) keyed by their key name from an INI string.
fn default_key_blocks() -> std::collections::HashMap<&'static str, String> {
    let mut map = std::collections::HashMap::new();
    let mut pending: Vec<&str> = Vec::new();

    for line in DEFAULT_CONFIG.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(';') || trimmed.starts_with('#') || trimmed.is_empty() {
            pending.push(line);
        } else if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim();
            let mut block = String::new();
            if !pending.is_empty() {
                block.push('\n');
            }
            for comment in pending.drain(..) {
                block.push_str(comment);
                block.push('\n');
            }
            block.push_str(line);
            block.push('\n');
            map.insert(key, block);
        } else {
            pending.clear();
        }
    }

    map
}

impl Config {
    pub fn load(config_path: &std::path::Path) -> Self {
        let mut config = Self::default();

        let content = match std::fs::read_to_string(config_path) {
            Ok(c) => c,
            Err(_) => {
                aviutl2::lprintln!(
                    warn,
                    "Failed to read config file at {:?}, using default config",
                    config_path
                );
                let _ = std::fs::write(config_path, DEFAULT_CONFIG);
                return config;
            }
        };

        for line in content.lines() {
            let line = line.trim();
            if line.starts_with(';')
                || line.starts_with('#')
                || line.starts_with('[')
                || line.is_empty()
            {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "log_level" => {
                        config.log_level = match value.to_ascii_lowercase().as_str() {
                            "trace" => tracing::Level::TRACE,
                            "debug" => tracing::Level::DEBUG,
                            "info" => tracing::Level::INFO,
                            "warn" | "warning" => tracing::Level::WARN,
                            "error" => tracing::Level::ERROR,
                            _ => tracing::Level::INFO,
                        };
                    }
                    "json_index" => {
                        config.json_index =
                            matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
                    }
                    "prefetch_buffer_mb" => {
                        if let Ok(v) = value.parse::<u32>() {
                            config.prefetch_buffer_mb = v;
                        }
                    }
                    "prefetch_total_buffer_mb" => {
                        if let Ok(v) = value.parse::<u32>() {
                            config.prefetch_total_buffer_mb = v;
                        }
                    }
                    "prefetch_frames" => {
                        if let Ok(v) = value.parse::<u32>() {
                            config.prefetch_frames = v;
                        }
                    }
                    "fps_precision" => {
                        if let Ok(v) = value.parse::<u32>() {
                            config.fps_precision = v;
                        }
                    }
                    "fps_mode" => {
                        config.fps_mode = match value.to_ascii_lowercase().as_str() {
                            "metadata" => FpsMode::Metadata,
                            "real" => FpsMode::Real,
                            _ => FpsMode::Real,
                        };
                    }
                    "hwaccel" => {
                        config.hwaccel = match value.to_ascii_lowercase().as_str() {
                            "none" | "off" | "false" => HwAccel::None,
                            "auto" => HwAccel::Auto,
                            "d3d11va" => HwAccel::D3d11va,
                            "dxva2" => HwAccel::Dxva2,
                            "cuda" => HwAccel::Cuda,
                            _ => HwAccel::None,
                        };
                    }
                    _ => {}
                }
            }
        }

        // Append any keys present in the default config but missing from the file
        let blocks = default_key_blocks();
        let mut additions = String::new();
        for (key, block) in &blocks {
            let is_present = content.lines().any(|l| {
                l.split_once('=')
                    .is_some_and(|(k, _)| k.trim() == *key)
            });
            if !is_present {
                aviutl2::lprintln!(
                    warn,
                    "Config key '{}' not found in {:?}, adding default",
                    key,
                    config_path
                );
                additions.push_str(block);
            }
        }
        if !additions.is_empty() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(config_path) {
                let _ = f.write_all(additions.as_bytes());
            }
        }

        config
    }
}
