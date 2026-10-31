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

if [[ "${1:-}" != "--no-service" ]]; then
    "$HOME/.local/bin/yoruplay" service install
fi

cat <<MSG

Installed $HOME/.local/bin/yoruplay
The user service was written but NOT enabled at login.
Start it with:
  systemctl --user start yoruplay

Open the TUI with:
  yoruplay
MSG
