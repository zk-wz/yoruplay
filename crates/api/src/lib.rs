//! Native Rust port of the NetEase Cloud Music endpoints used by YoruPlay.
//!
//! The implementation intentionally covers only what the player needs. See
//! `docs/upstream-api-map.md` for the mapping to api-enhanced modules.

pub mod client;
pub mod crypto;
pub mod error;
pub mod raw;
pub mod session;

pub use client::{Lyrics, NeteaseClient, StreamUrl};
pub use crypto::CryptoError;
pub use error::ApiError;
