# Known issues and limitations

- **Firefox-only automatic login**: YoruPlay reads the plaintext
  `cookies.sqlite` used by Firefox (Arch, upstream, Snap and Flatpak profiles).
  Chrome, Chromium, Brave and Edge encrypt cookies through a keyring, so they
  are not read automatically. Use `yoruplay auth login --cookie-stdin` for those
  browsers. See `docs/login.md`.
- **Firefox database lock**: Firefox keeps `cookies.sqlite` open while running.
  YoruPlay copies the database plus its WAL/SHM sidecars to a private temporary
  directory, queries only `MUSIC_U`/`__csrf` and related NetEase cookies, then
  deletes the copy. Cookies are never logged.
- **Real-account verification status**: an existing Firefox `MUSIC_U` session
  was imported with `yoruplay auth login --cookie-stdin`; the account profile,
  created/subscribed playlists, paged liked songs, daily songs/playlists,
  Personal FM, recent history, search, stream URL and lyrics were verified
  read-only with `crates/api/tests/live_account.rs`. Heartbeat returned
  `不支持该歌单类型` for the tested created playlist, so its real-account path
  remains unverified.
- **Liked songs are paged**: the TUI loads 100 songs per page and the CLI
  accepts `--offset/--limit`; playback still resolves the complete queue, but
  song-detail batches are paced at 250 ms and no full-list song cache is kept.
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
- **Audio routes**: output uses cpal's system ALSA devices. Install `pipewire-alsa`
  or a PulseAudio ALSA plugin; a native Pulse socket alone does not create an ALSA
  device named `pulse`. Empty `playback.audio_device` prefers shared routes.
  Explicit missing devices fail rather than silently taking exclusive hardware.
- **Device/network transitions**: API/CDN/device recovery shares a configurable
  retry budget (three retries, ten seconds apart by default). Local device
  recovery reuses the original local inode without network. Real Bluetooth
  unplug/replug, suspend/resume and a live 20-minute URL expiry are not verified.
- **Format coverage**: MP3, FLAC, M4A/AAC, common ADTS AAC, Ogg/Vorbis and WAV/PCM
  are supported; this is not mpv's full codec catalogue. Symphonia 0.5.5 ADTS
  supports common MPEG-4/no-CRC `fff1`, not every MPEG-2/CRC/multiple-block variant.
  No Opus, ALAC, NCM or DRM/decryption fallback is included.
- **Embedded tags**: Ogg retains its native audio bitstream without rewriting
  embedded comments; local lyrics/artwork remain in the managed index/files.
  Tag-write failures in other containers also preserve verified audio rather
  than discard it. Cache playback does not fetch missing metadata online.
- **Trial streams**: `freeTrialInfo` URLs are skipped before the media GET, even
  in repeat modes. They are not extended, cached or played as previews.
- **Local-only boundary**: no online lyrics/artwork or automatic FM/Heartbeat
  refill during cached playback. Missing local metadata remains missing; an
  exhausted local queue stops. Explicit online browsing/upgrades are still online.
- **Cache filesystem**: Linux `O_TMPFILE` and unprivileged link publication are
  required. Unsupported filesystems fail explicitly; no named partial fallback.
  Avoid tmpfs/ramfs: anonymous downloaded bytes still consume RAM there.
- **Quota and pins**: reducing the limit can remain over budget while the current
  inode is pinned. Accounting reports this and trims on release. Explicit clear
  unlinks preferred entries without interrupting an already open reader.
- **Quality upgrade**: a lower requested tier or a server fallback cannot replace
  a superior cached entry. Missing/unrecognized actual quality is not guessed.
  No live lossless/hi-res entitlement was available in the checked account;
  FLAC/AAC format playback was verified with generated fixtures.
- **Platform limits**: at most 32 decoded channels, 64 MiB PCM ring and a bounded
  decoded packet. Large user-selected buffers/high-rate multichannel content can
  exceed those limits and report an error instead of allocating without bound.
- **Private/cloud upload edge cases**: account-specific restricted tracks may
  return no playable URL; this is surfaced as an unplayable-track error.
- **God-mode/other clients**: YoruPlay does not implement recent upstream
  crypto layers (xeapi/neapi) or unblocking. If a future server change requires
  them for an endpoint, that row of the API map must be revisited.
