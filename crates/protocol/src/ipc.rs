use serde::{Deserialize, Serialize};

use crate::models::{
    AuthState, Notice, PlayMode, PlaybackState, PlaylistRef, PlaylistSummary, PlaylistTracks,
    QueueSnapshot, SearchKind, SearchPage, Song, StateSnapshot,
};

/// Bumped whenever the request/response names or semantics change. Clients
/// with a different version are rejected with a readable error.
pub const PROTOCOL_VERSION: u32 = 3;
/// Refuse absurd frames before attempting to allocate/deserialize them.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

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
    Shutdown,

    ImportBrowserCookie,
    ImportCookie {
        cookie: String,
    },
    Logout,

    Playlists {
        kind: PlaylistKind,
    },
    LikedSongs,
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
    State(Box<StateSnapshot>),
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
    Queue(Box<QueueSnapshot>),
    Auth(AuthState),
    Notice(Notice),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello { version: u32 },
    Request { id: u64, command: Command },
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
    use crate::models::PlayMode;

    #[test]
    fn request_roundtrip_preserves_command() {
        let msg = ClientMessage::Request {
            id: 42,
            command: Command::SetMode {
                mode: PlayMode::Intelligence,
            },
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(line.contains("\"cmd\":\"set_mode\""));
        let decoded: ClientMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn response_roundtrip_preserves_error() {
        let msg = ServerMessage::err(
            9,
            ErrorInfo::new("not_logged_in", "Log in first").with_hint("yoruplay login"),
        );
        let line = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMessage = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn defaults_are_applied_for_optional_fields() {
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"request","id":1,"command":{"cmd":"playlist_tracks","id":7}}"#,
        )
        .unwrap();
        match msg {
            ClientMessage::Request {
                command: Command::PlaylistTracks { offset, limit, .. },
                ..
            } => {
                assert_eq!(offset, 0);
                assert_eq!(limit, 100);
            }
            _ => panic!("wrong variant"),
        }
    }
}
