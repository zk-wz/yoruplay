# YoruPlay Project Conventions

## Communication
- Think in English; report to the user in Chinese.
- Be direct and evidence-first. State facts, decisions, and risks; avoid filler.

## Working tree
- Put temporary files and scripts in `./tmp` (gitignored). Keep the repository root clean.
- Read-only investigation unless the user explicitly asks for changes.
- After a task, remove every temporary file, backup, migration script, and generated artifact. No residue.
- `git status` must be clean before committing.

## Code
- Extreme RAM savings is a primary design goal: stream, page, and bound all buffers; never load whole queues, lyric sets, or artwork batches into memory.
- Prefer simple, boring designs. Delete dead code, compatibility shims, and redundant paths instead of keeping them.
- Cache layout: `<cache-root>/audio`, `<cache-root>/artwork`, `<cache-root>/meta`; `index.redb` and `cache.lock` stay at the root. `<cache-root>` is `~/.cache/yoruplay` by default.
- `index.redb` is the authoritative cache index. Per-entry JSON manifests under `meta/` are recovery records.
- Artwork: request 1024×1024 from the NetEase CDN with `?param=1024y1024`; store one external image and embed the same image in audio tags. Never store the original.
- Local playback may fetch only missing artwork. It must never resolve audio URLs or fetch lyrics.
- Do not add cache migration code to the shipped binary. Migrations are one-off local operations.

## Verification and delivery
- After every code change, build and install the binary so the user can test:
  `./scripts/install.sh`
  This installs to `~/.local/bin/yoruplay`.
- Run `cargo fmt --check`, `cargo clippy --all-targets`, and the narrowest relevant tests before committing.
- Version bumps: update the workspace version, commit, push, and create a GitHub release with the built binary attached.
