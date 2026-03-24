const DEFAULT_CONFIG: &str = include_str!("./default_config.ini");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwAccel {
    None,
    Auto,
    D3d11va,
    Dxva2,
    Cuda,
}

pub struct Config {
    pub log_level: tracing::Level,
    pub json_index: bool,
    pub prefetch_buffer_mb: u32,
    pub hwaccel: HwAccel,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: tracing::Level::INFO,
            json_index: false,
            prefetch_buffer_mb: 512,
            hwaccel: HwAccel::None,
        }
    }
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

        config
    }
}
