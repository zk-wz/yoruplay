use crate::config::Quality;
use serde::{Deserialize, Serialize};

use crate::models::{
    AuthState, Notice, PlayMode, PlaybackState, PlaylistRef, PlaylistSummary, PlaylistTracks,
    QueueSnapshot, SearchKind, SearchPage, Song, StateSnapshot,
};

/// Bumped whenever the request/response names or semantics change. Clients
/// with a different version are rejected with a readable error.
pub const PROTOCOL_VERSION: u32 = 7;
/// Refuse absurd frames before attempting to allocate/deserialize them.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistKind {
    Created,
    Subscribed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SongsView {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub songs: Vec<Song>,
    #[serde(default)]
    pub context: Option<PlaylistRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,
    GetState,
    GetPlayback,
    GetQueue {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_page_limit")]
        limit: usize,
    },
    Shutdown,

    ImportBrowserCookie {
        #[serde(default)]
        force: bool,
    },
    ImportCookie {
        cookie: String,
    },
    Logout,

    Playlists {
        kind: PlaylistKind,
    },
    LikedTracks {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_page_limit")]
        limit: usize,
    },
    RecentSongs,
    PlaylistTracks {
        id: u64,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_page_limit")]
        limit: usize,
    },
    DailySongs {
        #[serde(default)]
        refresh: bool,
    },
    DailyPlaylists {
        #[serde(default)]
        refresh: bool,
    },
    Radar {
        #[serde(default)]
        refresh: bool,
    },
    PersonalFm,
    Search {
        query: String,
        kind: SearchKind,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_page_limit")]
        limit: usize,
    },
    ArtistSongs {
        id: u64,
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_page_limit")]
        limit: usize,
    },
    AlbumSongs {
        id: u64,
    },

    PlaySongs {
        songs: Vec<Song>,
        #[serde(default)]
        start_index: usize,
        #[serde(default)]
        context: Option<PlaylistRef>,
        /// Human-readable queue source, e.g. "Search results".
        #[serde(default)]
        title: Option<String>,
    },
    StartPlaylist {
        id: u64,
        #[serde(default)]
        start_index: usize,
        #[serde(default)]
        start_song_id: Option<u64>,
    },
    AddSongs {
        songs: Vec<Song>,
        #[serde(default)]
        context: Option<PlaylistRef>,
    },
    AddPlaylist {
        id: u64,
    },
    QueueRemove {
        index: usize,
    },
    QueueClear,
    QueuePlay {
        index: usize,
    },

    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    ToggleLike,
    GetLiked {
        song_id: u64,
    },
    Stop,
    SeekRelative {
        offset_ms: i64,
    },
    SetPosition {
        position_ms: u64,
    },
    SetVolume {
        volume: f64,
    },
    SetMode {
        mode: PlayMode,
    },
    CycleMode,
    RefreshCurrent,
    ClearCache,
    PlayCached {
        song_id: u64,
    },
    CacheInfo,
    CacheList {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_page_limit")]
        limit: usize,
    },
    ClearAudioCache {
        all: bool,
    },
    UpgradeAudio {
        song_id: u64,
        quality: Option<Quality>,
    },
    ReloadConfig,
}

fn default_page_limit() -> usize {
    100
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CommandResult {
    Ack {
        #[serde(default)]
        message: Option<String>,
    },
    Liked {
        song_id: u64,
        #[serde(default)]
        name: String,
        liked: bool,
    },
    State(Box<StateSnapshot>),
    Playback(Box<PlaybackState>),
    Queue(Box<QueueSnapshot>),
    Data(serde_json::Value),
    Playlists(Vec<PlaylistSummary>),
    Songs(SongsView),
    PlaylistTracks(PlaylistTracks),
    Search(SearchPage),
    Auth(AuthState),
}

impl CommandResult {
    pub fn ack() -> Self {
        CommandResult::Ack { message: None }
    }

    pub fn ack_msg(message: impl Into<String>) -> Self {
        CommandResult::Ack {
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// Stable machine-readable code.
    pub code: String,
    /// Short user-facing sentence.
    pub message: String,
    /// Optional actionable hint (do not include secrets).
    #[serde(default)]
    pub hint: Option<String>,
}

impl ErrorInfo {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Playback(Box<PlaybackState>),
    QueueChanged { revision: u64 },
    Auth(AuthState),
    Notice(Notice),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { version: u32 },
    Request { id: u64, command: Command },
    Cancel { id: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        version: u32,
        daemon_version: String,
    },
    Response {
        id: u64,
        #[serde(default)]
        result: Option<CommandResult>,
        #[serde(default)]
        error: Option<ErrorInfo>,
    },
    Event {
        event: Event,
    },
}

impl ServerMessage {
    pub fn ok(id: u64, result: CommandResult) -> Self {
        ServerMessage::Response {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, error: ErrorInfo) -> Self {
        ServerMessage::Response {
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liked_tracks_command_round_trips() {
        let command = Command::LikedTracks {
            offset: 100,
            limit: 50,
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "cmd": "liked_tracks",
                "offset": 100,
                "limit": 50,
            })
        );
        let decoded: Command = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, command);
    }

    #[test]
    fn playlist_tracks_next_offset_round_trips() {
        let tracks = PlaylistTracks {
            playlist: PlaylistSummary {
                id: 1,
                name: "Liked Songs".into(),
                cover_url: None,
                track_count: 200,
                subscribed: false,
                creator_id: 0,
                creator_name: String::new(),
                special_type: 5,
                description: None,
                play_count: 0,
                update_time_ms: 0,
            },
            songs: Vec::new(),
            offset: 100,
            total: 200,
            has_more: true,
            next_offset: 200,
        };
        let json = serde_json::to_value(&tracks).unwrap();
        assert_eq!(json["next_offset"], 200);
        let decoded: PlaylistTracks = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, tracks);
    }

    #[test]
    fn toggle_like_round_trips() {
        let command = Command::ToggleLike;
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json, serde_json::json!({ "cmd": "toggle_like" }));
        assert_eq!(serde_json::from_value::<Command>(json).unwrap(), command);

        let result = CommandResult::Liked {
            song_id: 42,
            name: "晴天".into(),
            liked: true,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "liked",
                "data": { "song_id": 42, "name": "晴天", "liked": true }
            })
        );
    }
}
