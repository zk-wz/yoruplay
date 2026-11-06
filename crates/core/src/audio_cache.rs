//! Local, FIFO-managed audio storage. Network acquisition lives in `transfer`.
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use redb::{Database, ReadableTable, TableDefinition};
use rustix::fs::{AtFlags, Mode, OFlags};
use serde::{Deserialize, Serialize};
use yoruplay_protocol::Song;

use crate::{
    audio::AudioInput,
    transfer::{Transfer, TransferControl, TransferOptions},
};

const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("audio_entries_v1");
const AGE: TableDefinition<(u64, u64), u64> = TableDefinition::new("audio_fifo_v1");
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_LYRICS: usize = 128 * 1024;
pub(crate) const MAX_ARTWORK: usize = 512 * 1024;
const MAX_LIST_ITEMS: usize = 100;
const MAX_LIST_BYTES: usize = 4 * 1024 * 1024 - 1024;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("音频缓存 I/O 错误：{0}")]
    Io(#[from] io::Error),
    #[error("音频缓存索引错误：{0}")]
    Index(String),
    #[error("音频网络请求失败：{0}")]
    Network(String),
    #[error("音频重试次数已用尽：{0}")]
    RetryExhausted(String),
    #[error("音频服务器拒绝请求（HTTP {0}）")]
    Http(u16),
    #[error("音频传输已取消")]
    Cancelled,
    #[error("不支持的音频：{0}")]
    Unsupported(String),
    #[error("音频完整性检查失败：{0}")]
    Integrity(String),
    #[error("当前缓存容量无法保留此音频；播放使用有界临时磁盘窗口")]
    NotCacheable,
    #[error("音频解码器仍持有文件，不能写入标签或发布缓存")]
    ReadersActive,
    #[error("音频缓存列表每页最多 100 项；请减小 limit")]
    PageLimit,
    #[error(
        "音频缓存列表超过 4 MiB 响应上限；请保持 offset={offset} 并将 limit 减至 {limit} 或以下"
    )]
    PageTooLarge { offset: usize, limit: usize },
}

impl CacheError {
    pub fn user_message(&self) -> String {
        self.to_string()
    }
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Network(_) | Self::Http(408 | 425 | 429 | 500..=599)
        )
    }
}

fn index_error(error: impl std::fmt::Display) -> CacheError {
    CacheError::Index(error.to_string())
}
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub song: Song,
    pub quality: Option<String>,
    pub bitrate: u64,
    pub format: String,
    pub bytes: u64,
    pub completed_at_ms: u64,
    pub path: PathBuf,
    pub lyrics: Option<String>,
    pub translated_lyrics: Option<String>,
    pub artwork: Option<PathBuf>,
    #[serde(default)]
    pub artwork_bytes: u64,
}

/// List DTO: serde ignores lyric fields in index/manifest records, rather than
/// allocating full lyrics and then pretending the full entry did not have them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheSummary {
    pub song: Song,
    pub quality: Option<String>,
    pub bitrate: u64,
    pub format: String,
    pub bytes: u64,
    pub completed_at_ms: u64,
    pub path: PathBuf,
    pub artwork: Option<PathBuf>,
    #[serde(default)]
    pub artwork_bytes: u64,
}

#[derive(Default, Clone)]
pub struct CacheMetadata {
    pub lyrics: Option<String>,
    pub translated_lyrics: Option<String>,
    pub artwork: Option<Vec<u8>>,
}

impl CacheMetadata {
    pub(crate) fn bounded(mut self) -> Self {
        for text in [&mut self.lyrics, &mut self.translated_lyrics]
            .into_iter()
            .flatten()
        {
            truncate_text(text, MAX_LYRICS);
        }
        if self
            .artwork
            .as_ref()
            .is_some_and(|data| data.len() > MAX_ARTWORK || artwork_extension(data).is_none())
        {
            self.artwork = None;
        }
        self
    }
}

pub struct CachedAudio {
    pub entry: CacheEntry,
    pub input: AudioInput,
    pub byte_len: u64,
}

#[derive(Clone)]
pub struct AudioCache {
    pub(crate) inner: Arc<CacheInner>,
}

