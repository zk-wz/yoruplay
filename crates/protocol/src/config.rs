use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::models::PlayMode;
use fs2::FileExt;
use serde::{Deserialize, Serialize};

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
            Self::Standard => "standard",
            Self::Higher => "higher",
            Self::Exhigh => "exhigh",
            Self::Lossless => "lossless",
            Self::Hires => "hires",
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct PlaybackConfig {
    pub default_mode: PlayMode,
    pub volume: f64,
    pub restore_queue: bool,
    pub audio_device: String,
    pub prebuffer_kb: u64,
    pub read_ahead_kb: u64,
    pub pcm_buffer_ms: u64,
    pub scratch_mb: u64,
    pub retry_count: u32,
    pub retry_interval_secs: u64,
}
impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            default_mode: PlayMode::ListLoop,
            volume: 0.7,
            restore_queue: true,
            audio_device: String::new(),
            prebuffer_kb: 256,
            read_ahead_kb: 512,
            pcm_buffer_ms: 100,
            scratch_mb: 8,
            retry_count: 3,
            retry_interval_secs: 10,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioCacheConfig {
    pub enabled: bool,
    pub max_gb: f64,
    pub dir: String,
}
impl Default for AudioCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_gb: 10.0,
            dir: String::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub max_mb: u64,
    pub lyrics_ttl_hours: u64,
    pub audio: AudioCacheConfig,
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_mb: 32,
            lyrics_ttl_hours: 720,
            audio: AudioCacheConfig::default(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api: ApiConfig,
    pub ui: UiConfig,
    pub playback: PlaybackConfig,
    pub cache: CacheConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigType {
    Boolean,
    Integer,
    Float,
    String,
    Enum,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ConfigKey {
    pub key: &'static str,
    pub value_type: ConfigType,
    pub unit: &'static str,
    pub apply: &'static str,
    pub choices: &'static [&'static str],
    pub min: f64,
    pub max: f64,
}
macro_rules! key {
    ($key:literal,$kind:ident,$unit:literal,$apply:literal,$min:expr,$max:expr) => { ConfigKey { key: $key, value_type: ConfigType::$kind, unit: $unit, apply: $apply, choices: &[], min: $min, max: $max } };
    ($key:literal,$apply:literal,[$($choice:literal),+]) => { ConfigKey { key: $key, value_type: ConfigType::Enum, unit: "", apply: $apply, choices: &[$($choice),+], min: 0.0, max: 0.0 } };
}
pub const CONFIG_KEYS: &[ConfigKey] = &[
    key!(
        "api.quality",
        "next_network_resolution",
        ["standard", "higher", "exhigh", "lossless", "hires"]
    ),
    key!(
        "api.timeout_secs",
        Integer,
        "seconds",
        "daemon_restart",
        1.0,
        300.0
    ),
    key!("ui.language", "tui_restart", ["en", "zh"]),
    key!(
        "playback.default_mode",
        "startup_default_restored_state_wins",
        [
            "list_loop",
            "track_loop",
            "sequential",
            "shuffle",
            "intelligence"
        ]
    ),
    key!(
        "playback.volume",
        Float,
        "ratio",
        "startup_default_restored_state_wins",
        0.0,
        1.0
    ),
    key!(
        "playback.restore_queue",
        Boolean,
        "",
        "daemon_restart",
        0.0,
        0.0
    ),
    key!(
        "playback.audio_device",
        String,
        "",
        "next_device_reopen",
        0.0,
        0.0
    ),
    key!(
        "playback.prebuffer_kb",
        Integer,
        "KiB",
        "next_source",
        1.0,
        65536.0
    ),
    key!(
        "playback.read_ahead_kb",
        Integer,
        "KiB",
        "next_source",
        1.0,
        65536.0
    ),
    key!(
        "playback.pcm_buffer_ms",
        Integer,
        "milliseconds",
        "next_source",
        1.0,
        5000.0
    ),
    key!(
        "playback.scratch_mb",
        Integer,
        "MiB_disk",
        "next_source",
        1.0,
        1024.0
    ),
    key!(
        "playback.retry_count",
        Integer,
        "retries_after_initial_attempt",
        "next_source",
        0.0,
        u32::MAX as f64
    ),
    key!(
        "playback.retry_interval_secs",
        Integer,
        "seconds",
        "next_source",
        0.0,
        86400.0
    ),
    key!(
        "cache.max_mb",
        Integer,
        "MiB",
        "hot_trim",
        1.0,
        (u64::MAX / (1024 * 1024)) as f64
    ),
    key!(
        "cache.lyrics_ttl_hours",
        Integer,
        "hours",
        "next_metadata_operation",
        1.0,
        (u64::MAX / 3600) as f64
    ),
    key!("cache.audio.enabled", Boolean, "", "hot", 0.0, 0.0),
    key!(
        "cache.audio.max_gb",
        Float,
        "GiB",
        "hot_trim",
        0.0,
        (u64::MAX / (1024 * 1024 * 1024)) as f64
    ),
    key!(
        "cache.audio.dir",
        String,
        "absolute_path_or_empty",
        "daemon_restart",
        0.0,
        0.0
    ),
    key!(
        "log.level",
        "daemon_restart_RUST_LOG_overrides",
        ["off", "error", "warn", "info", "debug", "trace"]
    ),
];
impl ConfigKey {
    pub fn find(key: &str) -> Result<&'static Self, ConfigError> {
        CONFIG_KEYS
            .iter()
            .find(|entry| entry.key == key)
            .ok_or_else(|| ConfigError::Invalid(format!("unknown configuration key `{key}`")))
    }
    pub fn parse(&self, input: &str) -> Result<toml::Value, ConfigError> {
        let invalid = || {
            ConfigError::Invalid(format!(
                "invalid value for `{}`: expected {:?}, range {}..={}, choices {:?}",
                self.key, self.value_type, self.min, self.max, self.choices
            ))
        };
        let value = match self.value_type {
            ConfigType::Boolean => toml::Value::Boolean(input.parse().map_err(|_| invalid())?),
            ConfigType::Integer => {
                let value: u64 = input.parse().map_err(|_| invalid())?;
                if value > i64::MAX as u64 || (value as f64) < self.min || (value as f64) > self.max
                {
                    return Err(invalid());
                }
                toml::Value::Integer(value as i64)
            }
            ConfigType::Float => {
                let input = if self.key == "cache.audio.max_gb" {
                    input.strip_suffix('G').unwrap_or(input)
                } else {
                    input
                };
                let value: f64 = input.parse().map_err(|_| invalid())?;
                if !value.is_finite() || value < self.min || value > self.max {
                    return Err(invalid());
                }
                toml::Value::Float(value)
            }
            ConfigType::Enum => {
                if !self.choices.contains(&input) {
                    return Err(invalid());
                }
                toml::Value::String(input.into())
            }
            ConfigType::String => {
                if input.contains('\0')
                    || (self.key == "cache.audio.dir"
                        && !input.is_empty()
                        && !Path::new(input).is_absolute())
                {
                    return Err(invalid());
                }
                toml::Value::String(input.into())
            }
        };
        Ok(value)
    }
}
impl Config {
    pub fn value(&self, key: &str) -> Result<serde_json::Value, ConfigError> {
        ConfigKey::find(key)?;
        let document =
            serde_json::to_value(self).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        Ok(config_value(&document, key)?.clone())
    }
    pub fn validate_strict(&self) -> Result<(), ConfigError> {
        let document =
            serde_json::to_value(self).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        for entry in CONFIG_KEYS {
            let value = config_value(&document, entry.key)?;
            match value.as_str() {
                Some(input) => {
                    entry.parse(input)?;
                }
                None => {
                    entry.parse(&value.to_string())?;
                }
            }
        }
        Ok(())
    }
    /// Does not create directories, locks or files. Missing files use defaults.
    pub fn load_readonly(path: &Path) -> Result<Self, ConfigError> {
        let cfg = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error.into()),
        };
        cfg.validate_strict()?;
        Ok(cfg)
    }
    pub fn persisted_keys(path: &Path) -> Result<Vec<String>, ConfigError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let document: toml::Value = toml::from_str(&text)?;
        Ok(CONFIG_KEYS
            .iter()
            .filter(|entry| {
                let mut value = &document;
                for part in entry.key.split('.') {
                    let Some(next) = value.get(part) else {
                        return false;
                    };
                    value = next;
                }
                true
            })
            .map(|entry| entry.key.to_string())
            .collect())
    }
    pub fn lock_path(path: &Path) -> PathBuf {
        path.with_extension("lock")
    }
    pub fn load_or_create(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            return Self::load_readonly(path);
        }
        Self::update(path, None, false)
    }
    pub fn set(path: &Path, key: &str, input: &str) -> Result<Self, ConfigError> {
        let value = ConfigKey::find(key)?.parse(input)?;
        Self::update(path, Some((key, value)), false)
    }
    pub fn reset(path: &Path, key: &str) -> Result<Self, ConfigError> {
        let entry = ConfigKey::find(key)?;
        let value = Self::default().value(key)?;
        let input = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        Self::update(path, Some((key, entry.parse(&input)?)), false)
    }
    pub fn reset_all(path: &Path) -> Result<Self, ConfigError> {
        Self::update(path, None, true)
    }
    fn update(
        path: &Path,
        change: Option<(&str, toml::Value)>,
        reset_all: bool,
    ) -> Result<Self, ConfigError> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(Self::lock_path(path))?;
        lock.lock_exclusive()?;
        // The lock covers read/modify/rename, so simultaneous setters cannot lose updates.
        let existing = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if existing.is_some() {
            Self::load_readonly(path)?;
        }
        let mut document: toml_edit::DocumentMut = existing
            .as_deref()
            .unwrap_or("")
            .parse()
            .map_err(|e: toml_edit::TomlError| ConfigError::Invalid(e.to_string()))?;
        if existing.is_none() || reset_all {
            // Updating individual registered leaves retains surrounding comments and tables.
            let defaults = Self::default();
            for entry in CONFIG_KEYS {
                let json = defaults.value(entry.key)?;
                let input = json
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| json.to_string());
                set_document_value(&mut document, entry.key, entry.parse(&input)?);
            }
        }
        if let Some((key, value)) = change {
            set_document_value(&mut document, key, value);
        }
        let text = document.to_string();
        let cfg: Self = toml::from_str(&text)?;
        cfg.validate_strict()?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let temporary = parent.join(format!(
            ".{name}.tmp-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<(), ConfigError> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&temporary)?;
            if let Ok(metadata) = fs::metadata(path) {
                file.set_permissions(metadata.permissions())?;
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(cfg)
    }
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}
fn config_value<'a>(
    mut value: &'a serde_json::Value,
    key: &str,
) -> Result<&'a serde_json::Value, ConfigError> {
    for part in key.split('.') {
        value = value
            .get(part)
            .ok_or_else(|| ConfigError::Invalid(format!("missing registered key `{key}`")))?;
    }
    Ok(value)
}
fn set_document_value(document: &mut toml_edit::DocumentMut, key: &str, value: toml::Value) {
    let mut item = document.as_item_mut();
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let value = match value {
                toml::Value::String(v) => toml_edit::Value::from(v),
                toml::Value::Integer(v) => toml_edit::Value::from(v),
                toml::Value::Float(v) => toml_edit::Value::from(v),
                toml::Value::Boolean(v) => toml_edit::Value::from(v),
                _ => unreachable!("registry contains scalar values only"),
            };
            let target = &mut item[part];
            let decor = target.as_value().map(|old| old.decor().clone());
            *target = toml_edit::Item::Value(value);
            if let (Some(decor), Some(value)) = (decor, target.as_value_mut()) {
                *value.decor_mut() = decor;
            }
            break;
        }
        if item.get(part).is_none() {
            item[part] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        item = &mut item[part];
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
    #[error("invalid configuration: {0}")]
    Invalid(String),
}
#[cfg(test)]
mod tests {
    use super::*;
    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("yoruplay-config-{}-{name}", std::process::id()))
    }
    #[test]
    fn readonly_missing_has_no_side_effects() {
        let path = temporary("missing/config.toml");
        assert!(!path.exists());
        let config = Config::load_readonly(&path).unwrap();
        assert_eq!(config.playback.retry_count, 3);
        assert_eq!(config.playback.retry_interval_secs, 10);
        assert!(!path.parent().unwrap().exists());
    }
    #[test]
    fn writes_preserve_comments_and_reject_invalid_values() {
        let path = temporary("comments.toml");
        fs::write(&path, "# settings\n[cache.audio]\nmax_gb = 10 # budget\n").unwrap();
        Config::set(&path, "cache.audio.max_gb", "0.5G").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# settings") && text.contains("# budget"));
        for invalid in ["NaN", "-1", "1e99"] {
            assert!(Config::set(&path, "cache.audio.max_gb", invalid).is_err());
        }
        assert_eq!(text, fs::read_to_string(&path).unwrap());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Config::lock_path(&path));
    }
    #[test]
    fn simultaneous_updates_do_not_overwrite_other_keys() {
        let path = temporary("parallel.toml");
        let _ = fs::remove_file(&path);
        let a = path.clone();
        let b = path.clone();
        let a = std::thread::spawn(move || Config::set(&a, "playback.retry_count", "5").unwrap());
        let b = std::thread::spawn(move || Config::set(&b, "api.timeout_secs", "30").unwrap());
        a.join().unwrap();
        b.join().unwrap();
        let config = Config::load_readonly(&path).unwrap();
        assert_eq!(config.playback.retry_count, 5);
        assert_eq!(config.api.timeout_secs, 30);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Config::lock_path(&path));
    }
    #[test]
    fn resetting_one_key_preserves_other_user_settings() {
        let path = temporary("reset-one.toml");
        let _ = fs::remove_file(&path);
        Config::set(&path, "cache.audio.max_gb", "2G").unwrap();
        Config::set(&path, "playback.retry_count", "5").unwrap();
        Config::reset(&path, "playback.retry_count").unwrap();
        let loaded = Config::load_readonly(&path).unwrap();
        assert_eq!(loaded.playback.retry_count, 3);
        assert_eq!(loaded.cache.audio.max_gb, 2.0);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Config::lock_path(&path));
    }
    #[test]
    fn malformed_files_are_never_replaced_even_by_reset_all() {
        let path = temporary("malformed.toml");
        fs::write(&path, "[broken").unwrap();
        assert!(Config::reset_all(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[broken");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(Config::lock_path(&path));
    }
}
