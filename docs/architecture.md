# Architecture

```text
TUI (Ratatui) ─┐
CLI (clap) ─────┼── Unix socket ($XDG_RUNTIME_DIR/yoruplay/yoruplay.sock)
MPRIS (zbus) ───┘            │
                             ▼
                    YoruPlay daemon
          ┌──────────────┼────────────────┐
          ▼              ▼                ▼
   NetEase API       Queue + mode      mpv JSON IPC
   native Rust       state machine     system mpv
          │              │                │
          └──── stream URL ───────────────┘
                             │
                       pipewire-pulse
```

## Crates

| Crate | Responsibility |
|---|---|
| `yoruplay-protocol` | Shared models, config, XDG paths, versioned IPC messages/events. |
| `yoruplay-api` | Native NetEase HTTP client, crypto, cookies/session, typed raw responses. |
| `yoruplay-ipc` | Async Unix-socket client used by TUI and CLI. |
| `yoruplay-core` | Queue, mpv controller, persistence, cache, daemon socket server, MPRIS. |
| `yoruplay-tui` | Ratatui UI, responsive layout, QR login, i18n labels. |
| `yoruplay` (binary) | CLI dispatch, logging, daemon auto-start, service install/uninstall. |

The API crate never touches playback. The TUI never touches mpv or NetEase
directly. MPRIS calls the same `Daemon::handle_command` path as the TUI/CLI, so
there is one authoritative queue.

## Process and task model

One daemon process, one Tokio runtime:

- accept loop for IPC connections;
- one reader + writer per client connection;
- a per-connection event forwarder reading a broadcast channel;
- daemon-owned ticker (1 Hz) for playback position events;
- one mpv event forwarder per spawned mpv process and one central mpv event
  handler task;
- MPRIS interface registration and one property-watcher task;
- detached lyrics-fetch tasks with cache lookup;
- mpv is spawned lazily on first playback and killed on daemon shutdown.

The daemon holds an exclusive `flock` on `$XDG_RUNTIME_DIR/yoruplay/yoruplay.lock`.
The socket and lock are in a `0700` directory; the socket is `0600`.

## Queue invariants

- `Queue` is a pure state machine; only the daemon mutates it.
- `current` always indexes `items`, or is `None` when the queue is empty.
- One `play_lock` serialises playback-affecting commands (play/pause/next/prev/
  seek/mode/queue mutations).
- A monotonically increasing generation counter invalidates stale URL fetches:
  rapid track changes cannot load an older track after a newer selection.
- A URL is fetched for every new load. The URL expiry timestamp is stored;
  resuming a long pause near expiry reloads from the current position. An mpv
  stream error triggers one URL refresh and retry, then a visible error.
- When mpv exits unexpectedly the daemon keeps the queue, marks mpv unavailable
  and resumes control on the next Play request.

Mode behaviour:

| Mode | Auto end | Manual Next | Previous |
|---|---|---|---|
| List repeat | wrap | next with wrap | history, then previous with wrap |
| Track repeat | mpv `loop-file=inf` | next track | history, then previous with wrap |
| Sequential | stop at last | stop at last | history, then previous if any |
| Shuffle | random with an 8-track short-repeat history | same | playback history |
| Heartbeat | API recommendation using playlist context | same | playback history |

Shuffle uses a recency window and falls back only when the whole queue is
exhausted. Heartbeat requires a playlist context and never silently falls back
to shuffle; an explicit error is shown if the current queue has no context.

## IPC contract

Newline-delimited JSON with a 64 MiB frame cap. The first client frame is
`Hello { version }`; the server replies with `Hello { version, daemon_version }`.
Each request has a numeric ID. Responses carry either a typed result or a stable
error code/message/hint. Events are pushed (playback, queue, auth, notice,
shutdown); clients can also call `GetState` to resynchronise. Slow clients drop
events rather than blocking playback.

Commands cover state, login, library browsing, search, playback, seek, volume,
mode, queue mutations, cache clear and shutdown. Playback commands return only
after the daemon has accepted the state update; network loads are serialised and
superseded loads are discarded. `yoruplay shutdown` sends `Shutdown` through the
normal client, with a raw version-tolerant handshake for an older daemon during
an upgrade, then waits for the socket to disappear.

## Persistence and cache

- `state.json`: queue items, current index, source, mode, volume, position.
  Atomic write, 0600. Loaded only when `playback.restore_queue = true`; restored
  queues start stopped and do not autoplay.
- `auth.json`: device identity, cookies, profile. Atomic write, 0600. Never
  logged. Invalid JSON is reported and the daemon starts anonymously.
- Cache: lyrics and daily responses only, TTL-based, atomically written and
  trimmed to `cache.max_mb`. No audio is cached.
- A corrupt state file is renamed aside instead of being overwritten silently.

## mpv policy

- Executable is resolved from PATH; no libmpv linkage.
- Arguments include `--no-config --no-video --ao=pulse --idle=yes
  --input-terminal=no --terminal=no --input-ipc-server=...`, cache/network
  limits and `--load-scripts=no`.
- The child environment sets `DISABLE_RTKIT=1`; system RTKit limits are not
  changed.
- IPC is the only control protocol; terminal output is never parsed.
- The daemon does not start mpv while only browsing.
- mpv is never allowed to register an MPRIS player.

## MPRIS mapping

`org.mpris.MediaPlayer2.yoruplay` owns both root and Player interfaces.
Properties are read from daemon state. `Metadata` uses a stable
`/org/yoruplay/track/<id>` ID, microseconds for length, artist arrays, cover URL
and plain lyrics. `Position` is in microseconds; no PropertiesChanged spam for
position. Loop/shuffle map to the nearest standard mode while the internal
Heartbeat mode remains in daemon state. SetPosition validates the track ID.

## Failure isolation

API failures, mpv failures, MPRIS registration failures and client
disconnects are independent. A failed MPRIS registration adds a startup warning;
a failed lyric fetch is silent; an unplayable track reports a specific error and
does not crash the service. The daemon never exits because a single external
request failed.
