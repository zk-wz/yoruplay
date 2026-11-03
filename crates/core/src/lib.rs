//! YoruPlay background service.
//!
//! The daemon owns the queue, mpv, the local IPC socket and MPRIS. The TUI is
//! only a client; it never talks to mpv or NetEase playback directly.

pub mod browser_login;
pub mod cache;
pub mod daemon;
pub mod mpris;
pub mod mpv;
pub mod queue;
pub mod service;
pub mod store;
