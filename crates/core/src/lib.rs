//! YoruPlay background service.
//!
//! The daemon owns the queue, Rust audio engine, local IPC socket and MPRIS.
//! The TUI is only a client; it never acquires or decodes audio directly.

pub mod audio;
pub mod audio_cache;
pub mod browser_login;
pub mod cache;
pub mod daemon;
pub mod mpris;
pub mod queue;
pub mod service;
pub mod store;
pub mod transfer;
