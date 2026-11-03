# Development

## Layout

```text
crates/api       NetEase HTTP, crypto, session, raw response models
crates/protocol  Shared models, config, paths, IPC wire format
crates/ipc       Async Unix-socket client
crates/core      queue, mpv, daemon server, MPRIS, persistence, cache
crates/tui       Ratatui application
crates/yoruplay  CLI binary
docs/            architecture, upstream map, known issues, resource report
systemd/         reference user unit
```

## Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

The mpv test is skipped only when `mpv` is absent from PATH; otherwise it spawns
a real mpv with `--ao=null` and a generated silent WAV. The daemon integration
test uses isolated XDG directories and does not touch the user's session or
music.

Live read-only API test:

```sh
cargo test -p yoruplay-api --test live_smoke -- --ignored --nocapture
```

It uses no account and performs low-frequency read calls. Do not add destructive
account tests.

## Reference upstream

`upstream/` is ignored by Git. The current reference commit and endpoint mapping
are in `docs/upstream-api-map.md`. Node.js was used only to generate ciphertext
test vectors during development; it is not a build or runtime dependency and
must not be added as one.

Secrets:

- never put cookies/tokens in source, tests, fixtures, logs or commits;
- fixtures are public/sanitized small responses;
- test auth files live in temporary directories and are removed.

## Manual real-environment checks

1. Build and install.
2. `yoruplay doctor` checks mpv, PipeWire/PulseAudio, socket and MPRIS.
3. `yoruplay login` opens the official page and imports the Firefox session.
4. `yoruplay status`, `liked`, `daily`, `fm`, `recent`, `radar`.
5. `yoruplay start-playlist <id>`, then test all five modes.
6. `busctl --user monitor org.mpris.MediaPlayer2.yoruplay` while using Noctalia.
7. Close the TUI while playing; reopen and verify reconnection.
8. `systemctl --user stop yoruplay` and verify mpv is reaped.

## Upstream sync

Read the upstream module before changing an endpoint. Update:
- request path/params and crypto mode,
- raw struct aliases/field names,
- sanitized fixture,
- upstream API map row and commit SHA,
- tests and the final verification notes.

Do not port unrelated upstream endpoints; YoruPlay intentionally implements a
small verified surface.
