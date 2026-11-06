//! Small, bounded on-disk cache for lyrics and recommendation responses.
//!
//! Audio uses a separate disk-backed cache. Metadata entries have a TTL and this
//! directory is trimmed to a configurable byte budget after writes.

use std::fs;
use std::io;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::store::atomic_write_json;

#[derive(Debug, Clone)]
pub struct FileCache {
    dir: PathBuf,
    max_bytes: Arc<AtomicU64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Entry<T> {
    saved_at_ms: u64,
    value: T,
}

fn is_metadata_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        // Audio manifests own this suffix even when configured paths overlap.
        && !key.ends_with(".entry")
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn is_metadata_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_suffix(".json"))
        .is_some_and(is_metadata_key)
}

impl FileCache {
    pub fn new(dir: PathBuf, max_mb: u64) -> Self {
        Self {
            dir,
            max_bytes: Arc::new(AtomicU64::new(max_mb.max(1).saturating_mul(1024 * 1024))),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    pub fn reconfigure(&self, max_mb: u64) -> io::Result<()> {
        self.max_bytes
            .store(max_mb.max(1).saturating_mul(1024 * 1024), Ordering::Relaxed);
        self.trim()
    }

    pub fn migrate_legacy(&self) -> io::Result<()> {
        let Some(root) = self.dir.parent() else {
            return Ok(());
        };
        if !root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let known = matches!(name, "daily_songs.json" | "daily_playlists.json")
                || name
                    .strip_prefix("lyrics_")
                    .and_then(|n| n.strip_suffix(".json"))
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit()));
            if known {
                let target = self.dir.join(name);
                fs::create_dir_all(&self.dir)?;
                if target.exists() {
                    fs::remove_file(entry.path())?;
                } else {
                    fs::rename(entry.path(), target)?;
                }
            }
        }
        self.trim()
    }

    fn path_for(&self, key: &str) -> Option<PathBuf> {
        if !is_metadata_key(key) {
            return None;
        }
        Some(self.dir.join(format!("{key}.json")))
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str, ttl_secs: u64) -> Option<T> {
        let path = self.path_for(key)?;
        let file = fs::File::open(&path).ok()?;
        if file.metadata().ok()?.len() > self.max_bytes() {
            return None;
        }
        let entry: Entry<T> = serde_json::from_reader(BufReader::new(file)).ok()?;
        if ttl_secs == 0
            || now_ms().saturating_sub(entry.saved_at_ms) > ttl_secs.saturating_mul(1000)
        {
            let _ = fs::remove_file(path);
            return None;
        }
        Some(entry.value)
    }

    pub fn put<T: Serialize>(&self, key: &str, value: &T) -> io::Result<()> {
        let Some(path) = self.path_for(key) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache key",
            ));
        };
        fs::create_dir_all(&self.dir)?;
        let entry = Entry {
            saved_at_ms: now_ms(),
            value,
        };
        atomic_write_json(&path, &entry)?;
        self.trim()?;
        Ok(())
    }

    pub fn remove(&self, key: &str) {
        if let Some(path) = self.path_for(key) {
            let _ = fs::remove_file(path);
        }
    }

    pub fn clear(&self) -> io::Result<()> {
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if is_metadata_name(&entry.file_name()) && entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    pub fn size_bytes(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| is_metadata_name(&entry.file_name()))
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    pub fn trim(&self) -> io::Result<()> {
        let mut files = Vec::new();
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !is_metadata_name(&entry.file_name()) {
                continue;
            }
            let meta = entry.metadata()?;
            if meta.is_file() {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, meta.len(), entry.path()));
            }
        }
        let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
        if total <= self.max_bytes() {
            return Ok(());
        }
        files.sort_by_key(|(mtime, _, _)| *mtime);
        for (_, len, path) in files {
            if total <= self.max_bytes() {
                break;
            }
            fs::remove_file(&path)?;
            total = total.saturating_sub(len);
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(dir.path().join("cache"), 1);
        cache.put("lyrics_1", &"hello").unwrap();
        assert_eq!(cache.get::<String>("lyrics_1", 60), Some("hello".into()));
        assert_eq!(cache.get::<String>("lyrics_1", 0), None);
        assert_eq!(cache.get::<String>("lyrics_1", 60), None);
    }

    #[test]
    fn rejects_unsafe_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(dir.path().to_path_buf(), 1);
        assert!(cache.put("../escape", &"x").is_err());
        assert!(cache.get::<String>("../escape", 60).is_none());
    }

    #[test]
    fn trims_to_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(dir.path().to_path_buf(), 1);
        let big = "x".repeat(400 * 1024);
        for i in 0..5 {
            cache.put(&format!("k{i}"), &big).unwrap();
        }
        assert!(cache.size_bytes() <= 1024 * 1024);
        assert_eq!(cache.get::<String>("k4", 60), Some(big));
        cache.clear().unwrap();
        assert_eq!(cache.size_bytes(), 0);
    }

    #[test]
    fn metadata_cleanup_never_owns_audio_files_in_a_shared_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(dir.path().to_path_buf(), 1);
        let audio = dir.path().join("100 - cached audio.flac");
        fs::write(&audio, b"audio").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&audio)
            .unwrap()
            .set_len(2 * 1024 * 1024)
            .unwrap();
        let preserved = [
            ("index.redb", "index"),
            ("100 - cached audio.flac.entry.json", "manifest"),
            ("yoruplay.log", "log"),
        ];
        for (name, contents) in preserved {
            fs::write(dir.path().join(name), contents).unwrap();
        }
        cache.put("lyrics_1", &"cached lyrics").unwrap();
        assert!(audio.exists(), "metadata quota deleted audio");
        assert_eq!(
            cache.size_bytes(),
            fs::metadata(dir.path().join("lyrics_1.json"))
                .unwrap()
                .len()
        );
        cache.clear().unwrap();
        assert!(!dir.path().join("lyrics_1.json").exists());
        assert_eq!(fs::metadata(audio).unwrap().len(), 2 * 1024 * 1024);
        for (name, contents) in preserved {
            assert_eq!(
                fs::read(dir.path().join(name)).unwrap(),
                contents.as_bytes()
            );
        }
    }
}
