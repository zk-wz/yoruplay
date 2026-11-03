#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! command -v mpv >/dev/null 2>&1; then
    cat >&2 <<'MSG'
YoruPlay requires mpv, but it was not found in PATH.

On Arch Linux, install it with:
  sudo pacman -S mpv
MSG
    exit 1
fi

cargo build --release
install -Dm755 target/release/yoruplay "$HOME/.local/bin/yoruplay"

cat <<MSG

Installed "$HOME/.local/bin/yoruplay"
No systemd unit or autostart entry was created.
The TUI auto-starts the daemon; use `yoruplay daemon start` to start it explicitly.
MSG
