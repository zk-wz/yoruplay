# Upstream API map

Reference repository:
`https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced`

- Last reviewed commit: `984cc021b8598cf94c22134f32a8426887fb2de7`
  (2026-10-25, "Merge pull request #295 ...")
- License: MIT (original NeteaseCloudMusicApi copyright 2013-2022 Binaryify)
- Local checkout: `upstream/api-enhanced/` (ignored by Git, never modified)
- Runtime dependency: **none**. Node.js is not used, required or shipped.

The port covers only endpoints YoruPlay needs. The Rust code lives in
`crates/api`; response fixtures are in `crates/api/tests/fixtures/`.

## Protocol layer

| Rust | Upstream | Notes |
|---|---|---|
| `crypto::weapi_encrypt` | `util/crypto.js` `weapi` | AES-128-CBC twice + raw RSA (no padding). Secret is reversed before raw RSA. |
| `crypto::eapi_encrypt` | `util/crypto.js` `eapi` | AES-128-ECB, uppercase hex, `nobody{url}use{json}md5forencrypt`. |
| `crypto::random_weapi_secret` | `weapi` | Base62, 16 characters. |
| `client::NeteaseClient::post_json` | `util/request.js` | Reuses one reqwest pool, form encoding, cookie handling, eapi/weapi only. |
| `session::SessionData` | cookie helpers in `util/request.js` / `util/index.js` | Device identity, `MUSIC_U`, `__csrf`, `NMTID`, atomic 0600 persistence. |
| `client::QrLogin` / QR URL | `login_qr_create` | URL assembled locally; no `qrcode` dependency on Node. ANSI/SVG QR rendered by YoruPlay. |

Deliberate differences:

- No `api`, `linuxapi`, `xeapi`, `neapi`, anonymous-token refresh, anti-cheat
  token, proxy or JS unblocking layers. YoruPlay uses eapi/weapi, which the
  live tests validated.
- Response encryption (`e_r` / `x-aeapi`) is not enabled, so plain JSON is
  expected and no response decryption is needed.
- Cookies are never serialized into logs, fixtures or tests.

## Endpoint modules

| Rust method | Upstream module | Endpoint | Crypto | Status / differences |
|---|---|---|---|---|
| `start_qr_login` | `login_qr_key.js` | `/api/login/qrcode/unikey` | eapi | Live-tested waiting state. |
| `poll_qr_login` | `login_qr_check.js` | `/api/login/qrcode/client/login` | eapi | Handles 800/801/802/803, profile fetch, session persistence. |
| `account`, `validate_session` | `user_account.js` | `/api/nuser/account/get` | weapi | Live-tested. |
| `playlists(Created/Subscribed)` | `user_playlist.js` | `/api/user/playlist` | weapi | Paginates with `more`; splits by `subscribed`; hides specialType 5 from Created. |
| `playlist_tracks` | `playlist_detail.js`, `playlist_track_all.js`, `song_detail.js` | `/api/v6/playlist/detail`, `/api/v3/song/detail` | eapi/eapi | Uses the ordered/deduplicated `trackIds`; chunked song details (500); 5-minute in-memory detail cache. |
| `liked_songs` | `likelist.js` + playlist detail | `/api/song/like/get` | eapi | Prefers the "liked music" playlist for order; falls back to unordered likelist. |
| `daily_songs` | `recommend_songs.js` | `/api/v3/discovery/recommend/songs` | weapi | Preserves dailySongs server order; 6h cache in the daemon. |
| `daily_playlists` | `recommend_resource.js` | `/api/v1/discovery/recommend/resource` | weapi | 6h cache. |
| `radar` (service) | no upstream module | `/api/v1/discovery/recommend/resource` | weapi | Discovers a playlist whose name contains 私人雷达/Private Radar. Never hard-codes an ID. Explicit error if absent. |
| `personal_fm` | `personal_fm.js` | `/api/v1/radio/get` | weapi | Starts a durable Personal FM source; refills only when ≤1 track remains. |
| `recent_songs` | `record_recent_song.js`, fallback `user_record.js` | `/api/play-record/song/list`, `/api/v1/play/record` | weapi | Server history only. Handles full-song or ID-only payloads. Label clearly says it is server data, not local YoruPlay history. |
| `search` | `cloudsearch.js` | `/api/cloudsearch/pc` | eapi | Songs/artists/albums/playlists; `total:true`; pages capped at 100 by YoruPlay. |
| `artist_songs` | `artist_songs.js` | `/api/v1/artist/songs` | eapi | Used when opening an artist search result. |
| `album_songs` | `album.js` | `/api/v1/album/{id}` | weapi | Used when opening an album search result. |
| `stream_url` | `song_url_v1.js` | `/api/song/enhance/player/url/v1` | eapi | Upstream uses xeapi for this module; live eapi works and avoids xeapi key/bootstrap state. Tries requested quality, then exhigh, then standard. Reports trial segments. |
| `lyrics` | `lyric.js` | `/api/song/lyric` | eapi | Cached; failures are non-fatal; MPRIS gets timestamp-stripped text. |
| `intelligence` | `playmode_intelligence_list.js` | `/api/playmode/intelligence/list` | eapi | `type=fromPlayOne`, playlist ID + seed + current song; response parser accepts `songInfo`/`song` variants. Requires real account context. |
| `import_cookie` | local extension | `/api/nuser/account/get` validation | weapi | Cookie is accepted only via stdin, never an argument. |
| `logout` | local extension | none | - | Clears in-memory cookies and removes auth.json. |

## Verification status

- Unit tests pin eapi/weapi ciphertext vectors generated from a development-time
  Node reference, cookie parsing/encoding, persistence permissions, fixture
  deserialisation and queue/mode state machines.
- Anonymous live smoke: QR key/poll, song detail, search (all four types),
  public playlist tracks, lyrics. See `crates/api/tests/live_smoke.rs`.
- Anonymous live verification succeeded for: QR key/poll/expiry, song detail,
  search (songs/artists/albums/playlists), public playlist tracks, lyrics,
  stream URL for free/trial tracks, and actual mpv playback through
  pipewire-pulse.
- **Real-account verification was not completed**: no QR scan was confirmed in
  this environment. Personal lists, daily data, Personal FM, Private Radar,
  recently played, Heartbeat recommendation, and account-quality streams remain
  implemented but not live-verified. This is recorded in
  `docs/resource-usage.md` and the final delivery report.

## Updating from upstream

1. Update `upstream/api-enhanced/` to a new commit (do not commit it).
2. Compare the modules listed above with the Rust client.
3. Check request path, crypto mode, parameter names/serialisation, cookie
   requirements and response shape. Look at `util/request.js` for any crypto
   bootstrap changes.
4. Update fixtures from sanitized, small real responses; never include a real
   cookie, token or account identifier.
5. Update this table's commit SHA and run `cargo test --workspace` plus the live
   smoke test. Real-account endpoints require the account owner's QR scan.
