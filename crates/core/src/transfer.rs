//! A single demand-driven HTTP acquisition shared by playback and explicit upgrade.
//! The decoder only sees an offset reader; it never owns a URL or HTTP client.
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    tag::{Accessor, ItemKey, Tag, TagExt, TagType},
};
use md5::{Digest, Md5};
use reqwest::{header, Response, StatusCode};
use tokio::sync::Notify;
use yoruplay_protocol::Song;

use crate::{
    audio::AudioInput,
    audio_cache::{
        anonymous_file, artwork_extension, lock, now_ms, truncate_text, AudioCache, CacheEntry,
        CacheError, CacheMetadata, CachePin,
    },
};

const IO_BLOCK: usize = 64 * 1024;
const MAX_RANGES: usize = 4096;
const MAX_TAG_BYTES: usize = 2 * 1024 * 1024;
const MAX_MOOV_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct TransferOptions {
    pub prebuffer_bytes: u64,
    pub read_ahead_bytes: u64,
    pub scratch_bytes: u64,
    pub retry_count: u32,
    pub retry_interval_secs: u64,
    pub retry_budget: Option<Arc<AtomicU32>>,
    pub refresh: Option<RefreshSource>,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            prebuffer_bytes: 256 * 1024,
            read_ahead_bytes: 512 * 1024,
            scratch_bytes: 8 * 1024 * 1024,
            retry_count: 3,
            retry_interval_secs: 10,
            retry_budget: None,
            refresh: None,
        }
    }
}

#[derive(Clone)]
pub struct RefreshSource {
    pub api: yoruplay_api::NeteaseClient,
    pub song_id: u64,
    pub quality: yoruplay_protocol::Quality,
}
impl std::fmt::Debug for RefreshSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RefreshSource")
            .field("song_id", &self.song_id)
            .field("quality", &self.quality)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct Transfer {
    pub(crate) control: Arc<TransferControl>,
}

pub(crate) struct TransferControl {
    shared: Arc<Shared>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    finalizer: tokio::sync::Mutex<()>,
}

struct Shared {
    cache: AudioCache,
    cache_epoch: u64,
    song: Song,
    stream: yoruplay_api::StreamUrl,
    metadata: CacheMetadata,
    current_url: Mutex<(yoruplay_api::StreamUrl, std::time::Instant)>,
    retry_budget: Arc<AtomicU32>,
    cancel_epoch: AtomicU64,
    discarded: AtomicBool,
    publication_gate: Mutex<()>,
    options: TransferOptions,
    state: Mutex<State>,
    wake: Notify,
    readers: Condvar,
}

struct State {
    file: Option<File>,
    ranges: Ranges,
    mode: StorageMode,
    length: Option<u64>,
    demand: u64,
    demand_revision: u64,
    reader_count: usize,
    paused: bool,
    cancelled: bool,
    stopped: bool,
    finalizing: bool,
    complete: bool,
    eof: bool,
    sniff_end: u64,
    drive_full: bool,
    initialized: bool,
    format: Option<Format>,
    error: Option<Failure>,
    published: Option<CacheEntry>,
    old_pin: Option<CachePin>,
    reservation: u64,
    scratch_accounted: u64,
    retries_used: u32,
    expected_md5: Option<[u8; 16]>,
}

#[derive(Clone)]
struct Failure {
    message: String,
    retryable: bool,
    exhausted: bool,
}
impl Failure {
    fn from_error(error: &CacheError) -> Self {
        Self {
            message: error.to_string(),
            retryable: error.is_retryable(),
            exhausted: matches!(error, CacheError::RetryExhausted(_)),
        }
    }
    fn error(&self) -> CacheError {
        if self.exhausted {
            CacheError::RetryExhausted(self.message.clone())
        } else if self.retryable {
            CacheError::Network(self.message.clone())
        } else {
            CacheError::Integrity(self.message.clone())
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageMode {
    Pending,
    Persistent,
    Window,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Mp3,
    Flac,
    Mp4,
    Aac,
    Ogg,
    Wav,
}
impl Format {
    fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Mp4 => "m4a",
            Self::Aac => "aac",
            Self::Ogg => "ogg",
            Self::Wav => "wav",
        }
    }
}

impl Drop for TransferControl {
    fn drop(&mut self) {
        invalidate_publication(&self.shared);
        let mut state = lock(&self.shared.state);
        state.cancelled = true;
        if !state.finalizing {
            state.file.take();
            release_storage(&self.shared, &mut state);
            state.old_pin.take();
        }
        drop(state);
        self.shared.wake.notify_waiters();
        self.shared.readers.notify_all();
        // If the last handle is dropped while its finish future is aborted,
        // cancellation still waits for bounded disk work to close every inode.
        let mut state = lock(&self.shared.state);
        while state.finalizing {
            state = self
                .shared
                .readers
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.reservation != 0 {
            self.cache.release_reservation(state.reservation);
        }
        self.cache.update_scratch(state.scratch_accounted, 0);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Finalization<'a> {
    shared: &'a Shared,
    epoch: u64,
}
impl Finalization<'_> {
    pub(crate) fn check(self) -> Result<(), CacheError> {
        if self.shared.discarded.load(Ordering::Acquire) {
            Err(CacheError::Cancelled)
        } else if self.shared.cache.epoch() != self.shared.cache_epoch {
            Err(CacheError::NotCacheable)
        } else if self.shared.cancel_epoch.load(Ordering::Acquire) == self.epoch {
            Ok(())
        } else {
            Err(CacheError::Cancelled)
        }
    }
    pub(crate) fn commit(
        self,
        write: impl FnOnce() -> Result<(), CacheError>,
    ) -> Result<(), CacheError> {
        let _publication = lock(&self.shared.publication_gate);
        self.check()?;
        write()
    }
}
fn invalidate_publication(shared: &Shared) {
    let _publication = lock(&shared.publication_gate);
    shared.cancel_epoch.fetch_add(1, Ordering::AcqRel);
}
struct FinishAttemptGuard {
    shared: Arc<Shared>,
    armed: bool,
}
impl Drop for FinishAttemptGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        invalidate_publication(&self.shared);
        self.shared.wake.notify_waiters();
        let mut state = lock(&self.shared.state);
        while state.finalizing {
            state = self
                .shared
                .readers
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
struct FinalizingGuard(Arc<Shared>);
impl Drop for FinalizingGuard {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.finalizing = false;
        release_storage(&self.0, &mut state);
        state.old_pin.take();
        drop(state);
        self.0.wake.notify_waiters();
        self.0.readers.notify_all();
    }
}
struct RewriteReservation {
    cache: AudioCache,
    bytes: u64,
}
impl Drop for RewriteReservation {
    fn drop(&mut self) {
        self.cache.release_reservation(self.bytes);
    }
}

fn finalize_complete(
    shared: &Shared,
    file: File,
    format: Format,
    length: u64,
    expected: Option<[u8; 16]>,
    reservation: u64,
    epoch: u64,
) -> Result<CacheEntry, CacheError> {
    let stop = Finalization { shared, epoch };
    stop.check()?;
    verify_complete(&file, format, length, expected, true, Some(stop))?;
    let rewrite_reservation = length.saturating_add(MAX_TAG_BYTES as u64);
    let mut retained_rewrite_reservation = None;
    let final_file = if shared.cache.reserve(rewrite_reservation).unwrap_or(false) {
        let mut guard = RewriteReservation {
            cache: shared.cache.clone(),
            bytes: rewrite_reservation,
        };
        let tagged = rewrite_tags(
            &shared.cache,
            &file,
            length,
            format,
            &shared.song,
            &shared.metadata,
            stop,
        );
        match tagged {
            Ok(tagged) => {
                let extra = tagged.metadata()?.len().saturating_sub(reservation);
                drop(file);
                shared
                    .cache
                    .release_reservation(guard.bytes.saturating_sub(extra));
                guard.bytes = extra;
                retained_rewrite_reservation = Some(guard);
                tagged
            }
            Err(CacheError::Cancelled) => return Err(CacheError::Cancelled),
            Err(error) => {
                tracing::warn!("保留验证后的原音频（标签写入失败）：{error}");
                file
            }
        }
    } else {
        file
    };
    stop.check()?;
    let bytes = final_file.metadata()?.len();
    let entry = CacheEntry {
        song: shared.song.clone(),
        quality: shared.stream.level.clone(),
        bitrate: shared.stream.bitrate,
        format: format.extension().into(),
        bytes,
        completed_at_ms: now_ms(),
        path: Default::default(),
        lyrics: shared.metadata.lyrics.clone(),
        translated_lyrics: shared.metadata.translated_lyrics.clone(),
        artwork: None,
        artwork_bytes: 0,
    };
    let credit = reservation.saturating_add(
        retained_rewrite_reservation
            .as_ref()
            .map_or(0, |guard| guard.bytes),
    );
    shared.cache.publish(
        final_file,
        entry,
        shared.metadata.artwork.as_deref(),
        credit,
        stop,
    )
}

impl Transfer {
    pub(crate) fn from_control(control: Arc<TransferControl>) -> Self {
        Self { control }
    }

