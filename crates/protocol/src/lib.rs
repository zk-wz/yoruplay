//! Shared models, configuration and local IPC contract for YoruPlay.
//!
//! The crate deliberately has no dependency on the HTTP API, the player or the
//! UI.  Both the daemon and the TUI use these types, which keeps the wire
//! contract and the user-visible data model in one place.

pub mod config;
pub mod ipc;
pub mod models;
pub mod paths;

pub use config::{Config, Language, PlaybackConfig, Quality};
pub use models::*;
pub use paths::Paths;
