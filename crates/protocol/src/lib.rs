//! Shared models, configuration and local IPC contract for YoruPlay.
//!
//! The crate deliberately has no dependency on the HTTP API, the player or the
//! UI.  Both the daemon and the TUI use these types, which keeps the wire
//! contract and the user-visible data model in one place.

pub mod config;
pub mod ipc;

/// Official NetEase web login page used by browser-assisted login.
pub const LOGIN_URL: &str = "https://music.163.com/login";
pub mod models;
pub mod paths;

pub use config::{
    ApiConfig, CacheConfig, Config, Language, LogConfig, PlaybackConfig, Quality, UiConfig,
};
pub use ipc::*;
pub use models::*;
pub use paths::Paths;