    pub(crate) async fn start(
        cache: AudioCache,
        mut song: Song,
        stream: yoruplay_api::StreamUrl,
        metadata: CacheMetadata,
        mut options: TransferOptions,
        old_pin: Option<CachePin>,
    ) -> Result<Self, CacheError> {
        options.scratch_bytes = options.scratch_bytes.max(64 * 1024);
        options.prebuffer_bytes = options
            .prebuffer_bytes
            .max(4096)
            .min(options.scratch_bytes / 2);
        options.read_ahead_bytes = options
            .read_ahead_bytes
            .max(options.prebuffer_bytes)
            .min(options.scratch_bytes / 2);
        cache.set_scratch_limit(options.scratch_bytes);
        // Display metadata is bounded independently of the HTTP object's size.
        truncate_text(&mut song.name, 4096);
        truncate_text(&mut song.album, 4096);
        song.artists.truncate(64);
        song.artist_ids.truncate(64);
        for artist in &mut song.artists {
            truncate_text(artist, 1024);
        }
        song.cover_url = None;
        let file = anonymous_file(cache.directory())?;
        let expected_md5 = stream.md5.as_deref().map(parse_md5).transpose()?;
        let retry_budget = options
            .retry_budget
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicU32::new(options.retry_count)));
        let shared = Arc::new(Shared {
            cache_epoch: cache.epoch(),
            current_url: Mutex::new((stream.clone(), std::time::Instant::now())),
            retry_budget,
            cancel_epoch: AtomicU64::new(0),
            discarded: AtomicBool::new(false),
            publication_gate: Mutex::new(()),
            cache,
            song,
            stream,
            metadata,
            options,
            state: Mutex::new(State {
                file: Some(file),
                ranges: Ranges::default(),
                mode: StorageMode::Pending,
                length: None,
                demand: 0,
                demand_revision: 0,
                reader_count: 0,
                paused: false,
                cancelled: false,
                stopped: false,
                finalizing: false,
                complete: false,
                eof: false,
                sniff_end: 12,
                drive_full: false,
                initialized: false,
                format: None,
                error: None,
                published: None,
                old_pin,
                reservation: 0,
                scratch_accounted: 0,
                retries_used: 0,
                expected_md5,
            }),
            wake: Notify::new(),
            readers: Condvar::new(),
        });
        let worker = shared.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = fetch(worker.clone()).await {
                let mut state = lock(&worker.state);
                if !state.cancelled && !state.stopped {
                    state.error = Some(Failure::from_error(&error));
                }
                if !state.complete {
                    state.file.take();
                    release_storage(&worker, &mut state);
                }
            }
            worker.wake.notify_waiters();
            worker.readers.notify_all();
        });
        let transfer = Self {
            control: Arc::new(TransferControl {
                shared,
                task: tokio::sync::Mutex::new(Some(task)),
                finalizer: tokio::sync::Mutex::new(()),
            }),
        };
        loop {
            let notified = transfer.control.shared.wake.notified();
            let ready = {
                let state = lock(&transfer.control.shared.state);
                if let Some(error) = &state.error {
                    return Err(error.error());
                }
                if state.cancelled {
                    return Err(CacheError::Cancelled);
                }
                let target = state
                    .length
                    .unwrap_or(u64::MAX)
                    .min(transfer.control.shared.options.prebuffer_bytes);
                state.format.is_some()
                    && (state.complete || state.ranges.end_at(0).unwrap_or(0) >= target)
            };
            if ready {
                drop(notified);
                return Ok(transfer);
            }
            notified.await;
        }
    }

    pub fn reader(&self) -> AudioInput {
        let shared = self.control.shared.clone();
        lock(&shared.state).reader_count += 1;
        Box::new(TransferReader {
            shared,
            position: 0,
        })
    }
    pub fn byte_len(&self) -> Option<u64> {
        lock(&self.control.shared.state).length
    }
    pub fn is_complete(&self) -> bool {
        lock(&self.control.shared.state).complete
    }
    pub fn retry_count(&self) -> u32 {
        lock(&self.control.shared.state).retries_used
    }
    pub fn pause(&self, paused: bool) {
        lock(&self.control.shared.state).paused = paused;
        self.control.shared.wake.notify_waiters();
    }

    pub async fn cancel(&self) {
        invalidate_publication(&self.control.shared);
        {
            let mut state = lock(&self.control.shared.state);
            state.cancelled = true;
            if !state.complete {
                state.file.take();
                release_storage(&self.control.shared, &mut state);
                state.old_pin.take();
            }
        }
        self.control.shared.wake.notify_waiters();
        self.control.shared.readers.notify_all();
        self.join().await;
        loop {
            let notified = self.control.shared.wake.notified();
            if !lock(&self.control.shared.state).finalizing {
                break;
            }
            notified.await;
        }
    }

    /// Permanently abandon an explicit upgrade, including a completed candidate.
    /// Ordinary playback uses cancel(), which may still retain already complete bytes.
    pub(crate) async fn discard(&self) {
        {
            let _publication = lock(&self.control.shared.publication_gate);
            self.control.shared.discarded.store(true, Ordering::Release);
        }
        self.cancel().await;
        let mut state = lock(&self.control.shared.state);
        state.file.take();
        release_storage(&self.control.shared, &mut state);
        state.old_pin.take();
    }

    async fn join(&self) {
        let mut task = self.control.task.lock().await;
        if let Some(handle) = task.take() {
            let _ = handle.await;
        }
    }

    pub async fn download_complete(&self) -> Result<(), CacheError> {
        {
            let mut state = lock(&self.control.shared.state);
            if state.cancelled {
                return Err(CacheError::Cancelled);
            }
            if state.mode == StorageMode::Window {
                return Err(CacheError::NotCacheable);
            }
            state.drive_full = true;
            state.paused = false;
        }
        self.control.shared.wake.notify_waiters();
        loop {
            let notified = self.control.shared.wake.notified();
            {
                let state = lock(&self.control.shared.state);
                if let Some(error) = &state.error {
                    return Err(error.error());
                }
                if state.cancelled {
                    return Err(CacheError::Cancelled);
                }
                if state.mode == StorageMode::Window {
                    return Err(CacheError::NotCacheable);
                }
                if state.complete {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub async fn finish(&self) -> Result<Option<CacheEntry>, CacheError> {
        let _finalizer = self.control.finalizer.lock().await;
        loop {
            let notified = self.control.shared.wake.notified();
            if !lock(&self.control.shared.state).finalizing {
                break;
            }
            notified.await;
        }
        let epoch = self.control.shared.cancel_epoch.load(Ordering::Acquire);
        {
            let mut state = lock(&self.control.shared.state);
            if let Some(entry) = &state.published {
                return Ok(Some(entry.clone()));
            }
            state.stopped = true;
        }
        self.control.shared.wake.notify_waiters();
        self.control.shared.readers.notify_all();
        self.join().await;
        let (file, format, length, expected, reservation) = {
            let mut state = lock(&self.control.shared.state);
            if state.reader_count != 0 {
                return Err(CacheError::ReadersActive);
            }
            if !state.complete
                || state.mode != StorageMode::Persistent
                || self.control.shared.cache.epoch() != self.control.shared.cache_epoch
                || self.control.shared.discarded.load(Ordering::Acquire)
            {
                state.file.take();
                release_storage(&self.control.shared, &mut state);
                state.old_pin.take();
                return Ok(None);
            }
            let length = state
                .length
                .ok_or_else(|| CacheError::Integrity("缺少完成对象长度".into()))?;
            if !state.ranges.covers(0, length) {
                return Err(CacheError::Integrity("文件区间未完整覆盖".into()));
            }
            let Some(file) = state.file.take() else {
                return Ok(None);
            };
            let format = state
                .format
                .ok_or_else(|| CacheError::Unsupported("未识别音频容器".into()))?;
            state.finalizing = true;
            (file, format, length, state.expected_md5, state.reservation)
        };
        let mut attempt = FinishAttemptGuard {
            shared: self.control.shared.clone(),
            armed: true,
        };
        let shared = self.control.shared.clone();
        let published = tokio::task::spawn_blocking(move || {
            let guard = FinalizingGuard(shared.clone());
            // All inode-owning work finishes before the completion guard wakes
            // cancel(). Dropping an awaiting future cannot detach a live inode.
            let result =
                finalize_complete(&shared, file, format, length, expected, reservation, epoch);
            {
                let mut state = lock(&shared.state);
                if let Ok(entry) = &result {
                    state.published = Some(entry.clone());
                } else if let Err(error) = &result {
                    state.error = Some(Failure::from_error(error));
                }
            }
            drop(guard);
            result
        })
        .await
        .unwrap_or_else(|_| Err(CacheError::Integrity("音频发布线程异常退出".into())));
        attempt.armed = false;
        match published {
            Ok(entry) => Ok(Some(entry)),
            Err(CacheError::NotCacheable) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn release_storage(shared: &Shared, state: &mut State) {
    if state.reservation != 0 {
        shared
            .cache
            .release_reservation(std::mem::take(&mut state.reservation));
    }
    if state.scratch_accounted != 0 {
        shared
            .cache
            .update_scratch(std::mem::take(&mut state.scratch_accounted), 0);
    }
}

struct TransferReader {
    shared: Arc<Shared>,
    position: u64,
}
impl Drop for TransferReader {
    fn drop(&mut self) {
        lock(&self.shared.state).reader_count -= 1;
        self.shared.wake.notify_waiters();
    }
}
impl Read for TransferReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let mut state = lock(&self.shared.state);
        loop {
            if state.cancelled || state.stopped {
                return Err(io::Error::other("音频传输已取消"));
            }
            if let Some(error) = &state.error {
                return Err(io::Error::new(
                    if error.retryable {
                        io::ErrorKind::ConnectionReset
                    } else {
                        io::ErrorKind::InvalidData
                    },
                    error.message.clone(),
                ));
            }
            if state.length.is_some_and(|length| self.position >= length) && state.eof {
                return Ok(0);
            }
            if let Some(end) = state.ranges.end_at(self.position) {
                let mut count = output.len().min((end - self.position) as usize);
                let offset = if state.mode == StorageMode::Window {
                    let offset = self.position % self.shared.options.scratch_bytes;
                    count = count.min((self.shared.options.scratch_bytes - offset) as usize);
                    offset
                } else {
                    self.position
                };
                let file = state
                    .file
                    .as_ref()
                    .ok_or_else(|| io::Error::other("音频文件已关闭"))?;
                let count = file.read_at(&mut output[..count], offset)?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "音频已下载区间读取失败",
                    ));
                }
                self.position += count as u64;
                state.demand = self.position;
                drop(state);
                self.shared.wake.notify_waiters();
                return Ok(count);
            }
            state.demand = self.position;
            self.shared.wake.notify_waiters();
            state = self
                .shared
                .readers
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
impl Seek for TransferReader {
    fn seek(&mut self, origin: SeekFrom) -> io::Result<u64> {
        let mut state = lock(&self.shared.state);
        let position = match origin {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => {
                i128::from(state.length.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "未知长度音频不能从末尾定位")
                })?) + i128::from(delta)
            }
        };
        if !(0..=i128::from(u64::MAX)).contains(&position) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "音频定位越界"));
        }
        self.position = position as u64;
        state.demand = self.position;
        state.demand_revision = state.demand_revision.wrapping_add(1);
        drop(state);
        self.shared.wake.notify_waiters();
        Ok(self.position)
    }
}

#[derive(Default)]
struct Ranges(BTreeMap<u64, u64>);
impl Ranges {
    fn end_at(&self, offset: u64) -> Option<u64> {
        self.0
            .range(..=offset)
            .next_back()
            .and_then(|(_, &end)| (end > offset).then_some(end))
    }
    fn covers(&self, start: u64, end: u64) -> bool {
        start == end || self.end_at(start).is_some_and(|covered| covered >= end)
    }
    fn missing(&self, start: u64, end: u64) -> Option<u64> {
        let next = self.end_at(start).unwrap_or(start);
        (next < end).then_some(next)
    }
    fn insert(&mut self, mut start: u64, mut end: u64) -> Result<(), CacheError> {
        if start >= end {
            return Ok(());
        }
        if let Some((&a, &b)) = self.0.range(..=start).next_back() {
            if b >= start {
                start = a;
                end = end.max(b);
                self.0.remove(&a);
            }
        }
        while let Some((&a, &b)) = self.0.range(start..).next() {
            if a > end {
                break;
            }
            end = end.max(b);
            self.0.remove(&a);
        }
        if self.0.len() >= MAX_RANGES {
            return Err(CacheError::Integrity("音频定位产生过多离散区间".into()));
        }
        self.0.insert(start, end);
        Ok(())
    }
    fn remove(&mut self, start: u64, end: u64) {
        loop {
            let overlap = self
                .0
                .range(..=start)
                .next_back()
                .and_then(|(&a, &b)| (b > start).then_some((a, b)))
                .or_else(|| self.0.range(start..end).next().map(|(&a, &b)| (a, b)));
            let Some((a, b)) = overlap else { break };
            self.0.remove(&a);
            if a < start {
                self.0.insert(a, start);
            }
            if b > end {
                self.0.insert(end, b);
                break;
            }
        }
    }
    fn invalidate_physical(&mut self, start: u64, end: u64, capacity: u64) {
        loop {
            let overlap = self.0.iter().find_map(|(&a, &b)| {
                for epoch in a / capacity..=(b.saturating_sub(1)) / capacity {
                    let base = epoch * capacity;
                    let left = base.saturating_add(start).max(a);
                    let right = base.saturating_add(end).min(b);
                    if left < right {
                        return Some((left, right));
                    }
                }
                None
            });
            let Some((left, right)) = overlap else { break };
            self.remove(left, right);
        }
    }
}

