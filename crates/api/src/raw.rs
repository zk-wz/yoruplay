//! Tolerant deserialisation of the upstream JSON.
//!
//! NetEase uses different song shapes depending on the endpoint (`ar`/`al`
//! vs `artists`/`album`) and occasionally emits `null` for arrays.  Keeping the
//! raw layer tolerant and converting once into the shared model avoids those
//! differences leaking into the player.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use yoruplay_protocol::{
    AlbumSummary, ArtistSummary, PlaylistSummary, Song, UserProfile,
};

fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawArtist {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "picUrl")]
    pub pic_url: Option<String>,
    #[serde(default, alias = "albumSize")]
    pub album_size: u64,
    #[serde(default, alias = "musicSize")]
    pub music_size: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawAlbum {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "picUrl")]
    pub pic_url: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default, alias = "publishTime")]
    pub publish_time: u64,
    #[serde(default)]
    pub artists: Option<Vec<RawArtist>>,
    #[serde(default)]
    pub artist: Option<RawArtist>,
    #[serde(default)]
    pub songs: Vec<RawSong>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawPrivilege {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub st: Option<i64>,
    #[serde(default)]
    pub code: Option<i64>,
    #[serde(default)]
    pub fee: i64,
    #[serde(default, alias = "plLevel")]
    pub pl_level: Option<String>,
    #[serde(default, alias = "maxBrLevel")]
    pub max_br_level: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawSong {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "ar")]
    pub ar: Option<Vec<RawArtist>>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub artists: Vec<RawArtist>,
    #[serde(default, rename = "al")]
    pub al: Option<RawAlbum>,
    #[serde(default)]
    pub album: Option<RawAlbum>,
    #[serde(default, rename = "dt")]
    pub dt: u64,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub fee: i64,
    #[serde(default)]
    pub st: Option<i64>,
    #[serde(default)]
    pub privilege: Option<RawPrivilege>,
}

