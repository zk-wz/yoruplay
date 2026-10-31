use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::models::PlayMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Standard,
    Higher,
    Exhigh,
    Lossless,
    Hires,
}

impl Quality {
    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Standard => "standard",
            Quality::Higher => "higher",
            Quality::Exhigh => "exhigh",
            Quality::Lossless => "lossless",
            Quality::Hires => "hires",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    En,
    Zh,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub quality: Quality,
    pub timeout_secs: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            quality: Quality::Exhigh,
            timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub language: Language,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: Language::En,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackConfig {
    pub default_mode: PlayMode,
    pub volume: f64,
    pub restore_queue: bool,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            default_mode: PlayMode::ListLoop,
            volume: 0.7,
            restore_queue: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub max_mb: u64,
    pub lyrics_ttl_hours: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_mb: 32,
            lyrics_ttl_hours: 24 * 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub api: ApiConfig,
    pub ui: UiConfig,
    pub playback: PlaybackConfig,
    pub cache: CacheConfig,
    pub log: LogConfig,
}

impl Config {
    pub fn validate(&mut self) {
        if self.api.timeout_secs == 0 || self.api.timeout_secs > 300 {
            self.api.timeout_secs = self.api.timeout_secs.clamp(1, 300);
        }
        if !self.playback.volume.is_finite() {
            self.playback.volume = 0.7;
        }
        self.playback.volume = self.playback.volume.clamp(0.0, 1.0);
        if self.cache.max_mb == 0 {
            self.cache.max_mb = 32;
        }
        if self.cache.lyrics_ttl_hours == 0 {
            self.cache.lyrics_ttl_hours = 24 * 30;
        }
    }

    /// Loads a TOML config, writing the default file first when it does not
    /// exist. A malformed file is never overwritten.
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            let mut cfg = Self::default();
            cfg.validate();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let text = toml::to_string_pretty(&cfg)?;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(path)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            return Ok(cfg);
        }
        let text = fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&text)?;
        cfg.validate();
        Ok(cfg)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read or write configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("failed to serialise configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "yoruplay-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn creates_and_reloads_default_config() {
        let path = temp_path("default.toml");
        let cfg = Config::load_or_create(&path).unwrap();
        assert_eq!(cfg.api.quality, Quality::Exhigh);
        let cfg2 = Config::load_or_create(&path).unwrap();
        assert_eq!(cfg2.playback.default_mode, PlayMode::ListLoop);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn clamps_invalid_values() {
        let mut cfg: Config = toml::from_str(
            r#"
[api]
timeout_secs = 0
[playback]
volume = 4.0
[cache]
max_mb = 0
"#,
        )
        .unwrap();
        cfg.validate();
        assert_eq!(cfg.api.timeout_secs, 1);
        assert_eq!(cfg.playback.volume, 1.0);
        assert_eq!(cfg.cache.max_mb, 32);
    }

    #[test]
    fn rejects_bad_toml_without_overwriting() {
        let path = temp_path("bad.toml");
        fs::write(&path, "this is not toml = [").unwrap();
        assert!(Config::load_or_create(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "this is not toml = [");
        let _ = fs::remove_file(path);
    }
}