struct Active {
    response: Response,
    position: u64,
    end: Option<u64>,
    revision: u64,
    digest: Option<(Md5, [u8; 16])>,
}
#[derive(Clone, PartialEq, Eq)]
struct Identity {
    etag: Option<String>,
}
impl Identity {
    fn from_response(response: &Response) -> Self {
        let text = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let etag = text(header::ETAG)
            .filter(|tag| !tag.starts_with("W/") && tag.starts_with('"') && tag.ends_with('"'));
        Self { etag }
    }
    // A bare Last-Modified timestamp is not necessarily a strong validator.
    // Without a strong ETag or an expected whole-object hash we replay one
    // sequential entity instead of joining unprovable ranges.
    fn validator(&self) -> Option<&str> {
        self.etag.as_deref()
    }
}

async fn fetch(shared: Arc<Shared>) -> Result<(), CacheError> {
    let mut active: Option<Active> = None;
    let mut identity: Option<Identity> = None;
    let mut ranges_supported = true;
    let mut failures = 0_u32;
    loop {
        let notified = shared.wake.notified();
        let plan = {
            let state = lock(&shared.state);
            if state.cancelled || state.stopped {
                return Ok(());
            }
            if state.complete {
                return Ok(());
            }
            if state.paused {
                None
            } else if !state.initialized {
                Some((0, shared.options.prebuffer_bytes, state.demand_revision))
            } else {
                let target = if state.drive_full {
                    state.length.unwrap_or(u64::MAX)
                } else {
                    state.length.unwrap_or(u64::MAX).min(
                        state
                            .demand
                            .saturating_add(shared.options.read_ahead_bytes)
                            .max(state.sniff_end),
                    )
                };
                let start = if state.drive_full {
                    state.ranges.missing(0, target)
                } else {
                    state.ranges.missing(state.demand, target)
                };
                start.map(|start| (start, target, state.demand_revision))
            }
        };
        let plan = plan.or_else(|| {
            let state = lock(&shared.state);
            if state.paused {
                None
            } else {
                active
                    .as_ref()
                    .filter(|request| {
                        request.end == Some(request.position)
                            || state.length == Some(request.position)
                    })
                    .map(|request| (request.position, request.position, state.demand_revision))
            }
        });
        let Some((wanted, target, revision)) = plan else {
            if lock(&shared.state).paused {
                active = None;
            }
            notified.await;
            continue;
        };
        let can_range = identity
            .as_ref()
            .is_some_and(|value| value.validator().is_some())
            || lock(&shared.state).expected_md5.is_some();
        if let Some(request) = active.as_mut() {
            // No usable validator / no server range support means forward seeks
            // wait on this sequential response; they must not restart it.
            if request.position > wanted
                || (ranges_supported
                    && can_range
                    && request.position != wanted
                    && request.revision != revision)
            {
                active = None;
            } else {
                request.revision = revision;
            }
        }
        if active.is_none() {
            let offset = if ranges_supported && can_range {
                wanted
            } else {
                0
            };
            let result = {
                let request = request(&shared, offset, identity.as_ref(), revision);
                tokio::pin!(request);
                loop {
                    tokio::select! {
                        result = &mut request => break Some(result),
                        _ = shared.wake.notified() => {
                            let state = lock(&shared.state);
                            if state.cancelled || state.stopped { return Ok(()); }
                            if state.paused || state.demand_revision != revision { break None; }
                        }
                    }
                }
            };
            let Some(result) = result else { continue };
            match result {
                Ok((request, current_identity, range_honored)) => {
                    if let Some(previous) = &identity {
                        if previous.validator().is_some()
                            && previous.validator() != current_identity.validator()
                            && lock(&shared.state).expected_md5.is_none()
                        {
                            return Err(CacheError::Integrity("重连后音频实体标识发生变化".into()));
                        }
                    }
                    identity = Some(current_identity);
                    if !range_honored && offset != 0 {
                        ranges_supported = false;
                    }
                    if request.position == 0 {
                        let mut state = lock(&shared.state);
                        // Without a stable identity, sequential replay proves the
                        // new entity itself; no unverified old range is published.
                        if state.initialized && !can_range {
                            state.ranges.0.clear();
                        }
                    }
                    active = Some(request);
                }
                Err(error) => {
                    if matches!(error, CacheError::Http(403 | 410))
                        && shared.options.refresh.is_some()
                    {
                        let expired =
                            CacheError::Network("CDN 音频地址已过期，需要刷新同一音源".into());
                        if retry(&shared, &expired, &mut failures).await? {
                            refresh_url(&shared, &mut failures).await?;
                        }
                    } else if !retry(&shared, &error, &mut failures).await? {
                        return Err(error);
                    }
                    continue;
                }
            }
        }
        let safe_ranges = ranges_supported
            && (identity
                .as_ref()
                .is_some_and(|value| value.validator().is_some())
                || lock(&shared.state).expected_md5.is_some());
        let Some(request) = active.as_mut() else {
            continue;
        };
        let result = tokio::select! {
            result = request.response.chunk() => Some(result),
            _ = shared.wake.notified() => None,
        };
        let Some(result) = result else {
            let state = lock(&shared.state);
            if state.cancelled || state.stopped {
                return Ok(());
            }
            if state.paused {
                drop(state);
                active = None;
            }
            continue;
        };
        match result {
            Ok(Some(bytes)) => {
                if bytes.is_empty() {
                    continue;
                }
                let end = request
                    .position
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| CacheError::Integrity("音频长度溢出".into()))?;
                if request.end.is_some_and(|limit| end > limit)
                    || lock(&shared.state)
                        .length
                        .is_some_and(|length| end > length)
                {
                    return Err(CacheError::Integrity(
                        "HTTP 正文超出声明区间或 API 对象长度".into(),
                    ));
                }
                // A network chunk is consumed in fixed blocks and held only while
                // this one response is active; disk writes obey exact demand.
                let mut consumed = 0;
                while consumed < bytes.len() {
                    let wait = shared.wake.notified();
                    let allowed = {
                        let state = lock(&shared.state);
                        if state.cancelled || state.stopped {
                            return Ok(());
                        }
                        if !safe_ranges && state.demand >= request.position {
                            request.revision = state.demand_revision;
                        }
                        if state.paused || state.demand_revision != request.revision {
                            None
                        } else {
                            let limit = if state.drive_full {
                                state.length.unwrap_or(u64::MAX)
                            } else {
                                state
                                    .demand
                                    .saturating_add(shared.options.read_ahead_bytes)
                                    .max(state.sniff_end)
                                    .min(state.length.unwrap_or(u64::MAX))
                            };
                            let position = request.position;
                            Some(
                                limit
                                    .saturating_sub(position)
                                    .min(IO_BLOCK as u64)
                                    .min((bytes.len() - consumed) as u64)
                                    as usize,
                            )
                        }
                    };
                    match allowed {
                        None => {
                            active = None;
                            break;
                        }
                        Some(0) => {
                            wait.await;
                        }
                        Some(count) => {
                            store(
                                &shared,
                                request.position,
                                &bytes[consumed..consumed + count],
                            )?;
                            if let Some((digest, _)) = &mut request.digest {
                                digest.update(&bytes[consumed..consumed + count]);
                            }
                            request.position += count as u64;
                            consumed += count;
                        }
                    }
                }
            }
            Ok(None) => {
                let position = request.position;
                if request.end.is_some_and(|end| position != end) {
                    let error = CacheError::Network("HTTP 正文提前结束".into());
                    active = None;
                    if !retry(&shared, &error, &mut failures).await? {
                        return Err(error);
                    }
                    continue;
                }
                if let Some((digest, expected)) = request.digest.take() {
                    let actual: [u8; 16] = digest.finalize().into();
                    if actual != expected {
                        return Err(CacheError::Integrity("CDN 响应 Content-MD5 不匹配".into()));
                    }
                }
                {
                    let mut state = lock(&shared.state);
                    if state.length.is_none() {
                        state.length = Some(position);
                    }
                    if state.format.is_none() {
                        return Err(CacheError::Unsupported(
                            "音频在可识别的容器/帧头之前结束".into(),
                        ));
                    }
                    if state.length == Some(position) {
                        state.eof = true;
                    }
                    if state.length == Some(position)
                        && state.mode == StorageMode::Persistent
                        && state.ranges.covers(0, position)
                    {
                        state.complete = true;
                    }
                }
                shared.wake.notify_waiters();
                shared.readers.notify_all();
                active = None;
                if lock(&shared.state).complete {
                    return Ok(());
                }
                // EOF on a window is terminal until a new decoder seek demands
                // discarded data. Avoid repeatedly opening an EOF response.
                if wanted >= position || target >= position {
                    let wait = shared.wake.notified();
                    let should_wait = {
                        let state = lock(&shared.state);
                        state.ranges.end_at(state.demand).is_some() || state.demand >= position
                    };
                    if should_wait {
                        wait.await;
                    }
                }
            }
            Err(_) => {
                let error = CacheError::Network("读取 CDN 响应时连接中断或超时".into());
                active = None;
                if !retry(&shared, &error, &mut failures).await? {
                    return Err(error);
                }
            }
        }
    }
}

// AtomicU32::try_update is newer than our Rust 1.87 minimum.
pub(crate) fn consume_retry(budget: &AtomicU32) -> bool {
    let mut remaining = budget.load(Ordering::Acquire);
    while let Some(next) = remaining.checked_sub(1) {
        match budget.compare_exchange_weak(remaining, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => remaining = actual,
        }
    }
    false
}

async fn retry(
    shared: &Shared,
    error: &CacheError,
    failures: &mut u32,
) -> Result<bool, CacheError> {
    if !error.is_retryable() {
        return Ok(false);
    }
    if !consume_retry(&shared.retry_budget) {
        return Err(CacheError::RetryExhausted(error.to_string()));
    }
    *failures = failures.saturating_add(1);
    lock(&shared.state).retries_used = *failures;
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(shared.options.retry_interval_secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Ok(true),
            _ = shared.wake.notified() => {
                let state = lock(&shared.state);
                if state.cancelled || state.stopped { return Err(CacheError::Cancelled); }
            }
        }
    }
}

