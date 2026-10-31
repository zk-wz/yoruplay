//! Crash-safe persistence for the playback queue and related state.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yoruplay_protocol::{PlayMode, PlaySource, QueueItem};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub queue: Vec<QueueItem>,
    #[serde(default)]
    pub current: Option<usize>,
    #[serde(default)]
    pub mode: PlayMode,
    #[serde(default)]
    pub source: PlaySource,
    #[serde(default)]
    pub volume: f64,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub saved_at_ms: u64,
}

impl PersistedState {
    pub fn new(
        queue: Vec<QueueItem>,
        current: Option<usize>,
        mode: PlayMode,
        source: PlaySource,
        volume: f64,
        position_ms: u64,
    ) -> Self {
        Self {
            version: STATE_VERSION,
            queue,
            current,
            mode,
            source,
            volume: volume.clamp(0.0, 1.0),
            position_ms,
            saved_at_ms: now_ms(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to read state: {0}")]
    Io(#[from] io::Error),
    #[error("state file is invalid and was moved aside: {0}")]
    Corrupt(String),
}

/// Loads state. A missing file is not an error. A corrupt file is renamed to
/// `state.json.corrupt-<timestamp>` so the daemon can continue and the user can
/// inspect it later.
pub fn load_state(path: &Path) -> Result<Option<PersistedState>, StoreError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let parsed: Result<PersistedState, _> = serde_json::from_str(&text);
    let mut state = match parsed {
        Ok(state) => state,
        Err(e) => {
            let aside = corrupt_path(path);
            let _ = fs::rename(path, &aside);
            return Err(StoreError::Corrupt(e.to_string()));
        }
    };
    if state.version != 0 && state.version != STATE_VERSION {
        let aside = corrupt_path(path);
        let _ = fs::rename(path, &aside);
        return Err(StoreError::Corrupt(format!(
            "unsupported state version {}",
            state.version
        )));
    }
    if state.queue.is_empty() {
        state.current = None;
    } else if state.current.is_some_and(|i| i >= state.queue.len()) {
        state.current = state.queue.len().checked_sub(1);
    }
    if !state.volume.is_finite() {
        state.volume = 0.7;
    }
    state.volume = state.volume.clamp(0.0, 1.0);
    Ok(Some(state))
}

pub fn save_state(path: &Path, state: &PersistedState) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|e| StoreError::Corrupt(format!("could not serialise state: {e}")))?;
    atomic_write_private(path, &bytes)?;
    Ok(())
}

pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn corrupt_path(path: &Path) -> PathBuf {
    path.with_extension(format!("json.corrupt-{}", now_ms()))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yoruplay_protocol::Song;

    fn item(id: u64) -> QueueItem {
        QueueItem {
            song: Song {
                id,
                name: format!("song {id}"),
                artists: vec![],
                artist_ids: vec![],
                album: String::new(),
                album_id: 0,
                cover_url: None,
                duration_ms: 1,
                fee: 0,
                playable: None,
            },
            context: None,
        }
    }

    #[test]
    fn state_roundtrip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = PersistedState::new(
            vec![item(1), item(2)],
            Some(1),
            PlayMode::ListLoop,
            PlaySource::none(),
            0.42,
            1234,
        );
        save_state(&path, &state).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = load_state(&path).unwrap().unwrap();
        assert_eq!(loaded.queue.len(), 2);
        assert_eq!(loaded.current, Some(1));
        assert_eq!(loaded.volume, 0.42);
        assert_eq!(loaded.position_ms, 1234);
    }

    #[test]
    fn corrupt_state_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "{broken").unwrap();
        assert!(load_state(&path).is_err());
        assert!(!path.exists());
        let found = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(found);
    }

    #[test]
    fn invalid_current_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = PersistedState::new(
            vec![item(1), item(2)],
            Some(9),
            PlayMode::Sequential,
            PlaySource::none(),
            0.7,
            0,
        );
        state.version = STATE_VERSION;
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path).unwrap().unwrap().current, Some(1));
    }
}