impl RawSong {
    pub fn into_song(self) -> Song {
        let mut artists = self.ar.unwrap_or_default();
        if artists.is_empty() {
            artists = self.artists;
        }
        let album = self.al.or(self.album);
        let playable = match (
            self.privilege.as_ref().and_then(|p| p.st),
            self.privilege.as_ref().and_then(|p| p.code),
            self.st,
        ) {
            (Some(st), _, _) if st < 0 => Some(false),
            (_, Some(code), _) if code != 0 => Some(false),
            (Some(_), _, _) => Some(true),
            (None, None, Some(st)) if st < 0 => Some(false),
            (None, None, Some(_)) => Some(true),
            _ => None,
        };
        let duration_ms = if self.dt > 0 { self.dt } else { self.duration };
        let album_name = album.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let album_id = album.as_ref().map(|a| a.id).unwrap_or(0);
        let cover_url = album.and_then(|a| a.pic_url);
        let fee = self.privilege.as_ref().map(|p| p.fee).filter(|v| *v != 0).unwrap_or(self.fee);
        Song {
            id: self.id,
            name: if self.name.is_empty() { format!("Track {}", self.id) } else { self.name },
            artists: artists.iter().map(|a| a.name.clone()).filter(|n| !n.is_empty()).collect(),
            artist_ids: artists.iter().map(|a| a.id).filter(|id| *id != 0).collect(),
            album: album_name,
            album_id,
            cover_url,
            duration_ms,
            fee,
            playable,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawCreator {
    #[serde(default, alias = "userId")]
    pub user_id: u64,
    #[serde(default)]
    pub nickname: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawTrackId {
    #[serde(default)]
    pub id: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawPlaylist {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "coverImgUrl")]
    pub cover_img_url: Option<String>,
    #[serde(default, alias = "trackCount")]
    pub track_count: u64,
    #[serde(default)]
    pub subscribed: Option<bool>,
    #[serde(default, alias = "userId")]
    pub user_id: u64,
    #[serde(default)]
    pub creator: Option<RawCreator>,
    #[serde(default, alias = "specialType")]
    pub special_type: i64,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "playCount")]
    pub play_count: u64,
    #[serde(default, alias = "updateTime")]
    pub update_time: u64,
    #[serde(default, alias = "trackIds")]
    pub track_ids: Vec<RawTrackId>,
    #[serde(default)]
    pub tracks: Vec<RawSong>,
}

impl RawPlaylist {
    pub fn into_summary(self) -> PlaylistSummary {
        let creator_id = self.creator.as_ref().map(|c| c.user_id).unwrap_or(self.user_id);
        let creator_name = self
            .creator
            .as_ref()
            .map(|c| c.nickname.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_default();
        let track_count = if self.track_count > 0 {
            self.track_count
        } else if !self.track_ids.is_empty() {
            self.track_ids.len() as u64
        } else {
            self.tracks.len() as u64
        };
        PlaylistSummary {
            id: self.id,
            name: if self.name.is_empty() { format!("Playlist {}", self.id) } else { self.name },
            cover_url: self.cover_img_url,
            track_count,
            subscribed: self.subscribed.unwrap_or(false),
            creator_id,
            creator_name,
            special_type: self.special_type,
            description: self.description,
            play_count: self.play_count,
            update_time_ms: self.update_time,
        }
    }

    /// Ordered, deduplicated track ids. If the detail endpoint omitted
    /// `trackIds`, fall back to the embedded first page of tracks.
    pub fn ordered_track_ids(&self) -> Vec<u64> {
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for raw in &self.track_ids {
            if raw.id != 0 && seen.insert(raw.id) {
                ids.push(raw.id);
            }
        }
        if ids.is_empty() {
            for song in &self.tracks {
                if song.id != 0 && seen.insert(song.id) {
                    ids.push(song.id);
                }
            }
        }
        ids
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawProfile {
    #[serde(default, alias = "userId")]
    pub user_id: u64,
    #[serde(default)]
    pub nickname: String,
    #[serde(default, alias = "avatarUrl")]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default, alias = "backgroundUrl")]
    pub background_url: Option<String>,
}

impl RawProfile {
    pub fn into_profile(self) -> UserProfile {
        UserProfile {
            user_id: self.user_id,
            nickname: if self.nickname.is_empty() { format!("User {}", self.user_id) } else { self.nickname },
            avatar_url: self.avatar_url,
            signature: self.signature,
            background_url: self.background_url,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawAccount {
    #[serde(default)]
    pub id: u64,
    #[serde(default, alias = "userName")]
    pub user_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub account: Option<RawAccount>,
    #[serde(default)]
    pub profile: Option<RawProfile>,
}

impl AccountResponse {
    pub fn into_profile(self) -> Option<UserProfile> {
        if let Some(profile) = self.profile {
            return Some(profile.into_profile());
        }
        self.account.map(|a| UserProfile {
            user_id: a.id,
            nickname: if a.user_name.is_empty() { format!("User {}", a.id) } else { a.user_name },
            avatar_url: None,
            signature: None,
            background_url: None,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawSearchResult {
    #[serde(default)]
    pub songs: Vec<RawSong>,
    #[serde(default, alias = "songCount")]
    pub song_count: usize,
    #[serde(default)]
    pub artists: Vec<RawArtist>,
    #[serde(default, alias = "artistCount")]
    pub artist_count: usize,
    #[serde(default)]
    pub albums: Vec<RawAlbum>,
    #[serde(default, alias = "albumCount")]
    pub album_count: usize,
    #[serde(default)]
    pub playlists: Vec<RawPlaylist>,
    #[serde(default, alias = "playlistCount")]
    pub playlist_count: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub result: RawSearchResult,
}

impl RawArtist {
    pub fn into_summary(self) -> ArtistSummary {
        ArtistSummary {
            id: self.id,
            name: if self.name.is_empty() { format!("Artist {}", self.id) } else { self.name },
            cover_url: self.pic_url,
            album_count: self.album_size,
            song_count: self.music_size,
        }
    }
}

impl RawAlbum {
    pub fn into_summary(self) -> AlbumSummary {
        let artist_name = self
            .artists
            .as_ref()
            .and_then(|a| a.first())
            .map(|a| a.name.clone())
            .or_else(|| self.artist.as_ref().map(|a| a.name.clone()))
            .unwrap_or_default();
        AlbumSummary {
            id: self.id,
            name: if self.name.is_empty() { format!("Album {}", self.id) } else { self.name },
            cover_url: self.pic_url,
            artist_name,
            track_count: self.size,
            publish_time_ms: self.publish_time,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawLyricText {
    #[serde(default)]
    pub lyric: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LyricResponse {
    #[serde(default)]
    pub lrc: Option<RawLyricText>,
    #[serde(default)]
    pub tlyric: Option<RawLyricText>,
    #[serde(default)]
    pub nolyric: bool,
    #[serde(default)]
    pub uncollected: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawStreamUrl {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub br: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub expi: u64,
    #[serde(default, rename = "type")]
    pub format: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub fee: i64,
    #[serde(default, alias = "freeTrialInfo")]
    pub free_trial_info: Option<Value>,
    #[serde(default, alias = "freeTrialPrivilege")]
    pub free_trial_privilege: Option<RawFreeTrialPrivilege>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawFreeTrialPrivilege {
    #[serde(default, alias = "cannotListenReason")]
    pub cannot_listen_reason: Option<i64>,
    #[serde(default, alias = "resConsumable")]
    pub res_consumable: bool,
    #[serde(default, alias = "userConsumable")]
    pub user_consumable: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub data: Vec<RawStreamUrl>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaylistDetailResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub playlist: Option<RawPlaylist>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserPlaylistsResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub more: bool,
    #[serde(default)]
    pub playlist: Vec<RawPlaylist>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SongDetailResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub songs: Vec<RawSong>,
    #[serde(default)]
    pub privileges: Vec<RawPrivilege>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ArtistSongsResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub songs: Vec<RawSong>,
    #[serde(default)]
    pub more: bool,
    #[serde(default)]
    pub total: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AlbumResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub album: Option<RawAlbum>,
    #[serde(default)]
    pub songs: Vec<RawSong>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FmResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub data: Vec<RawSong>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DailySongsResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub data: Option<DailySongsData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DailySongsData {
    #[serde(default, rename = "dailySongs")]
    pub daily_songs: Vec<RawSong>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecommendResourceResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub recommend: Option<Vec<RawPlaylist>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LikeListResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub ids: Vec<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QrKeyResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub unikey: Option<String>,
    #[serde(default)]
    pub data: Option<QrKeyData>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QrKeyData {
    #[serde(default)]
    pub unikey: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct QrCheckResponse {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub nickname: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_song_detail_fixture() {
        let resp: SongDetailResponse = serde_json::from_str(include_str!(
            "../tests/fixtures/song_detail.json"
        ))
        .unwrap();
        assert_eq!(resp.code, 200);
        let song = resp.songs.into_iter().next().unwrap().into_song();
        assert_eq!(song.id, 186016);
        assert_eq!(song.name, "晴天");
        assert_eq!(song.artists, vec!["周杰伦"]);
        assert_eq!(song.album, "叶惠美");
        assert!(song.duration_ms > 0);
        assert!(song.cover_url.is_some());
    }

    #[test]
    fn parses_search_fixtures() {
        let songs: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_song.json")).unwrap();
        assert!(songs.result.song_count > 0);
        assert!(!songs.result.songs.is_empty());
        let artists: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_artist.json")).unwrap();
        assert!(!artists.result.artists.is_empty());
        let albums: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_album.json")).unwrap();
        assert!(!albums.result.albums.is_empty());
        let playlists: SearchResponse =
            serde_json::from_str(include_str!("../tests/fixtures/search_playlist.json")).unwrap();
        assert!(!playlists.result.playlists.is_empty());
    }

    #[test]
    fn playlist_deduplicates_and_orders_ids() {
        let resp: PlaylistDetailResponse = serde_json::from_str(include_str!(
            "../tests/fixtures/playlist_detail.json"
        ))
        .unwrap();
        let raw = resp.playlist.unwrap();
        let ids = raw.ordered_track_ids();
        assert!(!ids.is_empty());
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
        let summary = raw.into_summary();
        assert_eq!(summary.name, "热歌榜");
        assert!(summary.track_count >= ids.len() as u64);
    }

    #[test]
    fn parses_fm_and_lyrics() {
        let fm: FmResponse =
            serde_json::from_str(include_str!("../tests/fixtures/fm.json")).unwrap();
        assert_eq!(fm.code, 200);
        assert!(!fm.data.is_empty());
        let song = fm.data.into_iter().next().unwrap().into_song();
        assert!(!song.artists.is_empty());
        let lyric: LyricResponse =
            serde_json::from_str(include_str!("../tests/fixtures/lyric.json")).unwrap();
        assert!(lyric.lrc.unwrap().lyric.contains("故事的小黄花"));
    }

    #[test]
    fn parses_stream_url_fixture() {
        let resp: StreamResponse =
            serde_json::from_str(include_str!("../tests/fixtures/url_v1.json")).unwrap();
        assert_eq!(resp.code, 200);
        let item = resp.data.into_iter().next().unwrap();
        assert_eq!(item.id, 186016);
        assert_eq!(item.code, 404);
        assert_eq!(item.expi, 1200);
    }

    #[test]
    fn handles_null_artist_arrays() {
        let raw: RawSong = serde_json::from_str(
            r#"{"id":1,"name":"x","ar":null,"artists":null,"al":null,"album":null,"dt":1000,"fee":0}"#,
        )
        .unwrap();
        let song = raw.into_song();
        assert!(song.artists.is_empty());
        assert_eq!(song.duration_ms, 1000);
    }
}