async fn refresh_url(shared: &Shared, failures: &mut u32) -> Result<(), CacheError> {
    let refresh = shared
        .options
        .refresh
        .as_ref()
        .ok_or(CacheError::Http(410))?;
    loop {
        let notified = shared.wake.notified();
        let paused = {
            let state = lock(&shared.state);
            if state.cancelled || state.stopped {
                return Err(CacheError::Cancelled);
            }
            state.paused
        };
        if paused {
            notified.await;
            continue;
        }
        let future = refresh.api.stream_url_at(refresh.song_id, refresh.quality);
        tokio::pin!(future);
        let result = loop {
            tokio::select! {
                result = &mut future => break result,
                _ = shared.wake.notified() => {
                    let state = lock(&shared.state);
                    if state.cancelled || state.stopped { return Err(CacheError::Cancelled); }
                }
            }
        };
        let refreshed = match result {
            Ok(stream) => stream,
            Err(error) => {
                let error = match error {
                    yoruplay_api::ApiError::Network(_) | yoruplay_api::ApiError::Timeout => {
                        CacheError::Network("刷新音频地址时连接中断或超时".into())
                    }
                    yoruplay_api::ApiError::HttpStatus { status, .. } => CacheError::Http(status),
                    yoruplay_api::ApiError::AuthRequired
                    | yoruplay_api::ApiError::SessionExpired => {
                        CacheError::Unsupported("登录已失效，不能刷新网络音源".into())
                    }
                    yoruplay_api::ApiError::Api { code, .. } => {
                        CacheError::Unsupported(format!("音源 API 拒绝刷新（代码 {code}）"))
                    }
                    _ => CacheError::Unsupported("音源 API 无法提供可播放的同一音源".into()),
                };
                if retry(shared, &error, failures).await? {
                    continue;
                }
                return Err(error);
            }
        };
        if refreshed.trial {
            return Err(CacheError::Unsupported("刷新地址返回了试听音源".into()));
        }
        if shared.stream.level.is_some() && refreshed.level != shared.stream.level {
            return Err(CacheError::Integrity("刷新地址改变了实际音质".into()));
        }
        let normalize = |format: &str| match format.to_ascii_lowercase().as_str() {
            "mp4" | "m4a" => "m4a".to_owned(),
            value => value.to_owned(),
        };
        if let (Some(old), Some(new)) = (&shared.stream.format, &refreshed.format) {
            if normalize(old) != normalize(new) {
                return Err(CacheError::Integrity("刷新地址改变了原生容器格式".into()));
            }
        }
        let hash = refreshed.md5.as_deref().map(parse_md5).transpose()?;
        {
            let mut state = lock(&shared.state);
            if let (Some(actual), Some(advertised)) = (state.format, refreshed.format.as_deref()) {
                if normalize(actual.extension()) != normalize(advertised) {
                    return Err(CacheError::Integrity(
                        "刷新地址的容器与已接收魔数不一致".into(),
                    ));
                }
            }
            if refreshed.size > 0 && state.length.is_some_and(|length| length != refreshed.size) {
                return Err(CacheError::Integrity("刷新地址改变了音频总长".into()));
            }
            if shared.stream.size > 0 && refreshed.size != shared.stream.size {
                return Err(CacheError::Integrity("刷新地址改变了 API 音频长度".into()));
            }
            if shared.stream.md5.is_some() && hash.is_none() {
                return Err(CacheError::Integrity("刷新地址丢失了 API 音频 MD5".into()));
            }
            if let (Some(expected), Some(hash)) = (state.expected_md5, hash) {
                if hash != expected {
                    return Err(CacheError::Integrity("刷新地址改变了音频 MD5".into()));
                }
            } else if let Some(hash) = hash {
                state.expected_md5 = Some(hash);
            }
        }
        *lock(&shared.current_url) = (refreshed, std::time::Instant::now());
        return Ok(());
    }
}

async fn request(
    shared: &Shared,
    offset: u64,
    identity: Option<&Identity>,
    revision: u64,
) -> Result<(Active, Identity, bool), CacheError> {
    let mut request = {
        let current = lock(&shared.current_url);
        if shared.options.refresh.is_some()
            && current.0.expires_in_secs != 0
            && current.1.elapsed().as_secs() >= current.0.expires_in_secs
        {
            return Err(CacheError::Http(410));
        }
        shared
            .cache
            .inner
            .http
            .get(&current.0.url)
            .header(header::ACCEPT_ENCODING, "identity")
    };
    if offset != 0 {
        request = request.header(header::RANGE, format!("bytes={offset}-"));
        if let Some(validator) = identity.and_then(Identity::validator) {
            request = request.header(header::IF_RANGE, validator);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|_| CacheError::Network("连接 CDN 失败或超时".into()))?;
    let status = response.status();
    if !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        return Err(CacheError::Http(status.as_u16()));
    }
    if response
        .headers()
        .get(header::CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes() != b"identity")
    {
        return Err(CacheError::Integrity(
            "服务器对字节寻址音频使用了 Content-Encoding".into(),
        ));
    }
    let current_identity = Identity::from_response(&response);
    let (position, end, length, range_honored) = if status == StatusCode::PARTIAL_CONTENT {
        let range = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| CacheError::Integrity("206 缺少 Content-Range".into()))?;
        let (start, end, total) = parse_content_range(range)?;
        if start != offset {
            return Err(CacheError::Integrity("Content-Range 起点不匹配请求".into()));
        }
        if response
            .content_length()
            .is_some_and(|length| length != end - start)
        {
            return Err(CacheError::Integrity(
                "Content-Range 与 Content-Length 不一致".into(),
            ));
        }
        (start, Some(end), Some(total), true)
    } else {
        (
            0,
            response.content_length(),
            response
                .content_length()
                .or((shared.stream.size > 0).then_some(shared.stream.size)),
            false,
        )
    };
    if shared.stream.size > 0 && length.is_some_and(|length| length != shared.stream.size) {
        return Err(CacheError::Integrity(
            "CDN 长度与音源 API 声明不一致".into(),
        ));
    }
    let response_md5 = response
        .headers()
        .get("content-md5")
        .and_then(|value| value.to_str().ok())
        .map(decode_md5_base64)
        .transpose()?;
    {
        let mut state = lock(&shared.state);
        if state.length.is_some() && length.is_some() && state.length != length {
            return Err(CacheError::Integrity("重连后的音频总长发生变化".into()));
        }
        if !state.initialized {
            state.length = length;
            let persistent = if let Some(length) = length {
                shared.cache.reserve(length).unwrap_or(false)
            } else {
                false
            };
            if persistent {
                state.mode = StorageMode::Persistent;
                state.reservation = length.unwrap_or(0);
            } else {
                state.mode = StorageMode::Window;
            }
            state.initialized = true;
        }
        if position == 0 && end == length {
            if let Some(hash) = response_md5 {
                if state.expected_md5.is_some_and(|expected| expected != hash) {
                    return Err(CacheError::Integrity("CDN 与 API MD5 不匹配".into()));
                }
                state.expected_md5 = Some(hash);
            }
        }
    }
    shared.wake.notify_waiters();
    Ok((
        Active {
            response,
            position,
            end,
            revision,
            digest: response_md5.map(|hash| (Md5::new(), hash)),
        },
        current_identity,
        range_honored,
    ))
}

fn store(shared: &Shared, position: u64, bytes: &[u8]) -> Result<(), CacheError> {
    let mut state = lock(&shared.state);
    if state.cancelled || state.stopped {
        return Err(CacheError::Cancelled);
    }
    let end = position
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| CacheError::Integrity("音频长度溢出".into()))?;
    if state.length.is_some_and(|length| end > length) {
        return Err(CacheError::Integrity("CDN 音频超出声明总长".into()));
    }
    if state.mode == StorageMode::Window {
        let capacity = shared.options.scratch_bytes;
        let offset = position % capacity;
        let first = bytes.len().min((capacity - offset) as usize);
        state
            .ranges
            .invalidate_physical(offset, offset + first as u64, capacity);
        if first != bytes.len() {
            state
                .ranges
                .invalidate_physical(0, (bytes.len() - first) as u64, capacity);
        }
        let file = state.file.as_ref().ok_or(CacheError::Cancelled)?;
        file.write_all_at(&bytes[..first], offset)?;
        if first != bytes.len() {
            file.write_all_at(&bytes[first..], 0)?;
        }
        let scratch = file.metadata()?.len();
        shared
            .cache
            .update_scratch(state.scratch_accounted, scratch);
        state.scratch_accounted = scratch;
    } else {
        let file = state.file.as_ref().ok_or(CacheError::Cancelled)?;
        if let Err(error) = file.write_all_at(bytes, position) {
            // A retained-cache reservation is not a promise that the filesystem
            // has room. Keep viable playback by converting the SAME acquisition
            // to the bounded ring, never by launching another downloader.
            if matches!(error.raw_os_error(), Some(28 | 122)) {
                state.file.take();
                state.file = Some(anonymous_file(shared.cache.directory())?);
                state.ranges.0.clear();
                shared
                    .cache
                    .release_reservation(std::mem::take(&mut state.reservation));
                state.mode = StorageMode::Window;
                drop(state);
                return store(shared, position, bytes);
            }
            return Err(error.into());
        }
    }
    state.ranges.insert(position, end)?;
    if state.mode == StorageMode::Persistent
        && state.eof
        && state
            .length
            .is_some_and(|length| state.ranges.covers(0, length))
    {
        state.complete = true;
    }
    if state.format.is_none() && state.ranges.end_at(0).unwrap_or(0) >= 12 {
        let available = state.ranges.end_at(0).unwrap_or(0);
        let file = state.file.as_ref().ok_or(CacheError::Cancelled)?;
        let mut header = [0; 12];
        file.read_exact_at(&mut header, 0)?;
        if header.starts_with(b"ID3") {
            if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
                return Err(CacheError::Integrity("ID3 长度编码无效".into()));
            }
            let offset = header[6..10]
                .iter()
                .fold(0_u64, |size, byte| size * 128 + u64::from(*byte))
                + 10
                + if header[3] == 4 && header[5] & 0x10 != 0 {
                    10
                } else {
                    0
                };
            if offset > MAX_TAG_BYTES as u64 || offset + 12 > shared.options.scratch_bytes {
                return Err(CacheError::Unsupported(
                    "原始 ID3 超出有界元数据窗口".into(),
                ));
            }
            if available >= offset + 12 {
                file.read_exact_at(&mut header, offset)?;
                state.format = Some(sniff(&header)?);
            } else {
                state.sniff_end = offset + 12;
            }
        } else {
            state.format = Some(sniff(&header)?);
        }
    }
    drop(state);
    shared.readers.notify_all();
    shared.wake.notify_waiters();
    Ok(())
}

fn parse_content_range(value: &str) -> Result<(u64, u64, u64), CacheError> {
    let invalid = || CacheError::Integrity("无效的 Content-Range".into());
    let value = value.strip_prefix("bytes ").ok_or_else(invalid)?;
    let (range, total) = value.split_once('/').ok_or_else(invalid)?;
    let (start, end) = range.split_once('-').ok_or_else(invalid)?;
    let start: u64 = start.parse().map_err(|_| invalid())?;
    let end: u64 = end.parse().map_err(|_| invalid())?;
    let total: u64 = total.parse().map_err(|_| invalid())?;
    let end = end.checked_add(1).ok_or_else(invalid)?;
    if start >= end || end > total {
        return Err(invalid());
    }
    Ok((start, end, total))
}
fn parse_md5(value: &str) -> Result<[u8; 16], CacheError> {
    if value.len() != 32 {
        return Err(CacheError::Integrity("音源 MD5 长度无效".into()));
    }
    let mut hash = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        hash[index] = digit(pair[0])
            .zip(digit(pair[1]))
            .map(|(a, b)| a * 16 + b)
            .ok_or_else(|| CacheError::Integrity("音源 MD5 编码无效".into()))?;
    }
    Ok(hash)
}
fn decode_md5_base64(value: &str) -> Result<[u8; 16], CacheError> {
    let mut output = [0; 16];
    let mut bits = 0_u32;
    let mut count = 0;
    let mut index = 0;
    for byte in value.trim().bytes() {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(CacheError::Integrity("无效的 Content-MD5".into())),
        };
        bits = (bits << 6) | u32::from(digit);
        count += 6;
        if count >= 8 {
            count -= 8;
            if index >= output.len() {
                return Err(CacheError::Integrity("无效的 Content-MD5 长度".into()));
            }
            output[index] = (bits >> count) as u8;
            index += 1;
        }
    }
    if index != 16 {
        return Err(CacheError::Integrity("无效的 Content-MD5 长度".into()));
    }
    Ok(output)
}

