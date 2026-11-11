use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use reqwest::header::{CONTENT_TYPE, COOKIE, REFERER, USER_AGENT};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use yoruplay_protocol::{
    AlbumSummary, ArtistSummary, AuthState, PlaylistKind, PlaylistSummary, PlaylistTracks, Quality,
    SearchItems, SearchKind, SearchPage, Song, SongsView, UserProfile,
};

use crate::crypto::{eapi_encrypt, random_weapi_secret, weapi_encrypt};
use crate::raw::{
    AccountResponse, AlbumResponse, ArtistSongsResponse, DailySongsResponse, FmResponse,
    LikeListResponse, LyricResponse, PlaylistDetailResponse, RawPlaylist, RawSong,
    RecommendResourceResponse, SearchResponse, SongDetailResponse, StreamResponse,
    UserPlaylistsResponse,
};
use crate::session::{now_ms, SessionData};
use crate::ApiError;

const WEAPI_HOST: &str = "https://music.163.com";
const EAPI_HOST: &str = "https://interfacepc.music.163.com";
const WEAPI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36 Edg/124.0.0.0";
const EAPI_UA: &str = "NeteaseMusic 9.0.90/5038 (iPhone; iOS 16.2; zh_CN)";
const MAX_SONG_DETAIL_CHUNK: usize = 500;
const MAX_PAGE_LIMIT: usize = 1_000;
const PLAYLIST_CACHE_TTL: Duration = Duration::from_secs(300);
const LIKED_PLAYLIST_CACHE_TTL: Duration = Duration::from_secs(600);
const SONG_DETAIL_MIN_INTERVAL: Duration = Duration::from_millis(250);
const MAX_CACHED_PLAYLISTS: usize = 8;
const MAX_CACHED_PLAYLIST_TRACKS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CryptoMode {
    Weapi,
    Eapi,
}

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub weapi: String,
    pub eapi: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            weapi: WEAPI_HOST.into(),
            eapi: EAPI_HOST.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamUrl {
    pub url: String,
    pub expires_in_secs: u64,
    pub bitrate: u64,
    pub format: Option<String>,
    pub size: u64,
    pub md5: Option<String>,
    pub level: Option<String>,
    pub trial: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Lyrics {
    pub lrc: String,
    pub translated: Option<String>,
}

type PlaylistCache = HashMap<u64, (Instant, Arc<RawPlaylist>)>;

#[derive(Clone)]
pub struct NeteaseClient {
    http: reqwest::Client,
    endpoints: Endpoints,
    quality: Quality,
    session: Arc<RwLock<SessionData>>,
    startup_warning: Option<String>,
    playlist_cache: Arc<RwLock<PlaylistCache>>,
    liked_playlist_cache: Arc<RwLock<Option<(Instant, u64)>>>,
    song_detail_gate: Arc<tokio::sync::Mutex<Option<Instant>>>,
}

impl NeteaseClient {
    pub fn new(
        config: &yoruplay_protocol::ApiConfig,
        auth_path: Option<PathBuf>,
    ) -> Result<Self, ApiError> {
        Self::with_endpoints(config, auth_path, Endpoints::default())
    }

    pub fn with_endpoints(
        config: &yoruplay_protocol::ApiConfig,
        auth_path: Option<PathBuf>,
        endpoints: Endpoints,
    ) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs.clamp(1, 300)))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let (session, startup_warning) = match SessionData::load(auth_path.clone()) {
            Ok(session) => (session, None),
            Err(e) => {
                let warning = format!(
                    "Ignoring unreadable session file: {}. Log in again.",
                    e.user_message()
                );
                (SessionData::anonymous(auth_path), Some(warning))
            }
        };
        if startup_warning.is_none() && !session.logged_in() {
            if let Err(e) = session.persist_device() {
                tracing::warn!(error = %e, "could not persist the device identity");
            }
        }
        Ok(Self {
            http,
            endpoints,
            quality: config.quality,
            session: Arc::new(RwLock::new(session)),
            startup_warning,
            playlist_cache: Arc::new(RwLock::new(HashMap::new())),
            liked_playlist_cache: Arc::new(RwLock::new(None)),
            song_detail_gate: Arc::new(tokio::sync::Mutex::new(None)),
        })
    }

    pub fn startup_warning(&self) -> Option<&str> {
        self.startup_warning.as_deref()
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// Current MUSIC_U value, used only to detect a forced account switch.
    pub async fn current_music_u(&self) -> Option<String> {
        self.session
            .read()
            .await
            .cookie("MUSIC_U")
            .map(str::to_owned)
    }

    pub async fn auth_state(&self) -> AuthState {
        self.session.read().await.auth_state()
    }

    pub async fn is_logged_in(&self) -> bool {
        let s = self.session.read().await;
        s.logged_in() && !s.expired
    }

    pub async fn require_login(&self) -> Result<(), ApiError> {
        let s = self.session.read().await;
        if s.expired {
            return Err(ApiError::SessionExpired);
        }
        if !s.logged_in() {
            return Err(ApiError::AuthRequired);
        }
        Ok(())
    }

    async fn post_json(
        &self,
        mode: CryptoMode,
        path: &str,
        payload: Value,
    ) -> Result<Value, ApiError> {
        self.post_response(mode, path, payload).await
    }

    async fn post_response<T: serde::de::DeserializeOwned>(
        &self,
        mode: CryptoMode,
        path: &str,
        payload: Value,
    ) -> Result<T, ApiError> {
        let request_id = format!("{}_{:04}", now_ms(), rand::thread_rng().gen_range(0..1000));
        let endpoint_path = path
            .strip_prefix("/api/")
            .ok_or_else(|| ApiError::InvalidArgument(format!("bad endpoint path {path}")))?;
        let (url, form, cookie_header, user_agent, referer) = {
            let session = self.session.read().await;
            match mode {
                CryptoMode::Eapi => {
                    let mut object = payload;
                    let object_map = object.as_object_mut().ok_or_else(|| {
                        ApiError::InvalidArgument("eapi payload must be a JSON object".into())
                    })?;
                    let header = session.eapi_header(&request_id);
                    object_map.insert(
                        "header".into(),
                        Value::Object(
                            header
                                .into_iter()
                                .map(|(k, v)| (k, Value::String(v)))
                                .collect(),
                        ),
                    );
                    let text = serde_json::to_string(&object).map_err(|e| ApiError::Decode {
                        endpoint: path.into(),
                        message: e.to_string(),
                    })?;
                    let params = eapi_encrypt(path, &text);
                    (
                        format!(
                            "{}/eapi/{endpoint_path}",
                            self.endpoints.eapi.trim_end_matches('/')
                        ),
                        vec![("params".to_string(), params)],
                        session.cookie_header(),
                        EAPI_UA,
                        None,
                    )
                }
                CryptoMode::Weapi => {
                    let mut object = payload;
                    let object_map = object.as_object_mut().ok_or_else(|| {
                        ApiError::InvalidArgument("weapi payload must be a JSON object".into())
                    })?;
                    object_map.insert("csrf_token".into(), Value::String(session.csrf()));
                    let text = serde_json::to_string(&object).map_err(|e| ApiError::Decode {
                        endpoint: path.into(),
                        message: e.to_string(),
                    })?;
                    let secret = random_weapi_secret();
                    let (params, enc_sec_key) = weapi_encrypt(&text, &secret)?;
                    (
                        format!(
                            "{}/weapi/{endpoint_path}",
                            self.endpoints.weapi.trim_end_matches('/')
                        ),
                        vec![
                            ("params".to_string(), params),
                            ("encSecKey".to_string(), enc_sec_key),
                        ],
                        session.cookie_header(),
                        WEAPI_UA,
                        Some(self.endpoints.weapi.as_str()),
                    )
                }
            }
        };
        tracing::debug!(endpoint = path, ?mode, "NetEase request");
        let mut builder = self
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(COOKIE, cookie_header)
            .header(USER_AGENT, user_agent);
        if let Some(referer) = referer {
            builder = builder.header(REFERER, referer);
        }
        let mut response = builder.form(&form).send().await.map_err(|e| {
            if e.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::Network(e.to_string())
            }
        })?;
        let status = response.status();
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_string)
            .collect();
        const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
        let mut body =
            Vec::with_capacity(response.content_length().unwrap_or(8192).min(65536) as usize);
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            if e.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::Network(e.to_string())
            }
        })? {
            if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                return Err(ApiError::Decode {
                    endpoint: path.into(),
                    message: "response exceeds 16 MiB metadata limit".into(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        if !set_cookies.is_empty() {
            let mut session = self.session.write().await;
            session.merge_cookies(set_cookies.iter().map(String::as_str));
        }
        let parsed = serde_json::from_slice::<T>(&body);
        match parsed {
            Ok(value) => {
                if !status.is_success() {
                    tracing::debug!(endpoint = path, %status, "NetEase returned a non-2xx JSON response");
                }
                Ok(value)
            }
            Err(e) => {
                if status.is_success() {
                    Err(ApiError::Decode {
                        endpoint: path.into(),
                        message: format!("invalid JSON: {e}"),
                    })
                } else {
                    Err(ApiError::HttpStatus {
                        status: status.as_u16(),
                        endpoint: path.into(),
                    })
                }
            }
        }
    }

    async fn post_ok(
        &self,
        mode: CryptoMode,
        path: &str,
        payload: Value,
    ) -> Result<Value, ApiError> {
        self.post_ok_inner(mode, path, payload, true).await
    }

    async fn post_ok_inner(
        &self,
        mode: CryptoMode,
        path: &str,
        payload: Value,
        destructive_auth_failure: bool,
    ) -> Result<Value, ApiError> {
        let value = self.post_json(mode, path, payload).await?;
        let code = json_i64(&value, "code").unwrap_or(200);
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("");
        self.check_response_code(code, message, path, destructive_auth_failure)
            .await?;
        Ok(value)
    }

    async fn check_response_code(
        &self,
        code: i64,
        message: &str,
        path: &str,
        destructive_auth_failure: bool,
    ) -> Result<(), ApiError> {
        if code == 200 {
            return Ok(());
        }
        if code == 301 {
            let mut session = self.session.write().await;
            let was_logged_in = session.logged_in();
            if destructive_auth_failure && was_logged_in {
                session.mark_expired();
                if let Err(e) = session.remove_persisted() {
                    tracing::warn!(error = %e, "failed to remove expired session file");
                }
            }
            return Err(if was_logged_in {
                ApiError::SessionExpired
            } else {
                ApiError::AuthRequired
            });
        }
        Err(ApiError::Api {
            code,
            message: message.to_string(),
            endpoint: path.into(),
        })
    }

    async fn fetch_account(&self) -> Result<Option<UserProfile>, ApiError> {
        let value = self
            .post_ok_inner(
                CryptoMode::Weapi,
                "/api/nuser/account/get",
                json!({}),
                false,
            )
            .await?;
        let response: AccountResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/nuser/account/get".into(),
                message: e.to_string(),
            })?;
        Ok(response.into_profile())
    }

    /// Fetch the account profile and update/persist the session when valid.
    pub async fn account(&self) -> Result<Option<UserProfile>, ApiError> {
        let profile = self.fetch_account().await?;
        let mut session = self.session.write().await;
        if let Some(profile) = profile.clone() {
            let was_logged_in = session.logged_in();
            let changed = session.profile.as_ref() != Some(&profile);
            session.set_profile(profile.clone());
            if !was_logged_in || changed {
                if let Err(e) = session.persist() {
                    tracing::warn!(error = %e, "failed to persist account session");
                }
            }
            Ok(Some(profile))
        } else {
            if session.logged_in() {
                session.mark_expired();
                if let Err(e) = session.remove_persisted() {
                    tracing::warn!(error = %e, "failed to remove expired session file");
                }
            }
            Ok(None)
        }
    }

    /// Validate the loaded session at startup without asking the user again.
    pub async fn validate_session(&self) -> Result<AuthState, ApiError> {
        if self.session.read().await.logged_in() {
            let _ = self.account().await?;
        }
        Ok(self.auth_state().await)
    }

    pub async fn import_cookie(&self, cookie: &str) -> Result<UserProfile, ApiError> {
        let old = self.session.read().await.clone();
        {
            let mut session = self.session.write().await;
            session.import_cookie_string(cookie)?;
        }
        let profile = match self.fetch_account().await {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                *self.session.write().await = old;
                return Err(ApiError::InvalidCookie(
                    "the server did not accept this cookie (no account)".into(),
                ));
            }
            Err(e) => {
                *self.session.write().await = old;
                return Err(e);
            }
        };
        let mut session = self.session.write().await;
        session.set_profile(profile.clone());
        if let Err(e) = session.persist() {
            tracing::warn!(error = %e, "failed to persist imported cookie session");
        }
        drop(session);
        self.invalidate_account_caches().await;
        Ok(profile)
    }

    pub async fn logout(&self) -> Result<AuthState, ApiError> {
        let mut session = self.session.write().await;
        session.clear_auth();
        session.remove_persisted()?;
        let state = session.auth_state();
        drop(session);
        self.invalidate_account_caches().await;
        Ok(state)
    }

    async fn invalidate_account_caches(&self) {
        *self.liked_playlist_cache.write().await = None;
        self.playlist_cache.write().await.clear();
    }

    async fn current_uid(&self) -> Result<u64, ApiError> {
        if let Some(profile) = self.session.read().await.profile.clone() {
            return Ok(profile.user_id);
        }
        match self.account().await? {
            Some(profile) => Ok(profile.user_id),
            None => {
                if self.session.read().await.expired {
                    Err(ApiError::SessionExpired)
                } else {
                    Err(ApiError::AuthRequired)
                }
            }
        }
    }

    async fn all_playlists_raw(&self) -> Result<Vec<RawPlaylist>, ApiError> {
        self.require_login().await?;
        let uid = self.current_uid().await?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut offset = 0usize;
        for _ in 0..100 {
            let value = self
                .post_ok(
                    CryptoMode::Weapi,
                    "/api/user/playlist",
                    json!({
                        "uid": uid,
                        "limit": 1000,
                        "offset": offset,
                        "includeVideo": true,
                    }),
                )
                .await?;
            let response: UserPlaylistsResponse =
                serde_json::from_value(value).map_err(|e| ApiError::Decode {
                    endpoint: "/api/user/playlist".into(),
                    message: e.to_string(),
                })?;
            let count = response.playlist.len();
            for playlist in response.playlist {
                if seen.insert(playlist.id) {
                    out.push(playlist);
                }
            }
            if !response.more || count == 0 {
                break;
            }
            offset += count;
        }
        if let Some(id) = find_liked_playlist(&out) {
            *self.liked_playlist_cache.write().await = Some((Instant::now(), id));
        }
        Ok(out)
    }

    async fn liked_playlist(&self) -> Result<u64, ApiError> {
        if let Some((at, id)) = *self.liked_playlist_cache.read().await {
            if at.elapsed() < LIKED_PLAYLIST_CACHE_TTL {
                return Ok(id);
            }
        }
        let all = self.all_playlists_raw().await?;
        find_liked_playlist(&all).ok_or_else(|| ApiError::NotFound("liked songs playlist".into()))
    }

    pub async fn playlists(&self, kind: PlaylistKind) -> Result<Vec<PlaylistSummary>, ApiError> {
        let raw = self.all_playlists_raw().await?;
        Ok(raw
            .into_iter()
            .filter(|p| match kind {
                PlaylistKind::Created => !p.subscribed.unwrap_or(false) && p.special_type != 5,
                PlaylistKind::Subscribed => p.subscribed.unwrap_or(false),
            })
            .map(RawPlaylist::into_summary)
            .collect())
    }

    async fn playlist_cache_get(&self, id: u64) -> Option<Arc<RawPlaylist>> {
        let cache = self.playlist_cache.read().await;
        cache
            .get(&id)
            .filter(|(at, _)| at.elapsed() < PLAYLIST_CACHE_TTL)
            .map(|(_, p)| p.clone())
    }

    async fn playlist_detail_raw(&self, id: u64) -> Result<Arc<RawPlaylist>, ApiError> {
        if let Some(cached) = self.playlist_cache_get(id).await {
            return Ok(cached);
        }
        let response: PlaylistDetailResponse = self
            .post_response(
                CryptoMode::Eapi,
                "/api/v6/playlist/detail",
                json!({ "id": id, "n": 100000, "s": 8 }),
            )
            .await?;
        self.check_response_code(response.code, "", "/api/v6/playlist/detail", true)
            .await?;
        let playlist = response
            .playlist
            .ok_or_else(|| ApiError::NotFound(format!("playlist {id}")))?;
        let playlist = Arc::new(playlist);
        if playlist
            .track_ids
            .len()
            .saturating_add(playlist.tracks.len())
            <= MAX_CACHED_PLAYLIST_TRACKS
        {
            let mut cache = self.playlist_cache.write().await;
            cache.retain(|_, (at, cached)| {
                at.elapsed() < PLAYLIST_CACHE_TTL
                    && cached.track_ids.len().saturating_add(cached.tracks.len())
                        <= MAX_CACHED_PLAYLIST_TRACKS
            });
            while cache.len() >= MAX_CACHED_PLAYLISTS {
                let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(id, _)| *id)
                else {
                    break;
                };
                cache.remove(&oldest);
            }
            cache.insert(id, (Instant::now(), playlist.clone()));
        }
        Ok(playlist)
    }

    pub async fn playlist_tracks(
        &self,
        id: u64,
        offset: usize,
        limit: usize,
    ) -> Result<PlaylistTracks, ApiError> {
        let limit = validate_page_limit(limit)?;
        let raw = self.playlist_detail_raw(id).await?;
        let ids = raw.ordered_track_ids();
        let total = ids.len();
        let (offset, end) = page_bounds(total, offset, limit);
        let slice = &ids[offset..end];
        let songs = self.song_details(slice).await?;
        let summary = raw.summary();
        Ok(PlaylistTracks {
            playlist: summary,
            songs,
            offset,
            total,
            has_more: end < total,
            next_offset: end,
        })
    }

    pub async fn playlist_tracks_all(&self, id: u64) -> Result<PlaylistTracks, ApiError> {
        let raw = self.playlist_detail_raw(id).await?;
        let ids = raw.ordered_track_ids();
        let total = ids.len();
        let songs = self.song_details(&ids).await?;
        let summary = raw.summary();
        Ok(PlaylistTracks {
            playlist: summary,
            songs,
            offset: 0,
            total,
            has_more: false,
            next_offset: total,
        })
    }

    pub async fn liked_tracks(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<PlaylistTracks, ApiError> {
        self.require_login().await?;
        let limit = validate_page_limit(limit)?;
        match self.liked_playlist().await {
            Ok(id) => match self.playlist_tracks(id, offset, limit).await {
                Err(ApiError::NotFound(_)) => {
                    *self.liked_playlist_cache.write().await = None;
                    self.liked_tracks_fallback(offset, limit).await
                }
                other => other,
            },
            Err(ApiError::NotFound(_)) => self.liked_tracks_fallback(offset, limit).await,
            Err(e) => Err(e),
        }
    }

    async fn liked_tracks_fallback(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<PlaylistTracks, ApiError> {
        let uid = self.current_uid().await?;
        let value = self
            .post_ok(
                CryptoMode::Eapi,
                "/api/song/like/get",
                json!({ "uid": uid }),
            )
            .await?;
        let response: LikeListResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/song/like/get".into(),
                message: e.to_string(),
            })?;
        let mut seen = HashSet::new();
        let ids: Vec<u64> = response
            .ids
            .into_iter()
            .filter(|id| *id != 0 && seen.insert(*id))
            .collect();
        let total = ids.len();
        let (offset, end) = page_bounds(total, offset, limit);
        let songs = self.song_details(&ids[offset..end]).await?;
        let playlist = PlaylistSummary {
            id: 0,
            name: "Liked Songs".into(),
            cover_url: None,
            track_count: total as u64,
            subscribed: false,
            creator_id: 0,
            creator_name: String::new(),
            special_type: 5,
            description: Some("Liked songs returned by NetEase (unordered fallback).".into()),
            play_count: 0,
            update_time_ms: 0,
        };
        Ok(PlaylistTracks {
            playlist,
            songs,
            offset,
            total,
            has_more: end < total,
            next_offset: end,
        })
    }

    pub async fn song_details(&self, ids: &[u64]) -> Result<Vec<Song>, ApiError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            if *id != 0 && seen.insert(*id) {
                unique.push(*id);
            }
        }
        let mut songs = Vec::with_capacity(unique.len());
        for chunk in unique.chunks(MAX_SONG_DETAIL_CHUNK) {
            self.wait_song_detail_turn().await;
            let c = format!(
                "[{}]",
                chunk
                    .iter()
                    .map(|id| format!("{{\"id\":{id}}}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let response: SongDetailResponse = self
                .post_response(CryptoMode::Eapi, "/api/v3/song/detail", json!({ "c": c }))
                .await?;
            self.check_response_code(response.code, "", "/api/v3/song/detail", true)
                .await?;
            let mut by_id = HashMap::with_capacity(chunk.len());
            for raw in response.songs {
                let song = raw.into_song();
                by_id.insert(song.id, song);
            }
            songs.extend(chunk.iter().filter_map(|id| by_id.remove(id)));
        }
        Ok(songs)
    }

    async fn wait_song_detail_turn(&self) {
        let mut last = self.song_detail_gate.lock().await;
        if let Some(last_at) = *last {
            let next = last_at + SONG_DETAIL_MIN_INTERVAL;
            let now = Instant::now();
            if next > now {
                tokio::time::sleep(next - now).await;
            }
        }
        *last = Some(Instant::now());
    }

    pub async fn daily_songs(&self) -> Result<Vec<Song>, ApiError> {
        self.require_login().await?;
        let value = self
            .post_ok(
                CryptoMode::Weapi,
                "/api/v3/discovery/recommend/songs",
                json!({}),
            )
            .await?;
        let response: DailySongsResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/v3/discovery/recommend/songs".into(),
                message: e.to_string(),
            })?;
        let songs: Vec<Song> = response
            .data
            .map(|d| d.daily_songs)
            .unwrap_or_default()
            .into_iter()
            .map(RawSong::into_song)
            .collect();
        if songs.is_empty() {
            return Err(ApiError::NotFound("daily recommended songs".into()));
        }
        Ok(songs)
    }

    pub async fn daily_playlists(&self) -> Result<Vec<PlaylistSummary>, ApiError> {
        self.require_login().await?;
        let value = self
            .post_ok(
                CryptoMode::Weapi,
                "/api/v1/discovery/recommend/resource",
                json!({}),
            )
            .await?;
        let response: RecommendResourceResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/v1/discovery/recommend/resource".into(),
                message: e.to_string(),
            })?;
        let playlists: Vec<PlaylistSummary> = response
            .recommend
            .unwrap_or_default()
            .into_iter()
            .map(RawPlaylist::into_summary)
            .collect();
        if playlists.is_empty() {
            return Err(ApiError::NotFound("daily recommended playlists".into()));
        }
        Ok(playlists)
    }

    pub async fn personal_fm(&self) -> Result<Vec<Song>, ApiError> {
        self.require_login().await?;
        let value = self
            .post_ok(CryptoMode::Weapi, "/api/v1/radio/get", json!({}))
            .await?;
        let response: FmResponse = serde_json::from_value(value).map_err(|e| ApiError::Decode {
            endpoint: "/api/v1/radio/get".into(),
            message: e.to_string(),
        })?;
        let songs: Vec<Song> = response.data.into_iter().map(RawSong::into_song).collect();
        if songs.is_empty() {
            return Err(ApiError::NotFound("personal FM recommendations".into()));
        }
        Ok(songs)
    }

    /// NetEase has no documented stable "private radar" endpoint in the
    /// upstream project. The radar playlist is delivered through the account's
    /// daily recommended playlists, so discover it by name instead of
    /// hard-coding a playlist id that is not verified for this account.
    pub async fn radar(&self) -> Result<PlaylistTracks, ApiError> {
        self.require_login().await?;
        let playlists = self.daily_playlists().await?;
        let found = playlists.into_iter().find(|p| {
            let name = p.name.to_lowercase();
            name.contains("私人雷达")
                || name.contains("private radar")
                || name.contains("private_radar")
        });
        let Some(found) = found else {
            return Err(ApiError::Unsupported(
                "Private Radar is not present in this account's daily recommended playlists".into(),
            ));
        };
        self.playlist_tracks_all(found.id).await
    }

    pub async fn recent_songs(&self) -> Result<SongsView, ApiError> {
        self.require_login().await?;
        let uid = self.current_uid().await?;
        let primary = self
            .post_ok(
                CryptoMode::Weapi,
                "/api/play-record/song/list",
                json!({ "limit": 100 }),
            )
            .await;
        let mut entries = Vec::new();
        if let Ok(value) = primary {
            entries = extract_recent(&value);
        }
        if entries.is_empty() {
            let value = self
                .post_ok(
                    CryptoMode::Weapi,
                    "/api/v1/play/record",
                    json!({ "uid": uid, "type": 0 }),
                )
                .await?;
            entries = extract_recent(&value);
        }
        let mut seen = HashSet::new();
        entries.retain(|entry| seen.insert(entry.id));
        entries.truncate(100);
        let missing: Vec<u64> = entries
            .iter()
            .filter(|e| e.song.is_none())
            .map(|e| e.id)
            .collect();
        let fetched = self.song_details(&missing).await?;
        let mut fetched: HashMap<u64, Song> = fetched.into_iter().map(|s| (s.id, s)).collect();
        let mut songs = Vec::new();
        for entry in entries {
            if let Some(mut song) = entry.song.map(RawSong::into_song) {
                if song.name.starts_with("Track ") {
                    if let Some(full) = fetched.remove(&song.id) {
                        song = full;
                    }
                }
                songs.push(song);
            } else if let Some(song) = fetched.remove(&entry.id) {
                songs.push(song);
            }
        }
        Ok(SongsView {
            title: "Recently played (NetEase server)".into(),
            description: Some("Server-side listening history, not local YoruPlay history.".into()),
            songs,
            context: None,
        })
    }

    pub async fn search(
        &self,
        query: &str,
        kind: SearchKind,
        offset: usize,
        limit: usize,
    ) -> Result<SearchPage, ApiError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(ApiError::InvalidArgument("search query is empty".into()));
        }
        let limit = limit.clamp(1, 100);
        let value = self
            .post_ok(
                CryptoMode::Eapi,
                "/api/cloudsearch/pc",
                json!({
                    "s": query,
                    "type": kind.upstream_type(),
                    "limit": limit,
                    "offset": offset,
                    "total": true,
                }),
            )
            .await?;
        let response: SearchResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/cloudsearch/pc".into(),
                message: e.to_string(),
            })?;
        let r = response.result;
        let (items, total) = match kind {
            SearchKind::Songs => {
                let items: Vec<Song> = r.songs.into_iter().map(RawSong::into_song).collect();
                let total = r.song_count.max(items.len());
                (SearchItems::Songs(items), total)
            }
            SearchKind::Artists => {
                let items: Vec<ArtistSummary> =
                    r.artists.into_iter().map(|a| a.into_summary()).collect();
                let total = r.artist_count.max(items.len());
                (SearchItems::Artists(items), total)
            }
            SearchKind::Albums => {
                let items: Vec<AlbumSummary> =
                    r.albums.into_iter().map(|a| a.into_summary()).collect();
                let total = r.album_count.max(items.len());
                (SearchItems::Albums(items), total)
            }
            SearchKind::Playlists => {
                let items: Vec<PlaylistSummary> =
                    r.playlists.into_iter().map(|p| p.into_summary()).collect();
                let total = r.playlist_count.max(items.len());
                (SearchItems::Playlists(items), total)
            }
        };
        let has_more = offset.saturating_add(items.len()) < total;
        Ok(SearchPage {
            query: query.into(),
            offset,
            limit,
            total,
            has_more,
            items,
        })
    }

    pub async fn artist_songs(
        &self,
        id: u64,
        offset: usize,
        limit: usize,
    ) -> Result<SongsView, ApiError> {
        let limit = limit.clamp(1, 500);
        let value = self
            .post_ok(
                CryptoMode::Eapi,
                "/api/v1/artist/songs",
                json!({
                    "id": id,
                    "private_cloud": "true",
                    "work_type": 1,
                    "order": "hot",
                    "offset": offset,
                    "limit": limit,
                }),
            )
            .await?;
        let response: ArtistSongsResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/v1/artist/songs".into(),
                message: e.to_string(),
            })?;
        let songs: Vec<Song> = response.songs.into_iter().map(RawSong::into_song).collect();
        if songs.is_empty() {
            return Err(ApiError::NotFound("artist songs".into()));
        }
        Ok(SongsView {
            title: "Artist songs".into(),
            description: None,
            songs,
            context: None,
        })
    }

    pub async fn album_songs(&self, id: u64) -> Result<SongsView, ApiError> {
        let value = self
            .post_ok(CryptoMode::Weapi, &format!("/api/v1/album/{id}"), json!({}))
            .await?;
        let response: AlbumResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: format!("/api/v1/album/{id}"),
                message: e.to_string(),
            })?;
        let album = response.album;
        let mut songs: Vec<Song> = response.songs.into_iter().map(RawSong::into_song).collect();
        if songs.is_empty() {
            if let Some(album) = &album {
                songs = album
                    .songs
                    .iter()
                    .cloned()
                    .map(RawSong::into_song)
                    .collect();
            }
        }
        if songs.is_empty() {
            return Err(ApiError::NotFound("album songs".into()));
        }
        let title = album
            .as_ref()
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("Album {id}"));
        let description = album
            .as_ref()
            .and_then(|a| a.artists.as_ref().and_then(|v| v.first()))
            .map(|a| a.name.clone());
        Ok(SongsView {
            title,
            description,
            songs,
            context: None,
        })
    }

    pub async fn stream_url(&self, id: u64) -> Result<StreamUrl, ApiError> {
        self.stream_url_at(id, self.quality).await
    }

    pub async fn stream_url_at(&self, id: u64, requested: Quality) -> Result<StreamUrl, ApiError> {
        let mut levels = vec![requested, Quality::Exhigh, Quality::Standard];
        let mut seen = HashSet::new();
        levels.retain(|q| seen.insert(q.as_str()));
        let mut last_reason = String::new();
        let mut last_code = 0i64;
        for level in levels {
            let value = self
                .post_ok(
                    CryptoMode::Eapi,
                    "/api/song/enhance/player/url/v1",
                    json!({
                        "ids": format!("[{id}]"),
                        "level": level.as_str(),
                        "encodeType": "flac",
                    }),
                )
                .await?;
            let response: StreamResponse =
                serde_json::from_value(value).map_err(|e| ApiError::Decode {
                    endpoint: "/api/song/enhance/player/url/v1".into(),
                    message: e.to_string(),
                })?;
            let item = response.data.into_iter().find(|item| item.id == id);
            let Some(item) = item else {
                last_reason = "no stream entry returned".into();
                continue;
            };
            if item.code != 200 {
                last_code = item.code;
            }
            if let Some(url) = item.url.as_deref().filter(|u| !u.is_empty()) {
                let trial = item.free_trial_info.is_some();
                return Ok(StreamUrl {
                    url: url.to_string(),
                    expires_in_secs: if item.expi == 0 { 1200 } else { item.expi },
                    bitrate: item.br,
                    format: item.format,
                    size: item.size,
                    md5: item.md5,
                    level: item.level,
                    trial,
                });
            }
            last_reason = item
                .message
                .clone()
                .filter(|m| !m.is_empty())
                .or_else(|| {
                    item.free_trial_privilege
                        .as_ref()
                        .and_then(|p| p.cannot_listen_reason)
                        .map(|r| format!("account cannot listen (reason {r})"))
                })
                .unwrap_or_else(|| format!("no URL (code {})", item.code));
        }
        if last_reason.is_empty() {
            last_reason = format!("no playable URL (last code {last_code})");
        }
        Err(ApiError::Unplayable(last_reason))
    }

    pub async fn lyrics(&self, id: u64) -> Result<Lyrics, ApiError> {
        let value = self
            .post_ok(
                CryptoMode::Eapi,
                "/api/song/lyric",
                json!({ "id": id, "tv": -1, "lv": -1, "rv": -1, "kv": -1, "_nmclfl": 1 }),
            )
            .await?;
        let response: LyricResponse =
            serde_json::from_value(value).map_err(|e| ApiError::Decode {
                endpoint: "/api/song/lyric".into(),
                message: e.to_string(),
            })?;
        let lrc = response.lrc.map(|l| l.lyric).unwrap_or_default();
        if lrc.trim().is_empty() || response.nolyric {
            return Err(ApiError::NotFound("lyrics".into()));
        }
        let translated = response
            .tlyric
            .map(|t| t.lyric)
            .filter(|t| !t.trim().is_empty());
        Ok(Lyrics { lrc, translated })
    }

    /// Ask NetEase Heartbeat for the next track in a playlist context.
    pub async fn intelligence(
        &self,
        playlist_id: u64,
        seed_id: u64,
        current_id: u64,
        count: usize,
    ) -> Result<Vec<Song>, ApiError> {
        self.require_login().await?;
        let value = self
            .post_ok(
                CryptoMode::Eapi,
                "/api/playmode/intelligence/list",
                json!({
                    "songId": current_id,
                    "type": "fromPlayOne",
                    "playlistId": playlist_id,
                    "startMusicId": seed_id,
                    "count": count.clamp(1, 10),
                }),
            )
            .await?;
        let (raw_songs, ids) = extract_intelligence(&value);
        let mut songs: Vec<Song> = raw_songs.into_iter().map(RawSong::into_song).collect();
        if songs.is_empty() && !ids.is_empty() {
            songs = self.song_details(&ids).await?;
        }
        if songs.is_empty() {
            return Err(ApiError::Unsupported(
                "Heartbeat returned no recommendations for this playlist".into(),
            ));
        }
        let mut seen = HashSet::new();
        songs.retain(|s| seen.insert(s.id));
        songs.truncate(count.max(1));
        Ok(songs)
    }
}