pub(crate) struct CacheInner {
    pub(crate) dir: PathBuf,
    db: Database,
    _process_lock: File,
    pub(crate) http: reqwest::Client,
    pub(crate) coordinator: tokio::sync::Mutex<()>,
    pub(crate) current: Mutex<Weak<TransferControl>>,
    state: Mutex<CacheState>,
    epoch: AtomicU64,
    memory_backed: bool,
}

struct CacheState {
    enabled: bool,
    limit: u64,
    retained: u64,
    reserved: u64,
    scratch: u64,
    scratch_limit: u64,
    entries: u64,
    last_completed_ms: u64,
    pins: HashMap<PathBuf, PinState>,
}

struct PinState {
    count: usize,
    bytes: u64,
    retired: Option<CacheEntry>,
}

pub(crate) struct CachePin {
    cache: AudioCache,
    path: PathBuf,
}

impl Drop for CachePin {
    fn drop(&mut self) {
        let mut state = lock(&self.cache.inner.state);
        let retired = if let Some(pin) = state.pins.get_mut(&self.path) {
            pin.count = pin.count.saturating_sub(1);
            if pin.count == 0 {
                state.pins.remove(&self.path).and_then(|pin| pin.retired)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(entry) = retired {
            if self.cache.remove_files(&entry).is_ok() {
                state.retained = state
                    .retained
                    .saturating_sub(self.cache.entry_bytes(&entry));
            }
        }
        if let Err(error) = self.cache.trim_locked(&mut state, 0) {
            tracing::warn!("释放音频 pin 后无法收缩缓存：{error}");
        }
    }
}

struct PinnedFile {
    file: File,
    _pin: CachePin,
}
impl Read for PinnedFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}
impl Seek for PinnedFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl AudioCache {
    pub fn open(dir: PathBuf, max_bytes: u64, enabled: bool) -> Result<Self, CacheError> {
        fs::create_dir_all(&dir)?;
        let dir = fs::canonicalize(dir)?;
        let process_lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("cache.lock"))?;
        fs2::FileExt::try_lock_exclusive(&process_lock)?;
        let memory_backed = probe_filesystem(&dir)?["memory_backed"]
            .as_bool()
            .unwrap_or(false);
        let mut builder = Database::builder();
        builder.set_cache_size(1024 * 1024);
        let db = builder
            .create(dir.join("index.redb"))
            .map_err(index_error)?;
        let transaction = db.begin_write().map_err(index_error)?;
        {
            transaction.open_table(ENTRIES).map_err(index_error)?;
            transaction.open_table(AGE).map_err(index_error)?;
        }
        transaction.commit().map_err(index_error)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(30))
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(|_| CacheError::Network("无法创建 HTTP 客户端".into()))?;
        let cache = Self {
            inner: Arc::new(CacheInner {
                dir,
                db,
                _process_lock: process_lock,
                http,
                coordinator: tokio::sync::Mutex::new(()),
                current: Mutex::new(Weak::new()),
                state: Mutex::new(CacheState {
                    enabled,
                    limit: max_bytes,
                    retained: 0,
                    reserved: 0,
                    scratch: 0,
                    scratch_limit: 8 * 1024 * 1024,
                    entries: 0,
                    last_completed_ms: 0,
                    pins: HashMap::new(),
                }),
                epoch: AtomicU64::new(0),
                memory_backed,
            }),
        };
        cache.reconcile()?;
        cache.reconfigure(max_bytes, enabled)?;
        Ok(cache)
    }

    pub fn directory(&self) -> &Path {
        &self.inner.dir
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }
    pub(crate) fn memory_backed(&self) -> bool {
        self.inner.memory_backed
    }

    /// Offline inspection deliberately does not open redb: redb 2.x may repair or
    /// update its header even when a caller only intends to begin read transactions.
    pub fn inspect(
        dir: &Path,
        max_bytes: u64,
        enabled: bool,
    ) -> Result<serde_json::Value, CacheError> {
        let mut bytes = 0_u64;
        let mut count = 0_u64;
        visit_summaries(dir, |entry| {
            bytes = bytes
                .saturating_add(entry.bytes)
                .saturating_add(entry.artwork_bytes);
            count += 1;
        })?;
        Ok(serde_json::json!({
            "dir": dir, "enabled": enabled, "audio_bytes": bytes, "reserved_bytes": 0,
            "scratch_bytes": 0, "max_bytes": max_bytes, "limit_bytes": max_bytes,
            "entry_count": count, "pinned_bytes": 0, "over_budget_bytes": bytes.saturating_sub(max_bytes),
            "over_budget": bytes > max_bytes, "index_bytes": local_file_bytes(&dir.join("index.redb"))?,
        }))
    }

    pub fn list_offline(
        dir: &Path,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<CacheSummary>, CacheError> {
        if limit > MAX_LIST_ITEMS {
            return Err(CacheError::PageLimit);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(limit);
        let mut response_bytes = 2;
        let mut after = None;
        let mut remaining_offset = offset;
        loop {
            let mut batch: BTreeMap<u64, CacheSummary> = BTreeMap::new();
            visit_summaries(dir, |entry| {
                let id = entry.song.id;
                if after.is_some_and(|after| id <= after) {
                    return;
                }
                if let Some(current) = batch.get_mut(&id) {
                    if entry.completed_at_ms > current.completed_at_ms {
                        *current = entry;
                    }
                    return;
                }
                if batch.len() < MAX_LIST_ITEMS {
                    batch.insert(id, entry);
                } else if batch
                    .last_key_value()
                    .is_some_and(|(&largest, _)| id < largest)
                {
                    batch.pop_last();
                    batch.insert(id, entry);
                }
            })?;
            let batch_len = batch.len();
            if batch_len == 0 {
                break;
            }
            after = batch.last_key_value().map(|(&id, _)| id);
            for (_, entry) in batch {
                if remaining_offset != 0 {
                    remaining_offset -= 1;
                    continue;
                }
                push_summary(&mut result, entry, &mut response_bytes, offset)?;
                if result.len() == limit {
                    return Ok(result);
                }
            }
            if batch_len < MAX_LIST_ITEMS {
                break;
            }
        }
        Ok(result)
    }

    fn read_entry(&self, id: u64) -> Result<Option<CacheEntry>, CacheError> {
        let transaction = self.inner.db.begin_read().map_err(index_error)?;
        let table = transaction.open_table(ENTRIES).map_err(index_error)?;
        let entry = table
            .get(id)
            .map_err(index_error)?
            .map(|value| serde_json::from_slice(value.value()).map_err(index_error))
            .transpose();
        entry
    }

    pub fn entry(&self, id: u64) -> Result<Option<CacheEntry>, CacheError> {
        let mut state = lock(&self.inner.state);
        let Some(entry) = self.read_entry(id)? else {
            return Ok(None);
        };
        if self.valid_entry(&entry) {
            return Ok(Some(entry));
        }
        self.remove_record(&entry)?;
        state.retained = state.retained.saturating_sub(self.entry_bytes(&entry));
        state.entries = state.entries.saturating_sub(1);
        self.remove_files(&entry)?;
        Ok(None)
    }

    pub fn lookup(&self, id: u64) -> Result<Option<CachedAudio>, CacheError> {
        let mut state = lock(&self.inner.state);
        let Some(entry) = self.read_entry(id)? else {
            return Ok(None);
        };
        if !self.valid_entry(&entry) {
            self.remove_record(&entry)?;
            state.retained = state.retained.saturating_sub(self.entry_bytes(&entry));
            state.entries = state.entries.saturating_sub(1);
            self.remove_files(&entry)?;
            return Ok(None);
        }
        let file = File::open(&entry.path)?;
        let pin = self.pin_locked(&mut state, &entry);
        let byte_len = entry.bytes;
        Ok(Some(CachedAudio {
            entry,
            input: Box::new(PinnedFile { file, _pin: pin }),
            byte_len,
        }))
    }

    pub fn list(&self, offset: usize, limit: usize) -> Result<Vec<CacheSummary>, CacheError> {
        if limit > MAX_LIST_ITEMS {
            return Err(CacheError::PageLimit);
        }
        let _state = lock(&self.inner.state);
        let transaction = self.inner.db.begin_read().map_err(index_error)?;
        let table = transaction.open_table(ENTRIES).map_err(index_error)?;
        let mut result = Vec::with_capacity(limit);
        let mut response_bytes = 2;
        for row in table.iter().map_err(index_error)?.skip(offset).take(limit) {
            let (_, value) = row.map_err(index_error)?;
            let entry: CacheSummary = serde_json::from_slice(value.value()).map_err(index_error)?;
            if managed_path(&self.inner.dir, &entry.path)
                && fs::symlink_metadata(&entry.path)
                    .is_ok_and(|meta| meta.file_type().is_file() && meta.len() == entry.bytes)
            {
                push_summary(&mut result, entry, &mut response_bytes, offset)?;
            }
        }
        Ok(result)
    }

    pub fn info(&self) -> Result<serde_json::Value, CacheError> {
        let state = lock(&self.inner.state);
        let pinned: u64 = state.pins.values().map(|pin| pin.bytes).sum();
        let used = state.retained.saturating_add(state.reserved);
        Ok(serde_json::json!({
            "dir": self.inner.dir, "enabled": state.enabled, "audio_bytes": state.retained,
            "reserved_bytes": state.reserved, "scratch_bytes": state.scratch,
            "scratch_limit_bytes": state.scratch_limit,
            "max_bytes": state.limit, "limit_bytes": state.limit, "entry_count": state.entries,
            "pinned_bytes": pinned, "over_budget_bytes": used.saturating_sub(state.limit),
            "over_budget": used > state.limit,
            "memory_backed": self.inner.memory_backed,
            "index_bytes": local_file_bytes(&self.inner.dir.join("index.redb"))?,
        }))
    }

    pub fn clear(&self) -> Result<serde_json::Value, CacheError> {
        let mut state = lock(&self.inner.state);
        // Serialize against publication through state: an in-flight candidate
        // either committed before this clear and is removed below, or can no
        // longer commit. Playback's anonymous reader can continue to EOF.
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        let mut removed = 0_u64;
        let mut pinned = 0_u64;
        // One row at a time: cache size never determines the resident allocation.
        let mut after: Option<u64> = None;
        loop {
            let candidate = {
                let transaction = self.inner.db.begin_read().map_err(index_error)?;
                let table = transaction.open_table(ENTRIES).map_err(index_error)?;
                let mut rows = table
                    .range(after.map_or(0, |id| id.saturating_add(1))..)
                    .map_err(index_error)?;
                let entry = rows
                    .next()
                    .transpose()
                    .map_err(index_error)?
                    .map(|(_, row)| {
                        serde_json::from_slice::<CacheEntry>(row.value()).map_err(index_error)
                    })
                    .transpose()?;
                entry
            };
            let Some(entry) = candidate else { break };
            after = Some(entry.song.id);
            if let Some(pin) = state.pins.get_mut(&entry.path) {
                pin.retired = Some(entry.clone());
                self.remove_files(&entry)?;
                self.remove_record(&entry)?;
                state.entries = state.entries.saturating_sub(1);
                pinned += 1;
                removed += 1;
            } else {
                self.evict_locked(&mut state, &entry)?;
                removed += 1;
            }
            if entry.song.id == u64::MAX {
                break;
            }
        }
        Ok(serde_json::json!({
            "removed": removed, "pinned_entries": pinned, "remaining_bytes": state.retained,
            "reserved_bytes": state.reserved, "scratch_bytes": state.scratch,
        }))
    }

    pub fn reconfigure(&self, max_bytes: u64, enabled: bool) -> Result<(), CacheError> {
        let mut state = lock(&self.inner.state);
        state.limit = max_bytes;
        state.enabled = enabled;
        self.trim_locked(&mut state, 0)?;
        Ok(())
    }

    pub async fn start(
        &self,
        song: Song,
        stream: yoruplay_api::StreamUrl,
        metadata: CacheMetadata,
        options: TransferOptions,
    ) -> Result<Transfer, CacheError> {
        let _coordinator = self.inner.coordinator.lock().await;
        let previous = lock(&self.inner.current).upgrade();
        if let Some(control) = previous {
            Transfer::from_control(control).cancel().await;
        }
        if stream.trial {
            return Err(CacheError::Unsupported("试听音源不能播放或缓存".into()));
        }
        let old_pin = {
            let mut state = lock(&self.inner.state);
            self.read_entry(song.id)?
                .filter(|entry| self.valid_entry(entry))
                .map(|entry| self.pin_locked(&mut state, &entry))
        };
        let transfer = Transfer::start(
            self.clone(),
            song,
            stream,
            metadata.bounded(),
            options,
            old_pin,
        )
        .await?;
        *lock(&self.inner.current) = Arc::downgrade(&transfer.control);
        Ok(transfer)
    }

    fn pin_locked(&self, state: &mut CacheState, entry: &CacheEntry) -> CachePin {
        let bytes = self.entry_bytes(entry);
        state
            .pins
            .entry(entry.path.clone())
            .or_insert(PinState {
                count: 0,
                bytes,
                retired: None,
            })
            .count += 1;
        CachePin {
            cache: self.clone(),
            path: entry.path.clone(),
        }
    }

    pub(crate) fn reserve(&self, bytes: u64) -> Result<bool, CacheError> {
        let mut state = lock(&self.inner.state);
        if !state.enabled || bytes == 0 || bytes > state.limit {
            return Ok(false);
        }
        self.trim_locked(&mut state, bytes)?;
        if state
            .retained
            .saturating_add(state.reserved)
            .saturating_add(bytes)
            > state.limit
        {
            return Ok(false);
        }
        state.reserved += bytes;
        Ok(true)
    }

    pub(crate) fn release_reservation(&self, bytes: u64) {
        let mut state = lock(&self.inner.state);
        state.reserved = state.reserved.saturating_sub(bytes);
    }
    pub(crate) fn update_scratch(&self, previous: u64, bytes: u64) {
        let mut state = lock(&self.inner.state);
        state.scratch = state.scratch.saturating_sub(previous).saturating_add(bytes);
    }
    pub(crate) fn set_scratch_limit(&self, bytes: u64) {
        lock(&self.inner.state).scratch_limit = bytes;
    }

    fn trim_locked(&self, state: &mut CacheState, admission: u64) -> Result<(), CacheError> {
        let mut after: Option<(u64, u64)> = None;
        while state
            .retained
            .saturating_add(state.reserved)
            .saturating_add(admission)
            > state.limit
        {
            let candidate = {
                let transaction = self.inner.db.begin_read().map_err(index_error)?;
                let ages = transaction.open_table(AGE).map_err(index_error)?;
                let entries = transaction.open_table(ENTRIES).map_err(index_error)?;
                let mut candidate = None;
                let start = after
                    .map(|(time, id)| {
                        if id == u64::MAX {
                            (time.saturating_add(1), 0)
                        } else {
                            (time, id + 1)
                        }
                    })
                    .unwrap_or((0, 0));
                for row in ages.range(start..).map_err(index_error)? {
                    let (age, id) = row.map_err(index_error)?;
                    after = Some(age.value());
                    if let Some(value) = entries.get(id.value()).map_err(index_error)? {
                        let entry: CacheEntry =
                            serde_json::from_slice(value.value()).map_err(index_error)?;
                        if !state.pins.contains_key(&entry.path) {
                            candidate = Some(entry);
                            break;
                        }
                    }
                }
                candidate
            };
            let Some(entry) = candidate else { break };
            self.evict_locked(state, &entry)?;
        }
        Ok(())
    }

    fn evict_locked(&self, state: &mut CacheState, entry: &CacheEntry) -> Result<(), CacheError> {
        // Removing the recovery manifest first makes an interrupted eviction non-resurrecting.
        self.remove_files(entry)?;
        self.remove_record(entry)?;
        state.retained = state.retained.saturating_sub(self.entry_bytes(entry));
        state.entries = state.entries.saturating_sub(1);
        Ok(())
    }

    fn remove_record(&self, entry: &CacheEntry) -> Result<(), CacheError> {
        let transaction = self.inner.db.begin_write().map_err(index_error)?;
        {
            transaction
                .open_table(ENTRIES)
                .map_err(index_error)?
                .remove(entry.song.id)
                .map_err(index_error)?;
            transaction
                .open_table(AGE)
                .map_err(index_error)?
                .remove((entry.completed_at_ms, entry.song.id))
                .map_err(index_error)?;
        }
        transaction.commit().map_err(index_error)
    }

    fn insert_record(&self, entry: &CacheEntry) -> Result<(), CacheError> {
        let bytes = serde_json::to_vec(entry).map_err(index_error)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(CacheError::Integrity("缓存元数据超出上限".into()));
        }
        let transaction = self.inner.db.begin_write().map_err(index_error)?;
        {
            let mut entries = transaction.open_table(ENTRIES).map_err(index_error)?;
            let mut ages = transaction.open_table(AGE).map_err(index_error)?;
            if let Some(previous) = entries.get(entry.song.id).map_err(index_error)? {
                let previous: CacheEntry =
                    serde_json::from_slice(previous.value()).map_err(index_error)?;
                ages.remove((previous.completed_at_ms, previous.song.id))
                    .map_err(index_error)?;
            }
            entries
                .insert(entry.song.id, bytes.as_slice())
                .map_err(index_error)?;
            ages.insert((entry.completed_at_ms, entry.song.id), entry.song.id)
                .map_err(index_error)?;
        }
        transaction.commit().map_err(index_error)
    }

    pub(crate) fn publish(
        &self,
        file: File,
        mut entry: CacheEntry,
        artwork: Option<&[u8]>,
        reservation: u64,
        stop: crate::transfer::Finalization<'_>,
    ) -> Result<CacheEntry, CacheError> {
        let mut state = lock(&self.inner.state);
        stop.check()?;
        let artwork_bytes = artwork.map_or(0, |data| data.len() as u64);
        let needed = entry
            .bytes
            .saturating_add(artwork_bytes)
            .saturating_sub(reservation);
        if !state.enabled {
            return Err(CacheError::NotCacheable);
        }
        self.trim_locked(&mut state, needed)?;
        if state
            .retained
            .saturating_add(state.reserved)
            .saturating_add(needed)
            > state.limit
        {
            return Err(CacheError::NotCacheable);
        }
        let previous = self.read_entry(entry.song.id)?;
        entry.completed_at_ms = now_ms().max(state.last_completed_ms.saturating_add(1));
        state.last_completed_ms = entry.completed_at_ms;
        let version = now_ns();
        let filename = format!(
            "{} - {} - {} [{}]-{}.{}",
            entry.song.id,
            safe_component(&entry.song.artist_text(), 48),
            safe_component(&entry.song.name, 72),
            safe_component(entry.quality.as_deref().unwrap_or("actual"), 24),
            version,
            entry.format
        );
        entry.path = self.inner.dir.join(filename);
        file.sync_all()?;
        // A manifest is the durable, bounded recovery record for a fully verified inode.
        // It is linked first; a crash before the audio link leaves only a removable manifest.
        let art_file = if let Some(data) = artwork {
            let art = anonymous_file(&self.inner.dir)?;
            (&art).write_all(data)?;
            art.sync_all()?;
            entry.artwork = Some(self.inner.dir.join(format!(
                "{}-{}.{}",
                entry.song.id,
                version,
                artwork_extension(data).unwrap_or("image")
            )));
            entry.artwork_bytes = data.len() as u64;
            Some(art)
        } else {
            None
        };
        let manifest = anonymous_file(&self.inner.dir)?;
        serde_json::to_writer(&manifest, &entry).map_err(index_error)?;
        manifest.sync_all()?;
        let publication = (|| -> Result<(), CacheError> {
            stop.check()?;
            link_anonymous(&manifest, &manifest_path(&entry.path))?;
            // Persist the recovery record before any complete audio/art link
            // can survive a crash independently of it.
            File::open(&self.inner.dir)?.sync_all()?;
            if let (Some(art), Some(path)) = (&art_file, &entry.artwork) {
                link_anonymous(art, path)?;
            }
            link_anonymous(&file, &entry.path)?;
            File::open(&self.inner.dir)?.sync_all()?;
            stop.commit(|| self.insert_record(&entry))
        })();
        if let Err(error) = publication {
            // The old preferred record and inode remain untouched until commit.
            if let Some(previous) = &previous {
                let _ = self.insert_record(previous);
            }
            let _ = self.remove_files(&entry);
            return Err(error);
        }
        state.retained = state.retained.saturating_add(self.entry_bytes(&entry));
        if let Some(previous) = previous {
            if let Some(pin) = state.pins.get_mut(&previous.path) {
                pin.retired = Some(previous);
            } else {
                self.remove_files(&previous)?;
                state.retained = state.retained.saturating_sub(self.entry_bytes(&previous));
            }
        } else {
            state.entries += 1;
        }
        // The caller owns and releases its reservation exactly once after publication.
        Ok(entry)
    }

    fn entry_bytes(&self, entry: &CacheEntry) -> u64 {
        entry.bytes.saturating_add(entry.artwork_bytes)
    }

    fn valid_entry(&self, entry: &CacheEntry) -> bool {
        managed_path(&self.inner.dir, &entry.path)
            && entry.bytes > 0
            && fs::symlink_metadata(manifest_path(&entry.path))
                .is_ok_and(|meta| meta.file_type().is_file() && meta.len() <= MAX_RECORD_BYTES)
            && fs::symlink_metadata(&entry.path)
                .is_ok_and(|meta| meta.file_type().is_file() && meta.len() == entry.bytes)
    }

    fn remove_files(&self, entry: &CacheEntry) -> Result<(), CacheError> {
        if managed_path(&self.inner.dir, &entry.path) {
            remove_if_present(&manifest_path(&entry.path))?;
            remove_if_present(&entry.path)?;
        }
        if let Some(art) = &entry.artwork {
            if managed_path(&self.inner.dir, art) {
                remove_if_present(art)?;
            }
        }
        // Make interrupted eviction non-resurrecting before its index removal.
        File::open(&self.inner.dir)?.sync_all()?;
        Ok(())
    }

    fn reconcile(&self) -> Result<(), CacheError> {
        // Scan manifests incrementally. No tags, covers or whole index are loaded.
        for item in fs::read_dir(&self.inner.dir)? {
            let item = item?;
            if !item.file_type()?.is_file()
                || !item.file_name().to_string_lossy().ends_with(".entry.json")
            {
                continue;
            }
            if item.metadata()?.len() > MAX_RECORD_BYTES {
                continue;
            }
            let entry: CacheEntry = match serde_json::from_reader(BufReader::with_capacity(
                64 * 1024,
                File::open(item.path())?,
            )) {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if manifest_path(&entry.path) != item.path()
                || !managed_path(&self.inner.dir, &entry.path)
            {
                continue;
            }
            if !self.valid_entry(&entry) {
                self.remove_files(&entry)?;
                continue;
            }
            match self.read_entry(entry.song.id)? {
                Some(current) if current.path == entry.path => {}
                Some(current)
                    if current.completed_at_ms >= entry.completed_at_ms
                        && self.valid_entry(&current) =>
                {
                    self.remove_files(&entry)?;
                }
                current => {
                    if crate::transfer::verify_recovered(
                        &File::open(&entry.path)?,
                        &entry.format,
                        entry.bytes,
                    )
                    .is_err()
                    {
                        self.remove_files(&entry)?;
                        continue;
                    }
                    self.insert_record(&entry)?;
                    if let Some(old) = current {
                        self.remove_files(&old)?;
                    }
                }
            }
        }
        let mut after = None;
        loop {
            let entry = {
                let transaction = self.inner.db.begin_read().map_err(index_error)?;
                let entries = transaction.open_table(ENTRIES).map_err(index_error)?;
                let mut rows = entries
                    .range(after.map_or(0, |id: u64| id.saturating_add(1))..)
                    .map_err(index_error)?;
                let entry = rows
                    .next()
                    .transpose()
                    .map_err(index_error)?
                    .map(|(_, row)| {
                        serde_json::from_slice::<CacheEntry>(row.value()).map_err(index_error)
                    })
                    .transpose()?;
                entry
            };
            let Some(entry) = entry else { break };
            after = Some(entry.song.id);
            if self.valid_entry(&entry) {
                let mut state = lock(&self.inner.state);
                state.retained = state.retained.saturating_add(self.entry_bytes(&entry));
                state.entries += 1;
                state.last_completed_ms = state.last_completed_ms.max(entry.completed_at_ms);
            } else {
                self.remove_record(&entry)?;
                self.remove_files(&entry)?;
            }
            if entry.song.id == u64::MAX {
                break;
            }
        }
        Ok(())
    }
}

pub(crate) fn anonymous_file(directory: &Path) -> Result<File, CacheError> {
    rustix::fs::open(
        directory,
        OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(|error| {
        CacheError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("缓存目录必须支持 Linux O_TMPFILE 与无特权 linkat 发布：{error}"),
        ))
    })
}

