//! Small, bounded on-disk cache for lyrics and recommendation responses.
//!
//! No audio is cached. Every entry has a TTL and the directory is trimmed to a
//! configurable byte budget after writes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::store::atomic_write_private;

#[derive(Debug, Clone)]
pub struct FileCache {
    dir: PathBuf,
    max_bytes: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Entry<T> {
    saved_at_ms: u64,
    value: T,
}

impl FileCache {
    pub fn new(dir: PathBuf, max_mb: u64) -> Self {
        Self {
            dir,
            max_bytes: max_mb.max(1) * 1024 * 1024,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    fn path_for(&self, key: &str) -> Option<PathBuf> {
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        {
            return None;
        }
        Some(self.dir.join(format!("{key}.json")))
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str, ttl_secs: u64) -> Option<T> {
        let path = self.path_for(key)?;
        let bytes = fs::read(&path).ok()?;
        let entry: Entry<T> = serde_json::from_slice(&bytes).ok()?;
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
        let bytes = serde_json::to_vec(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        atomic_write_private(&path, &bytes)?;
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
            if entry.file_type()?.is_file() {
                let _ = fs::remove_file(entry.path());
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
            let meta = entry.metadata()?;
            if meta.is_file() {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, meta.len(), entry.path()));
            }
        }
        let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
        if total <= self.max_bytes {
            return Ok(());
        }
        files.sort_by_key(|(mtime, _, _)| *mtime);
        for (_, len, path) in files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
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
        assert!(cache.size_bytes() > 0);
        cache.clear().unwrap();
        assert_eq!(cache.size_bytes(), 0);
    }
}
