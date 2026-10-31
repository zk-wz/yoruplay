use yoruplay_api::NeteaseClient;
use yoruplay_protocol::{ApiConfig, SearchItems, SearchKind};

#[tokio::test]
#[ignore = "live read-only stream URL probe"]
async fn find_anonymously_playable_song() {
    let client = NeteaseClient::new(&ApiConfig::default(), None).unwrap();
    for query in ["网易云音乐", "免费音乐", "纯音乐", "古典音乐", "白噪音"] {
        let page = client
            .search(query, SearchKind::Songs, 0, 30)
            .await
            .unwrap();
        let SearchItems::Songs(songs) = page.items else {
            continue;
        };
        for song in songs.into_iter().filter(|s| s.fee == 0).take(12) {
            match client.stream_url(song.id).await {
                Ok(url) => {
                    println!(
                        "PLAYABLE query={query} id={} name={} fee={} level={:?} expiry={} trial={}",
                        song.id, song.name, song.fee, url.level, url.expires_in_secs, url.trial
                    );
                    return;
                }
                Err(e) => println!(
                    "no query={query} id={} name={} fee={} -> {}",
                    song.id,
                    song.name,
                    song.fee,
                    e.user_message()
                ),
            }
        }
    }
    panic!("no anonymously playable song found");
}