fn sniff(data: &[u8]) -> Result<Format, CacheError> {
    if data.starts_with(b"CTENFDAM") {
        return Err(CacheError::Unsupported("NCM 加密容器".into()));
    }
    if data.starts_with(b"fLaC") {
        return Ok(Format::Flac);
    }
    if data.starts_with(b"OggS") {
        if data.windows(8).any(|part| part == b"OpusHead") {
            return Err(CacheError::Unsupported("当前解码器未启用 Ogg/Opus".into()));
        }
        return Ok(Format::Ogg);
    }
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WAVE") {
        return Ok(Format::Wav);
    }
    if data.get(4..8) == Some(b"ftyp") {
        return Ok(Format::Mp4);
    }
    if data.starts_with(b"ID3") {
        return Ok(Format::Mp3);
    }
    if data.len() >= 2 && data[0] == 0xff {
        if data[1] & 0xf6 == 0xf0 {
            return Ok(Format::Aac);
        }
        if data[1] & 0xe0 == 0xe0 && data[1] & 0x06 != 0 {
            return Ok(Format::Mp3);
        }
    }
    Err(CacheError::Unsupported(
        "魔数不是支持的原生音频容器（拒绝网页、JSON 与未知/加密数据）".into(),
    ))
}

fn verify_complete(
    file: &File,
    format: Format,
    length: u64,
    expected: Option<[u8; 16]>,
    full_decode: bool,
    stop: Option<Finalization<'_>>,
) -> Result<(), CacheError> {
    if length == 0 || file.metadata()?.len() != length {
        return Err(CacheError::Integrity(
            "实际文件大小与完整覆盖总长不一致".into(),
        ));
    }
    if let Some(expected) = expected {
        let mut digest = Md5::new();
        let mut buffer = [0; IO_BLOCK];
        let mut offset = 0;
        while offset < length {
            if let Some(stop) = stop {
                stop.check()?;
            }
            let count = ((length - offset) as usize).min(buffer.len());
            file.read_exact_at(&mut buffer[..count], offset)?;
            digest.update(&buffer[..count]);
            offset += count as u64;
        }
        let actual: [u8; 16] = digest.finalize().into();
        if actual != expected {
            return Err(CacheError::Integrity(
                "完整音频 MD5 不匹配；未发布缓存".into(),
            ));
        }
    }
    if format == Format::Mp4 {
        validate_mp4(file, length)?;
    }
    let mut source = file.try_clone()?;
    source.seek(SeekFrom::Start(0))?;
    use symphonia::core::{
        codecs::{DecoderOptions, CODEC_TYPE_NULL},
        formats::FormatOptions,
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::{Limit, MetadataOptions},
        probe::Hint,
    };
    let stream = MediaSourceStream::new(
        Box::new(source),
        MediaSourceStreamOptions {
            buffer_len: IO_BLOCK,
        },
    );
    let metadata = MetadataOptions {
        limit_metadata_bytes: Limit::Maximum(128 * 1024),
        limit_visual_bytes: Limit::Maximum(512 * 1024),
    };
    let mut hint = Hint::new();
    hint.with_extension(format.extension());
    let mut probed = symphonia::default::get_probe()
        .format(&hint, stream, &FormatOptions::default(), &metadata)
        .map_err(|_| CacheError::Unsupported("解码器拒绝此音频容器".into()))?;
    let (track, expected_frames, mut decoder) = probed
        .format
        .tracks()
        .iter()
        .filter(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .find_map(|track| {
            symphonia::default::get_codecs()
                .make(&track.codec_params, &DecoderOptions::default())
                .ok()
                .map(|decoder| (track.id, track.codec_params.n_frames, decoder))
        })
        .ok_or_else(|| CacheError::Unsupported("完整文件没有受支持的普通音轨".into()))?;
    let mut decoded_frames = 0_u64;
    loop {
        if let Some(stop) = stop {
            stop.check()?;
        }
        let packet = match probed.format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(error))
                if error.kind() == io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(_) => return Err(CacheError::Integrity("音频容器未能正常读至结尾".into())),
        };
        {
            let mut metadata = probed.format.metadata();
            while !metadata.is_latest() {
                metadata.pop();
            }
        }
        if packet.track_id() != track {
            continue;
        }
        let audio = decoder
            .decode(&packet)
            .map_err(|_| CacheError::Integrity("完整音频含无法解码的帧".into()))?;
        decoded_frames = decoded_frames.saturating_add(audio.frames() as u64);
        if !full_decode && decoded_frames != 0 {
            return Ok(());
        }
    }
    if decoded_frames == 0 {
        return Err(CacheError::Unsupported("完整文件没有可解码的音频帧".into()));
    }
    // MP3/ADTS n_frames may be a bitrate estimate, never integrity evidence.
    if matches!(format, Format::Flac | Format::Wav | Format::Mp4)
        && expected_frames.is_some_and(|expected| decoded_frames < expected)
    {
        return Err(CacheError::Integrity(
            "音频帧数少于原生容器声明；拒绝截断对象".into(),
        ));
    }
    Ok(())
}

pub(crate) fn verify_recovered(file: &File, format: &str, length: u64) -> Result<(), CacheError> {
    let format = match format {
        "mp3" => Format::Mp3,
        "flac" => Format::Flac,
        "m4a" => Format::Mp4,
        "aac" => Format::Aac,
        "ogg" => Format::Ogg,
        "wav" => Format::Wav,
        _ => return Err(CacheError::Unsupported("恢复清单中的音频格式未知".into())),
    };
    verify_complete(file, format, length, None, true, None)
}

struct LimitedTag(Vec<u8>);
impl Write for LimitedTag {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_TAG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "序列化标签超出有界元数据上限",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn tag_bytes(format: Format, song: &Song, metadata: &CacheMetadata) -> Result<Vec<u8>, CacheError> {
    let kind = match format {
        Format::Flac | Format::Ogg => TagType::VorbisComments,
        Format::Mp4 => TagType::Mp4Ilst,
        _ => TagType::Id3v2,
    };
    let mut tag = Tag::new(kind);
    tag.set_title(song.name.clone());
    tag.set_artist(song.artist_text());
    tag.set_album(song.album.clone());
    let lyrics = match (&metadata.lyrics, &metadata.translated_lyrics) {
        (Some(original), Some(translated)) => {
            Some(format!("{original}\n\n[TRANSLATED_LYRICS]\n{translated}"))
        }
        (Some(original), None) => Some(original.clone()),
        (None, Some(translated)) => Some(format!("[TRANSLATED_LYRICS]\n{translated}")),
        (None, None) => None,
    };
    if let Some(lyrics) = lyrics {
        tag.insert_text(ItemKey::Lyrics, lyrics);
    }
    if let Some(data) = &metadata.artwork {
        let mime = match artwork_extension(data) {
            Some("png") => MimeType::Png,
            _ => MimeType::Jpeg,
        };
        tag.push_picture(Picture::new_unchecked(
            PictureType::CoverFront,
            Some(mime),
            None,
            data.clone(),
        ));
    }
    let mut output = LimitedTag(Vec::new());
    tag.dump_to(&mut output, WriteOptions::default())
        .map_err(|_| CacheError::Integrity("lofty 无法序列化有界音频标签".into()))?;
    Ok(output.0)
}

/// Lofty is only allowed to serialize the bounded tag, never to rewrite a full
/// media file into an internal Vec. Container rewrites below stream disk-to-disk.
fn rewrite_tags(
    cache: &AudioCache,
    source: &File,
    length: u64,
    format: Format,
    song: &Song,
    metadata: &CacheMetadata,
    stop: Finalization<'_>,
) -> Result<File, CacheError> {
    let bytes = tag_bytes(format, song, metadata)?;
    let mut output = anonymous_file(cache.directory())?;
    match format {
        Format::Mp3 | Format::Aac => {
            let offset = leading_id3_size(source, length)?;
            output.write_all(&bytes)?;
            copy_range(source, offset, length - offset, &mut output, stop)?;
        }
        Format::Wav => {
            if length.saturating_add(bytes.len() as u64).saturating_add(9) > u64::from(u32::MAX) + 8
            {
                return Err(CacheError::Unsupported(
                    "WAV 标签重写超出 RIFF 32 位长度".into(),
                ));
            }
            copy_range(source, 0, length, &mut output, stop)?;
            if !length.is_multiple_of(2) {
                output.write_all(&[0])?;
            }
            output.write_all(b"id3 ")?;
            output.write_all(&(bytes.len() as u32).to_le_bytes())?;
            output.write_all(&bytes)?;
            if bytes.len() % 2 != 0 {
                output.write_all(&[0])?;
            }
            let size = output.stream_position()?;
            output.write_all_at(&((size - 8) as u32).to_le_bytes(), 4)?;
        }
        Format::Flac => rewrite_flac(source, length, &bytes, &mut output, stop)?,
        Format::Ogg => rewrite_ogg(source, length, &bytes, &mut output, stop)?,
        Format::Mp4 => rewrite_mp4(source, length, &bytes, &mut output, stop)?,
    }
    output.sync_all()?;
    // Container offset updates are checked before a rewritten inode can replace
    // its verified source; failed optional tags never poison the old entry.
    let final_length = output.metadata()?.len();
    verify_complete(&output, format, final_length, None, false, Some(stop))?;
    output.seek(SeekFrom::Start(0))?;
    Ok(output)
}

fn copy_range(
    source: &File,
    mut offset: u64,
    mut length: u64,
    output: &mut File,
    stop: Finalization<'_>,
) -> Result<(), CacheError> {
    let mut buffer = [0; IO_BLOCK];
    while length != 0 {
        stop.check()?;
        let count = length.min(IO_BLOCK as u64) as usize;
        source.read_exact_at(&mut buffer[..count], offset)?;
        output.write_all(&buffer[..count])?;
        offset += count as u64;
        length -= count as u64;
    }
    Ok(())
}

fn leading_id3_size(source: &File, length: u64) -> Result<u64, CacheError> {
    if length < 10 {
        return Ok(0);
    }
    let mut header = [0; 10];
    source.read_exact_at(&mut header, 0)?;
    if !header.starts_with(b"ID3") {
        return Ok(0);
    }
    if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
        return Err(CacheError::Integrity("ID3 长度编码无效".into()));
    }
    let size = header[6..10]
        .iter()
        .fold(0_u64, |size, byte| size * 128 + u64::from(*byte))
        + 10
        + if header[3] == 4 && header[5] & 0x10 != 0 {
            10
        } else {
            0
        };
    if size > length || size > MAX_TAG_BYTES as u64 {
        return Err(CacheError::Unsupported(
            "原始 ID3 标签超出有界元数据上限".into(),
        ));
    }
    Ok(size)
}

fn rewrite_flac(
    source: &File,
    length: u64,
    comments: &[u8],
    output: &mut File,
    stop: Finalization<'_>,
) -> Result<(), CacheError> {
    if comments.len() > 0x00ff_ffff {
        return Err(CacheError::Integrity("FLAC 评论块过大".into()));
    }
    output.write_all(b"fLaC")?;
    let mut offset = 4;
    let mut first = true;
    loop {
        let mut header = [0; 4];
        source.read_exact_at(&mut header, offset)?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let size = u32::from_be_bytes([0, header[1], header[2], header[3]]) as u64;
        if offset + 4 + size > length || (first && (kind != 0 || size != 34)) {
            return Err(CacheError::Integrity("FLAC 元数据块边界无效".into()));
        }
        if !matches!(kind, 4 | 6) {
            header[0] &= 0x7f;
            output.write_all(&header)?;
            copy_range(source, offset + 4, size, output, stop)?;
        }
        first = false;
        offset += 4 + size;
        if last {
            break;
        }
    }
    let size = (comments.len() as u32).to_be_bytes();
    output.write_all(&[0x84, size[1], size[2], size[3]])?;
    output.write_all(comments)?;
    copy_range(source, offset, length - offset, output, stop)
}

