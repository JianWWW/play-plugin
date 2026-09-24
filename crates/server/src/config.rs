//! Plugin configuration (TOML), loaded from `%APPDATA%\PlayPlugin\config.toml`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Preferred localhost port. Occupied → falls back through `port_fallback`.
    pub port: u16,
    /// Number of extra ports to try after `port`.
    pub port_fallback: u16,
    /// Allowed page origins for the local WebSocket. Empty list = allow all
    /// (development mode; a warning is logged). Production installers preload
    /// this list.
    pub origins: Vec<String>,
    /// Maximum concurrently playing streams.
    pub max_streams: u32,
    /// tracing filter, e.g. "info" or "play_plugin=debug".
    pub log_level: String,
    /// Data directory (logs/snapshots/config). Default: %APPDATA%\PlayPlugin.
    pub data_dir: Option<PathBuf>,
    #[serde(skip)]
    pub force_test_source: bool,
    /// Enable D3D11VA hardware decode (falls back to software automatically).
    pub hardware_decode: bool,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct UpdateConfig {
    pub enabled: bool,
    pub manifest_url: Option<String>,
    /// If true, a verified update is installed silently; otherwise the page
    /// is only notified (app.updateAvailable event).
    pub auto_install: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 17653,
            port_fallback: 8,
            origins: Vec::new(),
            max_streams: 32,
            log_level: "info".into(),
            data_dir: None,
            force_test_source: false,
            hardware_decode: true,
            update: UpdateConfig::default(),
        }
    }
}

impl Config {
    /// Loads config from `path` (or default location), filling defaults and
    /// creating a template file on first run.
    pub fn load(path: Option<PathBuf>) -> (Config, PathBuf) {
        let dir = path.unwrap_or_else(default_data_dir);
        let file = dir.join("config.toml");
        let cfg = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| toml::from_str::<Config>(&s).ok())
            .unwrap_or_default();
        if !file.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&file, template_toml(&cfg));
        }
        (cfg, dir)
    }

    /// Origin check for the WS upgrade. Empty whitelist = dev mode (allow all).
    pub fn origin_allowed(&self, origin: &str) -> bool {
        if self.origins.is_empty() {
            return true;
        }
        let o = origin.trim().to_ascii_lowercase();
        if o.is_empty() {
            return false; // non-browser clients must identify via Origin
        }
        self.origins
            .iter()
            .any(|allowed| normalize_origin(allowed) == o)
    }
}

pub fn normalize_origin(o: &str) -> String {
    o.trim().trim_end_matches('/').to_ascii_lowercase()
}

pub fn default_data_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("PlayPlugin")
}

fn template_toml(cfg: &Config) -> String {
    let origins = if cfg.origins.is_empty() {
        "# origins = [\"https://player.example.com\"]".to_string()
    } else {
        format!(
            "origins = [{}]",
            cfg.origins
                .iter()
                .map(|o| format!("\"{o}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "# PlayPlugin configuration\n\
         port = {}\n\
         port_fallback = {}\n\
         max_streams = {}\n\
         log_level = \"{}\"\n\
         hardware_decode = {}\n\
         {origins}\n\n\
         [update]\n\
         enabled = false\n\
         # manifest_url = \"https://example.com/play-plugin/manifest.json\"\n\
         auto_install = false\n",
        cfg.port, cfg.port_fallback, cfg.max_streams, cfg.log_level, cfg.hardware_decode
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_whitelist_matching() {
        let cfg = Config {
            origins: vec!["https://Player.Example.com".into()],
            ..Default::default()
        };
        assert!(cfg.origin_allowed("https://player.example.com"));
        assert!(cfg.origin_allowed("HTTPS://PLAYER.EXAMPLE.COM"));
        assert!(!cfg.origin_allowed("https://evil.com"));
        assert!(!cfg.origin_allowed("null"));
    }

    #[test]
    fn empty_whitelist_is_dev_mode() {
        let cfg = Config::default();
        assert!(cfg.origin_allowed("https://anything.local"));
    }

    #[test]
    fn parses_toml() {
        let cfg: Config = toml::from_str("port = 1234\norigins = [\"https://a.com\"]\n").unwrap();
        assert_eq!(cfg.port, 1234);
        assert_eq!(cfg.max_streams, 32); // default preserved
        assert_eq!(cfg.origins.len(), 1);
    }
}
