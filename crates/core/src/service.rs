//! The YoruPlay daemon state machine.
//!
//! All playback mutations go through `Daemon::handle_command` (or the pure
//! queue helpers it calls). TUI, CLI and MPRIS therefore observe and mutate one
//! authoritative queue and player state.

use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex, RwLock, Semaphore};
use yoruplay_api::{ApiError, Lyrics, NeteaseClient};
use yoruplay_protocol::{
    AuthState, Command, CommandResult, ErrorInfo, Event, Notice, NoticeLevel, PlayMode, PlaySource,
    PlaybackState, PlaybackStatus, PlaylistRef, PlaylistSummary, PlaylistTracks, QueueItem,
    QueueSnapshot, Song, SongsView, StateSnapshot,
};

use crate::audio::{AudioEngine, AudioError, AudioEvent, AudioInput, AudioOptions};
use crate::audio_cache::{AudioCache, CacheEntry, CacheError, CacheMetadata};
use crate::browser_login::{self, BrowserLoginError};
use crate::cache::FileCache;
use crate::queue::{Advance, Queue, QueueError, RemoveOutcome};
use crate::store::{self, PersistedState};
use crate::transfer::{consume_retry, RefreshSource, Transfer, TransferOptions};

const DAILY_CACHE_TTL_SECS: u64 = 6 * 60 * 60;
const CONTROL_TRANSACTIONS: u32 = 8;
const URGENT_TRANSACTIONS: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    Api(#[from] ApiError),
    #[error("{0}")]
    Audio(#[from] AudioError),
    #[error("{message}")]
    AudioEvent { message: String, retryable: bool },
    #[error("{0}")]
    Cache(#[from] CacheError),
    #[error("{0}")]
    Queue(#[from] QueueError),
    #[error("{0}")]
    Browser(#[from] BrowserLoginError),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unsupported(String),
}

impl ServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            ServiceError::Api(e) => e.code(),
            ServiceError::Audio(_) | ServiceError::AudioEvent { .. } => "audio_error",
            ServiceError::Cache(_) => "audio_cache",
            ServiceError::Queue(QueueError::HeartbeatRequiresContext) => "heartbeat_unavailable",
            ServiceError::Queue(_) => "queue_error",
            ServiceError::Browser(e) => e.code(),
            ServiceError::Invalid(_) => "invalid_argument",
            ServiceError::Unsupported(_) => "unsupported",
        }
    }

    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            ServiceError::Api(ApiError::SessionExpired | ApiError::AuthRequired)
        )
    }

    pub fn into_error_info(self) -> ErrorInfo {
        let code = self.code();
        let (message, hint) = match self {
            ServiceError::Api(e) => {
                let hint = e.hint().map(str::to_string);
                (e.user_message(), hint)
            }
            ServiceError::Audio(e) => (
                e.user_message(),
                Some("Run `yoruplay doctor` and check the audio output device.".into()),
            ),
            ServiceError::AudioEvent { message, .. } => (message, None),
            ServiceError::Cache(e) => (e.user_message(), None),
            ServiceError::Queue(QueueError::HeartbeatRequiresContext) => (
                "Heartbeat needs a playlist context. Start playback from a playlist first.".into(),
                Some("Open a playlist, play a song, then enable Heartbeat.".into()),
            ),
            ServiceError::Queue(e) => (e.to_string(), None),
            ServiceError::Browser(e) => (e.to_string(), e.hint().map(str::to_string)),
            ServiceError::Invalid(e) => (e, None),
            ServiceError::Unsupported(e) => (e, None),
        };
        ErrorInfo {
            code: code.into(),
            message,
            hint,
        }
    }
}

#[derive(Debug)]
struct DaemonState {
    queue: Queue,
    status: PlaybackStatus,
    position_ms: u64,
    duration_ms: u64,
    volume: f64,
    error: Option<String>,
    warning: Option<String>,
    auth: AuthState,
    audio_available: bool,
    audio_source: Option<String>,
    actual_quality: Option<String>,
    local_artwork: Option<String>,
    local_only: bool,
    url_refreshed_at_ms: Option<u64>,
    desired_paused: bool,
    lyrics: Option<String>,
    warnings: Vec<String>,
}

impl DaemonState {
    fn playback(&self) -> PlaybackState {
        let current = self.queue.current().map(|item| {
            let mut song = item.song.clone();
            if self.audio_source.as_deref() != Some("network") {
                song.cover_url = self.local_artwork.clone();
            }
            song
        });
        PlaybackState {
            status: self.status,
            position_ms: self.position_ms.min(self.duration_ms.max(self.position_ms)),
            duration_ms: self.duration_ms,
            volume: self.volume,
            mode: self.queue.mode(),
            source: self.queue.source().clone(),
            current_index: self.queue.current_index(),
            queue_len: self.queue.len(),
            current,
            error: self.error.clone(),
            warning: self.warning.clone(),
            audio_available: self.audio_available,
            audio_source: self.audio_source.clone(),
            actual_quality: self.actual_quality.clone(),
            url_refreshed_at_ms: self.url_refreshed_at_ms,
        }
    }

    fn sync_current_metadata(&mut self, reset_position: bool) {
        if let Some(item) = self.queue.current() {
            self.duration_ms = item.song.duration_ms;
            if reset_position {
                self.position_ms = 0;
            } else {
                self.position_ms = self.position_ms.min(self.duration_ms);
            }
        } else {
            self.duration_ms = 0;
            self.position_ms = 0;
            self.lyrics = None;
        }
    }

    fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
        self.status = PlaybackStatus::Stopped;
        self.position_ms = 0;
    }

    fn clear_track_transient(&mut self) {
        self.error = None;
        self.warning = None;
        self.lyrics = None;
        self.audio_source = None;
        self.actual_quality = None;
        self.local_artwork = None;
        self.desired_paused = false;
        self.url_refreshed_at_ms = None;
    }
}

struct DaemonInner {
    config: yoruplay_protocol::Config,
    runtime_config: RwLock<yoruplay_protocol::Config>,
    paths: yoruplay_protocol::Paths,
    metadata_http: reqwest::Client,
    api: NeteaseClient,
    cache: FileCache,
    state: RwLock<DaemonState>,
    audio_cache: Result<AudioCache, String>,
    audio: Mutex<Option<AudioEngine>>,
    retiring_audio: Mutex<Option<tokio::task::JoinHandle<()>>>,
    transfer: Mutex<Option<Transfer>>,
    local_source: Mutex<Option<Arc<LocalAudio>>>,
    load_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    retry_budget: Mutex<Arc<AtomicU32>>,
    generation_tx: watch::Sender<u64>,
    upgrade: Mutex<Option<UpgradeState>>,
    cache_admin: Mutex<()>,
    transfer_start: Mutex<()>,
    play_lock: Mutex<()>,
    control_slots: Arc<Semaphore>,
    urgent_slots: Arc<Semaphore>,
    generation: AtomicU64,
    events: broadcast::Sender<Event>,
    shutdown_tx: watch::Sender<bool>,
    last_save: Mutex<Instant>,
    save_lock: Arc<Mutex<()>>,
    shutting_down: AtomicBool,
    audio_event_tx: mpsc::Sender<(u64, u64, AudioEvent)>,
    audio_generation: AtomicU64,
    loading_source: AtomicU64,
    pending_audio_error: Mutex<Option<(u64, ServiceError)>>,
}

struct UpgradeState {
    owner: u64,
    transfer: Option<Transfer>,
    cancel: watch::Sender<bool>,
    finished: oneshot::Receiver<()>,
}

struct UpgradeCompletion(Option<oneshot::Sender<()>>);

impl Drop for UpgradeCompletion {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
}

struct LocalAudio {
    entry: CacheEntry,
    input: std::sync::Mutex<AudioInput>,
    byte_len: u64,
}

impl LocalAudio {
    fn reader(self: &Arc<Self>) -> io::Result<AudioInput> {
        self.input
            .lock()
            .map_err(|_| io::Error::other("local audio reader lock poisoned"))?
            .seek(SeekFrom::Start(0))?;
        Ok(Box::new(LocalReader(self.clone())))
    }
}

struct LocalReader(Arc<LocalAudio>);

impl Read for LocalReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0
            .input
            .lock()
            .map_err(|_| io::Error::other("local audio reader lock poisoned"))?
            .read(bytes)
    }
}

impl Seek for LocalReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.0
            .input
            .lock()
            .map_err(|_| io::Error::other("local audio reader lock poisoned"))?
            .seek(position)
    }
}

pub fn request_owner() -> u64 {
    static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<DaemonInner>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon").finish_non_exhaustive()
    }
}

