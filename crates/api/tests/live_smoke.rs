//! Manual live compatibility check. Run with:
//! `cargo test -p yoruplay-api --test live_smoke -- --ignored --nocapture`
//!
//! These calls are low-frequency and read-only. They are ignored by default so
//! the normal test suite does not depend on NetEase availability.

use yoruplay_api::NeteaseClient;
use yoruplay_protocol::{ApiConfig, SearchKind};

#[tokio::test]
#[ignore = "requires live NetEase service"]
async fn live_read_only_smoke() {
    let client = NeteaseClient::new(&ApiConfig::default(), None).unwrap();

    let songs = client
        .song_details(&[186016, 33894312])
        .await
        .expect("song detail");
    println!("songs: {}", songs.len());
    assert!(!songs.is_empty());

    let search = client
        .search("晴天", SearchKind::Songs, 0, 5)
        .await
        .expect("search");
    println!("search total={} items={}", search.total, search.items.len());
    assert!(!search.items.is_empty());

    let playlist = client
        .playlist_tracks(3778678, 0, 3)
        .await
        .expect("playlist");
    println!(
        "playlist {} tracks {} total {}",
        playlist.playlist.name,
        playlist.songs.len(),
        playlist.total
    );
    assert!(!playlist.songs.is_empty());

    let lyrics = client.lyrics(186016).await.expect("lyrics");
    println!("lyrics bytes: {}", lyrics.lrc.len());
    assert!(!lyrics.lrc.is_empty());

    let artists = client
        .search("周杰伦", SearchKind::Artists, 0, 3)
        .await
        .expect("artist search");
    let albums = client
        .search("叶惠美", SearchKind::Albums, 0, 3)
        .await
        .expect("album search");
    let lists = client
        .search("华语", SearchKind::Playlists, 0, 3)
        .await
        .expect("playlist search");
    println!(
        "search other kinds: {} {} {}",
        artists.items.len(),
        albums.items.len(),
        lists.items.len()
    );
}
