#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists alsa; then
    cat >&2 <<'MSG'
YoruPlay's Rust audio backend needs the ALSA development library and pkg-config.

On Arch Linux, install them with:
  sudo pacman -S alsa-lib pkgconf
Use pipewire-alsa or a PulseAudio ALSA route for shared desktop audio.
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
