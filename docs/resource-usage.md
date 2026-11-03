# Resource usage and verification measurements

Environment: Arch Linux rolling, linux-surface kernel 7.2.11, Surface Pro 6
(i5-8250U, 8 GiB RAM), Niri + Noctalia, PipeWire 1.6.10 with pipewire-pulse,
mpv 0.41.0. Release build (`cargo build --release`, thin LTO, stripped).

PSS is read from `/proc/<pid>/smaps_rollup`; per-process PSS divides shared
pages, so the total is a useful approximation, not a naive RSS sum.
`scripts/measure-pss.sh [pid...]` prints the same fields.

## Idle and browsing (release)

| Scenario | Daemon PSS / RSS | TUI PSS / RSS | Threads (daemon/TUI) | Total PSS | Notes |
|---|---:|---:|---:|---:|---|
| Daemon idle, no mpv | **6.7 MiB** / 9.9 MiB | — | 9 / — | ≈ 7 MiB | Earlier anonymous, empty queue; retained as historical baseline |
| Daemon idle, logged-in account + restored 10-track queue | **10.3–10.8 MiB** / 13.6–14.1 MiB | — | 10 / — | ≈ 11 MiB | Current service state; no mpv |
| Daemon + TUI open | 7.0 MiB / 14 MiB | **3.2 MiB** / 9.2 MiB | 10 / 10 | **≈ 10 MiB** | 120×36 terminal, login overlay dismissed |
| 200-track public playlist loaded | **19.7 MiB** / 22.5 MiB | — | 10 / — | ≈ 20 MiB | 热歌榜, full song details; allocator retains memory |
| Search (50 results) | **18.6 MiB** / 21.3 MiB | — | 10 / — | ≈ 19 MiB | |
| Cache after browsing | — | — | — | — | 60 KiB of lyrics, no audio |

Idle CPU: 0.0–0.3% of one core. No mpv process is started until playback.

## Playback (release, systemd user service)

Measured while playing a real NetEase stream through mpv/PipeWire:

| Scenario | Daemon PSS / RSS | mpv PSS / RSS | TUI PSS | Total PSS | CPU / threads |
|---|---:|---:|---:|---:|---|
| 10-song account queue, normal stream | **10.8 MiB / 14.2 MiB** | **56.4 MiB / 81.1 MiB** | — | **≈ 67.2 MiB** | mpv cache 8 MiB forward / 2 MiB back / 30 s; daemon 0.1%, mpv ~8% during start |
| Trial/preview stream (30 s) | 11.3 MiB / 14.0 MiB | 57.1 MiB / 77.6 MiB | — | 68.4 MiB | mpv startup spike |
| Same playback + TUI open | 7.0 MiB / 14 MiB | 57.2 MiB / 77.7 MiB | 3.2 MiB | **≈ 67.4 MiB** | TUI adds 10 threads |
| TUI closed, playback continues | 2.2 MiB / 5.8 MiB | 63.5 MiB / 83.9 MiB | — | 65.7 MiB | Verified: mpv keeps running |
| Long pause (30 s, position frozen) | unchanged | unchanged | — | 81.6 MiB | MPRIS Position exactly stable; resume works |
| Stop / queue clear while playing | process stays | reaped immediately | — | — | mpv exit is intentional; no error state; new Play respawns it |
| Service stop while playing | process exits | reaped | — | — | socket removed, no orphan mpv |
| Service restart | queue restored (50/200 tracks), current track/position/mode retained | starts only on Play | — | — | Verified 200-track and 50-track queues |

The target of <100 MiB total PSS during normal playback is comfortably met.
With the reduced cache, the measured total is ≈67 MiB (mpv 56.4 + daemon 10.8).
mpv's cache is now 8 MiB forward / 2 MiB back / 30 s, down from the earlier
16 MiB / 4 MiB / 60 s setting. Playlist metadata is capped at 8 entries and
20,000 tracks per entry, so browsing very large playlists cannot grow the daemon
without limit.

`systemd` `MemoryCurrent` was ≈94 MiB during playback; that includes page cache
and cgroup accounting, while the PSS sum is ≈66 MiB.

## Functional checks performed

- Real NetEase stream URL fetched and played through system mpv and
  pipewire-pulse (`application.name = YoruPlay`, `client.api = pipewire-pulse`).
- mpv command line uses `--ao=pulse`, `DISABLE_RTKIT=1`, no config, no video,
  protected JSON IPC socket; cache is 8 MiB forward / 2 MiB back / 30 s.
- Explicit Stop and queue clear reap mpv immediately while keeping the daemon,
  queue and MPRIS available; the next Play respawns mpv.
- MPRIS metadata: title, artist array, album, album artist, cover, duration,
  position in microseconds, stable `/org/yoruplay/track/<id>`, lyrics in
  `xesam:asText`.
- MPRIS Play/Pause/PlayPause/Stop/Next/Previous/Seek/SetPosition/Volume, wrong
  track-ID rejection, LoopStatus and Shuffle round-trips.
- Automatic end: list repeat advances, track repeat loops the same file
  (`loop-file=inf`), sequential advances then stops at the last queue entry.
- Shuffle next/previous uses history; rapid next commands leave a consistent
  final track.
- Queue and mode/volume persistence across daemon restart; service stop reaps
  mpv and removes the socket; service restart restores queue and position.
- TUI search (anonymous), queue view, help overlay, login panel,
  responsive single-column layout, exit while playing, reconnection to the
  running daemon, English/Chinese label unit checks.
- Unauthenticated guard: Heartbeat with a public playlist context is accepted,
  then the login-required API error is shown explicitly; no silent shuffle
  fallback.

## Not completed

- **Real NetEase account login**: verified by importing a Firefox web session.
  The account profile, playlists, liked songs, daily data, Personal FM, recent
  history, search, stream URL and lyrics were verified read-only. Heartbeat
  returned “不支持该歌单类型” for the tested playlist; Private Radar is not
  present in every daily recommendation response.
- QR login was removed; it is not a supported path.
- Session expiry against the live server was not reproduced.
- URL-expiry refresh after a 20-minute pause, live network loss/recovery, Wi-Fi
  switch, suspend/resume, Bluetooth disconnect/reconnect were not reproduced in
  this session.
- Noctalia's visual media widget was not screen-captured; the MPRIS interface
  was verified directly over D-Bus.