impl Daemon {
    pub async fn build(
        config: yoruplay_protocol::Config,
        paths: yoruplay_protocol::Paths,
        api: NeteaseClient,
        cache: FileCache,
        shutdown_tx: watch::Sender<bool>,
    ) -> Self {
        let mut warnings = Vec::new();
        if let Some(warning) = api.startup_warning() {
            warnings.push(warning.to_string());
        }
        let mut volume = config.playback.volume;
        let mut mode = config.playback.default_mode;
        let mut queue = Queue::new(mode);
        let mut position_ms = 0;
        let mut duration_ms = 0;
        queue.set_revision(store::now_ms().saturating_mul(1_000_000));
        let mut restored = false;
        if config.playback.restore_queue {
            match store::load_state(&paths.state_file()) {
                Ok(Some(state)) => {
                    volume = state.volume;
                    mode = state.mode;
                    if !state.queue.is_empty() {
                        queue.replace(
                            Arc::unwrap_or_clone(state.queue),
                            state.current.unwrap_or(0),
                            state.source,
                        );
                        duration_ms = queue
                            .current()
                            .map(|item| item.song.duration_ms)
                            .unwrap_or(0);
                        position_ms = state.position_ms.min(duration_ms);
                    }
                    if let Err(e) = queue.set_mode(state.mode) {
                        warnings.push(format!(
                            "Could not restore mode {}: {e}; using list repeat.",
                            state.mode.label()
                        ));
                        let _ = queue.set_mode(PlayMode::ListLoop);
                    }
                    restored = true;
                    if state.queue_revision > 0 {
                        queue.set_revision(state.queue_revision);
                    }
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("Ignoring playback state: {e}")),
            }
        }
        if !restored {
            volume = config.playback.volume;
            let _ = queue.set_mode(mode);
        }
        let audio_cache = AudioCache::open(
            paths.audio_dir(&config),
            audio_budget(&config),
            config.cache.audio.enabled,
        )
        .map_err(|e| e.user_message());
        if let Err(error) = &audio_cache {
            warnings.push(error.clone());
        }
        if audio_cache
            .as_ref()
            .is_ok_and(|cache| cache.memory_backed())
        {
            warnings.push("Audio cache is on tmpfs/ramfs: downloaded audio consumes RAM. Set cache.audio.dir to a disk-backed directory.".into());
        }
        let auth = api.auth_state().await;
        let (events, _) = broadcast::channel(32);
        let local_only = queue.source().local_only;
        let state = DaemonState {
            queue,
            status: PlaybackStatus::Stopped,
            position_ms,
            duration_ms,
            volume,
            error: None,
            warning: None,
            auth,
            audio_available: true,
            audio_source: None,
            actual_quality: None,
            local_artwork: None,
            local_only,
            url_refreshed_at_ms: None,
            desired_paused: false,
            lyrics: None,
            warnings,
        };
        let last_save = Mutex::new(Instant::now() - Duration::from_secs(10));
        let (audio_event_tx, mut audio_event_rx) = mpsc::channel::<(u64, u64, AudioEvent)>(32);
        let (generation_tx, _) = watch::channel(0);
        let daemon = Self {
            inner: Arc::new(DaemonInner {
                runtime_config: RwLock::new(config.clone()),
                retry_budget: Mutex::new(Arc::new(AtomicU32::new(config.playback.retry_count))),
                config,
                paths,
                api,
                metadata_http: reqwest::Client::new(),
                cache,
                state: RwLock::new(state),
                audio_cache,
                audio: Mutex::new(None),
                retiring_audio: Mutex::new(None),
                transfer: Mutex::new(None),
                local_source: Mutex::new(None),
                load_task: Mutex::new(None),
                generation_tx,
                upgrade: Mutex::new(None),
                cache_admin: Mutex::new(()),
                transfer_start: Mutex::new(()),
                play_lock: Mutex::new(()),
                control_slots: Arc::new(Semaphore::new(CONTROL_TRANSACTIONS as usize)),
                urgent_slots: Arc::new(Semaphore::new(URGENT_TRANSACTIONS as usize)),
                generation: AtomicU64::new(0),
                events,
                shutdown_tx,
                last_save,
                save_lock: Arc::new(Mutex::new(())),
                shutting_down: AtomicBool::new(false),
                audio_event_tx,
                audio_generation: AtomicU64::new(0),
                loading_source: AtomicU64::new(0),
                pending_audio_error: Mutex::new(None),
            }),
        };
        let handler = daemon.clone();
        tokio::spawn(async move {
            let mut shutdown = handler.subscribe_shutdown();
            loop {
                if handler.is_shutting_down() || *shutdown.borrow() {
                    break;
                }
                let message = tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    message = audio_event_rx.recv() => message,
                };
                let Some((generation, source_generation, event)) = message else {
                    break;
                };
                handler
                    .handle_audio_event(generation, source_generation, event)
                    .await;
            }
        });
        daemon
    }

    pub fn paths(&self) -> &yoruplay_protocol::Paths {
        &self.inner.paths
    }

    pub fn config(&self) -> &yoruplay_protocol::Config {
        &self.inner.config
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.inner.shutdown_tx.subscribe()
    }

    pub fn request_shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        self.next_generation();
        let _ = self.inner.shutdown_tx.send(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }

    pub fn cache(&self) -> &FileCache {
        &self.inner.cache
    }

    pub async fn snapshot(&self) -> StateSnapshot {
        let state = self.inner.state.read().await;
        StateSnapshot {
            playback: state.playback(),
            queue: state.queue.snapshot_page(0, 100),
            auth: state.auth.clone(),
            warnings: state.warnings.clone(),
        }
    }

    pub async fn playback(&self) -> PlaybackState {
        self.inner.state.read().await.playback()
    }

    pub async fn queue(&self) -> QueueSnapshot {
        self.inner.state.read().await.queue.snapshot_page(0, 100)
    }

    pub async fn auth(&self) -> AuthState {
        self.inner.state.read().await.auth.clone()
    }

    pub async fn lyrics(&self) -> Option<String> {
        self.inner.state.read().await.lyrics.clone()
    }

    pub async fn warnings(&self) -> Vec<String> {
        self.inner.state.read().await.warnings.clone()
    }

    pub async fn add_warning(&self, warning: impl Into<String>) {
        let warning = warning.into();
        tracing::warn!(%warning, "daemon warning");
        let mut state = self.inner.state.write().await;
        if !state.warnings.contains(&warning) {
            state.warnings.push(warning);
        }
    }

    async fn sync_auth(&self) {
        let auth = self.inner.api.auth_state().await;
        self.set_auth(auth).await;
    }

    async fn set_auth(&self, auth: AuthState) {
        let changed = {
            let mut state = self.inner.state.write().await;
            if state.auth == auth {
                false
            } else {
                state.auth = auth.clone();
                true
            }
        };
        if changed {
            let _ = self.inner.events.send(Event::Auth(auth));
        }
    }

    pub(crate) async fn state_events(&self) -> [Event; 3] {
        let state = self.inner.state.read().await;
        [
            Event::Playback(Box::new(state.playback())),
            Event::QueueChanged {
                revision: state.queue.revision(),
            },
            Event::Auth(state.auth.clone()),
        ]
    }

    async fn publish_playback(&self) {
        let playback = self.inner.state.read().await.playback();
        let _ = self.inner.events.send(Event::Playback(Box::new(playback)));
    }

    async fn publish_queue(&self) {
        let revision = self.inner.state.read().await.queue.revision();
        let _ = self.inner.events.send(Event::QueueChanged { revision });
    }

    fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let _ = self.inner.events.send(Event::Notice(Notice {
            level,
            message: message.into(),
        }));
    }

    fn next_generation(&self) -> u64 {
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.generation_tx.send_replace(generation);
        generation
    }

    fn current_generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    async fn save_state(&self, force: bool) {
        {
            let mut last = self.inner.last_save.lock().await;
            if !force && last.elapsed() < Duration::from_secs(2) {
                return;
            }
            *last = Instant::now();
        }
        let save = self.inner.save_lock.clone().lock_owned().await;
        let (queue, current, mode, source, volume, position, revision) = {
            let state = self.inner.state.read().await;
            (
                state.queue.shared_items(),
                state.queue.current_index(),
                state.queue.mode(),
                state.queue.source().clone(),
                state.volume,
                state.position_ms,
                state.queue.revision(),
            )
        };
        let persisted =
            PersistedState::new(queue, current, mode, source, volume, position, revision);
        let path = self.inner.paths.state_file();
        match tokio::task::spawn_blocking(move || {
            let _save = save;
            store::save_state(&path, &persisted)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "failed to persist playback state"),
            Err(e) => tracing::warn!(error = %e, "state writer failed"),
        }
    }

    async fn submit_control<F, Fut>(
        &self,
        urgent: bool,
        action: F,
    ) -> Result<CommandResult, ServiceError>
    where
        F: FnOnce(Daemon) -> Fut + Send + 'static,
        Fut: Future<Output = Result<CommandResult, ServiceError>> + Send + 'static,
    {
        if self.is_shutting_down() {
            return Err(ServiceError::Invalid("Daemon is shutting down".into()));
        }
        let slots = if urgent {
            self.inner.urgent_slots.clone()
        } else {
            self.inner.control_slots.clone()
        };
        let permit = slots
            .acquire_owned()
            .await
            .map_err(|_| ServiceError::Invalid("Daemon controls are closed".into()))?;
        if self.is_shutting_down() {
            return Err(ServiceError::Invalid("Daemon is shutting down".into()));
        }
        let daemon = self.clone();
        // An admitted short transaction belongs to the daemon, not to the
        // lifetime of the client's response future.
        tokio::spawn(async move {
            let _permit = permit;
            action(daemon).await
        })
        .await
        .map_err(|error| ServiceError::Invalid(format!("Playback control failed: {error}")))?
    }

    pub async fn handle_command(&self, command: Command) -> Result<CommandResult, ErrorInfo> {
        self.handle_owned_command(command, request_owner()).await
    }

    pub async fn handle_owned_command(
        &self,
        command: Command,
        owner: u64,
    ) -> Result<CommandResult, ErrorInfo> {
        let upgrade = matches!(&command, Command::UpgradeAudio { .. });
        let result = self.handle_command_inner(command, owner).await;
        if upgrade {
            self.cancel_owned_upgrade(owner).await;
        }
        if let Err(error) = &result {
            if error.is_auth_failure() {
                self.sync_auth().await;
            }
        }
        result.map_err(ServiceError::into_error_info)
    }

    async fn handle_command_inner(
        &self,
        command: Command,
        owner: u64,
    ) -> Result<CommandResult, ServiceError> {
        match command {
            Command::Ping => Ok(CommandResult::ack()),
            Command::GetState => Ok(CommandResult::State(Box::new(self.snapshot().await))),
            Command::GetPlayback => Ok(CommandResult::Playback(Box::new(self.playback().await))),
            Command::GetQueue { offset, limit } => Ok(CommandResult::Queue(Box::new(
                self.inner
                    .state
                    .read()
                    .await
                    .queue
                    .snapshot_page(offset, limit.min(100)),
            ))),
            Command::PlayCached { song_id } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.play_cached(song_id).await
                })
                .await
            }
            Command::CacheInfo => {
                let mut info = self.audio_cache()?.info()?;
                info["active_audio_dir"] = serde_json::json!(self.audio_cache()?.directory());
                info["active_config"] = serde_json::json!(*self.inner.runtime_config.read().await);
                info["metadata_bytes"] = serde_json::json!(self.inner.cache.size_bytes());
                info["metadata_max_bytes"] = serde_json::json!(self.inner.cache.max_bytes());
                Ok(CommandResult::Data(info))
            }
            Command::CacheList { offset, limit } => {
                Ok(CommandResult::Data(serde_json::json!(self
                    .audio_cache()?
                    .list(offset, limit)?)))
            }
            Command::ClearAudioCache { all } => {
                let _admin = self.inner.cache_admin.lock().await;
                self.cancel_upgrade().await;
                let result = self.audio_cache()?.clear()?;
                if all {
                    self.inner
                        .cache
                        .clear()
                        .map_err(|e| ServiceError::Invalid(e.to_string()))?;
                }
                Ok(CommandResult::Data(result))
            }
            Command::UpgradeAudio { song_id, quality } => {
                self.upgrade_audio(song_id, quality, owner).await
            }
            Command::ReloadConfig => {
                self.submit_control(false, |daemon| async move { daemon.reload_config().await })
                    .await
            }
            Command::Shutdown => {
                let daemon = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    daemon.request_shutdown();
                });
                Ok(CommandResult::ack_msg("Shutting down"))
            }
            Command::ImportBrowserCookie { force } => {
                let cookie = browser_login::firefox_cookie_header()?;
                if force {
                    let new_music_u = browser_login::music_u_from_header(&cookie);
                    if let (Some(current), Some(new)) =
                        (self.inner.api.current_music_u().await, new_music_u)
                    {
                        if current == new {
                            return Err(BrowserLoginError::SameAccount.into());
                        }
                    }
                }
                self.inner.api.import_cookie(&cookie).await?;
                self.sync_auth().await;
                Ok(CommandResult::Auth(self.auth().await))
            }
            Command::ImportCookie { cookie } => {
                self.inner.api.import_cookie(&cookie).await?;
                self.sync_auth().await;
                Ok(CommandResult::Auth(self.auth().await))
            }
            Command::Logout => {
                let auth = self.inner.api.logout().await?;
                self.set_auth(auth.clone()).await;
                Ok(CommandResult::Auth(auth))
            }
            Command::Playlists { kind } => {
                let playlists = self.inner.api.playlists(kind).await?;
                Ok(CommandResult::Playlists(playlists))
            }
            Command::LikedSongs => Ok(CommandResult::Songs(self.inner.api.liked_songs().await?)),
            Command::RecentSongs => Ok(CommandResult::Songs(self.inner.api.recent_songs().await?)),
            Command::PlaylistTracks { id, offset, limit } => Ok(CommandResult::PlaylistTracks(
                self.inner.api.playlist_tracks(id, offset, limit).await?,
            )),
            Command::DailySongs { refresh } => {
                Ok(CommandResult::Songs(self.daily_songs(refresh).await?))
            }
            Command::DailyPlaylists { refresh } => Ok(CommandResult::Playlists(
                self.daily_playlists(refresh).await?,
            )),
            Command::Radar { refresh } => {
                Ok(CommandResult::PlaylistTracks(self.radar(refresh).await?))
            }
            Command::PersonalFm => self.start_personal_fm().await,
            Command::Search {
                query,
                kind,
                offset,
                limit,
            } => Ok(CommandResult::Search(
                self.inner.api.search(&query, kind, offset, limit).await?,
            )),
            Command::ArtistSongs { id, offset, limit } => Ok(CommandResult::Songs(
                self.inner.api.artist_songs(id, offset, limit).await?,
            )),
            Command::AlbumSongs { id } => {
                Ok(CommandResult::Songs(self.inner.api.album_songs(id).await?))
            }
            Command::PlaySongs {
                songs,
                start_index,
                context,
                title,
            } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.play_songs(songs, start_index, context, title).await
                })
                .await
            }
            Command::StartPlaylist {
                id,
                start_index,
                start_song_id,
            } => self.start_playlist(id, start_index, start_song_id).await,
            Command::AddSongs { songs, context } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.add_songs(songs, context).await
                })
                .await
            }
            Command::AddPlaylist { id } => self.add_playlist(id).await,
            Command::QueueRemove { index } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.queue_remove(index).await
                })
                .await
            }
            Command::QueueClear => {
                self.submit_control(false, |daemon| async move { daemon.queue_clear().await })
                    .await
            }
            Command::QueuePlay { index } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.queue_play(index).await
                })
                .await
            }
            Command::Play => {
                self.submit_control(false, |daemon| async move { daemon.play().await })
                    .await
            }
            Command::Pause => {
                self.submit_control(false, |daemon| async move { daemon.pause().await })
                    .await
            }
            Command::PlayPause => {
                self.submit_control(false, |daemon| async move {
                    let playing = {
                        let state = daemon.inner.state.read().await;
                        state.status == PlaybackStatus::Playing
                            || (state.status == PlaybackStatus::Buffering && !state.desired_paused)
                    };
                    if playing {
                        daemon.pause().await
                    } else {
                        daemon.play().await
                    }
                })
                .await
            }
            Command::Next => {
                self.submit_control(true, |daemon| async move { daemon.next_track(false).await })
                    .await
            }
            Command::Previous => {
                self.submit_control(false, |daemon| async move { daemon.previous_track().await })
                    .await
            }
            Command::Stop => {
                self.submit_control(true, |daemon| async move { daemon.stop().await })
                    .await
            }
            Command::SeekRelative { offset_ms } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.seek_relative(offset_ms).await
                })
                .await
            }
            Command::SetPosition { position_ms } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.set_position(position_ms).await
                })
                .await
            }
            Command::SetVolume { volume } => {
                self.submit_control(false, move |daemon| async move {
                    daemon.set_volume(volume).await
                })
                .await
            }
            Command::SetMode { mode } => {
                self.submit_control(
                    false,
                    move |daemon| async move { daemon.set_mode(mode).await },
                )
                .await
            }
            Command::CycleMode => {
                self.submit_control(false, |daemon| async move { daemon.cycle_mode().await })
                    .await
            }
            Command::RefreshCurrent => {
                self.submit_control(
                    false,
                    |daemon| async move { daemon.refresh_current().await },
                )
                .await
            }
            Command::ClearCache => {
                let before = self.inner.cache.size_bytes();
                self.inner
                    .cache
                    .clear()
                    .map_err(|e| ServiceError::Invalid(format!("could not clear cache: {e}")))?;
                Ok(CommandResult::ack_msg(format!(
                    "Cache cleared ({} bytes)",
                    before
                )))
            }
        }
    }

    async fn daily_songs(&self, refresh: bool) -> Result<SongsView, ServiceError> {
        let key = "daily_songs";
        if !refresh {
            if let Some(songs) = self.inner.cache.get::<Vec<Song>>(key, DAILY_CACHE_TTL_SECS) {
                return Ok(SongsView {
                    title: "Daily recommendations (cached)".into(),
                    description: Some(
                        "Cached for up to 6 hours; use --refresh to fetch again.".into(),
                    ),
                    songs,
                    context: None,
                });
            }
        }
        let songs = self.inner.api.daily_songs().await?;
        if let Err(e) = self.inner.cache.put(key, &songs) {
            tracing::warn!(error = %e, "could not cache daily songs");
        }
        Ok(SongsView {
            title: "Daily recommendations".into(),
            description: None,
            songs,
            context: None,
        })
    }

    async fn daily_playlists(&self, refresh: bool) -> Result<Vec<PlaylistSummary>, ServiceError> {
        let key = "daily_playlists";
        if !refresh {
            if let Some(playlists) = self
                .inner
                .cache
                .get::<Vec<PlaylistSummary>>(key, DAILY_CACHE_TTL_SECS)
            {
                return Ok(playlists);
            }
        }
        let playlists = self.inner.api.daily_playlists().await?;
        if let Err(e) = self.inner.cache.put(key, &playlists) {
            tracing::warn!(error = %e, "could not cache daily playlists");
        }
        Ok(playlists)
    }

    async fn radar(&self, refresh: bool) -> Result<PlaylistTracks, ServiceError> {
        let playlists = self.daily_playlists(refresh).await?;
        let found = playlists.into_iter().find(|playlist| {
            let name = playlist.name.to_lowercase();
            name.contains("私人雷达")
                || name.contains("private radar")
                || name.contains("private_radar")
        });
        let Some(found) = found else {
            return Err(ServiceError::Unsupported(
                "Private Radar is not present in this account's daily recommended playlists".into(),
            ));
        };
        Ok(self.inner.api.playlist_tracks(found.id, 0, 100_000).await?)
    }

    async fn start_personal_fm(&self) -> Result<CommandResult, ServiceError> {
        let mode = self.inner.state.read().await.queue.mode();
        if mode == PlayMode::Intelligence {
            return Err(ServiceError::Queue(QueueError::HeartbeatRequiresContext));
        }
        let songs = self.inner.api.personal_fm().await?;
        if songs.is_empty() {
            return Err(ServiceError::Unsupported(
                "Personal FM returned no songs".into(),
            ));
        }
        let items = songs
            .into_iter()
            .map(|song| QueueItem {
                song,
                context: None,
            })
            .collect();
        self.submit_control(false, move |daemon| async move {
            daemon
                .replace_queue(items, 0, PlaySource::personal_fm())
                .await
        })
        .await
    }

    async fn play_songs(
        &self,
        songs: Vec<Song>,
        start_index: usize,
        context: Option<PlaylistRef>,
        _title: Option<String>,
    ) -> Result<CommandResult, ServiceError> {
        if songs.is_empty() {
            return Err(ServiceError::Invalid("no songs selected".into()));
        }
        let mode = self.inner.state.read().await.queue.mode();
        if mode == PlayMode::Intelligence && context.is_none() {
            return Err(ServiceError::Queue(QueueError::HeartbeatRequiresContext));
        }
        let items: Vec<QueueItem> = songs
            .into_iter()
            .map(|song| QueueItem {
                song,
                context: context.clone(),
            })
            .collect();
        let source = context
            .clone()
            .map(PlaySource::playlist)
            .unwrap_or_else(PlaySource::none);
        self.replace_queue(items, start_index, source).await
    }

    async fn replace_queue(
        &self,
        items: Vec<QueueItem>,
        start_index: usize,
        source: PlaySource,
    ) -> Result<CommandResult, ServiceError> {
        if items.is_empty() {
            return Err(ServiceError::Invalid("no songs selected".into()));
        }
        let _lock = self.inner.play_lock.lock().await;
        {
            let state = self.inner.state.read().await;
            let selected = start_index.min(items.len() - 1);
            if state.queue.mode() == PlayMode::Intelligence && items[selected].context.is_none() {
                return Err(ServiceError::Queue(QueueError::HeartbeatRequiresContext));
            }
        }
        let generation = self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state.local_only = source.local_only;
            state.queue.replace(items, start_index, source);
            state.clear_track_transient();
            state.status = PlaybackStatus::Buffering;
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn start_playlist(
        &self,
        id: u64,
        start_index: usize,
        start_song_id: Option<u64>,
    ) -> Result<CommandResult, ServiceError> {
        let tracks = self.inner.api.playlist_tracks(id, 0, 100_000).await?;
        if tracks.songs.is_empty() {
            return Err(ServiceError::Invalid(
                "playlist has no playable songs".into(),
            ));
        }
        let start_index = if let Some(song_id) = start_song_id {
            tracks
                .songs
                .iter()
                .position(|song| song.id == song_id)
                .ok_or_else(|| {
                    ServiceError::Invalid(format!(
                        "Selected song {song_id} is no longer available in this playlist"
                    ))
                })?
        } else {
            start_index
        };
        let context = PlaylistRef {
            id: tracks.playlist.id,
            name: tracks.playlist.name.clone(),
        };
        let items: Vec<QueueItem> = tracks
            .songs
            .into_iter()
            .map(|song| QueueItem {
                song,
                context: Some(context.clone()),
            })
            .collect();
        self.submit_control(false, move |daemon| async move {
            daemon
                .replace_queue(items, start_index, PlaySource::playlist(context))
                .await
        })
        .await
    }

    async fn add_songs(
        &self,
        songs: Vec<Song>,
        context: Option<PlaylistRef>,
    ) -> Result<CommandResult, ServiceError> {
        if songs.is_empty() {
            return Err(ServiceError::Invalid("no songs selected".into()));
        }
        let count = songs.len();
        let mut state = self.inner.state.write().await;
        let was_empty = state.queue.is_empty();
        state.queue.append(songs.into_iter().map(|song| QueueItem {
            song,
            context: context.clone(),
        }));
        if was_empty {
            state.sync_current_metadata(true);
        }
        drop(state);
        self.publish_queue().await;
        self.save_state(true).await;
        Ok(CommandResult::ack_msg(format!(
            "Added {count} song(s) to the queue"
        )))
    }

    async fn add_playlist(&self, id: u64) -> Result<CommandResult, ServiceError> {
        let tracks = self.inner.api.playlist_tracks(id, 0, 100_000).await?;
        if tracks.songs.is_empty() {
            return Err(ServiceError::Invalid("playlist has no songs".into()));
        }
        let context = PlaylistRef {
            id: tracks.playlist.id,
            name: tracks.playlist.name,
        };
        self.submit_control(false, move |daemon| async move {
            daemon.add_songs(tracks.songs, Some(context)).await
        })
        .await
    }

    async fn queue_play(&self, index: usize) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        {
            let mut state = self.inner.state.write().await;
            let heart = state.queue.mode() == PlayMode::Intelligence;
            if heart {
                let context = state
                    .queue
                    .items()
                    .get(index)
                    .and_then(|item| item.context.clone())
                    .ok_or(ServiceError::Queue(QueueError::HeartbeatRequiresContext))?;
                let _ = context;
            }
            state.queue.jump_to(index)?;
            if heart {
                // Re-seed Heartbeat from the manually selected context track.
                state.queue.set_mode(PlayMode::Intelligence)?;
            }
            state.status = PlaybackStatus::Buffering;
            state.clear_track_transient();
            state.sync_current_metadata(true);
        }
        let generation = self.next_generation();
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn queue_remove(&self, index: usize) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let mut cancel = false;
        {
            let mut state = self.inner.state.write().await;
            let outcome = state.queue.remove(index)?;
            match outcome {
                RemoveOutcome::RemovedCurrent { .. } => {
                    state.status = PlaybackStatus::Stopped;
                    cancel = true;
                    state.position_ms = 0;
                    state.clear_track_transient();
                    state.sync_current_metadata(true);
                }
                RemoveOutcome::Removed => {
                    state.sync_current_metadata(false);
                }
            }
        }
        if cancel {
            self.next_generation();
            self.shutdown_audio().await;
        }
        self.publish_queue().await;
        self.publish_playback().await;
        self.save_state(true).await;
        Ok(CommandResult::ack_msg("Removed from queue"))
    }

    async fn queue_clear(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state.queue.clear();
            state.status = PlaybackStatus::Stopped;
            state.position_ms = 0;
            state.duration_ms = 0;
            state.clear_track_transient();
            state.local_only = false;
        }
        self.publish_queue().await;
        self.publish_playback().await;
        self.shutdown_audio().await;
        self.save_state(true).await;
        Ok(CommandResult::ack_msg("Queue cleared"))
    }

    async fn play(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let (status, position) = {
            let state = self.inner.state.read().await;
            (state.status, state.position_ms)
        };
        if status == PlaybackStatus::Playing || status == PlaybackStatus::Buffering {
            self.inner.state.write().await.desired_paused = false;
            return Ok(CommandResult::ack_msg("Already playing"));
        }
        if status == PlaybackStatus::Paused {
            if let Some(audio) = self.inner.audio.lock().await.as_ref() {
                if let Some(transfer) = self.inner.transfer.lock().await.as_ref() {
                    transfer.pause(false);
                }
                audio.pause(false).await?;
                let mut state = self.inner.state.write().await;
                state.status = PlaybackStatus::Playing;
                state.desired_paused = false;
                drop(state);
                self.publish_playback().await;
                return Ok(CommandResult::ack());
            }
        }
        let generation = self.next_generation();
        self.load_current_locked(generation, position, false).await
    }

    async fn pause(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let status = self.inner.state.read().await.status;
        if status == PlaybackStatus::Buffering {
            self.inner.state.write().await.desired_paused = true;
            return Ok(CommandResult::ack_msg("Will start paused"));
        }
        if status != PlaybackStatus::Playing {
            return Ok(CommandResult::ack_msg("Not playing"));
        }
        if let Some(audio) = self.inner.audio.lock().await.as_ref() {
            audio.pause(true).await?;
        }
        if let Some(transfer) = self.inner.transfer.lock().await.as_ref() {
            transfer.pause(true);
        }
        {
            let mut state = self.inner.state.write().await;
            state.desired_paused = true;
            state.status = PlaybackStatus::Paused;
        }
        self.publish_playback().await;
        self.save_state(true).await;
        Ok(CommandResult::ack())
    }

    async fn stop(&self) -> Result<CommandResult, ServiceError> {
        self.interrupt_source().await;
        let _lock = self.inner.play_lock.lock().await;
        self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state.status = PlaybackStatus::Stopped;
            state.position_ms = 0;
            state.error = None;
            state.desired_paused = false;
        }
        self.publish_playback().await;
        self.shutdown_audio().await;
        self.save_state(true).await;
        Ok(CommandResult::ack())
    }

    async fn previous_track(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let moved = {
            let mut state = self.inner.state.write().await;
            let moved = state.queue.previous();
            if moved.is_some() {
                state.status = PlaybackStatus::Buffering;
                state.clear_track_transient();
                state.sync_current_metadata(true);
            }
            moved
        };
        let Some(_) = moved else {
            return Ok(CommandResult::ack_msg("No previous track"));
        };
        let generation = self.next_generation();
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn next_track(&self, auto: bool) -> Result<CommandResult, ServiceError> {
        self.next_track_for(auto, None).await
    }

    async fn next_track_for(
        &self,
        auto: bool,
        expected: Option<(u64, u64)>,
    ) -> Result<CommandResult, ServiceError> {
        if expected.is_none() {
            self.interrupt_source().await;
        }
        let _lock = self.inner.play_lock.lock().await;
        if expected.is_some_and(|(generation, source)| {
            generation != self.current_generation()
                || source != self.inner.audio_generation.load(Ordering::SeqCst)
        }) {
            return Ok(CommandResult::ack_msg("Superseded"));
        }
        let local = {
            let state = self.inner.state.read().await;
            state.local_only || state.audio_source.as_deref() == Some("local")
        };
        let generation = self.next_generation();
        self.shutdown_audio().await;
        // Commit known queue traversal before returning the command. Superseding
        // the decoder load must not discard a second rapid Next action.
        let ready = {
            let mut state = self.inner.state.write().await;
            let remaining = state
                .queue
                .len()
                .saturating_sub(state.queue.current_index().unwrap_or(0).saturating_add(1));
            if local && state.queue.source().personal_fm && remaining == 0 {
                Some(Advance::End)
            } else if !local
                && (state.queue.mode() == PlayMode::Intelligence
                    || (state.queue.source().personal_fm && remaining == 0))
            {
                None
            } else if state.queue.mode() == PlayMode::Intelligence {
                let index = state.queue.current_index().unwrap_or(0).saturating_add(1);
                if index < state.queue.len() {
                    state.queue.jump_to(index)?;
                    Some(Advance::Index(index))
                } else {
                    Some(Advance::End)
                }
            } else {
                let mut rng = rand::thread_rng();
                Some(state.queue.advance(auto, &mut rng))
            }
        };
        match ready {
            Some(Advance::Index(_)) => {
                {
                    let mut state = self.inner.state.write().await;
                    state.clear_track_transient();
                    state.sync_current_metadata(true);
                    state.status = PlaybackStatus::Buffering;
                }
                self.publish_queue().await;
                self.reset_retry_budget().await;
                self.spawn_load(generation, 0, false).await;
                self.publish_playback().await;
                return Ok(CommandResult::ack_msg("Loading next track"));
            }
            Some(Advance::End) => {
                let mut state = self.inner.state.write().await;
                state.status = PlaybackStatus::Stopped;
                state.position_ms = 0;
                drop(state);
                if local {
                    self.notice(
                        NoticeLevel::Info,
                        "End of locally known queue; no online recommendations requested",
                    );
                }
                self.publish_playback().await;
                self.save_state(true).await;
                return Ok(CommandResult::ack_msg("End of queue"));
            }
            Some(Advance::Heartbeat) => unreachable!("online Heartbeat is prepared asynchronously"),
            None => {}
        }
        self.inner.state.write().await.status = PlaybackStatus::Buffering;
        let daemon = self.clone();
        let mut cancel = self.inner.generation_tx.subscribe();
        let task = tokio::spawn(async move {
            if *cancel.borrow() != generation {
                return;
            }
            tokio::select! {
                biased;
                _ = cancel.changed() => {}
                result = daemon.navigate_next(generation, auto, local) => {
                    if let Err(error) = result {
                        let mut state = daemon.inner.state.write().await;
                        if daemon.current_generation() == generation {
                            state.set_error(error.into_error_info().message);
                            drop(state);
                            daemon.publish_playback().await;
                            daemon.save_state(true).await;
                        }
                    }
                }
            }
        });
        *self.inner.load_task.lock().await = Some(task);
        self.publish_playback().await;
        Ok(CommandResult::ack_msg("Loading next track"))
    }

    async fn navigate_next(
        &self,
        generation: u64,
        auto: bool,
        local: bool,
    ) -> Result<(), ServiceError> {
        if !local {
            self.maybe_refill_personal_fm(generation).await?;
        }
        let advance = {
            let mut state = self.inner.state.write().await;
            if self.current_generation() != generation {
                return Ok(());
            }
            if state.queue.mode() == PlayMode::Intelligence && local {
                let index = state.queue.current_index().unwrap_or(0).saturating_add(1);
                if index < state.queue.len() {
                    state.queue.jump_to(index)?;
                    Advance::Index(index)
                } else {
                    Advance::End
                }
            } else {
                let mut rng = rand::thread_rng();
                state.queue.advance(auto, &mut rng)
            }
        };
        match advance {
            Advance::End => {
                let mut state = self.inner.state.write().await;
                if self.current_generation() != generation {
                    return Ok(());
                }
                state.status = PlaybackStatus::Stopped;
                state.position_ms = 0;
                drop(state);
                if local {
                    self.notice(
                        NoticeLevel::Info,
                        "End of locally known queue; no online recommendations requested",
                    );
                }
                self.publish_playback().await;
                self.save_state(true).await;
            }
            Advance::Heartbeat => {
                self.heartbeat(generation).await?;
                self.reset_retry_budget().await;
                self.run_load(generation, 0, false).await;
            }
            Advance::Index(_) => {
                {
                    let mut state = self.inner.state.write().await;
                    if self.current_generation() != generation {
                        return Ok(());
                    }
                    state.clear_track_transient();
                    state.sync_current_metadata(true);
                    state.status = PlaybackStatus::Buffering;
                }
                self.publish_queue().await;
                self.reset_retry_budget().await;
                self.run_load(generation, 0, false).await;
            }
        }
        Ok(())
    }

    async fn maybe_refill_personal_fm(&self, generation: u64) -> Result<(), ServiceError> {
        let (is_fm, remaining) = {
            let state = self.inner.state.read().await;
            let current = state.queue.current_index().unwrap_or(0);
            (
                state.queue.source().personal_fm,
                state.queue.len().saturating_sub(current + 1),
            )
        };
        if !is_fm || remaining > 1 {
            return Ok(());
        }
        match self.inner.api.personal_fm().await {
            Ok(songs) if !songs.is_empty() => {
                let mut state = self.inner.state.write().await;
                if self.current_generation() != generation {
                    return Ok(());
                }
                state.queue.append(songs.into_iter().map(|song| QueueItem {
                    song,
                    context: None,
                }));
                drop(state);
                self.publish_queue().await;
                Ok(())
            }
            Ok(_) => {
                self.notice(NoticeLevel::Warning, "Personal FM returned no new songs");
                Ok(())
            }
            Err(e) => {
                if remaining == 0 {
                    Err(ServiceError::Api(e))
                } else {
                    self.notice(
                        NoticeLevel::Warning,
                        format!("Could not refill Personal FM: {}", e.user_message()),
                    );
                    Ok(())
                }
            }
        }
    }

    async fn heartbeat(&self, generation: u64) -> Result<(), ServiceError> {
        let (playlist, seed, current_id, context) = {
            let state = self.inner.state.read().await;
            let current = state
                .queue
                .current()
                .ok_or_else(|| ServiceError::Invalid("queue is empty".into()))?;
            let context = current
                .context
                .clone()
                .ok_or(ServiceError::Queue(QueueError::HeartbeatRequiresContext))?;
            (
                context.clone(),
                state.queue.heartbeat_seed().unwrap_or(current.song.id),
                current.song.id,
                context,
            )
        };
        let songs = self
            .inner
            .api
            .intelligence(playlist.id, seed, current_id, 1)
            .await?;
        if self.current_generation() != generation {
            return Ok(());
        }
        let Some(song) = songs.into_iter().next() else {
            return Err(ServiceError::Unsupported(
                "Heartbeat returned no recommendations".into(),
            ));
        };
        {
            let mut state = self.inner.state.write().await;
            if self.current_generation() != generation {
                return Ok(());
            }
            state.queue.insert_after_current(QueueItem {
                song,
                context: Some(context),
            });
            state.status = PlaybackStatus::Buffering;
            state.clear_track_transient();
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        Ok(())
    }

    async fn seek_relative(&self, offset_ms: i64) -> Result<CommandResult, ServiceError> {
        let (status, duration, position) = {
            let state = self.inner.state.read().await;
            (state.status, state.duration_ms, state.position_ms)
        };
        if status == PlaybackStatus::Stopped {
            return Err(ServiceError::Invalid("nothing is playing".into()));
        }
        let target = if offset_ms >= 0 {
            position.saturating_add(offset_ms as u64)
        } else {
            position.saturating_sub(offset_ms.unsigned_abs())
        };
        let target = if duration > 0 {
            target.min(duration)
        } else {
            target
        };
        self.seek_to(target, status).await
    }

    async fn set_position(&self, position_ms: u64) -> Result<CommandResult, ServiceError> {
        let (status, duration) = {
            let state = self.inner.state.read().await;
            (state.status, state.duration_ms)
        };
        if status == PlaybackStatus::Stopped {
            return Err(ServiceError::Invalid("nothing is playing".into()));
        }
        let target = if duration > 0 {
            position_ms.min(duration)
        } else {
            position_ms
        };
        self.seek_to(target, status).await
    }

    async fn seek_to(
        &self,
        target_ms: u64,
        status: PlaybackStatus,
    ) -> Result<CommandResult, ServiceError> {
        let generation = self.current_generation();
        let source_generation = self.inner.audio_generation.load(Ordering::SeqCst);
        let transfer = self.inner.transfer.lock().await.clone();
        if let Some(transfer) = &transfer {
            transfer.pause(false);
        }
        let result = if let Some(audio) = self.inner.audio.lock().await.as_ref() {
            if generation != self.current_generation()
                || source_generation != self.inner.audio_generation.load(Ordering::SeqCst)
            {
                return Ok(CommandResult::ack_msg("Superseded"));
            }
            audio
                .seek_ms(target_ms)
                .await
                .map(|()| audio.position_ms())
                .map_err(ServiceError::from)
        } else {
            Err(ServiceError::Invalid("audio is still loading".into()))
        };
        if generation == self.current_generation()
            && source_generation == self.inner.audio_generation.load(Ordering::SeqCst)
        {
            if let Some(transfer) = &transfer {
                transfer.pause(status == PlaybackStatus::Paused);
            }
        }
        let actual_position = result?;
        {
            let mut state = self.inner.state.write().await;
            if self.current_generation() != generation
                || source_generation != self.inner.audio_generation.load(Ordering::SeqCst)
            {
                return Ok(CommandResult::ack_msg("Superseded"));
            }
            state.position_ms = actual_position;
        }
        self.publish_playback().await;
        Ok(CommandResult::ack())
    }

    async fn set_volume(&self, volume: f64) -> Result<CommandResult, ServiceError> {
        if !volume.is_finite() {
            return Err(ServiceError::Invalid("volume must be a number".into()));
        }
        let volume = volume.clamp(0.0, 1.0);
        if let Some(audio) = self.inner.audio.lock().await.as_ref() {
            audio.set_volume(volume).await?;
        }
        {
            let mut state = self.inner.state.write().await;
            state.volume = volume;
        }
        self.publish_playback().await;
        self.save_state(false).await;
        Ok(CommandResult::ack())
    }

    async fn set_mode(&self, mode: PlayMode) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        {
            let mut state = self.inner.state.write().await;
            state.queue.set_mode(mode)?;
        }
        drop(_lock);
        if let Some(audio) = self.inner.audio.lock().await.as_ref() {
            audio.set_loop_file(mode == PlayMode::TrackLoop).await?;
        }
        self.publish_playback().await;
        self.save_state(true).await;
        Ok(CommandResult::ack_msg(format!("Mode: {}", mode.label())))
    }

    async fn cycle_mode(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let mut candidate = self.inner.state.read().await.queue.mode();
        let mut skipped = false;
        let mut applied = None;
        for _ in 0..PlayMode::ALL.len() {
            candidate = candidate.next();
            let mut state = self.inner.state.write().await;
            match state.queue.set_mode(candidate) {
                Ok(()) => {
                    applied = Some(candidate);
                    break;
                }
                Err(QueueError::HeartbeatRequiresContext) => {
                    skipped = true;
                    continue;
                }
                Err(e) => return Err(ServiceError::Queue(e)),
            }
        }
        let Some(applied) = applied else {
            return Err(ServiceError::Queue(QueueError::HeartbeatRequiresContext));
        };
        drop(_lock);
        if let Some(audio) = self.inner.audio.lock().await.as_ref() {
            audio.set_loop_file(applied == PlayMode::TrackLoop).await?;
        }
        self.publish_playback().await;
        self.save_state(true).await;
        let message = if skipped {
            format!(
                "Mode: {} (Heartbeat skipped: needs a playlist context)",
                applied.label()
            )
        } else {
            format!("Mode: {}", applied.label())
        };
        Ok(CommandResult::ack_msg(message))
    }

    async fn refresh_current(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        let position = self.inner.state.read().await.position_ms;
        self.load_current_locked(generation, position, false).await
    }

    fn audio_cache(&self) -> Result<&AudioCache, ServiceError> {
        self.inner
            .audio_cache
            .as_ref()
            .map_err(|e| ServiceError::Invalid(e.clone()))
    }

    async fn load_current_locked(
        &self,
        generation: u64,
        start_ms: u64,
        paused: bool,
    ) -> Result<CommandResult, ServiceError> {
        if self.inner.state.read().await.queue.is_empty() {
            return Err(ServiceError::Invalid("queue is empty".into()));
        }
        self.shutdown_audio().await;
        {
            let mut state = self.inner.state.write().await;
            state.clear_track_transient();
            state.status = PlaybackStatus::Buffering;
            state.position_ms = start_ms;
            state.desired_paused = paused;
        }
        self.reset_retry_budget().await;
        self.spawn_load(generation, start_ms, false).await;
        self.publish_playback().await;
        Ok(CommandResult::ack_msg("Loading"))
    }

    async fn spawn_load(&self, generation: u64, start_ms: u64, delay: bool) {
        let daemon = self.clone();
        let mut cancelled = self.inner.generation_tx.subscribe();
        let task = tokio::spawn(async move {
            if *cancelled.borrow() != generation {
                return;
            }
            tokio::select! {
                biased;
                _ = cancelled.changed() => {}
                _ = daemon.run_load(generation, start_ms, delay) => {}
            }
        });
        *self.inner.load_task.lock().await = Some(task);
    }

    async fn run_load(&self, generation: u64, mut start_ms: u64, mut delay: bool) {
        let budget = self.inner.retry_budget.lock().await.clone();
        self.release_engine().await;
        let mut rejected = std::collections::HashSet::new();
        // Once selected, a local source never acquires online capability on retry.
        let mut selected_local =
            self.inner.state.read().await.audio_source.as_deref() == Some("local");
        loop {
            if self.current_generation() != generation {
                return;
            }
            let config = self.inner.runtime_config.read().await.clone();
            if delay {
                tokio::time::sleep(Duration::from_secs(config.playback.retry_interval_secs)).await;
            }
            if self.current_generation() != generation {
                return;
            }
            let (song, local_only) = {
                let state = self.inner.state.read().await;
                let Some(item) = state.queue.current() else {
                    return;
                };
                (item.song.clone(), state.local_only)
            };
            let result = async {
                let existing = self.inner.transfer.lock().await.clone();
                let mut local = self.inner.local_source.lock().await.clone();
                if existing.is_none() && local.is_none() {
                    local = self.audio_cache()?.lookup(song.id)?.map(|cached| Arc::new(LocalAudio {
                        entry: cached.entry,
                        input: std::sync::Mutex::new(cached.input),
                        byte_len: cached.byte_len,
                    }));
                }
                let (input, byte_len, quality, lyrics, artwork, source) = if let Some(transfer) = existing {
                    transfer.pause(false);
                    let state = self.inner.state.read().await;
                    (transfer.reader(), transfer.byte_len(), state.actual_quality.clone(), state.lyrics.clone(), None, "network")
                } else if let Some(local) = local {
                    selected_local = true;
                    *self.inner.local_source.lock().await = Some(local.clone());
                    let input = local.reader().map_err(AudioError::from)?;
                    let entry = &local.entry;
                    let art = entry.artwork.as_ref().and_then(|p| reqwest::Url::from_file_path(p).ok()).map(|u| u.to_string());
                    (input, Some(local.byte_len), entry.quality.clone(), entry.lyrics.clone(), art, "local")
                } else {
                    if selected_local || local_only {
                        return Err(ServiceError::Invalid(format!("No complete local audio for song {}", song.id)));
                    }
                    let stream = self.inner.api.stream_url_at(song.id, config.api.quality).await?;
                    if stream.trial {
                        return Err(ServiceError::Unsupported("trial audio is not played or downloaded".into()));
                    }
                    let quality = stream.level.clone();
                    let metadata = self.online_metadata(&song, &config).await;
                    let lyrics = metadata.lyrics.clone();
                    self.cancel_upgrade().await;
                    let _start = self.inner.transfer_start.lock().await;
                    let refresh = RefreshSource { api: self.inner.api.clone(), song_id: song.id, quality: config.api.quality };
                    let transfer = self.audio_cache()?.start(song.clone(), stream, metadata, transfer_options(&config, budget.clone(), Some(refresh))).await?;
                    let input = transfer.reader();
                    let len = transfer.byte_len();
                    *self.inner.transfer.lock().await = Some(transfer);
                    {
                        let mut state = self.inner.state.write().await;
                        if self.current_generation() != generation { return Ok(()); }
                        state.url_refreshed_at_ms = Some(store::now_ms());
                    }
                    (input, len, quality, lyrics, None, "network")
                };
                {
                    let mut state = self.inner.state.write().await;
                    if self.current_generation() != generation { return Ok(()); }
                    state.audio_source = Some(source.into());
                    state.actual_quality = quality;
                    state.local_artwork = artwork;
                    state.lyrics = lyrics.as_deref().map(plain_lyrics).filter(|s| !s.is_empty());
                }
                let engine = AudioEngine::spawn(AudioOptions { device: config.playback.audio_device.clone(), pcm_buffer_ms: config.playback.pcm_buffer_ms }).await?;
                let mut events = engine.subscribe();
                let event_tx = self.inner.audio_event_tx.clone();
                let mut cancel = self.inner.generation_tx.subscribe();
                let source_generation = self.inner.audio_generation.fetch_add(1, Ordering::SeqCst) + 1;
                *self.inner.pending_audio_error.lock().await = None;
                self.inner.loading_source.store(source_generation, Ordering::SeqCst);
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel.changed() => break,
                            event = events.recv() => match event {
                                Ok(event @ AudioEvent::Position { .. }) => {
                                    let _ = event_tx.try_send((generation, source_generation, event));
                                }
                                Ok(event) => {
                                    tokio::select! {
                                        biased;
                                        _ = cancel.changed() => break,
                                        sent = event_tx.send((generation, source_generation, event)) => {
                                            if sent.is_err() { break; }
                                        }
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                });
                let (volume, mode) = {
                    let state = self.inner.state.read().await;
                    (state.volume, state.queue.mode())
                };
                let mut guard = self.inner.audio.lock().await;
                *guard = Some(engine);
                let engine = guard.as_ref().expect("engine installed");
                engine.set_volume(volume).await?;
                engine.set_loop_file(mode == PlayMode::TrackLoop).await?;
                let paused = self.inner.state.read().await.desired_paused;
                engine.load(input, byte_len, start_ms, paused).await?;
                let _control = self.inner.play_lock.lock().await;
                if let Some((source, error)) = self.inner.pending_audio_error.lock().await.take() {
                    if source == source_generation { return Err(error); }
                }
                let paused = self.inner.state.read().await.desired_paused;
                let mode = self.inner.state.read().await.queue.mode();
                let volume = self.inner.state.read().await.volume;
                engine.set_volume(volume).await?;
                engine.set_loop_file(mode == PlayMode::TrackLoop).await?;
                engine.pause(paused).await?;
                if let Some(transfer) = self.inner.transfer.lock().await.as_ref() { transfer.pause(paused); }
                {
                    let mut state = self.inner.state.write().await;
                    if self.current_generation() != generation { return Ok(()); }
                    state.audio_available = true;
                    state.status = if paused { PlaybackStatus::Paused } else { PlaybackStatus::Playing };
                    state.error = None;
                    state.warning = None;
                }
                self.inner.loading_source.store(0, Ordering::SeqCst);
                drop(guard);
                drop(_control);
                self.publish_playback().await;
                self.save_state(false).await;
                Ok::<(), ServiceError>(())
            }.await;
            match result {
                Ok(()) => return,
                Err(ServiceError::Unsupported(message))
                    if message == "trial audio is not played or downloaded" =>
                {
                    self.release_source().await;
                    rejected.insert(song.id);
                    self.notice(
                        NoticeLevel::Warning,
                        format!("Skipping trial-only track: {}", song.name),
                    );
                    let mut state = self.inner.state.write().await;
                    if self.current_generation() != generation {
                        return;
                    }
                    let len = state.queue.len();
                    let current = state.queue.current_index().unwrap_or(0);
                    let candidate = (1..=len)
                        .map(|n| (current + n) % len)
                        .find(|&index| !rejected.contains(&state.queue.items()[index].song.id));
                    if let Some(index) = candidate {
                        if state.queue.jump_to(index).is_err() {
                            return;
                        }
                        state.clear_track_transient();
                        state.sync_current_metadata(true);
                        drop(state);
                        self.publish_queue().await;
                        start_ms = 0;
                        delay = false;
                        selected_local = false;
                        budget.store(config.playback.retry_count, Ordering::SeqCst);
                        continue;
                    }
                    state.set_error("No full playable tracks remain in the queue");
                    drop(state);
                    self.publish_playback().await;
                    self.save_state(true).await;
                    return;
                }
                Err(error) => {
                    if self.current_generation() != generation {
                        return;
                    }
                    let retryable = match &error {
                        ServiceError::Api(error) => api_retryable(error),
                        ServiceError::Audio(error) => error.is_retryable(),
                        ServiceError::Cache(error) => error.is_retryable(),
                        ServiceError::AudioEvent { retryable, .. } => *retryable,
                        _ => false,
                    };
                    let mut state = self.inner.state.write().await;
                    if self.current_generation() != generation {
                        return;
                    }
                    if retryable && consume_retry(&budget) {
                        let retry_count = config
                            .playback
                            .retry_count
                            .saturating_sub(budget.load(Ordering::SeqCst));
                        state.status = PlaybackStatus::Buffering;
                        state.warning = Some(format!(
                            "Playback failed; retry {retry_count}/{} in {} seconds",
                            config.playback.retry_count, config.playback.retry_interval_secs
                        ));
                        drop(state);
                        self.release_engine().await;
                        self.publish_playback().await;
                        delay = true;
                        continue;
                    }
                    if matches!(
                        &error,
                        ServiceError::Audio(
                            AudioError::Device(_) | AudioError::Spawn(_) | AudioError::Closed
                        )
                    ) {
                        state.audio_available = false;
                    }
                    state.set_error(error.into_error_info().message);
                    drop(state);
                    self.release_source().await;
                    self.publish_playback().await;
                    self.save_state(true).await;
                    return;
                }
            }
        }
    }

    async fn online_metadata(
        &self,
        song: &Song,
        config: &yoruplay_protocol::Config,
    ) -> CacheMetadata {
        let key = format!("lyrics_{}", song.id);
        let ttl = config.cache.lyrics_ttl_hours.saturating_mul(3600);
        let lyrics = if let Some(lyrics) = self.inner.cache.get::<Lyrics>(&key, ttl) {
            Some(lyrics)
        } else {
            match self.inner.api.lyrics(song.id).await {
                Ok(lyrics) => {
                    let _ = self.inner.cache.put(&key, &lyrics);
                    Some(lyrics)
                }
                Err(_) => None,
            }
        };
        CacheMetadata {
            lyrics: lyrics.as_ref().map(|l| bounded_text(&l.lrc, 128 * 1024)),
            translated_lyrics: lyrics
                .and_then(|l| l.translated)
                .map(|s| bounded_text(&s, 128 * 1024)),
            artwork: self.fetch_artwork(song.cover_url.as_deref()).await,
        }
    }

    async fn fetch_artwork(&self, url: Option<&str>) -> Option<Vec<u8>> {
        let url = url?;
        let response = self
            .inner
            .metadata_http
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        if response.content_length().is_some_and(|n| n > 512 * 1024) {
            return None;
        }
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            if bytes.len().saturating_add(chunk.len()) > 512 * 1024 {
                return None;
            }
            bytes.extend_from_slice(&chunk);
        }
        Some(bytes)
    }

    async fn reset_retry_budget(&self) {
        let count = self.inner.runtime_config.read().await.playback.retry_count;
        *self.inner.retry_budget.lock().await = Arc::new(AtomicU32::new(count));
    }

    async fn release_engine(&self) {
        self.inner.audio_generation.fetch_add(1, Ordering::SeqCst);
        self.inner.loading_source.store(0, Ordering::SeqCst);
        if let Some(transfer) = self.inner.transfer.lock().await.as_ref() {
            transfer.pause(false);
        }
        let mut retiring = self.inner.retiring_audio.lock().await;
        if let Some(engine) = self.inner.audio.lock().await.take() {
            *retiring = Some(tokio::spawn(async move {
                if let Err(error) = engine.shutdown().await {
                    tracing::warn!(error = %error, "audio shutdown failed");
                }
            }));
        }
        if let Some(task) = retiring.as_mut() {
            let _ = task.await;
        }
        retiring.take();
    }

    async fn release_source(&self) {
        self.inner.audio_generation.fetch_add(1, Ordering::SeqCst);
        let transfer = self.inner.transfer.lock().await.take();
        if let Some(transfer) = &transfer {
            transfer.cancel().await;
        }
        self.release_engine().await;
        self.inner.local_source.lock().await.take();
        if let Some(transfer) = transfer {
            if let Err(error) = transfer.finish().await {
                tracing::warn!(error = %error, "could not publish complete audio");
            }
        }
    }

    async fn interrupt_source(&self) {
        self.next_generation();
        let transfer = self.inner.transfer.lock().await.clone();
        if let Some(transfer) = transfer {
            transfer.cancel().await;
        }
    }

    async fn shutdown_audio(&self) {
        // Wake compressed reads before awaiting the load task and decoder thread.
        if let Some(transfer) = self.inner.transfer.lock().await.as_ref() {
            transfer.cancel().await;
        }
        if let Some(task) = self.inner.load_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.release_source().await;
    }

    pub async fn cancel_upgrade(&self) {
        let upgrade = self.inner.upgrade.lock().await.take();
        self.close_upgrade(upgrade).await;
    }

    pub async fn cancel_owned_upgrade(&self, owner: u64) {
        let upgrade = {
            let mut slot = self.inner.upgrade.lock().await;
            if slot.as_ref().is_some_and(|value| value.owner == owner) {
                slot.take()
            } else {
                None
            }
        };
        self.close_upgrade(upgrade).await;
    }

    async fn close_upgrade(&self, upgrade: Option<UpgradeState>) {
        if let Some(upgrade) = upgrade {
            upgrade.cancel.send_replace(true);
            if let Some(transfer) = upgrade.transfer {
                transfer.discard().await;
            }
            // Cancellation acknowledgement includes dropping the pending start
            // future and closing its anonymous inode, not just setting a flag.
            let _ = upgrade.finished.await;
        }
    }

    async fn handle_audio_event(&self, generation: u64, source_generation: u64, event: AudioEvent) {
        if self.is_shutting_down()
            || generation != self.current_generation()
            || source_generation != self.inner.audio_generation.load(Ordering::SeqCst)
        {
            return;
        }
        match event {
            AudioEvent::Loaded { duration_ms } => {
                let mut state = self.inner.state.write().await;
                if generation == self.current_generation()
                    && source_generation == self.inner.audio_generation.load(Ordering::SeqCst)
                    && duration_ms > 0
                {
                    state.duration_ms = duration_ms;
                }
            }
            AudioEvent::Position {
                position_ms,
                duration_ms,
            } => {
                let mut state = self.inner.state.write().await;
                if generation == self.current_generation()
                    && source_generation == self.inner.audio_generation.load(Ordering::SeqCst)
                    && state.status == PlaybackStatus::Playing
                {
                    state.position_ms = position_ms;
                    if duration_ms > 0 {
                        state.duration_ms = duration_ms;
                    }
                }
            }
            AudioEvent::End => {
                if let Err(error) = self
                    .next_track_for(true, Some((generation, source_generation)))
                    .await
                {
                    self.notice(NoticeLevel::Error, error.into_error_info().message);
                }
            }
            AudioEvent::Error { message, retryable } => {
                let _lock = self.inner.play_lock.lock().await;
                if generation != self.current_generation()
                    || source_generation != self.inner.audio_generation.load(Ordering::SeqCst)
                {
                    return;
                }
                if self.inner.loading_source.load(Ordering::SeqCst) == source_generation {
                    *self.inner.pending_audio_error.lock().await = Some((
                        source_generation,
                        ServiceError::AudioEvent { message, retryable },
                    ));
                    return;
                }
                let config = self.inner.runtime_config.read().await.clone();
                let budget = self.inner.retry_budget.lock().await.clone();
                let (position, retry) = {
                    let mut state = self.inner.state.write().await;
                    if generation != self.current_generation()
                        || source_generation != self.inner.audio_generation.load(Ordering::SeqCst)
                        || state.status == PlaybackStatus::Stopped
                    {
                        return;
                    }
                    if retryable && consume_retry(&budget) {
                        let retry_count = config
                            .playback
                            .retry_count
                            .saturating_sub(budget.load(Ordering::SeqCst));
                        state.status = PlaybackStatus::Buffering;
                        self.inner.audio_generation.fetch_add(1, Ordering::SeqCst);
                        state.warning = Some(format!(
                            "Playback failed; retry {retry_count}/{} in {} seconds",
                            config.playback.retry_count, config.playback.retry_interval_secs
                        ));
                        (state.position_ms, true)
                    } else {
                        state.set_error(message);
                        (0, false)
                    }
                };
                if retry {
                    if let Some(task) = self.inner.load_task.lock().await.take() {
                        task.abort();
                        let _ = task.await;
                    }
                    self.spawn_load(generation, position, true).await;
                } else {
                    self.next_generation();
                    self.shutdown_audio().await;
                }
                self.publish_playback().await;
                self.save_state(true).await;
            }
        }
    }

    async fn play_cached(&self, song_id: u64) -> Result<CommandResult, ServiceError> {
        let entry = self.audio_cache()?.entry(song_id)?.ok_or_else(|| {
            ServiceError::Invalid(format!("No complete local audio for song {song_id}"))
        })?;
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            if state.queue.mode() == PlayMode::Intelligence {
                state.queue.set_mode(PlayMode::ListLoop)?;
            }
            let mut source = PlaySource::none();
            source.local_only = true;
            state.queue.replace(
                vec![QueueItem {
                    song: entry.song,
                    context: None,
                }],
                0,
                source,
            );
            state.local_only = true;
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn upgrade_audio(
        &self,
        song_id: u64,
        quality: Option<yoruplay_protocol::Quality>,
        owner: u64,
    ) -> Result<CommandResult, ServiceError> {
        let result = {
            let (done, finished) = oneshot::channel();
            let _completion = UpgradeCompletion(Some(done));
            let (cancel, mut cancelled) = watch::channel(false);
            {
                let _admin = self.inner.cache_admin.lock().await;
                let mut slot = self.inner.upgrade.lock().await;
                if slot.is_some() {
                    return Err(ServiceError::Invalid(
                        "Another audio upgrade is already running".into(),
                    ));
                }
                *slot = Some(UpgradeState {
                    owner,
                    transfer: None,
                    cancel,
                    finished,
                });
            }
            tokio::select! {
                biased;
                _ = cancelled.changed() => Err(ServiceError::Invalid("Audio upgrade cancelled by playback or its owner".into())),
                result = self.run_upgrade(song_id, quality, owner) => result,
            }
        };
        self.cancel_owned_upgrade(owner).await;
        result
    }

    async fn run_upgrade(
        &self,
        song_id: u64,
        quality: Option<yoruplay_protocol::Quality>,
        owner: u64,
    ) -> Result<CommandResult, ServiceError> {
        let config = self.inner.runtime_config.read().await.clone();
        let cached = self.audio_cache()?.lookup(song_id)?.ok_or_else(|| {
            ServiceError::Invalid("Upgrade requires an existing complete cache entry".into())
        })?;
        let song = cached.entry.song.clone();
        let quality = quality.unwrap_or(config.api.quality);
        let current = cached.entry.quality.as_deref().and_then(quality_rank)
            .ok_or_else(|| ServiceError::Unsupported("Cached audio has no comparable quality level; it will not be overwritten by an unverified upgrade".into()))?;
        let requested =
            quality_rank(quality.as_str()).expect("configured quality is a supported level");
        if requested < current {
            return Err(ServiceError::Invalid(
                "Requested quality is below the cached quality; the existing entry was preserved"
                    .into(),
            ));
        }
        if requested == current {
            return Ok(CommandResult::ack_msg(
                "Cached audio already meets the requested quality",
            ));
        }
        let budget = Arc::new(AtomicU32::new(config.playback.retry_count));
        let stream = loop {
            match self.inner.api.stream_url_at(song_id, quality).await {
                Ok(stream) => break stream,
                Err(error) if api_retryable(&error) && consume_retry(&budget) => {
                    tokio::time::sleep(Duration::from_secs(config.playback.retry_interval_secs))
                        .await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        if stream.trial {
            return Err(ServiceError::Unsupported(
                "Trial audio cannot replace a complete cache entry".into(),
            ));
        }
        let actual = stream.level.as_deref().and_then(quality_rank)
            .ok_or_else(|| ServiceError::Unsupported("Server did not report a comparable actual quality; the existing entry was preserved".into()))?;
        if actual <= current {
            return Err(ServiceError::Unsupported(
                "Server quality fallback is not an upgrade; the existing entry was preserved"
                    .into(),
            ));
        }
        let metadata = self.online_metadata(&song, &config).await;
        let transfer = {
            let _start = self.inner.transfer_start.lock().await;
            if self.inner.transfer.lock().await.is_some() {
                return Err(ServiceError::Invalid(
                    "Network playback has priority; stop it before upgrading".into(),
                ));
            }
            if !self
                .inner
                .upgrade
                .lock()
                .await
                .as_ref()
                .is_some_and(|slot| slot.owner == owner)
            {
                return Err(ServiceError::Invalid("Audio upgrade cancelled".into()));
            }
            let refresh = RefreshSource {
                api: self.inner.api.clone(),
                song_id,
                quality,
            };
            let transfer = self
                .audio_cache()?
                .start(
                    song,
                    stream,
                    metadata,
                    transfer_options(&config, budget, Some(refresh)),
                )
                .await?;
            let mut slot = self.inner.upgrade.lock().await;
            if let Some(slot) = slot.as_mut().filter(|slot| slot.owner == owner) {
                slot.transfer = Some(transfer.clone());
            } else {
                drop(slot);
                transfer.discard().await;
                return Err(ServiceError::Invalid("Audio upgrade cancelled".into()));
            }
            transfer
        };
        transfer.download_complete().await?;
        let entry = transfer.finish().await?;
        {
            let mut slot = self.inner.upgrade.lock().await;
            if slot.as_ref().is_some_and(|value| value.owner == owner) {
                slot.take();
            }
        }
        drop(cached);
        let entry = entry.ok_or_else(|| {
            ServiceError::Invalid(
                "Upgrade was cancelled or could not be retained within the cache budget".into(),
            )
        })?;
        Ok(CommandResult::Data(serde_json::json!(entry)))
    }

    async fn reload_config(&self) -> Result<CommandResult, ServiceError> {
        let mut config = yoruplay_protocol::Config::load_readonly(&self.inner.paths.config_file())
            .map_err(|e| ServiceError::Invalid(e.to_string()))?;
        let mut pending = Vec::new();
        let active = self.inner.runtime_config.read().await.clone();
        if config.api.timeout_secs != active.api.timeout_secs {
            pending.push("api.timeout_secs");
        }
        config.api.timeout_secs = active.api.timeout_secs;
        if config.log.level != active.log.level {
            pending.push("log.level");
        }
        config.log.level = active.log.level.clone();
        if config.ui.language != active.ui.language {
            pending.push("ui.language");
        }
        config.ui.language = active.ui.language;
        if config.playback.volume != active.playback.volume {
            pending.push("playback.volume");
        }
        config.playback.volume = active.playback.volume;
        if config.playback.default_mode != active.playback.default_mode {
            pending.push("playback.default_mode");
        }
        config.playback.default_mode = active.playback.default_mode;
        if config.playback.restore_queue != active.playback.restore_queue {
            pending.push("playback.restore_queue");
        }
        config.playback.restore_queue = active.playback.restore_queue;
        if self.inner.paths.audio_dir(&config) != self.audio_cache()?.directory() {
            pending.push("cache.audio.dir");
            config.cache.audio.dir = active.cache.audio.dir.clone();
        }
        self.audio_cache()?
            .reconfigure(audio_budget(&config), config.cache.audio.enabled)?;
        self.inner
            .cache
            .reconfigure(config.cache.max_mb)
            .map_err(|e| ServiceError::Invalid(e.to_string()))?;
        *self.inner.runtime_config.write().await = config.clone();
        Ok(CommandResult::Data(serde_json::json!({
            "active_config": config,
            "pending": pending,
            "applied": ["api.quality", "cache.max_mb", "cache.lyrics_ttl_hours", "cache.audio.max_gb", "cache.audio.enabled", "playback.audio_device", "playback.pcm_buffer_ms", "playback.prebuffer_kb", "playback.read_ahead_kb", "playback.scratch_mb", "playback.retry_count", "playback.retry_interval_secs"]
        })))
    }

    pub async fn current_song_id(&self) -> Option<u64> {
        self.inner
            .state
            .read()
            .await
            .queue
            .current()
            .map(|i| i.song.id)
    }

    pub async fn shutdown(&self) {
        self.request_shutdown();
        self.interrupt_source().await;
        let _controls = self
            .inner
            .control_slots
            .clone()
            .acquire_many_owned(CONTROL_TRANSACTIONS)
            .await;
        let _urgent = self
            .inner
            .urgent_slots
            .clone()
            .acquire_many_owned(URGENT_TRANSACTIONS)
            .await;
        self.inner.control_slots.close();
        self.inner.urgent_slots.close();
        let _guard = self.inner.play_lock.lock().await;
        self.next_generation();
        self.shutdown_audio().await;
        self.cancel_upgrade().await;
        self.save_state(true).await;
        let _ = self.inner.events.send(Event::Shutdown);
    }

    pub fn start_ticker(&self) -> tokio::task::JoinHandle<()> {
        let daemon = self.clone();
        let mut shutdown = self.subscribe_shutdown();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let playing = daemon.inner.state.read().await.status == PlaybackStatus::Playing;
                        if playing {
                            daemon.publish_playback().await;
                        }
                    }
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
        })
    }
}

fn quality_rank(quality: &str) -> Option<u8> {
    match quality {
        "standard" => Some(0),
        "higher" => Some(1),
        "exhigh" => Some(2),
        "lossless" => Some(3),
        "hires" => Some(4),
        _ => None,
    }
}

fn audio_budget(config: &yoruplay_protocol::Config) -> u64 {
    (config.cache.audio.max_gb * 1024.0 * 1024.0 * 1024.0) as u64
}

fn transfer_options(
    config: &yoruplay_protocol::Config,
    budget: Arc<AtomicU32>,
    refresh: Option<RefreshSource>,
) -> TransferOptions {
    TransferOptions {
        prebuffer_bytes: config.playback.prebuffer_kb.saturating_mul(1024),
        read_ahead_bytes: config.playback.read_ahead_kb.saturating_mul(1024),
        scratch_bytes: config.playback.scratch_mb.saturating_mul(1024 * 1024),
        retry_count: config.playback.retry_count,
        retry_budget: Some(budget),
        refresh,
        retry_interval_secs: config.playback.retry_interval_secs,
    }
}

fn api_retryable(error: &ApiError) -> bool {
    match error {
        ApiError::Network(_) | ApiError::Timeout => true,
        ApiError::HttpStatus { status, .. } => matches!(*status, 408 | 425 | 429 | 500..=599),
        _ => false,
    }
}

fn bounded_text(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

pub fn plain_lyrics(lrc: &str) -> String {
    let mut out = String::new();
    for line in lrc.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut rest = trimmed;
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            rest = stripped[end + 1..].trim_start();
        }
        if rest.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(rest);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upgrades_preserve_superior_audio_before_requested_or_server_downgrades() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const WAV: &[u8] = b"RIFF\x28\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x40\x1f\x00\x00\x80\x3e\x00\x00\x02\x00\x10\x00data\x04\x00\x00\x00\x00\x00\x00\x00";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let api_requests = Arc::new(AtomicU32::new(0));
        let media_requests = Arc::new(AtomicU32::new(0));
        let response = Arc::new(
            serde_json::to_vec(&serde_json::json!({
                "code": 200, "data": [{
                    "id": 900, "url": format!("{endpoint}/audio"), "size": WAV.len(),
                    "br": 128000, "type": "wav", "level": "exhigh", "expi": 1200,
                }]
            }))
            .unwrap(),
        );
        let server = {
            let api = api_requests.clone();
            let media = media_requests.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let (api, media, response) = (api.clone(), media.clone(), response.clone());
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 1024];
                        loop {
                            let count = socket.read(&mut buffer).await.unwrap();
                            if count == 0 {
                                return;
                            }
                            request.extend_from_slice(&buffer[..count]);
                            assert!(request.len() <= 64 * 1024);
                            if let Some(end) =
                                request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                            {
                                let headers = std::str::from_utf8(&request[..end]).unwrap();
                                let body = headers
                                    .lines()
                                    .filter_map(|line| line.split_once(':'))
                                    .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                    .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
                                if request.len() >= end + 4 + body {
                                    break;
                                }
                            }
                        }
                        let body = if request.starts_with(b"POST ") {
                            api.fetch_add(1, Ordering::Relaxed);
                            response.as_slice()
                        } else {
                            media.fetch_add(1, Ordering::Relaxed);
                            WAV
                        };
                        let headers = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"quality-fixture\"\r\nConnection: close\r\n\r\n", body.len());
                        socket.write_all(headers.as_bytes()).await.unwrap();
                        socket.write_all(body).await.unwrap();
                    });
                }
            })
        };
        let root = tempfile::tempdir().unwrap();
        let paths = yoruplay_protocol::Paths {
            config_dir: root.path().join("config"),
            data_dir: root.path().join("data"),
            cache_dir: root.path().join("cache"),
            runtime_dir: root.path().join("run"),
        };
        let config = yoruplay_protocol::Config::default();
        let api = NeteaseClient::with_endpoints(
            &config.api,
            None,
            yoruplay_api::client::Endpoints {
                weapi: endpoint.clone(),
                eapi: endpoint.clone(),
            },
        )
        .unwrap();
        let (shutdown, _) = watch::channel(false);
        let daemon = Daemon::build(
            config,
            paths.clone(),
            api,
            FileCache::new(paths.metadata_dir(), 32),
            shutdown,
        )
        .await;
        let cache = daemon.audio_cache().unwrap();
        let transfer = cache
            .start(
                Song::empty_with_id(900),
                yoruplay_api::StreamUrl {
                    url: format!("{endpoint}/audio"),
                    expires_in_secs: 1200,
                    bitrate: 128000,
                    format: Some("wav".into()),
                    size: WAV.len() as u64,
                    level: Some("lossless".into()),
                    md5: None,
                    trial: false,
                },
                CacheMetadata::default(),
                TransferOptions::default(),
            )
            .await
            .unwrap();
        transfer.download_complete().await.unwrap();
        let original = transfer.finish().await.unwrap().unwrap().path;
        drop(transfer);
        let media_before = media_requests.load(Ordering::Relaxed);
        let _ = daemon
            .handle_command(Command::UpgradeAudio {
                song_id: 900,
                quality: Some(yoruplay_protocol::Quality::Exhigh),
            })
            .await;
        assert_eq!(api_requests.load(Ordering::Relaxed), 0);
        let _ = daemon
            .handle_command(Command::UpgradeAudio {
                song_id: 900,
                quality: Some(yoruplay_protocol::Quality::Hires),
            })
            .await;
        assert_eq!(api_requests.load(Ordering::Relaxed), 1);
        assert_eq!(media_requests.load(Ordering::Relaxed), media_before);
        assert_eq!(cache.entry(900).unwrap().unwrap().path, original);
        daemon.shutdown().await;
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn metadata_limits_preserve_utf8_boundaries() {
        assert_eq!(bounded_text("a中文", 4), "a中");
        assert_eq!(bounded_text("中文", 1), "");
        assert_eq!(bounded_text("short", 128), "short");
    }

    #[test]
    fn authentication_and_unsupported_api_errors_do_not_retry() {
        assert!(!api_retryable(&ApiError::AuthRequired));
        assert!(!api_retryable(&ApiError::SessionExpired));
        assert!(!api_retryable(&ApiError::Unsupported("format".into())));
        assert!(api_retryable(&ApiError::Timeout));
        assert!(api_retryable(&ApiError::HttpStatus {
            status: 503,
            endpoint: "test".into()
        }));
        assert!(api_retryable(&ApiError::HttpStatus {
            status: 408,
            endpoint: "timeout-fixture".into()
        }));
    }
}
