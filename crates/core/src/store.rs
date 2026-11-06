//! Crash-safe persistence for the playback queue and related state.

use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use yoruplay_protocol::{PlayMode, PlaySource, QueueItem};

const STATE_VERSION: u32 = 2;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub version: u32,
    #[serde(default, skip_serializing)]
    pub queue: Arc<Vec<QueueItem>>,
    #[serde(default)]
    pub queue_revision: u64,
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
        queue: Arc<Vec<QueueItem>>,
        current: Option<usize>,
        mode: PlayMode,
        source: PlaySource,
        volume: f64,
        position_ms: u64,
        queue_revision: u64,
    ) -> Self {
        Self {
            version: STATE_VERSION,
            queue,
            queue_revision,
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
    let file = fs::File::open(path)?;
    let parsed: Result<PersistedState, _> = serde_json::from_reader(BufReader::new(file));
    let mut state = match parsed {
        Ok(state) => state,
        Err(e) => {
            let aside = corrupt_path(path);
            let _ = fs::rename(path, &aside);
            return Err(StoreError::Corrupt(e.to_string()));
        }
    };
    if state.version > STATE_VERSION {
        let aside = corrupt_path(path);
        let _ = fs::rename(path, &aside);
        return Err(StoreError::Corrupt(format!(
            "unsupported state version {}",
            state.version
        )));
    }
    if state.version == STATE_VERSION {
        let queue_path = queue_path(path, state.queue_revision);
        let loaded = fs::File::open(&queue_path).and_then(|file| {
            serde_json::from_reader(BufReader::new(file)).map_err(io::Error::other)
        });
        match loaded {
            Ok(queue) => state.queue = Arc::new(queue),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(StoreError::Io(error));
            }
            Err(error) => {
                let _ = fs::rename(path, corrupt_path(path));
                return Err(StoreError::Corrupt(format!("queue snapshot: {error}")));
            }
        }
        remove_unreferenced_queues(path, state.queue_revision)?;
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
    let queue_path = queue_path(path, state.queue_revision);
    if !queue_path.exists() {
        atomic_write_json(&queue_path, state.queue.as_ref())?;
    }
    atomic_write_json(path, state)?;
    remove_unreferenced_queues(path, state.queue_revision)?;
    Ok(())
}

fn remove_unreferenced_queues(path: &Path, committed_revision: u64) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(revision) = name
            .strip_prefix("queue-")
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if revision != committed_revision && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn queue_path(state_path: &Path, revision: u64) -> PathBuf {
    state_path.with_file_name(format!("queue-{revision}.json"))
}

pub(crate) fn atomic_write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> io::Result<()> {
    atomic_write_with(path, |writer| {
        serde_json::to_writer(writer, value).map_err(io::Error::other)
    })
}

pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_write_with(path, |writer| writer.write_all(bytes))
}

fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<fs::File>) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    let result = (|| {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let mut writer = BufWriter::with_capacity(16 * 1024, file);
        write(&mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
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
            Arc::new(vec![item(1), item(2)]),
            Some(1),
            PlayMode::ListLoop,
            PlaySource::none(),
            0.42,
            1234,
            1,
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
            Arc::new(vec![item(1), item(2)]),
            Some(9),
            PlayMode::Sequential,
            PlaySource::none(),
            0.7,
            0,
            1,
        );
        state.version = STATE_VERSION;
        save_state(&path, &state).unwrap();
        assert_eq!(load_state(&path).unwrap().unwrap().current, Some(1));
    }

    #[test]
    fn legacy_queue_migrates_without_losing_local_only_or_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let legacy = serde_json::json!({
            "version": 1,
            "queue": [item(7), item(9)],
            "current": 1,
            "source": { "local_only": true },
            "position_ms": 1234,
            "volume": 0.5,
            "mode": "sequential"
        });
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let mut loaded = load_state(&path).unwrap().unwrap();
        loaded.version = STATE_VERSION;
        loaded.queue_revision = 42;
        save_state(&path, &loaded).unwrap();
        let restored = load_state(&path).unwrap().unwrap();
        assert_eq!(restored.queue[restored.current.unwrap()].song.id, 9);
        assert_eq!(restored.position_ms, 1234);
        assert!(restored.source.local_only);
        assert_eq!(restored.mode, PlayMode::Sequential);
    }

    #[test]
    fn missing_queue_snapshot_does_not_restore_an_empty_queue_silently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = PersistedState::new(
            Arc::new(vec![item(7)]),
            Some(0),
            PlayMode::Sequential,
            PlaySource::none(),
            0.5,
            0,
            42,
        );
        save_state(&path, &state).unwrap();
        fs::remove_file(queue_path(&path, 42)).unwrap();
        assert!(matches!(load_state(&path), Err(StoreError::Corrupt(_))));
        assert!(!path.exists());
    }

    #[test]
    fn interrupted_queue_commit_cannot_replace_the_next_real_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let committed = PersistedState::new(
            Arc::new(vec![item(7)]),
            Some(0),
            PlayMode::Sequential,
            PlaySource::none(),
            0.5,
            123,
            10,
        );
        save_state(&path, &committed).unwrap();
        atomic_write_json(&queue_path(&path, 11), &vec![item(8)]).unwrap();
        let mut recovered = load_state(&path).unwrap().unwrap();
        recovered.queue = Arc::new(vec![item(9)]);
        recovered.queue_revision = 11;
        save_state(&path, &recovered).unwrap();
        let restarted = load_state(&path).unwrap().unwrap();
        assert_eq!(restarted.queue[restarted.current.unwrap()].song.id, 9);
        assert_eq!(restarted.position_ms, 123);
    }
}
