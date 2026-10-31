# YoruPlay

A lightweight Rust terminal music player for NetEase Cloud Music.

YoruPlay has three independent surfaces over one authoritative background
service:

- **TUI** (Ratatui + Crossterm) for browsing and selecting music.
- **CLI** for scripting and day-to-day control.
- **MPRIS** for Noctalia Media and other Linux desktop media controllers.
- **Core daemon** (`yoruplay daemon`) owns the queue, NetEase session, stream
  URLs and the system `mpv` child process.

The TUI exits without stopping playback. Reopen it later and it reconnects to
the same daemon and queue.

## Features

- Native Rust NetEase Web API client (eapi + weapi, no Node.js at runtime).
- QR-code login, session reuse, cookie import, logout, expiry detection.
- Liked songs, created/subscribed playlists, full playlist tracks, server
  recently-played history, daily songs/playlists, Personal FM and Private Radar
  discovery.
- Search songs, artists, albums and playlists with pagination.
- Five modes: list repeat, track repeat, sequential, shuffle with short-repeat
  avoidance, and NetEase Heartbeat (心动模式) with playlist context.
- System `mpv` controlled only through JSON IPC; PulseAudio output through
  pipewire-pulse; `DISABLE_RTKIT=1` for the mpv child.
- MPRIS 2.2 (`org.mpris.MediaPlayer2.yoruplay`), including metadata, cover,
  position in microseconds, seek, volume, loop/shuffle mapping and plain-text
  lyrics in `xesam:asText`.
- Bounded on-disk cache, atomic state persistence, protected Unix socket,
  single-instance daemon, systemd user unit (not enabled by default).
- English/Chinese UI labels (`ui.language = "en" | "zh"`).

## Requirements

Arch Linux:

```sh
sudo pacman -S mpv pipewire pipewire-pulse wireplumber
```

Rust stable (1.85+) is enough to build. No root privileges are needed at
runtime. Do not run the daemon or TUI as root.

YoruPlay expects a working user D-Bus session and PipeWire/PulseAudio socket.
It never installs packages or changes audio/scheduling settings.

## Build

```sh
cargo build --release
install -Dm755 target/release/yoruplay ~/.local/bin/yoruplay
```

`Cargo.lock` is committed. `cargo fmt --check`, `cargo clippy` and `cargo test`
must pass before a release build.

## Install the user service

The daemon can be started manually:

```sh
yoruplay daemon
```

For a user service:

```sh
yoruplay service install     # writes ~/.config/systemd/user/yoruplay.service
systemctl --user start yoruplay
systemctl --user status yoruplay
```

`service install` intentionally does **not** enable login autostart. If you want
it:

```sh
systemctl --user enable yoruplay
```

The unit runs `yoruplay daemon` as your user, restarts on failure and is never a
system/root service. The reference unit is also in `systemd/yoruplay.service`.

## First login

```sh
yoruplay login
```

A Unicode QR is printed in the terminal. Scan it with the NetEase Cloud Music
app and confirm. For SSH or terminals too small for a QR, write an SVG:

```sh
yoruplay login --qr-svg ~/yoruplay-login.svg
```

Import an existing cookie as a fallback. The cookie must contain `MUSIC_U`:

```sh
printf '%s\n' "$NETEASE_COOKIE" | yoruplay login --cookie-stdin
```

Avoid passing cookies as command-line arguments; YoruPlay only accepts stdin
for this reason. Credentials are stored in
`$XDG_DATA_HOME/yoruplay/auth.json` with mode `0600`; the runtime directory is
`0700` and the IPC/mpv sockets are `0600`.

Logout removes the saved session:

```sh
yoruplay logout
```

## Use

Open the TUI:

```sh
yoruplay
# or: yoruplay tui
```

The TUI auto-starts a daemon if one is not already running. It auto-connects to
an existing service. Closing the TUI (`q`) does not stop playback.

Common keys:

| Key | Action |
|---|---|
| `j`/`k`, arrows | Move |
| `Tab`, `h`/`l` | Switch navigation/content pane |
| `Enter` | Open / play selected |
| `Esc` | Back |
| `/` | Search |
| `Space` | Pause / resume |
| `n` / `p` | Next / previous |
| `m` | Cycle mode |
| `a` / `A` | Add selected / add all to queue |
| `r` | Refresh current view |
| `L` | Login/account panel |
| `?` | Help |
| `q` | Close TUI only |

On narrow terminals the two-pane layout folds into one column. Large queues and
playlists are rendered as a virtual viewport; only visible rows are built.

CLI examples:

