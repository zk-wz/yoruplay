//! The YoruPlay daemon state machine.
//!
//! All playback mutations go through `Daemon::handle_command` (or the pure
//! queue helpers it calls). TUI, CLI and MPRIS therefore observe and mutate one
//! authoritative queue and player state.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock};
use yoruplay_api::{ApiError, Lyrics, NeteaseClient};
use yoruplay_protocol::{
    AuthState, Command, CommandResult, ErrorInfo, Event, Notice, NoticeLevel, PlayMode, PlaySource,
    PlaybackState, PlaybackStatus, PlaylistRef, PlaylistSummary, PlaylistTracks, QrLogin,
    QrLoginStatus, QueueItem, QueueSnapshot, Song, SongsView, StateSnapshot,
};

use crate::cache::FileCache;
use crate::mpv::{MpvController, MpvError, MpvEvent, MpvOptions};
use crate::queue::{Advance, Queue, QueueError, RemoveOutcome};
use crate::store::{self, PersistedState};

const DAILY_CACHE_TTL_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    Api(#[from] ApiError),
    #[error("{0}")]
    Mpv(#[from] MpvError),
    #[error("{0}")]
    Queue(#[from] QueueError),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unsupported(String),
}

impl ServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            ServiceError::Api(e) => e.code(),
            ServiceError::Mpv(MpvError::Missing) => "mpv_missing",
            ServiceError::Mpv(_) => "mpv_error",
            ServiceError::Queue(QueueError::HeartbeatRequiresContext) => "heartbeat_unavailable",
            ServiceError::Queue(_) => "queue_error",
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
            ServiceError::Mpv(e) => {
                let hint = match e {
                    MpvError::Missing => {
                        Some("Install mpv with `sudo pacman -S mpv`, then retry.".into())
                    }
                    _ => Some(
                        "Run `yoruplay doctor` and check `journalctl --user -u yoruplay`.".into(),
                    ),
                };
                (e.user_message(), hint)
            }
            ServiceError::Queue(QueueError::HeartbeatRequiresContext) => (
                "Heartbeat needs a playlist context. Start playback from a playlist first.".into(),
                Some("Open a playlist, play a song, then enable Heartbeat.".into()),
            ),
            ServiceError::Queue(e) => (e.to_string(), None),
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
    mpv_available: bool,
    url_expires_at_ms: Option<u64>,
    url_refreshed_at_ms: Option<u64>,
    retry_count: u8,
    lyrics: Option<String>,
    warnings: Vec<String>,
}

impl DaemonState {
    fn playback(&self) -> PlaybackState {
        let current = self.queue.current().map(|item| item.song.clone());
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
            mpv_available: self.mpv_available,
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
        self.retry_count = 0;
        self.url_expires_at_ms = None;
        self.url_refreshed_at_ms = None;
    }
}

struct DaemonInner {
    config: yoruplay_protocol::Config,
    paths: yoruplay_protocol::Paths,
    api: NeteaseClient,
    cache: FileCache,
    state: RwLock<DaemonState>,
    mpv: Mutex<Option<MpvController>>,
    play_lock: Mutex<()>,
    generation: AtomicU64,
    events: broadcast::Sender<Event>,
    shutdown_tx: watch::Sender<bool>,
    pending_qr: Mutex<Option<QrLogin>>,
    last_save: Mutex<Instant>,
    shutting_down: AtomicBool,
    mpv_event_tx: mpsc::Sender<(u64, MpvEvent)>,
    mpv_generation: AtomicU64,
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
        let mut restored = false;
        if config.playback.restore_queue {
            match store::load_state(&paths.state_file()) {
                Ok(Some(state)) => {
                    volume = state.volume;
                    mode = state.mode;
                    if !state.queue.is_empty() {
                        queue.replace(state.queue, state.current.unwrap_or(0), state.source);
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
                }
                Ok(None) => {}
                Err(e) => warnings.push(format!("Ignoring playback state: {e}")),
            }
        }
        if !restored {
            volume = config.playback.volume;
            let _ = queue.set_mode(mode);
        }
        let (mpv_available, mpv_warning) = match MpvController::available_version().await {
            Ok(version) => {
                tracing::info!(%version, "mpv found in PATH");
                (true, None)
            }
            Err(e) => (false, Some(e.user_message())),
        };
        if let Some(w) = mpv_warning {
            warnings.push(w);
        }
        let auth = api.auth_state().await;
        let (events, _) = broadcast::channel(512);
        let state = DaemonState {
            queue,
            status: PlaybackStatus::Stopped,
            position_ms,
            duration_ms,
            volume,
            error: None,
            warning: None,
            auth,
            mpv_available,
            url_expires_at_ms: None,
            url_refreshed_at_ms: None,
            retry_count: 0,
            lyrics: None,
            warnings,
        };
        let last_save = Mutex::new(Instant::now() - Duration::from_secs(10));
        let (mpv_event_tx, mut mpv_event_rx) = mpsc::channel::<(u64, MpvEvent)>(256);
        let daemon = Self {
            inner: Arc::new(DaemonInner {
                config,
                paths,
                api,
                cache,
                state: RwLock::new(state),
                mpv: Mutex::new(None),
                play_lock: Mutex::new(()),
                generation: AtomicU64::new(0),
                events,
                shutdown_tx,
                pending_qr: Mutex::new(None),
                last_save,
                shutting_down: AtomicBool::new(false),
                mpv_event_tx,
                mpv_generation: AtomicU64::new(0),
            }),
        };
        let handler = daemon.clone();
        tokio::spawn(async move {
            while let Some((generation, event)) = mpv_event_rx.recv().await {
                if generation == handler.inner.mpv_generation.load(Ordering::SeqCst) {
                    handler.handle_mpv_event(event).await;
                }
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
            queue: state.queue.snapshot(),
            auth: state.auth.clone(),
            warnings: state.warnings.clone(),
        }
    }

    pub async fn playback(&self) -> PlaybackState {
        self.inner.state.read().await.playback()
    }

    pub async fn queue(&self) -> QueueSnapshot {
        self.inner.state.read().await.queue.snapshot()
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

    pub async fn initialise_auth(&self) {
        if !self.inner.api.is_logged_in().await {
            self.sync_auth().await;
            return;
        }
        match self.inner.api.validate_session().await {
            Ok(auth) => {
                if auth.session_expired {
                    self.add_warning("The saved NetEase session expired. Log in again.")
                        .await;
                }
                self.set_auth(auth).await;
            }
            Err(e) => {
                self.add_warning(format!(
                    "Could not validate the NetEase session: {}",
                    e.user_message()
                ))
                .await;
                self.sync_auth().await;
            }
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

    async fn publish_playback(&self) {
        let playback = self.inner.state.read().await.playback();
        let _ = self.inner.events.send(Event::Playback(Box::new(playback)));
    }

    async fn publish_queue(&self) {
        let queue = self.inner.state.read().await.queue.snapshot();
        let _ = self.inner.events.send(Event::Queue(Box::new(queue)));
    }

    fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let _ = self.inner.events.send(Event::Notice(Notice {
            level,
            message: message.into(),
        }));
    }

    fn next_generation(&self) -> u64 {
        self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1
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
        let (queue, current, mode, source, volume, position) = {
            let state = self.inner.state.read().await;
            (
                state.queue.items().to_vec(),
                state.queue.current_index(),
                state.queue.mode(),
                state.queue.source().clone(),
                state.volume,
                state.position_ms,
            )
        };
        let persisted = PersistedState::new(queue, current, mode, source, volume, position);
        if let Err(e) = store::save_state(&self.inner.paths.state_file(), &persisted) {
            tracing::warn!(error = %e, "failed to persist playback state");
        }
    }

    async fn fail_playback(&self, message: impl Into<String>) {
        {
            let mut state = self.inner.state.write().await;
            state.set_error(message);
        }
        self.publish_playback().await;
        self.save_state(true).await;
        self.shutdown_mpv().await;
    }

    pub async fn handle_command(&self, command: Command) -> Result<CommandResult, ErrorInfo> {
        let result = self.handle_command_inner(command).await;
        if let Err(error) = &result {
            if error.is_auth_failure() {
                self.sync_auth().await;
            }
        }
        result.map_err(ServiceError::into_error_info)
    }

    async fn handle_command_inner(&self, command: Command) -> Result<CommandResult, ServiceError> {
        match command {
            Command::Ping => Ok(CommandResult::ack()),
            Command::GetState => Ok(CommandResult::State(Box::new(self.snapshot().await))),
            Command::Shutdown => {
                let shutdown = self.inner.shutdown_tx.clone();
                let daemon = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    daemon.inner.shutting_down.store(true, Ordering::SeqCst);
                    let _ = shutdown.send(true);
                });
                Ok(CommandResult::ack_msg("Shutting down"))
            }
            Command::LoginQrStart => {
                let qr = self.inner.api.start_qr_login().await?;
                *self.inner.pending_qr.lock().await = Some(qr.clone());
                Ok(CommandResult::Qr(qr))
            }
            Command::LoginQrPoll { key } => {
                let result = self.inner.api.poll_qr_login(&key).await?;
                if result.status == QrLoginStatus::Success
                    || result.status == QrLoginStatus::Expired
                {
                    *self.inner.pending_qr.lock().await = None;
                    self.sync_auth().await;
                }
                Ok(CommandResult::QrStatus {
                    status: result.status,
                    message: result.message,
                })
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
            } => self.play_songs(songs, start_index, context, title).await,
            Command::StartPlaylist { id, start_index } => {
                self.start_playlist(id, start_index).await
            }
            Command::AddSongs { songs, context } => self.add_songs(songs, context).await,
            Command::AddPlaylist { id } => self.add_playlist(id).await,
            Command::QueueRemove { index } => self.queue_remove(index).await,
            Command::QueueClear => self.queue_clear().await,
            Command::QueuePlay { index } => self.queue_play(index).await,
            Command::Play => self.play().await,
            Command::Pause => self.pause().await,
            Command::PlayPause => {
                let playing = self.playback().await.status == PlaybackStatus::Playing;
                if playing {
                    self.pause().await
                } else {
                    self.play().await
                }
            }
            Command::Next => self.next_track(false).await,
            Command::Previous => self.previous_track().await,
            Command::Stop => self.stop().await,
            Command::SeekRelative { offset_ms } => self.seek_relative(offset_ms).await,
            Command::SetPosition { position_ms } => self.set_position(position_ms).await,
            Command::SetVolume { volume } => self.set_volume(volume).await,
            Command::SetMode { mode } => self.set_mode(mode).await,
            Command::CycleMode => self.cycle_mode().await,
            Command::RefreshCurrent => self.refresh_current().await,
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
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state.queue.replace(items, 0, PlaySource::personal_fm());
            state.status = PlaybackStatus::Buffering;
            state.position_ms = 0;
            state.clear_track_transient();
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
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
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state.queue.replace(items, start_index, source);
            state.status = PlaybackStatus::Buffering;
            state.position_ms = 0;
            state.clear_track_transient();
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn start_playlist(
        &self,
        id: u64,
        start_index: usize,
    ) -> Result<CommandResult, ServiceError> {
        let tracks = self.inner.api.playlist_tracks(id, 0, 100_000).await?;
        if tracks.songs.is_empty() {
            return Err(ServiceError::Invalid(
                "playlist has no playable songs".into(),
            ));
        }
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
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state
                .queue
                .replace(items, start_index, PlaySource::playlist(context));
            state.status = PlaybackStatus::Buffering;
            state.position_ms = 0;
            state.clear_track_transient();
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
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
        let count = tracks.songs.len();
        let context = PlaylistRef {
            id: tracks.playlist.id,
            name: tracks.playlist.name,
        };
        let mut state = self.inner.state.write().await;
        let was_empty = state.queue.is_empty();
        state
            .queue
            .append(tracks.songs.into_iter().map(|song| QueueItem {
                song,
                context: Some(context.clone()),
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

    async fn queue_play(&self, index: usize) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
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
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn queue_remove(&self, index: usize) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        let outcome = {
            let mut state = self.inner.state.write().await;
            let outcome = state.queue.remove(index)?;
            match outcome {
                RemoveOutcome::RemovedCurrent { .. } => {
                    state.status = PlaybackStatus::Stopped;
                    state.position_ms = 0;
                    state.sync_current_metadata(true);
                }
                RemoveOutcome::Removed => {
                    state.sync_current_metadata(false);
                }
            }
            outcome
        };
        if outcome == (RemoveOutcome::RemovedCurrent { stopped: true }) {
            self.shutdown_mpv().await;
        }
        let _ = generation;
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
            state.lyrics = None;
            state.error = None;
            state.warning = None;
        }
        self.publish_queue().await;
        self.publish_playback().await;
        self.save_state(true).await;
        self.shutdown_mpv().await;
        Ok(CommandResult::ack_msg("Queue cleared"))
    }

    async fn play(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let (status, position, needs_refresh, has_mpv) = {
            let state = self.inner.state.read().await;
            let needs_refresh = state
                .url_expires_at_ms
                .is_some_and(|expires| store::now_ms().saturating_add(120_000) >= expires);
            (
                state.status,
                state.position_ms,
                needs_refresh,
                state.mpv_available,
            )
        };
        if status == PlaybackStatus::Playing {
            return Ok(CommandResult::ack_msg("Already playing"));
        }
        if status == PlaybackStatus::Paused && has_mpv && !needs_refresh {
            let generation = self.next_generation();
            let mpv_guard = self.inner.mpv.lock().await;
            if let Some(mpv) = mpv_guard.as_ref() {
                mpv.pause(false).await?;
            }
            drop(mpv_guard);
            {
                let mut state = self.inner.state.write().await;
                state.status = PlaybackStatus::Playing;
                state.error = None;
                let _ = generation;
            }
            self.publish_playback().await;
            self.save_state(false).await;
            return Ok(CommandResult::ack());
        }
        let generation = self.next_generation();
        // A restored queue remembers its position; explicit Stop resets it to
        // zero, so using the stored position here is safe.
        self.load_current_locked(generation, position, false).await
    }

    async fn pause(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let status = self.inner.state.read().await.status;
        if status != PlaybackStatus::Playing {
            return Ok(CommandResult::ack_msg("Not playing"));
        }
        if let Some(mpv) = self.inner.mpv.lock().await.as_ref() {
            mpv.pause(true).await?;
        }
        {
            let mut state = self.inner.state.write().await;
            state.status = PlaybackStatus::Paused;
        }
        self.publish_playback().await;
        self.save_state(true).await;
        Ok(CommandResult::ack())
    }

    async fn stop(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        self.next_generation();
        {
            let mut state = self.inner.state.write().await;
            state.status = PlaybackStatus::Stopped;
            state.position_ms = 0;
            state.error = None;
        }
        self.publish_playback().await;
        self.save_state(true).await;
        self.shutdown_mpv().await;
        Ok(CommandResult::ack())
    }

    async fn previous_track(&self) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
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
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn next_track(&self, auto: bool) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        if let Err(e) = self.maybe_refill_personal_fm().await {
            self.fail_playback(format!("Could not refill Personal FM: {e}"))
                .await;
            return Err(e);
        }
        let advance = {
            let mut state = self.inner.state.write().await;
            if state.queue.mode() == PlayMode::Intelligence {
                Advance::Heartbeat
            } else {
                let mut rng = rand::thread_rng();
                state.queue.advance(auto, &mut rng)
            }
        };
        match advance {
            Advance::Index(_) => {
                {
                    let mut state = self.inner.state.write().await;
                    state.status = PlaybackStatus::Buffering;
                    state.clear_track_transient();
                    state.sync_current_metadata(true);
                }
                self.publish_queue().await;
                self.load_current_locked(generation, 0, false).await
            }
            Advance::End => {
                {
                    let mut state = self.inner.state.write().await;
                    state.status = PlaybackStatus::Stopped;
                    state.position_ms = 0;
                }
                self.publish_playback().await;
                self.save_state(true).await;
                self.shutdown_mpv().await;
                Ok(CommandResult::ack_msg("End of queue"))
            }
            Advance::Heartbeat => match self.heartbeat_locked(generation).await {
                Ok(result) => Ok(result),
                Err(e) => {
                    self.fail_playback(format!("Heartbeat failed: {e}")).await;
                    Err(e)
                }
            },
        }
    }

    async fn maybe_refill_personal_fm(&self) -> Result<(), ServiceError> {
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

    async fn heartbeat_locked(&self, generation: u64) -> Result<CommandResult, ServiceError> {
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
            return Ok(CommandResult::ack_msg("Superseded"));
        }
        let Some(song) = songs.into_iter().next() else {
            return Err(ServiceError::Unsupported(
                "Heartbeat returned no recommendations".into(),
            ));
        };
        {
            let mut state = self.inner.state.write().await;
            state.queue.insert_after_current(QueueItem {
                song,
                context: Some(context),
            });
            state.status = PlaybackStatus::Buffering;
            state.clear_track_transient();
            state.sync_current_metadata(true);
        }
        self.publish_queue().await;
        self.load_current_locked(generation, 0, false).await
    }

    async fn seek_relative(&self, offset_ms: i64) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
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
        }
        .min(duration.max(position));
        self.seek_to(target, status).await
    }

    async fn set_position(&self, position_ms: u64) -> Result<CommandResult, ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let (status, duration) = {
            let state = self.inner.state.read().await;
            (state.status, state.duration_ms)
        };
        if status == PlaybackStatus::Stopped {
            return Err(ServiceError::Invalid("nothing is playing".into()));
        }
        let target = position_ms.min(duration.max(position_ms));
        self.seek_to(target, status).await
    }

    async fn seek_to(
        &self,
        target_ms: u64,
        _status: PlaybackStatus,
    ) -> Result<CommandResult, ServiceError> {
        if let Some(mpv) = self.inner.mpv.lock().await.as_ref() {
            mpv.seek_ms(target_ms).await?;
        }
        {
            let mut state = self.inner.state.write().await;
            state.position_ms = target_ms;
        }
        self.publish_playback().await;
        Ok(CommandResult::ack())
    }

    async fn set_volume(&self, volume: f64) -> Result<CommandResult, ServiceError> {
        if !volume.is_finite() {
            return Err(ServiceError::Invalid("volume must be a number".into()));
        }
        let volume = volume.clamp(0.0, 1.0);
        if let Some(mpv) = self.inner.mpv.lock().await.as_ref() {
            mpv.set_volume(volume).await?;
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
        if let Some(mpv) = self.inner.mpv.lock().await.as_ref() {
            mpv.set_loop_file(mode == PlayMode::TrackLoop).await?;
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
        if let Some(mpv) = self.inner.mpv.lock().await.as_ref() {
            mpv.set_loop_file(applied == PlayMode::TrackLoop).await?;
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

    async fn load_current_locked(
        &self,
        generation: u64,
        start_ms: u64,
        paused: bool,
    ) -> Result<CommandResult, ServiceError> {
        let song = {
            let state = self.inner.state.read().await;
            state
                .queue
                .current()
                .map(|item| item.song.clone())
                .ok_or_else(|| ServiceError::Invalid("queue is empty".into()))?
        };
        {
            let mut state = self.inner.state.write().await;
            state.status = if paused {
                PlaybackStatus::Paused
            } else {
                PlaybackStatus::Buffering
            };
            state.position_ms = start_ms;
            state.duration_ms = song.duration_ms;
            state.clear_track_transient();
        }
        self.publish_playback().await;
        let stream = match self.inner.api.stream_url(song.id).await {
            Ok(stream) => stream,
            Err(e) => {
                self.fail_playback(e.user_message()).await;
                return Err(ServiceError::Api(e));
            }
        };
        if self.current_generation() != generation {
            return Ok(CommandResult::ack_msg("Superseded"));
        }
        {
            let state = self.inner.state.read().await;
            if state.queue.current().map(|i| i.song.id) != Some(song.id) {
                return Ok(CommandResult::ack_msg("Superseded"));
            }
        }
        if let Err(e) = self.ensure_mpv().await {
            self.fail_playback(e.to_string()).await;
            return Err(e);
        }
        let mode = self.inner.state.read().await.queue.mode();
        let guard = self.inner.mpv.lock().await;
        let Some(mpv) = guard.as_ref() else {
            drop(guard);
            let err = ServiceError::Mpv(MpvError::Exited("mpv is not running".into()));
            self.fail_playback(err.to_string()).await;
            return Err(err);
        };
        if let Err(e) = mpv.set_loop_file(mode == PlayMode::TrackLoop).await {
            drop(guard);
            let err = ServiceError::Mpv(e);
            self.fail_playback(err.to_string()).await;
            return Err(err);
        }
        if let Err(e) = mpv.loadfile(&stream.url, start_ms, paused).await {
            drop(guard);
            let err = ServiceError::Mpv(e);
            self.fail_playback(err.to_string()).await;
            return Err(err);
        }
        drop(guard);
        let now = store::now_ms();
        let warning = if stream.trial {
            Some("Only a preview/trial segment is available for this account.".to_string())
        } else {
            None
        };
        {
            let mut state = self.inner.state.write().await;
            state.status = if paused {
                PlaybackStatus::Paused
            } else {
                PlaybackStatus::Playing
            };
            state.position_ms = start_ms;
            state.duration_ms = state.duration_ms.max(song.duration_ms);
            state.error = None;
            state.warning = warning;
            state.url_expires_at_ms =
                Some(now.saturating_add(stream.expires_in_secs.saturating_mul(1000)));
            state.url_refreshed_at_ms = Some(now);
            state.retry_count = 0;
        }
        self.publish_playback().await;
        self.save_state(false).await;
        self.fetch_lyrics(song.id);
        Ok(CommandResult::ack())
    }

    async fn ensure_mpv(&self) -> Result<(), ServiceError> {
        if self.inner.mpv.lock().await.is_some() {
            return Ok(());
        }
        let volume = self.inner.state.read().await.volume;
        let options = MpvOptions::new(self.inner.paths.mpv_socket_file(), volume);
        let mpv = match MpvController::spawn(options).await {
            Ok(mpv) => mpv,
            Err(e) => {
                let mut state = self.inner.state.write().await;
                state.mpv_available = false;
                let warning = e.user_message();
                if !state.warnings.contains(&warning) {
                    state.warnings.push(warning);
                }
                drop(state);
                self.publish_playback().await;
                return Err(ServiceError::Mpv(e));
            }
        };
        let generation = self.inner.mpv_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut events = mpv.subscribe();
        let event_tx = self.inner.mpv_event_tx.clone();
        // Forwarding only. The event handler lives in one task created in
        // build(); this keeps the async type graph acyclic and therefore Send.
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if event_tx.send((generation, event)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "mpv event receiver lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *self.inner.mpv.lock().await = Some(mpv);
        {
            let mut state = self.inner.state.write().await;
            state.mpv_available = true;
            state
                .warnings
                .retain(|w| !w.starts_with("YoruPlay requires mpv"));
        }
        Ok(())
    }

    async fn shutdown_mpv(&self) {
        // Invalidate all events from the old process before/while it exits.
        self.inner.mpv_generation.fetch_add(1, Ordering::SeqCst);
        let controller = self.inner.mpv.lock().await.take();
        if let Some(controller) = controller {
            controller.shutdown().await;
        }
    }

    async fn handle_mpv_event(&self, event: MpvEvent) {
        if self.is_shutting_down() {
            return;
        }
        match event {
            MpvEvent::FileLoaded => {
                let changed = {
                    let mut state = self.inner.state.write().await;
                    if state.status == PlaybackStatus::Buffering {
                        state.status = PlaybackStatus::Playing;
                        true
                    } else {
                        false
                    }
                };
                if changed {
                    self.publish_playback().await;
                }
            }
            MpvEvent::EndFile { reason, error } => match reason.as_str() {
                "eof" => {
                    let status = self.inner.state.read().await.status;
                    if status == PlaybackStatus::Stopped {
                        return;
                    }
                    if let Err(e) = self.next_track(true).await {
                        let info = e.into_error_info();
                        self.notice(NoticeLevel::Error, info.message);
                    }
                }
                "error" => {
                    let error = error.unwrap_or_else(|| "unknown stream error".into());
                    if let Err(e) = self.handle_stream_error(error).await {
                        let info = e.into_error_info();
                        self.notice(NoticeLevel::Error, info.message);
                    }
                }
                _ => {}
            },
            MpvEvent::Property { name, value } => match name.as_str() {
                "time-pos" => {
                    if let Some(seconds) = value.as_f64() {
                        let mut state = self.inner.state.write().await;
                        state.position_ms = (seconds.max(0.0) * 1000.0) as u64;
                        if state.duration_ms > 0 {
                            state.position_ms = state.position_ms.min(state.duration_ms);
                        }
                    }
                }
                "duration" => {
                    if let Some(seconds) = value.as_f64() {
                        let mut state = self.inner.state.write().await;
                        let duration = (seconds.max(0.0) * 1000.0) as u64;
                        if duration > 0 {
                            state.duration_ms = duration;
                        }
                    }
                }
                "pause" => {
                    if let Some(paused) = value.as_bool() {
                        let changed = {
                            let mut state = self.inner.state.write().await;
                            match (state.status, paused) {
                                (PlaybackStatus::Playing, true) => {
                                    state.status = PlaybackStatus::Paused;
                                    true
                                }
                                (PlaybackStatus::Paused, false) => {
                                    state.status = PlaybackStatus::Playing;
                                    true
                                }
                                _ => false,
                            }
                        };
                        if changed {
                            self.publish_playback().await;
                        }
                    }
                }
                _ => {}
            },
            MpvEvent::Exited { error } => self.handle_mpv_exit(error).await,
        }
    }

    async fn handle_stream_error(&self, error: String) -> Result<(), ServiceError> {
        let _lock = self.inner.play_lock.lock().await;
        let generation = self.next_generation();
        let (song, position, retry) = {
            let state = self.inner.state.read().await;
            let Some(song) = state.queue.current().map(|i| i.song.clone()) else {
                return Ok(());
            };
            (song, state.position_ms, state.retry_count)
        };
        if retry >= 1 {
            let mut state = self.inner.state.write().await;
            state.set_error(format!(
                "Playback failed after refreshing the stream URL: {error}"
            ));
            drop(state);
            self.publish_playback().await;
            self.save_state(true).await;
            self.shutdown_mpv().await;
            return Err(ServiceError::Unsupported(
                "the audio stream could not be opened".into(),
            ));
        }
        {
            let mut state = self.inner.state.write().await;
            state.retry_count = 1;
            state.warning = Some("Stream URL failed; refreshing it once.".into());
        }
        self.publish_playback().await;
        let stream = self.inner.api.stream_url(song.id).await?;
        if self.current_generation() != generation {
            return Ok(());
        }
        self.ensure_mpv().await?;
        let mode = self.inner.state.read().await.queue.mode();
        let guard = self.inner.mpv.lock().await;
        let Some(mpv) = guard.as_ref() else {
            return Err(ServiceError::Mpv(MpvError::Exited(
                "mpv is not running".into(),
            )));
        };
        mpv.set_loop_file(mode == PlayMode::TrackLoop).await?;
        mpv.loadfile(&stream.url, position, false).await?;
        drop(guard);
        let now = store::now_ms();
        {
            let mut state = self.inner.state.write().await;
            state.status = PlaybackStatus::Playing;
            state.error = None;
            state.position_ms = position;
            state.url_expires_at_ms =
                Some(now.saturating_add(stream.expires_in_secs.saturating_mul(1000)));
            state.url_refreshed_at_ms = Some(now);
            state.warning = if stream.trial {
                Some("Only a preview/trial segment is available for this account.".into())
            } else {
                Some("Stream URL was refreshed after a playback error.".into())
            };
        }
        self.publish_playback().await;
        Ok(())
    }

    async fn handle_mpv_exit(&self, error: Option<String>) {
        if self.is_shutting_down() {
            return;
        }
        {
            let mut state = self.inner.state.write().await;
            state.mpv_available = false;
            state.status = PlaybackStatus::Stopped;
            state.position_ms = 0;
            state.error = Some(match error {
                Some(error) => format!("mpv exited unexpectedly: {error}"),
                None => "mpv exited unexpectedly.".into(),
            });
        }
        let controller = self.inner.mpv.lock().await.take();
        drop(controller);
        self.publish_playback().await;
        self.notice(
            NoticeLevel::Error,
            "mpv exited; playback stopped. Press play to restart it.",
        );
    }

    fn fetch_lyrics(&self, song_id: u64) {
        let daemon = self.clone();
        tokio::spawn(async move {
            let key = format!("lyrics_{song_id}");
            let ttl = daemon
                .inner
                .config
                .cache
                .lyrics_ttl_hours
                .saturating_mul(3600);
            if let Some(cached) = daemon.inner.cache.get::<Lyrics>(&key, ttl) {
                if daemon.current_song_id().await == Some(song_id) {
                    let plain = plain_lyrics(&cached.lrc);
                    let mut state = daemon.inner.state.write().await;
                    if state.queue.current().map(|i| i.song.id) == Some(song_id) {
                        state.lyrics = if plain.is_empty() { None } else { Some(plain) };
                    }
                    drop(state);
                    daemon.publish_playback().await;
                }
                return;
            }
            match daemon.inner.api.lyrics(song_id).await {
                Ok(lyrics) => {
                    if let Err(e) = daemon.inner.cache.put(&key, &lyrics) {
                        tracing::debug!(error = %e, "could not cache lyrics");
                    }
                    let plain = plain_lyrics(&lyrics.lrc);
                    if daemon.current_song_id().await == Some(song_id) {
                        let mut state = daemon.inner.state.write().await;
                        if state.queue.current().map(|i| i.song.id) == Some(song_id) {
                            state.lyrics = if plain.is_empty() { None } else { Some(plain) };
                        }
                        drop(state);
                        daemon.publish_playback().await;
                    }
                }
                Err(e) => {
                    tracing::debug!(song_id, error = %e, "lyrics unavailable");
                    let mut state = daemon.inner.state.write().await;
                    if state.queue.current().map(|i| i.song.id) == Some(song_id) {
                        state.lyrics = None;
                    }
                }
            }
        });
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
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let _guard = self.inner.play_lock.lock().await;
        if let Some(mpv) = self.inner.mpv.lock().await.as_ref() {
            let _ = mpv.pause(true).await;
        }
        self.save_state(true).await;
        let controller = self.inner.mpv.lock().await.take();
        if let Some(controller) = controller {
            controller.shutdown().await;
        }
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
