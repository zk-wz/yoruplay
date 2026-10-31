//! MPRIS 2.2 implementation over zbus.
//!
//! YoruPlay's daemon is the only MPRIS owner; mpv is deliberately not allowed
//! to register a second player. The daemon state is the source of truth for
//! every method and property here.

use std::borrow::Cow;
use std::collections::HashMap;

use zbus::fdo::{self, Properties};
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::Connection;

use yoruplay_protocol::{Command, PlayMode, PlaybackState, PlaybackStatus, Song};

use crate::service::Daemon;

const SERVICE_NAME: &str = "org.mpris.MediaPlayer2.yoruplay";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

pub struct MprisHandle {
    pub connection: Connection,
    task: tokio::task::JoinHandle<()>,
}

impl MprisHandle {
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.connection.release_name(SERVICE_NAME).await;
    }
}

pub async fn start(daemon: Daemon) -> zbus::Result<MprisHandle> {
    let connection = Connection::session().await?;
    // Register the interfaces first so calls arriving immediately after the
    // well-known name is acquired are routed correctly. Do not replace another
    // player's name: only YoruPlay may own this name.
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            MprisRoot {
                daemon: daemon.clone(),
            },
        )
        .await?;
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            MprisPlayer {
                daemon: daemon.clone(),
            },
        )
        .await?;
    connection.request_name(SERVICE_NAME).await?;
    let watch_connection = connection.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = watch_properties(watch_connection, daemon).await {
            tracing::warn!(error = %e, "MPRIS property watcher stopped");
        }
    });
    Ok(MprisHandle { connection, task })
}

struct MprisRoot {
    daemon: Daemon,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn identity(&self) -> &str {
        "YoruPlay"
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        "yoruplay"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn set_fullscreen(&self, _value: bool) {}

    fn raise(&self) {}

    fn quit(&self) {
        self.daemon.request_shutdown();
    }
}

struct MprisPlayer {
    daemon: Daemon,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    async fn next(&self) -> fdo::Result<()> {
        self.command(Command::Next).await
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.command(Command::Previous).await
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.command(Command::Pause).await
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.command(Command::PlayPause).await
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.command(Command::Stop).await
    }

    async fn play(&self) -> fdo::Result<()> {
        self.command(Command::Play).await
    }

    async fn seek(&self, offset: i64) -> fdo::Result<()> {
        self.command(Command::SeekRelative {
            offset_ms: offset / 1000,
        })
        .await
    }

    async fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        if position < 0 {
            return Err(fdo::Error::InvalidArgs(
                "position must be non-negative".into(),
            ));
        }
        let playback = self.daemon.playback().await;
        let Some(song) = playback.current.as_ref() else {
            return Err(fdo::Error::Failed("there is no current track".into()));
        };
        let expected = track_path(song.id);
        if track_id.as_str() != expected {
            return Err(fdo::Error::InvalidArgs(
                "SetPosition referred to a different track".into(),
            ));
        }
        self.command(Command::SetPosition {
            position_ms: (position as u64) / 1000,
        })
        .await
    }

