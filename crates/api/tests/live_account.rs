//! Real-account verification. Requires an existing auth file created by
//! `yoruplay login`. Read-only calls only; never prints cookies or stream URLs.
//!
//! `YORUPLAY_AUTH_FILE=$HOME/.local/share/yoruplay/auth.json cargo test \
//!   -p yoruplay-api --test live_account -- --ignored --nocapture`

use std::path::PathBuf;

use yoruplay_api::NeteaseClient;
use yoruplay_protocol::{ApiConfig, PlaylistKind, SearchKind};

fn auth_path() -> PathBuf {
    if let Ok(path) = std::env::var("YORUPLAY_AUTH_FILE") {
        return path.into();
    }
    let home = std::env::var("HOME").expect("HOME");
    PathBuf::from(home).join(".local/share/yoruplay/auth.json")
}

#[tokio::test]
#[ignore = "requires a real NetEase account session"]
async fn live_account_read_only() {
    let client = NeteaseClient::new(&ApiConfig::default(), Some(auth_path())).expect("client");
    let auth = client.auth_state().await;
    assert!(auth.logged_in, "auth file missing or not logged in");
    let profile = client.account().await.expect("account").expect("profile");
    println!("account: {} ({})", profile.nickname, profile.user_id);

    let created = client
        .playlists(PlaylistKind::Created)
        .await
        .expect("created playlists");
    let subscribed = client
        .playlists(PlaylistKind::Subscribed)
        .await
        .expect("subscribed playlists");
    println!(
        "playlists: {} created, {} subscribed",
        created.len(),
        subscribed.len()
    );
    for playlist in created.iter().take(3) {
        println!(
            "  created: [{}] {} ({} tracks)",
            playlist.id, playlist.name, playlist.track_count
        );
    }
    for playlist in subscribed.iter().take(3) {
        println!(
            "  subscribed: [{}] {} ({} tracks)",
            playlist.id, playlist.name, playlist.track_count
        );
    }

    let liked = client.liked_tracks(0, 50).await.expect("liked songs");
    println!(
        "liked: {} songs ({} total, has_more={})",
        liked.songs.len(),
        liked.total,
        liked.has_more
    );

    let daily = client.daily_songs().await.expect("daily songs");
    println!("daily songs: {}", daily.len());

    let daily_lists = client.daily_playlists().await.expect("daily playlists");
    println!("daily playlists: {}", daily_lists.len());
    for playlist in daily_lists.iter().take(5) {
        println!("  daily: [{}] {}", playlist.id, playlist.name);
    }

    let fm = client.personal_fm().await.expect("personal fm");
    println!("personal fm: {} songs", fm.len());

    let recent = client.recent_songs().await.expect("recent songs");
    println!("recent: {} songs", recent.songs.len());
    for song in recent.songs.iter().take(3) {
        println!("  recent: {} — {}", song.name, song.artist_text());
    }

    let radar = client.radar().await;
    match &radar {
        Ok(tracks) => println!(
            "radar: [{}] {} ({} tracks)",
            tracks.playlist.id,
            tracks.playlist.name,
            tracks.songs.len()
        ),
        Err(e) => println!("radar: ERROR {}", e.user_message()),
    }

    for kind in SearchKind::ALL {
        let page = client.search("晴天", kind, 0, 3).await.expect("search");
        println!(
            "search {:?}: {} (total {})",
            kind,
            page.items.len(),
            page.total
        );
    }

    let first_song = liked
        .songs
        .first()
        .or_else(|| daily.first())
        .expect("a song to play");
    println!("stream candidate: [{}] {}", first_song.id, first_song.name);
    let stream = client.stream_url(first_song.id).await.expect("stream url");
    println!(
        "stream: level={:?} expiry={}s bitrate={} trial={}",
        stream.level, stream.expires_in_secs, stream.bitrate, stream.trial
    );

    let lyrics = client.lyrics(first_song.id).await;
    println!(
        "lyrics: {}",
        lyrics.as_ref().map(|l| l.lrc.len()).unwrap_or(0)
    );

    let context = created
        .iter()
        .find(|p| p.track_count > 0)
        .or_else(|| subscribed.iter().find(|p| p.track_count > 0))
        .or_else(|| radar.as_ref().ok().map(|t| &t.playlist));
    if let Some(playlist) = context {
        let tracks = client
            .playlist_tracks(playlist.id, 0, 5)
            .await
            .expect("playlist tracks");
        println!(
            "context playlist: [{}] {} ({} tracks)",
            tracks.playlist.id, tracks.playlist.name, tracks.total
        );
        if let Some(seed) = tracks.songs.first() {
            match client
                .intelligence(tracks.playlist.id, seed.id, seed.id, 1)
                .await
            {
                Ok(songs) => println!(
                    "heartbeat: {} songs, first={}",
                    songs.len(),
                    songs.first().map(|s| s.name.as_str()).unwrap_or("-")
                ),
                Err(e) => println!("heartbeat: ERROR {}", e.user_message()),
            }
        }
    }
}
