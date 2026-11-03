# YoruPlay

A lightweight Rust terminal music player for NetEase Cloud Music.

YoruPlay has three independent surfaces over one authoritative background
daemon:

- **TUI** (Ratatui + Crossterm) for browsing and selecting music.
- **CLI** for scripting and day-to-day control.
- **MPRIS** for Noctalia Media and other Linux desktop media controllers.
- **Core daemon** (`yoruplay daemon start`) owns the queue, NetEase session, stream
  URLs and the system `mpv` child process.

The TUI exits without stopping playback. Reopen it later and it reconnects to
the same daemon and queue.

## Features

- Native Rust NetEase Web API client (eapi + weapi, no Node.js at runtime).
- Browser-assisted Firefox login, Cookie import, session reuse, logout and
  expiry detection.
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
  single-instance daemon, protected IPC socket, graceful shutdown.
- English/Chinese UI labels (`ui.language = "en" | "zh"`).

## Requirements

Arch Linux:

```sh
sudo pacman -S mpv pipewire pipewire-pulse wireplumber sqlite
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

Or use the helper, which checks mpv and installs the binary:

```sh
./scripts/install.sh
```

`Cargo.lock` is committed. `cargo fmt --check`, `cargo clippy` and `cargo test`
must pass before a release build.

## Run the daemon

The TUI auto-starts a daemon if none is running. To start it explicitly:

```sh
yoruplay daemon start
```

`yoruplay daemon` without a subcommand prints the daemon help tree. YoruPlay
does not install a systemd unit, autostart entry, or any other system
registration. The binary is installed at `~/.local/bin/yoruplay`; command
resolution depends only on your existing `PATH`.

Stop the background daemon at any time:

```sh
yoruplay daemon stop
```

This sends a graceful IPC shutdown, saves the queue and reaps mpv. It is
idempotent when nothing is running. `yoruplay control stop` only stops playback.
Closing the TUI with `q` does not stop the daemon.

## First login

```sh
yoruplay auth login
```

If Firefox already has a valid `MUSIC_U` session, this imports it immediately
without opening a browser. It validates the session and writes `auth.json` with
mode `0600`. Only when no valid Firefox session exists does it open
`https://music.163.com/login` and wait for login.

To change accounts, force the browser flow:

```sh
yoruplay auth login --switch-account   # alias: --force
```

This opens the official page and waits until Firefox holds a different account
cookie than the current one.

Firefox is the only browser read automatically. Its profiles are discovered in
the Arch (`~/.config/mozilla/firefox`), upstream, Snap and Flatpak locations.
Chrome/Chromium/Brave/Edge encrypt their cookie databases with a keyring, so use
the manual path for those.

In the TUI, press `L` to open the login panel, then `b` to open the official
page and `i` to import the Firefox cookie.

Manual Cookie import (any browser):

```sh
yoruplay auth login --cookie-stdin
# paste MUSIC_U=...; __csrf=... and press Ctrl-D
```

Or from a protected file:

```sh
chmod 600 ~/netease-cookie.txt
yoruplay auth login --cookie-stdin < ~/netease-cookie.txt
```

The header must contain `MUSIC_U=...`; `__csrf=...` is recommended. Do not put
cookies in shell history or chat. YoruPlay only accepts stdin for this reason.
Credentials are stored in `$XDG_DATA_HOME/yoruplay/auth.json` with mode `0600`;
the runtime directory is `0700` and the IPC/mpv sockets are `0600`.

Full details, the Firefox profile list and the manual extraction procedure are
in [`docs/login.md`](docs/login.md).

Logout removes the saved session:

```sh
yoruplay auth logout
```

## Use

Open the TUI:

```sh
yoruplay tui
```

Running `yoruplay` without a command prints the command tree. The TUI
auto-starts a daemon if one is not already running. It auto-connects to
an existing daemon. Closing the TUI (`q`) does not stop playback.

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
| `b` / `i` | Open login page / import Firefox cookie (inside login panel) |
| `?` | Help |
| `q` | Close TUI only |

On narrow terminals the two-pane layout folds into one column. Large queues and
playlists are rendered as a virtual viewport; only visible rows are built.

CLI examples:

```sh
yoruplay daemon start
yoruplay daemon status
yoruplay daemon stop
yoruplay control resume
yoruplay control pause
yoruplay control next
yoruplay control previous
yoruplay control mode intelligence
yoruplay control mode             # cycle modes
yoruplay control volume 65
yoruplay control seek +10
yoruplay queue list
yoruplay search "晴天" --kind songs
yoruplay library playlist 3778678
yoruplay queue start-playlist 3778678 --start 0
yoruplay queue add-playlist 3778678
yoruplay library daily
yoruplay library daily-playlists --refresh
yoruplay library radar
yoruplay library fm
yoruplay library liked
yoruplay library recent
yoruplay cache clear
yoruplay doctor
```

`--json` prints the daemon response as JSON for scripting:

```sh
yoruplay --json daemon status
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
configurable TTL for lyrics); no audio files are cached yet. `yoruplay cache
clear` removes the metadata cache. A song cache is designed but not implemented;
see [`docs/song-cache.md`](docs/song-cache.md). The daemon trims cache files to
`cache.max_mb` after writes.

The JSON cache and state files are written atomically. A corrupt state file is
moved aside as `state.json.corrupt-<timestamp>` so the daemon can still start.

## Logs

The daemon logs to `~/.cache/yoruplay/yoruplay.log` when started by the TUI/CLI.
The daemon logs to the file below:

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
yoruplay daemon stop                # stop any running daemon
yoruplay daemon start               # start the daemon
# or open the TUI with `yoruplay tui`
```

## Uninstall

```sh
yoruplay uninstall --yes
# or remove the executable too when it lives under $HOME:
yoruplay uninstall --yes --remove-binary
```

This removes the runtime socket/lock and cache. It never deletes
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

`scripts/measure-pss.sh [pid...]` prints PSS/RSS/threads/CPU for the daemon, TUI
and any passed mpv PID.

The NetEase API implementation is a selective native Rust port of
NeteaseCloudMusicApiEnhanced/api-enhanced (MIT). The upstream checkout under
`upstream/` is a local reference only and is ignored by Git.
