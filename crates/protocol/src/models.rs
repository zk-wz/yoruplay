use serde::{Deserialize, Serialize};

/// A track as displayed by YoruPlay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Song {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub artist_ids: Vec<u64>,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_id: u64,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub fee: i64,
    /// Whether the account is known to be able to play the track. `None` when
    /// the endpoint did not provide a privilege object.
    #[serde(default)]
    pub playable: Option<bool>,
}

impl Song {
    pub fn artist_text(&self) -> String {
        if self.artists.is_empty() {
            "Unknown artist".to_string()
        } else {
            self.artists.join(", ")
        }
    }

    pub fn display_line(&self) -> String {
        format!("{} — {}", self.name, self.artist_text())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub track_count: u64,
    #[serde(default)]
    pub subscribed: bool,
    #[serde(default)]
    pub creator_id: u64,
    #[serde(default)]
    pub creator_name: String,
    #[serde(default)]
    pub special_type: i64,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub play_count: u64,
    #[serde(default)]
    pub update_time_ms: u64,
}

impl PlaylistSummary {
    pub fn is_liked(&self) -> bool {
        self.special_type == 5
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistRef {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistTracks {
    pub playlist: PlaylistSummary,
    pub songs: Vec<Song>,
    pub offset: usize,
    pub total: usize,
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub user_id: u64,
    pub nickname: String,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub background_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistSummary {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub album_count: u64,
    #[serde(default)]
    pub song_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumSummary {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub track_count: u64,
    #[serde(default)]
    pub publish_time_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Songs,
    Artists,
    Albums,
    Playlists,
}

impl SearchKind {
    pub const ALL: [SearchKind; 4] = [
        SearchKind::Songs,
        SearchKind::Artists,
        SearchKind::Albums,
        SearchKind::Playlists,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SearchKind::Songs => "Songs",
            SearchKind::Artists => "Artists",
            SearchKind::Albums => "Albums",
            SearchKind::Playlists => "Playlists",
        }
    }

    pub fn next(self) -> Self {
        match self {
            SearchKind::Songs => SearchKind::Artists,
            SearchKind::Artists => SearchKind::Albums,
            SearchKind::Albums => SearchKind::Playlists,
            SearchKind::Playlists => SearchKind::Songs,
        }
    }

    /// NetEase search type codes.
    pub fn upstream_type(self) -> u32 {
        match self {
            SearchKind::Songs => 1,
            SearchKind::Artists => 100,
            SearchKind::Albums => 10,
            SearchKind::Playlists => 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "items", rename_all = "snake_case")]
pub enum SearchItems {
    Songs(Vec<Song>),
    Artists(Vec<ArtistSummary>),
    Albums(Vec<AlbumSummary>),
    Playlists(Vec<PlaylistSummary>),
}

impl SearchItems {
    pub fn kind(&self) -> SearchKind {
        match self {
            SearchItems::Songs(_) => SearchKind::Songs,
            SearchItems::Artists(_) => SearchKind::Artists,
            SearchItems::Albums(_) => SearchKind::Albums,
            SearchItems::Playlists(_) => SearchKind::Playlists,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            SearchItems::Songs(v) => v.len(),
            SearchItems::Artists(v) => v.len(),
            SearchItems::Albums(v) => v.len(),
            SearchItems::Playlists(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub query: String,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    pub has_more: bool,
    pub items: SearchItems,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayMode {
    /// Play the queue in order, wrap after the last track.
    #[default]
    ListLoop,
    /// Repeat the current track.
    TrackLoop,
    /// Play the queue in order, stop after the last track.
    Sequential,
    /// Random order, avoiding short-term repeats.
    Shuffle,
    /// NetEase intelligent recommendation based on the current playlist context.
    Intelligence,
}

impl PlayMode {
    pub const ALL: [PlayMode; 5] = [
        PlayMode::ListLoop,
        PlayMode::TrackLoop,
        PlayMode::Sequential,
        PlayMode::Shuffle,
        PlayMode::Intelligence,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PlayMode::ListLoop => "List repeat",
            PlayMode::TrackLoop => "Track repeat",
            PlayMode::Sequential => "Sequential",
            PlayMode::Shuffle => "Shuffle",
            PlayMode::Intelligence => "Heartbeat",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            PlayMode::ListLoop => "LIST",
            PlayMode::TrackLoop => "ONE",
            PlayMode::Sequential => "SEQ",
            PlayMode::Shuffle => "RAND",
            PlayMode::Intelligence => "HEART",
        }
    }

    pub fn next(self) -> Self {
        match self {
            PlayMode::ListLoop => PlayMode::TrackLoop,
            PlayMode::TrackLoop => PlayMode::Sequential,
            PlayMode::Sequential => PlayMode::Shuffle,
            PlayMode::Shuffle => PlayMode::Intelligence,
            PlayMode::Intelligence => PlayMode::ListLoop,
        }
    }

    /// MPRIS only models a subset of the modes. The full internal mode is
    /// preserved separately.
    pub fn mpris_loop_status(self) -> &'static str {
        match self {
            PlayMode::ListLoop | PlayMode::Shuffle | PlayMode::Intelligence => "Playlist",
            PlayMode::TrackLoop => "Track",
            PlayMode::Sequential => "None",
        }
    }

    pub fn mpris_shuffle(self) -> bool {
        matches!(self, PlayMode::Shuffle)
    }

    pub fn from_mpris(loop_status: Option<&str>, shuffle: Option<bool>, _previous: Self) -> Self {
        if shuffle == Some(true) {
            return PlayMode::Shuffle;
        }
        match loop_status {
            Some("Track") => PlayMode::TrackLoop,
            Some("Playlist") => PlayMode::ListLoop,
            Some("None") => PlayMode::Sequential,
            _ => _previous,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
    Buffering,
}

impl PlaybackStatus {
    pub fn mpris(self) -> &'static str {
        match self {
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
            PlaybackStatus::Buffering => "Playing",
            PlaybackStatus::Stopped => "Stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub song: Song,
    /// Playlist this item was started from, used as heartbeat context.
    #[serde(default)]
    pub context: Option<PlaylistRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaySource {
    #[serde(default)]
    pub playlist: Option<PlaylistRef>,
    #[serde(default)]
    pub personal_fm: bool,
}

impl Default for PlaySource {
    fn default() -> Self {
        Self::none()
    }
}

impl PlaySource {
    pub fn none() -> Self {
        Self {
            playlist: None,
            personal_fm: false,
        }
    }
    pub fn playlist(p: PlaylistRef) -> Self {
        Self {
            playlist: Some(p),
            personal_fm: false,
        }
    }
    pub fn personal_fm() -> Self {
        Self {
            playlist: None,
            personal_fm: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub items: Vec<QueueItem>,
    pub current: Option<usize>,
    pub source: PlaySource,
    pub mode: PlayMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackState {
    pub status: PlaybackStatus,
    pub position_ms: u64,
    pub duration_ms: u64,
    /// Normalised 0.0..=1.0, matching MPRIS.
    pub volume: f64,
    pub mode: PlayMode,
    pub source: PlaySource,
    pub current: Option<Song>,
    pub current_index: Option<usize>,
    pub queue_len: usize,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub mpv_available: bool,
    #[serde(default)]
    pub url_refreshed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthState {
    pub logged_in: bool,
    #[serde(default)]
    pub profile: Option<UserProfile>,
    /// Set when a session exists but the server rejected it.
    #[serde(default)]
    pub session_expired: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub playback: PlaybackState,
    pub queue: QueueSnapshot,
    pub auth: AuthState,
    /// Non-fatal startup diagnostics (missing mpv, MPRIS registration failure,
    /// corrupt state file that was ignored, ...).
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub level: NoticeLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QrLoginStatus {
    Waiting,
    Scanned,
    Expired,
    Success,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QrLogin {
    pub key: String,
    pub url: String,
}

impl Song {
    pub fn empty_with_id(id: u64) -> Self {
        Self {
            id,
            name: format!("Track {id}"),
            artists: Vec::new(),
            artist_ids: Vec::new(),
            album: String::new(),
            album_id: 0,
            cover_url: None,
            duration_ms: 0,
            fee: 0,
            playable: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycle_covers_all_modes() {
        let mut mode = PlayMode::ListLoop;
        for _ in 0..5 {
            mode = mode.next();
        }
        assert_eq!(mode, PlayMode::ListLoop);
    }

    #[test]
    fn mpris_mode_mapping_is_lossless_where_possible() {
        assert_eq!(PlayMode::ListLoop.mpris_loop_status(), "Playlist");
        assert_eq!(PlayMode::TrackLoop.mpris_loop_status(), "Track");
        assert_eq!(PlayMode::Sequential.mpris_loop_status(), "None");
        assert!(PlayMode::Shuffle.mpris_shuffle());
        assert!(!PlayMode::Intelligence.mpris_shuffle());
        assert_eq!(
            PlayMode::from_mpris(Some("Playlist"), Some(false), PlayMode::Sequential),
            PlayMode::ListLoop
        );
        assert_eq!(
            PlayMode::from_mpris(Some("None"), Some(true), PlayMode::Sequential),
            PlayMode::Shuffle
        );
    }

    #[test]
    fn search_kind_cycles() {
        let mut kind = SearchKind::Songs;
        for _ in 0..4 {
            kind = kind.next();
        }
        assert_eq!(kind, SearchKind::Songs);
        assert_eq!(SearchKind::Albums.upstream_type(), 10);
    }
}