fn is_liked_playlist(playlist: &RawPlaylist) -> bool {
    playlist.special_type == 5
        || playlist.name.contains("喜欢的音乐")
        || playlist.name.eq_ignore_ascii_case("liked songs")
}

fn find_liked_playlist(playlists: &[RawPlaylist]) -> Option<u64> {
    playlists
        .iter()
        .find(|playlist| is_liked_playlist(playlist))
        .map(|playlist| playlist.id)
}

fn validate_page_limit(limit: usize) -> Result<usize, ApiError> {
    if (1..=MAX_PAGE_LIMIT).contains(&limit) {
        Ok(limit)
    } else {
        Err(ApiError::InvalidArgument(format!(
            "limit must be between 1 and {MAX_PAGE_LIMIT}"
        )))
    }
}

fn page_bounds(total: usize, offset: usize, limit: usize) -> (usize, usize) {
    let offset = offset.min(total);
    let end = offset.saturating_add(limit).min(total);
    (offset, end)
}

fn json_i64(value: &Value, key: &str) -> Option<i64> {
    match value.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn candidate_song(value: &Value) -> Option<RawSong> {
    let candidate = value
        .get("song")
        .or_else(|| value.get("songInfo"))
        .or_else(|| value.get("track"))
        .unwrap_or(value);
    serde_json::from_value::<RawSong>(candidate.clone())
        .ok()
        .filter(|s| s.id != 0 && !s.name.is_empty())
}

fn candidate_id(value: &Value) -> Option<u64> {
    for key in ["id", "songId", "song_id", "resourceId"] {
        match value.get(key) {
            Some(Value::Number(n)) => {
                if let Some(id) = n.as_u64() {
                    if id != 0 {
                        return Some(id);
                    }
                }
            }
            Some(Value::String(s)) => {
                if let Ok(id) = s.parse::<u64>() {
                    if id != 0 {
                        return Some(id);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone)]
struct RecentEntry {
    id: u64,
    song: Option<RawSong>,
}

fn extract_recent(value: &Value) -> Vec<RecentEntry> {
    let mut entries: Vec<RecentEntry> = Vec::new();
    let mut seen = HashSet::new();
    let arrays = [
        value.pointer("/data/list"),
        value.pointer("/data"),
        value.get("list"),
        value.pointer("/data/allData"),
        value.get("allData"),
        value.pointer("/data/weekData"),
        value.get("weekData"),
        value.pointer("/data/data"),
    ];
    for array in arrays.into_iter().flatten() {
        let Some(items) = array.as_array() else {
            continue;
        };
        for item in items {
            let song = [
                candidate_song(item),
                item.get("data").and_then(candidate_song),
                item.get("data")
                    .and_then(|d| d.get("song"))
                    .and_then(candidate_song),
                item.get("song").and_then(candidate_song),
            ]
            .into_iter()
            .flatten()
            .next();
            let id = song
                .as_ref()
                .map(|s| s.id)
                .or_else(|| candidate_id(item))
                .or_else(|| item.get("data").and_then(candidate_id))
                .or_else(|| item.get("song").and_then(candidate_id));
            if let Some(id) = id {
                if seen.insert(id) {
                    entries.push(RecentEntry { id, song });
                }
            }
        }
        if !entries.is_empty() {
            break;
        }
    }
    entries
}

fn extract_intelligence(value: &Value) -> (Vec<RawSong>, Vec<u64>) {
    let mut arrays = Vec::new();
    if let Some(data) = value.get("data") {
        match data {
            Value::Array(items) => arrays.push(items.clone()),
            Value::Object(map) => {
                for key in ["songs", "list", "data", "recommendList"] {
                    if let Some(Value::Array(items)) = map.get(key) {
                        arrays.push(items.clone());
                    }
                }
                arrays.push(vec![data.clone()]);
            }
            _ => {}
        }
    }
    for key in ["songs", "list", "recommendList"] {
        if let Some(Value::Array(items)) = value.get(key) {
            arrays.push(items.clone());
        }
    }
    let mut songs = Vec::new();
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for items in arrays {
        for item in items {
            if let Some(song) = candidate_song(&item) {
                if seen.insert(song.id) {
                    ids.push(song.id);
                    songs.push(song);
                }
            } else if let Some(id) = candidate_id(&item)
                .or_else(|| item.get("songInfo").and_then(candidate_id))
                .or_else(|| item.get("song").and_then(candidate_id))
            {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
        }
    }
    (songs, ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_recent_song_wrappers() {
        let value = json!({
            "code": 200,
            "data": {
                "list": [
                    {"data": {"id": 1, "name": "one", "ar": [{"id": 2, "name": "a"}], "al": {"id": 3, "name": "b"}, "dt": 1000}},
                    {"songId": 4},
                    {"song": {"id": 5, "name": "five", "ar": [], "al": null, "dt": 2000}}
                ]
            }
        });
        let entries = extract_recent(&value);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![1, 4, 5]
        );
        assert!(entries[0].song.is_some());
        assert!(entries[1].song.is_none());
    }

    #[test]
    fn extracts_intelligence_data_variants() {
        let value = json!({
            "code": 200,
            "data": [
                {"songInfo": {"id": 11, "name": "a", "ar": [], "dt": 1000}},
                {"song": {"id": 12, "name": "b", "ar": [], "dt": 1000}},
                {"songId": 13}
            ]
        });
        let (songs, ids) = extract_intelligence(&value);
        assert_eq!(songs.len(), 2);
        assert!(ids.contains(&13));
    }

    #[test]
    fn stream_quality_fallback_order_is_deduplicated() {
        let mut levels = vec![Quality::Exhigh, Quality::Exhigh, Quality::Standard];
        let mut seen = HashSet::new();
        levels.retain(|q| seen.insert(q.as_str()));
        assert_eq!(levels, vec![Quality::Exhigh, Quality::Standard]);
    }

    #[test]
    fn json_code_accepts_numeric_strings() {
        assert_eq!(json_i64(&json!({"code": "801"}), "code"), Some(801));
        assert_eq!(json_i64(&json!({"code": 200}), "code"), Some(200));
    }

    #[test]
    fn finds_liked_playlist_by_special_type_or_name() {
        let by_type = RawPlaylist {
            id: 7,
            special_type: 5,
            ..Default::default()
        };
        assert_eq!(find_liked_playlist(&[by_type]), Some(7));

        let by_zh = RawPlaylist {
            id: 8,
            name: "我喜欢的音乐".into(),
            ..Default::default()
        };
        assert_eq!(find_liked_playlist(&[by_zh]), Some(8));

        let by_en = RawPlaylist {
            id: 9,
            name: "Liked Songs".into(),
            ..Default::default()
        };
        assert_eq!(find_liked_playlist(&[by_en]), Some(9));

        let other = RawPlaylist {
            id: 10,
            name: "Other".into(),
            ..Default::default()
        };
        assert_eq!(find_liked_playlist(&[other]), None);
    }

    #[test]
    fn page_bounds_clamps_and_reports_next_offset() {
        assert_eq!(page_bounds(924, 0, 100), (0, 100));
        assert_eq!(page_bounds(924, 900, 100), (900, 924));
        assert_eq!(page_bounds(924, 2000, 100), (924, 924));
        assert_eq!(page_bounds(0, 0, 100), (0, 0));
    }

    #[test]
    fn page_limit_is_bounded() {
        assert!(validate_page_limit(1).is_ok());
        assert!(validate_page_limit(1000).is_ok());
        assert!(validate_page_limit(0).is_err());
        assert!(validate_page_limit(1001).is_err());
    }
}