struct OggPage {
    flags: u8,
    granule: u64,
    serial: u32,
    lacing: Vec<u8>,
    body: Vec<u8>,
}
fn read_ogg_page(
    source: &File,
    offset: &mut u64,
    length: u64,
) -> Result<Option<OggPage>, CacheError> {
    if *offset == length {
        return Ok(None);
    }
    if length.saturating_sub(*offset) < 27 {
        return Err(CacheError::Integrity("Ogg 页头被截断".into()));
    }
    let mut header = [0; 27];
    source.read_exact_at(&mut header, *offset)?;
    if &header[..4] != b"OggS" || header[4] != 0 {
        return Err(CacheError::Integrity("无效 Ogg 页".into()));
    }
    let mut lacing = vec![0; header[26] as usize];
    source.read_exact_at(&mut lacing, *offset + 27)?;
    let body_length: usize = lacing.iter().map(|byte| usize::from(*byte)).sum();
    let end = *offset + 27 + lacing.len() as u64 + body_length as u64;
    if end > length {
        return Err(CacheError::Integrity("Ogg 页正文被截断".into()));
    }
    let mut body = vec![0; body_length];
    source.read_exact_at(&mut body, *offset + 27 + lacing.len() as u64)?;
    *offset = end;
    Ok(Some(OggPage {
        flags: header[5],
        granule: u64::from_le_bytes(header[6..14].try_into().unwrap()),
        serial: u32::from_le_bytes(header[14..18].try_into().unwrap()),
        lacing,
        body,
    }))
}
fn write_ogg_page(output: &mut File, page: &OggPage, sequence: &mut u32) -> Result<(), CacheError> {
    let mut bytes = Vec::with_capacity(27 + page.lacing.len() + page.body.len());
    bytes.extend_from_slice(b"OggS");
    bytes.extend_from_slice(&[0, page.flags]);
    bytes.extend_from_slice(&page.granule.to_le_bytes());
    bytes.extend_from_slice(&page.serial.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.push(page.lacing.len() as u8);
    bytes.extend_from_slice(&page.lacing);
    bytes.extend_from_slice(&page.body);
    let mut crc = 0_u32;
    for &byte in &bytes {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    bytes[22..26].copy_from_slice(&crc.to_le_bytes());
    output.write_all(&bytes)?;
    *sequence = sequence
        .checked_add(1)
        .ok_or_else(|| CacheError::Integrity("Ogg 页序号溢出".into()))?;
    Ok(())
}
fn write_ogg_packet(
    output: &mut File,
    packet: &[u8],
    serial: u32,
    sequence: &mut u32,
    bos: bool,
) -> Result<(), CacheError> {
    let mut consumed = 0;
    let mut first = true;
    loop {
        let remaining = packet.len() - consumed;
        let count = remaining.min(65025);
        let mut lacing = vec![255; count / 255];
        if count < 65025 {
            lacing.push((count % 255) as u8);
        }
        let page = OggPage {
            flags: if first {
                if bos {
                    2
                } else {
                    0
                }
            } else {
                1
            },
            granule: if count == 65025 { u64::MAX } else { 0 },
            serial,
            lacing,
            body: packet[consumed..consumed + count].to_vec(),
        };
        write_ogg_page(output, &page, sequence)?;
        consumed += count;
        first = false;
        if count < 65025 {
            return Ok(());
        }
    }
}
fn rewrite_ogg(
    source: &File,
    length: u64,
    comments: &[u8],
    output: &mut File,
    stop: Finalization<'_>,
) -> Result<(), CacheError> {
    let mut offset = 0;
    let mut sequence = 0;
    let mut packets = Vec::with_capacity(3);
    let mut packet = Vec::new();
    let mut serial = None;
    loop {
        stop.check()?;
        let page = read_ogg_page(source, &mut offset, length)?
            .ok_or_else(|| CacheError::Integrity("Ogg/Vorbis 头被截断".into()))?;
        if serial.is_some_and(|serial| serial != page.serial) {
            return Err(CacheError::Unsupported("多逻辑流 Ogg 标签重写".into()));
        }
        serial = Some(page.serial);
        let mut consumed = 0;
        for (index, &size) in page.lacing.iter().enumerate() {
            if packet.len().saturating_add(size as usize) > MAX_TAG_BYTES {
                return Err(CacheError::Unsupported("Ogg 头包超出有界元数据上限".into()));
            }
            packet.extend_from_slice(&page.body[consumed..consumed + size as usize]);
            consumed += size as usize;
            if size != 255 {
                packets.push(std::mem::take(&mut packet));
                if packets.len() == 3 {
                    if !packets[0].starts_with(b"\x01vorbis")
                        || !packets[1].starts_with(b"\x03vorbis")
                        || !packets[2].starts_with(b"\x05vorbis")
                    {
                        return Err(CacheError::Unsupported("不是普通 Ogg/Vorbis 音频".into()));
                    }
                    let mut new_comments = Vec::with_capacity(comments.len() + 8);
                    new_comments.extend_from_slice(b"\x03vorbis");
                    new_comments.extend_from_slice(comments);
                    new_comments.push(1);
                    write_ogg_packet(output, &packets[0], page.serial, &mut sequence, true)?;
                    write_ogg_packet(output, &new_comments, page.serial, &mut sequence, false)?;
                    write_ogg_packet(output, &packets[2], page.serial, &mut sequence, false)?;
                    if index + 1 < page.lacing.len() {
                        let rest = OggPage {
                            flags: page.flags & !3,
                            granule: page.granule,
                            serial: page.serial,
                            lacing: page.lacing[index + 1..].to_vec(),
                            body: page.body[consumed..].to_vec(),
                        };
                        write_ogg_page(output, &rest, &mut sequence)?;
                    }
                    while let Some(page) = read_ogg_page(source, &mut offset, length)? {
                        stop.check()?;
                        if Some(page.serial) != serial {
                            return Err(CacheError::Unsupported("链式 Ogg 标签重写".into()));
                        }
                        write_ogg_page(output, &page, &mut sequence)?;
                    }
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Mp4Box {
    kind: [u8; 4],
    size: u64,
    header: usize,
}
fn mp4_header(bytes: &[u8], remaining: u64) -> Result<Mp4Box, CacheError> {
    if bytes.len() < 8 {
        return Err(CacheError::Integrity("MP4 box 头被截断".into()));
    }
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap());
    let (size, header) = if size == 1 {
        if bytes.len() < 16 {
            return Err(CacheError::Integrity("MP4 扩展 box 头被截断".into()));
        }
        (u64::from_be_bytes(bytes[8..16].try_into().unwrap()), 16)
    } else {
        (
            if size == 0 {
                remaining
            } else {
                u64::from(size)
            },
            8,
        )
    };
    if size < header as u64 || size > remaining {
        return Err(CacheError::Integrity("MP4 box 边界无效".into()));
    }
    Ok(Mp4Box {
        kind: bytes[4..8].try_into().unwrap(),
        size,
        header,
    })
}
fn mp4_header_at(source: &File, offset: u64, length: u64) -> Result<Mp4Box, CacheError> {
    let mut bytes = [0; 16];
    let count = length.saturating_sub(offset).min(16) as usize;
    source.read_exact_at(&mut bytes[..count], offset)?;
    mp4_header(&bytes[..count], length - offset)
}
fn mp4_child_start(kind: [u8; 4], header: usize) -> Option<usize> {
    match &kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"dinf" | b"udta" | b"moof"
        | b"traf" | b"mvex" | b"sinf" | b"schi" => Some(header),
        b"meta" => Some(header + 4),
        _ => None,
    }
}
fn inspect_mp4_boxes(
    bytes: &mut [u8],
    depth: usize,
    shift: Option<(u64, i64)>,
) -> Result<(), CacheError> {
    if depth > 16 {
        return Err(CacheError::Unsupported("MP4 box 嵌套过深".into()));
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let info = mp4_header(&bytes[offset..], (bytes.len() - offset) as u64)?;
        let end = offset + info.size as usize;
        let node = &mut bytes[offset..end];
        if matches!(
            &info.kind,
            b"enca" | b"encv" | b"sinf" | b"pssh" | b"tenc" | b"senc" | b"saiz" | b"saio"
        ) {
            return Err(CacheError::Unsupported(
                "MP4 包含加密 sample entry 或保护信息".into(),
            ));
        }
        if &info.kind == b"stsd" {
            let start = info.header + 8;
            if node.len() < start {
                return Err(CacheError::Integrity("MP4 stsd 被截断".into()));
            }
            let mut sample = start;
            while sample < node.len() {
                let entry = mp4_header(&node[sample..], (node.len() - sample) as u64)?;
                if matches!(&entry.kind, b"enca" | b"encv") {
                    return Err(CacheError::Unsupported("MP4 加密音轨".into()));
                }
                if matches!(
                    &entry.kind,
                    b"mp4a" | b"alac" | b"fLaC" | b"lpcm" | b"sowt" | b"twos"
                ) {
                    if entry.size < 36 {
                        return Err(CacheError::Integrity("MP4 音频 sample entry 被截断".into()));
                    }
                    let version =
                        u16::from_be_bytes(node[sample + 16..sample + 18].try_into().unwrap());
                    let child = sample
                        + match version {
                            0 => 36,
                            1 => 52,
                            2 => 72,
                            _ => {
                                return Err(CacheError::Unsupported(
                                    "MP4 音频 sample entry 版本".into(),
                                ))
                            }
                        };
                    let sample_end = sample + entry.size as usize;
                    if child > sample_end {
                        return Err(CacheError::Integrity("MP4 sample entry 边界无效".into()));
                    }
                    inspect_mp4_boxes(&mut node[child..sample_end], depth + 1, shift)?;
                }
                sample += entry.size as usize;
            }
        } else if matches!(&info.kind, b"stco" | b"co64") {
            if let Some((threshold, delta)) = shift {
                let start = info.header + 8;
                if node.len() < start {
                    return Err(CacheError::Integrity("MP4 chunk offset 表被截断".into()));
                }
                let count =
                    u32::from_be_bytes(node[info.header + 4..start].try_into().unwrap()) as usize;
                let width = if &info.kind == b"stco" { 4 } else { 8 };
                if count > (node.len() - start) / width {
                    return Err(CacheError::Integrity("MP4 chunk offset 数量越界".into()));
                }
                for index in 0..count {
                    let bytes = &mut node[start + index * width..start + (index + 1) * width];
                    let old = if width == 4 {
                        u64::from(u32::from_be_bytes(bytes[..4].try_into().unwrap()))
                    } else {
                        u64::from_be_bytes(bytes[..8].try_into().unwrap())
                    };
                    if old >= threshold {
                        let new = i128::from(old) + i128::from(delta);
                        if !(0..=i128::from(u64::MAX)).contains(&new)
                            || (width == 4 && new > i128::from(u32::MAX))
                        {
                            return Err(CacheError::Unsupported(
                                "MP4 标签重写需要扩展 chunk offset 表".into(),
                            ));
                        }
                        if width == 4 {
                            bytes.copy_from_slice(&(new as u32).to_be_bytes());
                        } else {
                            bytes.copy_from_slice(&(new as u64).to_be_bytes());
                        }
                    }
                }
            }
        } else if let Some(start) = mp4_child_start(info.kind, info.header) {
            if start > node.len() {
                return Err(CacheError::Integrity("MP4 容器头被截断".into()));
            }
            inspect_mp4_boxes(&mut node[start..], depth + 1, shift)?;
        }
        offset = end;
    }
    Ok(())
}
fn validate_mp4(source: &File, length: u64) -> Result<(), CacheError> {
    let mut offset = 0;
    let mut found = false;
    while offset < length {
        let info = mp4_header_at(source, offset, length)?;
        if matches!(&info.kind, b"pssh" | b"senc") {
            return Err(CacheError::Unsupported("MP4 保护信息".into()));
        }
        if matches!(&info.kind, b"moov" | b"moof") {
            if info.size > MAX_MOOV_BYTES {
                return Err(CacheError::Unsupported("MP4 索引超出有界元数据上限".into()));
            }
            let mut bytes = vec![0; info.size as usize];
            source.read_exact_at(&mut bytes, offset)?;
            inspect_mp4_boxes(&mut bytes, 0, None)?;
            if &info.kind == b"moov" {
                found = true;
            }
        }
        offset += info.size;
    }
    if !found {
        return Err(CacheError::Integrity("MP4 缺少 moov 索引".into()));
    }
    Ok(())
}
fn boxed(kind: &[u8; 4], body: &[u8]) -> Result<Vec<u8>, CacheError> {
    let size = body
        .len()
        .checked_add(8)
        .filter(|size| *size <= (MAX_MOOV_BYTES as usize + MAX_TAG_BYTES))
        .ok_or_else(|| CacheError::Unsupported("MP4 标签容器过大".into()))?;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&(size as u32).to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
    Ok(bytes)
}
fn replace_ilst(
    bytes: &[u8],
    ilst: &[u8],
    depth: usize,
    replaced: &mut bool,
    output: &mut Vec<u8>,
) -> Result<(), CacheError> {
    if depth > 16 {
        return Err(CacheError::Unsupported("MP4 元数据嵌套过深".into()));
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let info = mp4_header(&bytes[offset..], (bytes.len() - offset) as u64)?;
        let node = &bytes[offset..offset + info.size as usize];
        if &info.kind == b"ilst" {
            if !*replaced {
                output.extend_from_slice(ilst);
                *replaced = true;
            }
        } else if matches!(&info.kind, b"moov" | b"udta" | b"meta") {
            let child = mp4_child_start(info.kind, info.header).unwrap();
            if child > node.len() {
                return Err(CacheError::Integrity("MP4 元数据头无效".into()));
            }
            let start = output.len();
            output.extend_from_slice(&[0; 4]);
            output.extend_from_slice(&info.kind);
            output.extend_from_slice(&node[info.header..child]);
            replace_ilst(&node[child..], ilst, depth + 1, replaced, output)?;
            let size = (output.len() - start) as u32;
            output[start..start + 4].copy_from_slice(&size.to_be_bytes());
        } else {
            output.extend_from_slice(node);
        }
        if output.len() > MAX_MOOV_BYTES as usize + MAX_TAG_BYTES {
            return Err(CacheError::Unsupported("MP4 元数据重写超出上限".into()));
        }
        offset += info.size as usize;
    }
    Ok(())
}
fn rewrite_mp4(
    source: &File,
    length: u64,
    serialized: &[u8],
    output: &mut File,
    stop: Finalization<'_>,
) -> Result<(), CacheError> {
    let wrapped = if serialized.get(4..8) == Some(b"ilst") {
        None
    } else {
        Some(boxed(b"ilst", serialized)?)
    };
    let ilst = wrapped.as_deref().unwrap_or(serialized);
    let mut checked = 0;
    let mut seen_moov = false;
    while checked < length {
        stop.check()?;
        let info = mp4_header_at(source, checked, length)?;
        if &info.kind == b"moof" || (&info.kind == b"moov" && seen_moov) {
            return Err(CacheError::Unsupported(
                "保留分片/多索引 MP4 原始音频，不改写其绝对偏移标签".into(),
            ));
        }
        if &info.kind == b"moov" {
            seen_moov = true;
        }
        checked += info.size;
    }
    let mut offset = 0;
    while offset < length {
        stop.check()?;
        let info = mp4_header_at(source, offset, length)?;
        if &info.kind != b"moov" {
            copy_range(source, offset, info.size, output, stop)?;
        } else {
            if info.size > MAX_MOOV_BYTES {
                return Err(CacheError::Unsupported(
                    "MP4 moov 超出有界元数据上限".into(),
                ));
            }
            let mut original = vec![0; info.size as usize];
            source.read_exact_at(&mut original, offset)?;
            let mut replaced = false;
            let mut rewritten = Vec::with_capacity(original.len().saturating_add(ilst.len()));
            replace_ilst(&original, ilst, 0, &mut replaced, &mut rewritten)?;
            if !replaced {
                let mut handler = vec![0; 24];
                handler[8..12].copy_from_slice(b"mdir");
                let mut metadata = vec![0; 4];
                metadata.extend_from_slice(&boxed(b"hdlr", &handler)?);
                metadata.extend_from_slice(ilst);
                let udta = boxed(b"udta", &boxed(b"meta", &metadata)?)?;
                if rewritten.len().saturating_add(udta.len())
                    > MAX_MOOV_BYTES as usize + MAX_TAG_BYTES
                {
                    return Err(CacheError::Unsupported("MP4 标签容器超出有界上限".into()));
                }
                rewritten.extend_from_slice(&udta);
                let size = rewritten.len() as u32;
                rewritten[..4].copy_from_slice(&size.to_be_bytes());
            }
            let delta = rewritten.len() as i64 - info.size as i64;
            inspect_mp4_boxes(&mut rewritten, 0, Some((offset + info.size, delta)))?;
            output.write_all(&rewritten)?;
        }
        offset += info.size;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn song(id: u64) -> Song {
        serde_json::from_value(serde_json::json!({"id":id,"name":"缓存传输测试"})).unwrap()
    }
    fn wav_bytes(samples: usize) -> Vec<u8> {
        let size = samples * 2;
        let mut data = Vec::with_capacity(size + 44);
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&((size + 36) as u32).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16_u32.to_le_bytes());
        data.extend_from_slice(&1_u16.to_le_bytes());
        data.extend_from_slice(&1_u16.to_le_bytes());
        data.extend_from_slice(&8000_u32.to_le_bytes());
        data.extend_from_slice(&16000_u32.to_le_bytes());
        data.extend_from_slice(&2_u16.to_le_bytes());
        data.extend_from_slice(&16_u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&(size as u32).to_le_bytes());
        data.resize(size + 44, 0);
        data
    }
    struct Server {
        url: String,
        task: tokio::task::JoinHandle<()>,
        requests: Arc<Mutex<Vec<u64>>>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        DropFirst,
        WrongRange,
        ChangedEntity,
        AlwaysDrop,
        Chunked,
        ChunkedExtra,
    }
    async fn server(data: Vec<u8>, break_first_body: bool) -> Server {
        faulty_server(
            data,
            if break_first_body {
                Fault::DropFirst
            } else {
                Fault::None
            },
        )
        .await
    }
    async fn faulty_server(data: Vec<u8>, fault: Fault) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let data = Arc::new(data);
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let data = data.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        let count = socket.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 || request.len() > 8192 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let request = String::from_utf8_lossy(&request);
                    let offset = request
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("range: bytes=")
                                .and_then(|value| value.strip_suffix('-'))
                                .and_then(|value| value.parse::<u64>().ok())
                        })
                        .unwrap_or(0);
                    let first = {
                        let mut requests = lock(&seen);
                        let first = requests.is_empty();
                        requests.push(offset);
                        first
                    };
                    let etag = if !first && fault == Fault::ChangedEntity {
                        "\"changed-entity\""
                    } else {
                        "\"same-entity\""
                    };
                    let range_start = if !first && fault == Fault::WrongRange {
                        offset + 1
                    } else {
                        offset
                    };
                    let chunked = matches!(fault, Fault::Chunked | Fault::ChunkedExtra);
                    let header = if chunked {
                        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nETag: {etag}\r\nConnection: close\r\n\r\n")
                    } else if offset == 0 {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n", data.len())
                    } else {
                        format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: {etag}\r\nConnection: close\r\n\r\n", data.len() - offset as usize, range_start, data.len() - 1, data.len())
                    };
                    if socket.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    let break_body = fault == Fault::AlwaysDrop
                        || (first
                            && matches!(
                                fault,
                                Fault::DropFirst | Fault::WrongRange | Fault::ChangedEntity
                            ));
                    let end = if break_body {
                        (offset as usize + 16384).min(data.len())
                    } else {
                        data.len()
                    };
                    for chunk in data[offset as usize..end].chunks(4096) {
                        if chunked
                            && socket
                                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                                .await
                                .is_err()
                        {
                            return;
                        }
                        if socket.write_all(chunk).await.is_err() {
                            return;
                        }
                        if chunked && socket.write_all(b"\r\n").await.is_err() {
                            return;
                        }
                        tokio::task::yield_now().await;
                    }
                    if chunked {
                        if fault == Fault::ChunkedExtra
                            && socket.write_all(b"1\r\nx\r\n").await.is_err()
                        {
                            return;
                        }
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    }
                });
            }
        });
        Server {
            url,
            task,
            requests,
        }
    }
    fn stream(url: &str, data: &[u8]) -> yoruplay_api::StreamUrl {
        yoruplay_api::StreamUrl {
            url: url.into(),
            expires_in_secs: 3600,
            bitrate: 128000,
            format: Some("wav".into()),
            size: data.len() as u64,
            md5: Some(format!("{:x}", Md5::digest(data))),
            level: Some("standard".into()),
            trial: false,
        }
    }
    fn options() -> TransferOptions {
        TransferOptions {
            prebuffer_bytes: 4096,
            read_ahead_bytes: 8192,
            scratch_bytes: 64 * 1024,
            retry_interval_secs: 0,
            ..TransferOptions::default()
        }
    }

    #[test]
    fn exact_coverage_rejects_sparse_eof() {
        let mut ranges = Ranges::default();
        ranges.insert(0, 64).unwrap();
        ranges.insert(128, 192).unwrap();
        assert!(!ranges.covers(0, 192));
        assert_eq!(ranges.missing(0, 192), Some(64));
        ranges.insert(64, 128).unwrap();
        assert!(ranges.covers(0, 192));
        ranges.remove(48, 160);
        assert!(ranges.covers(0, 48));
        assert!(ranges.covers(160, 192));
        assert!(!ranges.covers(0, 192));
    }

    #[test]
    fn window_overwrite_invalidates_only_overlapping_physical_bytes() {
        let mut ranges = Ranges::default();
        ranges.insert(0, 64).unwrap();
        ranges.invalidate_physical(0, 32, 64);
        ranges.insert(64, 96).unwrap();
        assert!(!ranges.covers(0, 32));
        assert!(ranges.covers(32, 96));
        ranges.invalidate_physical(48, 64, 64);
        assert!(!ranges.covers(48, 64));
        assert!(ranges.covers(64, 96));
    }

    #[test]
    fn range_and_native_format_validation_is_strict() {
        assert_eq!(parse_content_range("bytes 5-9/20").unwrap(), (5, 10, 20));
        for invalid in [
            "bytes 5-20/20",
            "bytes 10-5/20",
            "bytes 0-3/*",
            "items 0-3/4",
        ] {
            assert!(parse_content_range(invalid).is_err());
        }
        assert!(matches!(
            sniff(b"CTENFDAMxxxx"),
            Err(CacheError::Unsupported(_))
        ));
        assert!(sniff(b"{\"error\":123}").is_err());
        assert_eq!(
            sniff(b"\xff\xf1\x50\x80xxxxxxxx").unwrap().extension(),
            "aac"
        );
        assert_eq!(
            parse_md5("d41d8cd98f00b204e9800998ecf8427e").unwrap(),
            decode_md5_base64("1B2M2Y8AsgTpgAmY7PhCfg==").unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upgrade_drives_the_playback_acquisition_without_second_download() {
        let data = wav_bytes(65536);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let transfer = cache
            .start(
                song(1),
                stream(&server.url, &data),
                CacheMetadata {
                    lyrics: Some("原歌词".into()),
                    ..CacheMetadata::default()
                },
                options(),
            )
            .await
            .unwrap();
        transfer.download_complete().await.unwrap();
        assert!(transfer.is_complete());
        let entry = transfer.finish().await.unwrap().unwrap();
        assert_eq!(entry.format, "wav");
        assert_eq!(entry.lyrics.as_deref(), Some("原歌词"));
        assert_eq!(lock(&server.requests).as_slice(), &[0]);
        let mut cached = cache.lookup(1).unwrap().unwrap();
        let mut magic = [0; 4];
        cached.input.read_exact(&mut magic).unwrap();
        assert_eq!(&magic, b"RIFF");
        assert!(cache.info().unwrap()["pinned_bytes"].as_u64().unwrap() >= entry.bytes);
        cache.reconfigure(0, true).unwrap();
        assert!(cache.entry(1).unwrap().is_some());
        drop(cached);
        assert!(cache.entry(1).unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forward_seek_does_not_turn_sparse_tail_into_complete_entry() {
        let data = wav_bytes(524288);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let transfer = cache
            .start(
                song(2),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        let mut reader = transfer.reader();
        tokio::task::spawn_blocking(move || {
            reader.seek(SeekFrom::End(-4096)).unwrap();
            let mut tail = vec![0; 4096];
            reader.read_exact(&mut tail).unwrap();
            assert_eq!(reader.read(&mut [0; 1]).unwrap(), 0);
        })
        .await
        .unwrap();
        assert!(!transfer.is_complete());
        assert!(lock(&server.requests).iter().any(|offset| *offset > 0));
        assert!(transfer.finish().await.unwrap().is_none());
        assert!(cache.lookup(2).unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_body_retries_missing_range_with_shared_budget() {
        let data = wav_bytes(65536);
        let server = server(data.clone(), true).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let budget = Arc::new(AtomicU32::new(1));
        let mut options = options();
        options.retry_budget = Some(budget.clone());
        let transfer = cache
            .start(
                song(3),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options,
            )
            .await
            .unwrap();
        use std::os::unix::fs::MetadataExt;
        let inode = lock(&transfer.control.shared.state)
            .file
            .as_ref()
            .unwrap()
            .metadata()
            .unwrap()
            .ino();
        transfer.download_complete().await.unwrap();
        assert_eq!(
            lock(&transfer.control.shared.state)
                .file
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .ino(),
            inode
        );
        assert_eq!(budget.load(Ordering::Acquire), 0);
        assert_eq!(transfer.retry_count(), 1);
        assert!(lock(&server.requests)
            .get(1)
            .is_some_and(|offset| *offset > 0));
        assert!(transfer.finish().await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_cache_is_a_bounded_disk_window_and_cancel_closes_it() {
        let data = wav_bytes(524288);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 0, false).unwrap();
        let transfer = cache
            .start(
                song(4),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        let mut reader = transfer.reader();
        transfer.pause(true);
        assert!(matches!(
            transfer.download_complete().await,
            Err(CacheError::NotCacheable)
        ));
        assert!(cache.info().unwrap()["scratch_bytes"].as_u64().unwrap() <= 64 * 1024);
        transfer.cancel().await;
        assert!(lock(&transfer.control.shared.state).file.is_none());
        assert_eq!(cache.info().unwrap()["scratch_bytes"], 0);
        assert!(reader.read(&mut [0; 1]).is_err());
        drop(reader);
        assert!(transfer.finish().await.unwrap().is_none());
        for entry in std::fs::read_dir(cache.directory()).unwrap() {
            assert!(!entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsafe_entity_or_content_range_never_publishes() {
        for fault in [Fault::WrongRange, Fault::ChangedEntity] {
            let data = wav_bytes(65536);
            let server = faulty_server(data.clone(), fault).await;
            let root = tempfile::tempdir().unwrap();
            let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
            let mut source = stream(&server.url, &data);
            source.md5 = None; // Strong ETag is the only identity in this fixture.
            let transfer = cache
                .start(song(5), source, CacheMetadata::default(), options())
                .await
                .unwrap();
            let error = transfer.download_complete().await.unwrap_err();
            assert!(!error.is_retryable());
            transfer.cancel().await;
            assert!(transfer.finish().await.unwrap().is_none());
            assert!(cache.lookup(5).unwrap().is_none());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_shared_retries_do_not_multiply_and_failed_upgrade_preserves_old() {
        let data = wav_bytes(131072);
        let good = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let old = cache
            .start(
                song(6),
                stream(&good.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        old.download_complete().await.unwrap();
        let old_entry = old.finish().await.unwrap().unwrap();
        let bad = faulty_server(data.clone(), Fault::AlwaysDrop).await;
        let budget = Arc::new(AtomicU32::new(3));
        let mut options = options();
        options.retry_budget = Some(budget.clone());
        let upgrade = cache
            .start(
                song(6),
                stream(&bad.url, &data),
                CacheMetadata::default(),
                options,
            )
            .await
            .unwrap();
        assert!(upgrade.download_complete().await.is_err());
        assert_eq!(budget.load(Ordering::Acquire), 0);
        assert_eq!(upgrade.retry_count(), 3);
        assert_eq!(lock(&bad.requests).len(), 4);
        upgrade.cancel().await;
        assert!(upgrade.finish().await.unwrap().is_none());
        assert_eq!(cache.lookup(6).unwrap().unwrap().entry.path, old_entry.path);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fifo_is_not_changed_by_local_playback() {
        let data = wav_bytes(4096);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let first = cache
            .start(
                song(7),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        first.download_complete().await.unwrap();
        first.finish().await.unwrap().unwrap();
        let second = cache
            .start(
                song(8),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        second.download_complete().await.unwrap();
        let second = second.finish().await.unwrap().unwrap();
        drop(cache.lookup(7).unwrap().unwrap());
        cache
            .reconfigure(second.bytes + second.artwork_bytes, true)
            .unwrap();
        assert!(cache.lookup(7).unwrap().is_none());
        assert!(cache.lookup(8).unwrap().is_some());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn api_length_without_content_length_confirms_eof_and_rejects_extra_bytes() {
        for extra in [false, true] {
            let data = wav_bytes(65536);
            let server = faulty_server(
                data.clone(),
                if extra {
                    Fault::ChunkedExtra
                } else {
                    Fault::Chunked
                },
            )
            .await;
            let root = tempfile::tempdir().unwrap();
            let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
            let transfer = cache
                .start(
                    song(9),
                    stream(&server.url, &data),
                    CacheMetadata::default(),
                    options(),
                )
                .await
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), transfer.download_complete())
                .await
                .expect("known API length must not suppress HTTP EOF confirmation");
            if extra {
                assert!(result.is_err());
                transfer.cancel().await;
                assert!(transfer.finish().await.unwrap().is_none());
                assert!(cache.lookup(9).unwrap().is_none());
            } else {
                result.unwrap();
                assert!(transfer.is_complete());
                assert!(transfer.finish().await.unwrap().is_some());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_finish_attempt_waits_for_inode_and_prevents_late_commit() {
        let data = wav_bytes(4096);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let old = cache
            .start(
                song(10),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        old.download_complete().await.unwrap();
        let old_entry = old.finish().await.unwrap().unwrap();
        let upgrade = cache
            .start(
                song(10),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        upgrade.download_complete().await.unwrap();
        let shared = upgrade.control.shared.clone();
        let epoch = shared.cancel_epoch.load(Ordering::Acquire);
        let file = {
            let mut state = lock(&shared.state);
            state.finalizing = true;
            state.file.take().unwrap()
        };
        let owner = shared.clone();
        let worker = std::thread::spawn(move || {
            let guard = FinalizingGuard(owner.clone());
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while owner.cancel_epoch.load(Ordering::Acquire) == epoch
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            let invalidated = owner.cancel_epoch.load(Ordering::Acquire) != epoch;
            drop(file);
            drop(guard);
            assert!(invalidated, "aborted finish must invalidate its publisher");
        });
        drop(FinishAttemptGuard {
            shared: shared.clone(),
            armed: true,
        });
        worker.join().unwrap();
        assert!(!lock(&shared.state).finalizing);
        let committed = std::sync::atomic::AtomicBool::new(false);
        let result = Finalization {
            shared: &shared,
            epoch,
        }
        .commit(|| {
            committed.store(true, Ordering::Release);
            Ok(())
        });
        assert!(matches!(result, Err(CacheError::Cancelled)));
        assert!(!committed.load(Ordering::Acquire));
        assert_eq!(
            cache.lookup(10).unwrap().unwrap().entry.path,
            old_entry.path
        );
        assert_eq!(cache.info().unwrap()["reserved_bytes"], 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_unlinks_pinned_entries_without_interrupting_the_open_reader() {
        let data = wav_bytes(4096);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let transfer = cache
            .start(
                song(20),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        transfer.download_complete().await.unwrap();
        let entry = transfer.finish().await.unwrap().unwrap();
        let expected = std::fs::read(&entry.path).unwrap();
        let mut local = cache.lookup(20).unwrap().unwrap();
        cache.clear().unwrap();
        assert!(cache.entry(20).unwrap().is_none());
        assert!(!entry.path.exists());
        let mut actual = Vec::new();
        local.input.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, expected);
        drop(local);
        assert_eq!(cache.info().unwrap()["audio_bytes"], 0);
        drop(transfer);
        drop(cache);
        let reopened = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        assert!(reopened.lookup(20).unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_prevents_an_already_downloaded_candidate_from_reappearing() {
        let data = wav_bytes(4096);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let transfer = cache
            .start(
                song(21),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        transfer.download_complete().await.unwrap();
        cache.clear().unwrap();
        assert!(transfer.finish().await.unwrap().is_none());
        assert!(cache.lookup(21).unwrap().is_none());
        assert_eq!(cache.info().unwrap()["reserved_bytes"], 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoning_a_completed_upgrade_is_sticky_and_preserves_the_old_entry() {
        let data = wav_bytes(4096);
        let server = server(data.clone(), false).await;
        let root = tempfile::tempdir().unwrap();
        let cache = AudioCache::open(root.path().to_path_buf(), 8 * 1024 * 1024, true).unwrap();
        let old = cache
            .start(
                song(30),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        old.download_complete().await.unwrap();
        let original = old.finish().await.unwrap().unwrap().path;
        drop(old);
        let candidate = cache
            .start(
                song(30),
                stream(&server.url, &data),
                CacheMetadata::default(),
                options(),
            )
            .await
            .unwrap();
        candidate.download_complete().await.unwrap();
        candidate.discard().await;
        assert_eq!(cache.info().unwrap()["reserved_bytes"], 0);
        assert!(candidate.finish().await.unwrap().is_none());
        assert_eq!(cache.lookup(30).unwrap().unwrap().entry.path, original);
    }
}