pub(crate) fn link_anonymous(file: &File, destination: &Path) -> Result<(), CacheError> {
    let source = format!("/proc/self/fd/{}", file.as_raw_fd());
    rustix::fs::linkat(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination,
        AtFlags::SYMLINK_FOLLOW,
    )
    .map_err(|error| CacheError::Io(error.into()))
}
fn managed_path(directory: &Path, path: &Path) -> bool {
    path.parent() == Some(directory) && path.file_name().is_some()
}
fn manifest_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".entry.json");
    PathBuf::from(name)
}
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
fn local_file_bytes(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
pub(crate) fn truncate_text(text: &mut String, limit: usize) {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}
fn safe_component(text: &str, limit: usize) -> String {
    let mut value: String = text
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    truncate_text(&mut value, limit);
    let value = value.trim_matches([' ', '.']);
    if value.is_empty() {
        "unknown".into()
    } else {
        value.into()
    }
}
pub(crate) fn artwork_extension(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else {
        None
    }
}

/// A deliberate diagnostic operation; ordinary list/info calls never perform it.
pub fn probe_filesystem(directory: &Path) -> Result<serde_json::Value, CacheError> {
    let requested = directory;
    let mut directory = directory;
    // Diagnose a not-yet-created cache without creating its directory tree.
    while !directory.try_exists()? {
        directory = directory
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing cache ancestor"))?;
    }
    let file = anonymous_file(directory)?;
    let path = directory.join(format!(".capability-{}-{}", std::process::id(), now_ns()));
    link_anonymous(&file, &path)?;
    fs::remove_file(path)?;
    File::open(directory)?.sync_all()?;
    let filesystem = rustix::fs::statfs(directory).map_err(io::Error::from)?;
    // Linux TMPFS_MAGIC and RAMFS_MAGIC; neither is a disk-backed audio cache.
    let memory_backed = matches!(filesystem.f_type as u64, 0x0102_1994 | 0x8584_58f6);
    Ok(serde_json::json!({
        "o_tmpfile": true, "unprivileged_linkat": true, "directory": directory,
        "requested_directory": requested, "memory_backed": memory_backed,
        "warning": if memory_backed { Some("Audio cache is memory-backed; downloaded audio consumes RAM. Choose a disk-backed directory for low memory usage.") } else { None },
    }))
}

fn visit_summaries(
    directory: &Path,
    mut visit: impl FnMut(CacheSummary),
) -> Result<(), CacheError> {
    let canonical = match fs::canonicalize(directory) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let directory = canonical.as_path();
    let rows = match fs::read_dir(directory) {
        Ok(rows) => rows,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for row in rows {
        let row = row?;
        if !row.file_type()?.is_file()
            || !row.file_name().to_string_lossy().ends_with(".entry.json")
            || row.metadata()?.len() > MAX_RECORD_BYTES
        {
            continue;
        }
        let entry: CacheSummary = match serde_json::from_reader(BufReader::with_capacity(
            64 * 1024,
            File::open(row.path())?,
        )) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if manifest_path(&entry.path) == row.path()
            && managed_path(directory, &entry.path)
            && fs::symlink_metadata(&entry.path)
                .is_ok_and(|m| m.file_type().is_file() && m.len() == entry.bytes)
        {
            visit(entry);
        }
    }
    Ok(())
}

struct JsonBytes(usize);
impl Write for JsonBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn push_summary(
    page: &mut Vec<CacheSummary>,
    entry: CacheSummary,
    bytes: &mut usize,
    offset: usize,
) -> Result<(), CacheError> {
    let mut size = JsonBytes(0);
    serde_json::to_writer(&mut size, &entry).map_err(index_error)?;
    let next = bytes.saturating_add(size.0).saturating_add(1);
    if next > MAX_LIST_BYTES {
        return Err(CacheError::PageTooLarge {
            offset,
            limit: page.len(),
        });
    }
    *bytes = next;
    page.push(entry);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_inspection_never_creates_missing_paths() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("not-created/audio");
        let info = AudioCache::inspect(&absent, 12345, false).unwrap();
        assert_eq!(info["audio_bytes"], 0);
        assert_eq!(info["entry_count"], 0);
        assert_eq!(info["limit_bytes"], 12345);
        assert!(AudioCache::list_offline(&absent, 0, 100)
            .unwrap()
            .is_empty());
        assert!(!absent.parent().unwrap().exists());
    }

    #[test]
    fn metadata_is_bounded_without_splitting_unicode() {
        let text = "音".repeat(MAX_LYRICS);
        let metadata = CacheMetadata {
            lyrics: Some(text),
            translated_lyrics: None,
            artwork: Some(vec![0; MAX_ARTWORK + 1]),
        }
        .bounded();
        assert!(metadata.lyrics.unwrap().len() <= MAX_LYRICS);
        assert!(metadata.artwork.is_none());
        assert_eq!(safe_component("123/歌\\名", 20), "123_歌_名");
    }
}
