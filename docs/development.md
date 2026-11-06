# Development

## Layout

```text
crates/api       NetEase HTTP, crypto, session, raw response models
crates/protocol  Shared models, config, paths, IPC wire format
crates/ipc       Async Unix-socket client
crates/core      queue, Rust audio/transfer, daemon, MPRIS, persistence, caches
crates/tui       Ratatui application
crates/yoruplay  CLI binary
docs/            architecture, upstream map, known issues, resource report
```

## Checks

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

Decoder regressions exercise real Symphonia containers without an output device.
Transfer tests use local HTTP servers for truncation, ranges, entity changes,
retry budgets, cancellation and FIFO/pin invariants. Daemon integration tests use
isolated XDG paths and do not touch the user's session or music. Hardware output
needs a separate smoke: use a shared logical output and verify actual captured
PCM, not just a position/End notification.

Rust 1.87 is the declared minimum; resolver 3 selects compatible dependencies.
Check it after lockfile changes with:

```sh
cargo +1.87.0 check --workspace --all-targets --locked
```

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
2. `yoruplay doctor` opens the actual Rust output and tests anonymous cache-file publication (without creating a missing directory tree).
3. `yoruplay auth login` imports the Firefox session (opening the official page only if needed).
4. `yoruplay daemon status`, `yoruplay library liked`, `yoruplay library daily`, `yoruplay library fm`, `yoruplay library recent`, `yoruplay library radar`.
5. `yoruplay queue start-playlist <id>`, then test all five modes with `yoruplay control ...`.
6. `busctl --user monitor org.mpris.MediaPlayer2.yoruplay` while using Noctalia.
7. Close the TUI while playing; reopen and verify reconnection.
8. `yoruplay daemon stop` and verify device/decoder/transfer handles are released.
9. Warm the audio cache; trace DNS/IP syscalls while restarting daemon/TUI and using local controls/MPRIS. Zero successful requests alone is not proof of zero attempts.
10. Inject a truncated body and server-quality fallback; verify bounded retries, cancellation and preservation of an older complete file.
11. Measure PSS/RSS/FDs/threads in release mode with large queues and an unread IPC client; include pinned and scratch bytes separately.

## Upstream sync

Read the upstream module before changing an endpoint. Update:
- request path/params and crypto mode,
- raw struct aliases/field names,
- sanitized fixture,
- upstream API map row and commit SHA,
- tests and the final verification notes.

Do not port unrelated upstream endpoints; YoruPlay intentionally implements a
small verified surface.
