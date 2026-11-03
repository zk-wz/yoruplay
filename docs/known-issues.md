# Known issues and limitations

- **QR risk control (NetEase code 8821 / "设备环境异常")**: YoruPlay now uses
  the current official web flow (`/st/platform/scanlogin`, weapi `type=1`,
  web `chainId`, web login headers). The previous `/login?codekey` URL and
  Android `deviceId` chain were obsolete and caused the phone to reject the
  scan before the API saw 802/803. NetEase can still reject a login based on IP
  or device environment; Clash TUN/proxy exit IPs are a common cause. This is
  not an account ban. If a scan still fails, use direct routing for NetEase
  domains or import a `MUSIC_U` cookie from the official web login. See
  `docs/login-protocol.md`.
- **Web QR fingerprint token**: the official page sends `ydDeviceToken` from
  NetEase's JavaScript fingerprint SDK. YoruPlay has no JS runtime and sends an
  empty value (the same fallback used by other native clients when unavailable).
  If NetEase makes a non-empty token mandatory, QR login will need a WebView or
  browser helper; cookie import remains available.
- **Real-account verification status**: an existing Firefox `MUSIC_U` session
  was imported with `yoruplay login --cookie-stdin`; the account profile,
  created/subscribed playlists, liked songs, daily songs/playlists, Personal FM,
  recent history, search, stream URL and lyrics were verified read-only with
  `crates/api/tests/live_account.rs`. The QR success callback itself is still
  unverified. Heartbeat returned `不支持该歌单类型` for the tested created
  playlist, so its real-account path remains unverified.
- **Private Radar variability**: the radar playlist appears in some
  `/api/v1/discovery/recommend/resource` responses but not every call. The
  daemon caches the daily playlist list; the direct API test can see a later
  response without the radar entry and report it as absent.
- **Private Radar discovery**: there is no separate radar module in the current
  upstream API. YoruPlay searches the account's daily recommended playlists for
  a name containing `私人雷达` / `Private Radar`. If NetEase changes that naming
  or stops including it, YoruPlay reports an explicit error instead of guessing
  a playlist ID.
- **Heartbeat context**: Heartbeat requires an original playlist context. It is
  rejected when the current queue comes from search results, daily songs or
  Personal FM, rather than silently becoming shuffle.
- **Heartbeat response**: the upstream endpoint's response shape is not
  documented. YoruPlay accepts the known `songInfo`/`song` shapes; a future
  server change needs a parser update and real-account test.
- **Recently played**: uses an undocumented server endpoint and falls back to
  the play-record ranking endpoint. It is server history, not a complete local
  history. Entries can be missing if the server omits unavailable tracks.
- **Search page size**: capped at 100 per page by the daemon; the TUI loads more
  as the selection approaches the end.
- **MPRIS does not represent Heartbeat**: LoopStatus/Shuffle are only an
  approximation. The internal mode is retained; select Heartbeat via CLI/TUI.
- **Lyrics**: no scrolling lyric view is implemented (by design). MPRIS exposes
  timestamp-stripped `xesam:asText` when available; Noctalia may choose not to
  display it.
- **Audio device changes**: mpv/PipeWire follows the default sink. A Bluetooth
  disconnect can cause an mpv stream error; YoruPlay retries the URL once and
  shows an error if audio still fails. Device switching is not explicitly
  orchestrated.
- **mpv cache**: reduced to 16 MiB forward / 4 MiB back / 60 s to meet the
  memory target. Very unstable networks may prefer a larger cache; this is
  currently a compiled default, not a config option.
- **Sleep/wake and network changes**: no background network poller. A stale
  stream is refreshed on resume/error; the next user action retries API calls.
- **Trial streams**: when the account can only play a preview, the daemon
  reports a trial warning but cannot extend it.
- **QR terminal size**: a QR needs roughly 40+ columns and 25+ rows. Use
  `yoruplay login --qr-svg <path>` in tiny terminals or over SSH.
- **No local audio cache**: YoruPlay does not persist audio files; stream URL
  expiry is handled by refresh.
- **Private/cloud upload edge cases**: account-specific restricted tracks may
  return no playable URL; this is surfaced as an unplayable-track error.
- **God-mode/other clients**: YoruPlay does not implement recent upstream
  crypto layers (xeapi/neapi) or unblocking. If a future server change requires
  them for an endpoint, that row of the API map must be revisited.