```sh
yoruplay status
yoruplay play
yoruplay pause
yoruplay next
yoruplay previous
yoruplay mode intelligence
yoruplay mode                # cycle modes
yoruplay volume 65
yoruplay seek +10
yoruplay queue
yoruplay search "晴天" --kind songs
yoruplay playlist 3778678
yoruplay start-playlist 3778678 --start 0
yoruplay add-playlist 3778678
yoruplay daily
yoruplay daily-playlists --refresh
yoruplay radar
yoruplay fm
yoruplay liked
yoruplay recent
yoruplay cache clear
yoruplay doctor
```

`--json` prints the daemon response as JSON for scripting:

```sh
yoruplay --json status
yoruplay --json search "Jay" --kind artists
```

## MPRIS / Noctalia

The daemon is the only MPRIS owner; mpv does not register a second player.
Use Noctalia Media or any MPRIS client to play/pause, seek, change volume and
skip tracks. Metadata includes title, artists, album, cover URL, duration,
stable track ID and the current plain-text lyrics (`xesam:asText`) when
available. Lyrics failures never affect playback.

MPRIS loop/shuffle is only an approximation for the five internal modes:

| Internal mode | LoopStatus | Shuffle |
|---|---|---|
| List repeat | `Playlist` | false |
| Track repeat | `Track` | false |
| Sequential | `None` | false |
| Shuffle | `Playlist` | true |
| Heartbeat | `Playlist` | false |

The full internal mode is always retained. Heartbeat cannot be selected through
standard MPRIS because MPRIS has no such property; use the TUI/CLI.

## Configuration and data

XDG paths:

```text
~/.config/yoruplay/config.toml
~/.local/share/yoruplay/auth.json       # 0600
~/.local/share/yoruplay/state.json      # 0600, queue/restore state
~/.cache/yoruplay/                      # bounded cache + logs
$XDG_RUNTIME_DIR/yoruplay/              # 0700: socket, lock, mpv socket
```

Default config:

```toml
[api]
quality = "exhigh"       # standard | higher | exhigh | lossless | hires
timeout_secs = 15

[ui]
language = "en"          # en | zh

[playback]
default_mode = "list_loop"
volume = 0.7
restore_queue = true

[cache]
max_mb = 32
lyrics_ttl_hours = 720

[log]
level = "info"
```

Daily recommendations and lyrics are cached briefly (6 hours for daily data,
configurable TTL for lyrics); no audio files are cached. `yoruplay cache clear`
or `Cache::ClearCache` removes the cache. The daemon trims cache files to
`cache.max_mb` after writes.

The JSON cache and state files are written atomically. A corrupt state file is
moved aside as `state.json.corrupt-<timestamp>` so the daemon can still start.

## Logs

With the systemd service:

```sh
journalctl --user -u yoruplay -f
```

When the TUI auto-started the daemon:

```sh
tail -f ~/.cache/yoruplay/yoruplay.log
```

TUI diagnostics are in `~/.cache/yoruplay/tui.log`. Cookies and tokens are not
logged.

## Update

```sh
git pull
cargo build --release
install -Dm755 target/release/yoruplay ~/.local/bin/yoruplay
systemctl --user restart yoruplay   # if using the service
yoruplay service install            # refresh the unit if needed
```

## Uninstall

```sh
yoruplay uninstall --yes
# or remove the executable too when it lives under $HOME:
yoruplay uninstall --yes --remove-binary
```

This removes the user unit, runtime socket/lock and cache. It never deletes
account data or configuration without an explicit manual action; the command
prints the preserved paths. To remove them yourself:

```sh
rm ~/.local/share/yoruplay/auth.json ~/.local/share/yoruplay/state.json
rm ~/.config/yoruplay/config.toml
```

## Testing

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Live read-only API checks (low frequency):

```sh
cargo test -p yoruplay-api --test live_smoke -- --ignored --nocapture
```

The integration tests spawn the real daemon with isolated XDG directories and
exercise two simultaneous IPC clients, event fan-out, permissions, single
instance enforcement and graceful shutdown. The mpv test uses `--ao=null` and a
generated silent WAV; normal playback uses `--ao=pulse`.

## Documentation

- [`docs/architecture.md`](docs/architecture.md)
- [`docs/upstream-api-map.md`](docs/upstream-api-map.md)
- [`docs/development.md`](docs/development.md)
- [`docs/resource-usage.md`](docs/resource-usage.md)
- [`docs/known-issues.md`](docs/known-issues.md)

The NetEase API implementation is a selective native Rust port of
NeteaseCloudMusicApiEnhanced/api-enhanced (MIT). The upstream checkout under
`upstream/` is a local reference only and is ignored by Git.