    async fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "YoruPlay does not accept external media URIs".into(),
        ))
    }

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.daemon.playback().await.status.mpris().to_string()
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn loop_status(&self) -> String {
        self.daemon
            .playback()
            .await
            .mode
            .mpris_loop_status()
            .to_string()
    }

    #[zbus(property)]
    async fn set_loop_status(&self, value: String) -> fdo::Result<()> {
        if !matches!(value.as_str(), "None" | "Track" | "Playlist") {
            return Err(fdo::Error::InvalidArgs(format!(
                "unsupported LoopStatus {value}"
            )));
        }
        let previous = self.daemon.playback().await.mode;
        let mode = PlayMode::from_mpris(Some(&value), None, previous);
        self.command(Command::SetMode { mode }).await
    }

    #[zbus(property)]
    async fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    async fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn shuffle(&self) -> bool {
        self.daemon.playback().await.mode.mpris_shuffle()
    }

    #[zbus(property)]
    async fn set_shuffle(&self, value: bool) -> fdo::Result<()> {
        let playback = self.daemon.playback().await;
        let mode = if value {
            PlayMode::Shuffle
        } else if playback.mode == PlayMode::Shuffle {
            PlayMode::Sequential
        } else {
            playback.mode
        };
        self.command(Command::SetMode { mode }).await
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        let playback = self.daemon.playback().await;
        let lyrics = self.daemon.lyrics().await;
        metadata_for(playback.current.as_ref(), lyrics.as_deref())
    }

    #[zbus(property(emits_changed_signal = "false"))]
    async fn volume(&self) -> f64 {
        self.daemon.playback().await.volume
    }

    #[zbus(property)]
    async fn set_volume(&self, value: f64) -> fdo::Result<()> {
        if !value.is_finite() {
            return Err(fdo::Error::InvalidArgs("volume must be finite".into()));
        }
        self.command(Command::SetVolume {
            volume: value.clamp(0.0, 1.0),
        })
        .await
    }

    #[zbus(property)]
    async fn position(&self) -> i64 {
        let playback = self.daemon.playback().await;
        playback.position_ms.min((i64::MAX / 1000) as u64) as i64 * 1000
    }

    #[zbus(property)]
    async fn can_play(&self) -> bool {
        self.daemon.playback().await.current.is_some()
    }

    #[zbus(property)]
    async fn can_pause(&self) -> bool {
        let playback = self.daemon.playback().await;
        playback.current.is_some() && playback.status != PlaybackStatus::Stopped
    }

    #[zbus(property)]
    async fn can_seek(&self) -> bool {
        let playback = self.daemon.playback().await;
        playback.current.is_some() && playback.status != PlaybackStatus::Stopped
    }

    #[zbus(property)]
    async fn can_go_next(&self) -> bool {
        can_next(&self.daemon.playback().await)
    }

    #[zbus(property)]
    async fn can_go_previous(&self) -> bool {
        can_previous(&self.daemon.playback().await)
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

impl MprisPlayer {
    async fn command(&self, command: Command) -> fdo::Result<()> {
        self.daemon
            .handle_command(command)
            .await
            .map(|_| ())
            .map_err(|e| fdo::Error::Failed(e.message))
    }
}

pub fn track_path(id: u64) -> String {
    format!("/org/yoruplay/track/{id}")
}

pub fn metadata_for(song: Option<&Song>, lyrics: Option<&str>) -> HashMap<String, OwnedValue> {
    let Some(song) = song else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    if let Ok(path) = ObjectPath::try_from(track_path(song.id)) {
        map.insert("mpris:trackid".into(), OwnedValue::from(path));
    }
    map.insert("xesam:title".into(), owned_string(song.name.clone()));
    if !song.artists.is_empty() {
        if let Ok(value) = owned_string_array(song.artists.clone()) {
            map.insert("xesam:artist".into(), value);
            if let Some(first) = song.artists.first() {
                map.insert("xesam:albumArtist".into(), owned_string(first.clone()));
            }
        }
    }
    if !song.album.is_empty() {
        map.insert("xesam:album".into(), owned_string(song.album.clone()));
    }
    if song.duration_ms > 0 {
        let micros = song.duration_ms.saturating_mul(1000).min(i64::MAX as u64) as i64;
        map.insert("mpris:length".into(), OwnedValue::from(micros));
    }
    if let Some(url) = song.cover_url.as_ref().filter(|u| !u.is_empty()) {
        map.insert("mpris:artUrl".into(), owned_string(url.clone()));
    }
    if let Some(lyrics) = lyrics.filter(|l| !l.is_empty()) {
        map.insert("xesam:asText".into(), owned_string(lyrics.to_string()));
    }
    map
}

fn owned_string(value: String) -> OwnedValue {
    OwnedValue::try_from(Value::from(value)).unwrap_or_else(|_| OwnedValue::from(true))
}

fn owned_string_array(strings: Vec<String>) -> Result<OwnedValue, zbus::zvariant::Error> {
    OwnedValue::try_from(Value::from(strings))
}

fn can_next(playback: &PlaybackState) -> bool {
    if playback.current.is_none() || playback.queue_len == 0 {
        return false;
    }
    if playback.mode == PlayMode::Sequential {
        return playback
            .current_index
            .is_some_and(|i| i + 1 < playback.queue_len);
    }
    true
}

fn can_previous(playback: &PlaybackState) -> bool {
    if playback.current.is_none() || playback.queue_len == 0 {
        return false;
    }
    playback.current_index.unwrap_or(0) > 0
        || matches!(playback.mode, PlayMode::ListLoop | PlayMode::TrackLoop)
        || playback.queue_len > 1
}

#[derive(PartialEq)]
struct Snapshot {
    status: &'static str,
    loop_status: &'static str,
    shuffle: bool,
    volume: f64,
    can_next: bool,
    can_previous: bool,
    can_play: bool,
    can_pause: bool,
    can_seek: bool,
    track_id: Option<u64>,
    duration_ms: u64,
    lyrics: Option<String>,
}

impl Snapshot {
    fn from(playback: &PlaybackState, lyrics: Option<&str>) -> Self {
        Self {
            status: playback.status.mpris(),
            loop_status: playback.mode.mpris_loop_status(),
            shuffle: playback.mode.mpris_shuffle(),
            volume: playback.volume,
            can_next: can_next(playback),
            can_previous: can_previous(playback),
            can_play: playback.current.is_some(),
            can_pause: playback.current.is_some() && playback.status != PlaybackStatus::Stopped,
            can_seek: playback.current.is_some() && playback.status != PlaybackStatus::Stopped,
            track_id: playback.current.as_ref().map(|s| s.id),
            duration_ms: playback.duration_ms,
            lyrics: lyrics.map(str::to_string),
        }
    }
}

async fn watch_properties(connection: Connection, daemon: Daemon) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(&connection, OBJECT_PATH)?;
    let interface = InterfaceName::from_static_str(PLAYER_INTERFACE)?;
    let mut events = daemon.subscribe();
    let mut shutdown = daemon.subscribe_shutdown();
    let mut previous = {
        let playback = daemon.playback().await;
        let lyrics = daemon.lyrics().await;
        Snapshot::from(&playback, lyrics.as_deref())
    };
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
        };
        match event {
            Ok(yoruplay_protocol::Event::Playback(playback)) => {
                let lyrics = daemon.lyrics().await;
                let current = Snapshot::from(&playback, lyrics.as_deref());
                emit_changes(
                    &emitter,
                    &interface,
                    &previous,
                    &current,
                    playback.current.as_ref(),
                    lyrics.as_deref(),
                )
                .await?;
                previous = current;
            }
            Ok(yoruplay_protocol::Event::Queue(_)) => {
                let playback = daemon.playback().await;
                let lyrics = daemon.lyrics().await;
                let current = Snapshot::from(&playback, lyrics.as_deref());
                emit_changes(
                    &emitter,
                    &interface,
                    &previous,
                    &current,
                    playback.current.as_ref(),
                    lyrics.as_deref(),
                )
                .await?;
                previous = current;
            }
            Ok(yoruplay_protocol::Event::Shutdown) => break,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let playback = daemon.playback().await;
                let lyrics = daemon.lyrics().await;
                let current = Snapshot::from(&playback, lyrics.as_deref());
                emit_changes(
                    &emitter,
                    &interface,
                    &previous,
                    &current,
                    playback.current.as_ref(),
                    lyrics.as_deref(),
                )
                .await?;
                previous = current;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

async fn emit_changes(
    emitter: &SignalEmitter<'_>,
    interface: &InterfaceName<'_>,
    previous: &Snapshot,
    current: &Snapshot,
    song: Option<&Song>,
    lyrics: Option<&str>,
) -> zbus::Result<()> {
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    if previous.status != current.status {
        changed.insert("PlaybackStatus", Value::from(current.status));
    }
    if previous.loop_status != current.loop_status {
        changed.insert("LoopStatus", Value::from(current.loop_status));
    }
    if previous.shuffle != current.shuffle {
        changed.insert("Shuffle", Value::from(current.shuffle));
    }
    if previous.can_next != current.can_next {
        changed.insert("CanGoNext", Value::from(current.can_next));
    }
    if previous.can_previous != current.can_previous {
        changed.insert("CanGoPrevious", Value::from(current.can_previous));
    }
    if previous.can_play != current.can_play {
        changed.insert("CanPlay", Value::from(current.can_play));
    }
    if previous.can_pause != current.can_pause {
        changed.insert("CanPause", Value::from(current.can_pause));
    }
    if previous.can_seek != current.can_seek {
        changed.insert("CanSeek", Value::from(current.can_seek));
    }
    if (previous.volume - current.volume).abs() > f64::EPSILON {
        changed.insert("Volume", Value::from(current.volume));
    }
    if previous.track_id != current.track_id
        || previous.duration_ms != current.duration_ms
        || previous.lyrics != current.lyrics
    {
        let metadata = metadata_for(song, lyrics);
        if let Ok(value) = OwnedValue::try_from(Value::from(metadata)) {
            changed.insert("Metadata", Value::from(value));
        } else {
            changed.insert("Metadata", Value::new(HashMap::<String, OwnedValue>::new()));
        }
    }
    if changed.is_empty() {
        return Ok(());
    }
    Properties::properties_changed(emitter, interface.clone(), changed, Cow::Borrowed(&[])).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn song() -> Song {
        Song {
            id: 42,
            name: "title".into(),
            artists: vec!["a".into(), "b".into()],
            artist_ids: vec![],
            album: "album".into(),
            album_id: 1,
            cover_url: Some("https://example.invalid/cover.jpg".into()),
            duration_ms: 1234,
            fee: 0,
            playable: Some(true),
        }
    }

    #[test]
    fn metadata_uses_microseconds_and_stable_track_path() {
        let map = metadata_for(Some(&song()), Some("line"));
        assert_eq!(track_path(42), "/org/yoruplay/track/42");
        assert_eq!(
            i64::try_from(map.get("mpris:length").unwrap().clone()).unwrap(),
            1_234_000
        );
        assert_eq!(
            String::try_from(map.get("xesam:title").unwrap().clone()).unwrap(),
            "title"
        );
        assert_eq!(
            String::try_from(map.get("xesam:asText").unwrap().clone()).unwrap(),
            "line"
        );
    }

    #[test]
    fn empty_metadata_when_no_track() {
        assert!(metadata_for(None, None).is_empty());
    }

    #[test]
    fn next_previous_flags_respect_sequential_end() {
        let mut playback = PlaybackState {
            status: PlaybackStatus::Playing,
            position_ms: 0,
            duration_ms: 1,
            volume: 1.0,
            mode: PlayMode::Sequential,
            source: yoruplay_protocol::PlaySource::none(),
            current: Some(song()),
            current_index: Some(1),
            queue_len: 2,
            error: None,
            warning: None,
            mpv_available: true,
            url_refreshed_at_ms: None,
        };
        assert!(!can_next(&playback));
        assert!(can_previous(&playback));
        playback.current_index = Some(0);
        assert!(can_next(&playback));
    }
}
