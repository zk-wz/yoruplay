# Resource usage

Environment: Arch Linux rolling, linux-surface kernel 7.2.11, Surface Pro 6
(i5-8250U, 8 GiB RAM), PipeWire 1.6.10 with pipewire-pulse, mpv 0.41.0.
Release build (`cargo build --release`, thin LTO, stripped). PSS comes from
`/proc/<pid>/smaps_rollup`; RSS is reported only for context. `scripts/measure-pss.sh`
prints the same values for supplied PIDs.

## Measured so far

| Scenario | Daemon PSS | Daemon RSS | TUI PSS | TUI RSS | Threads (daemon/TUI) | Notes |
|---|---:|---:|---:|---:|---:|---|
| Daemon idle, no mpv | **6.9 MiB** | 9.2 MiB | — | — | 10 / — | Fresh start; MPRIS registered; no account |
| Daemon idle + TUI open | **9.7 MiB** | 12.4 MiB | **3.9 MiB** | 8.5 MiB | 10 / 10 | TUI on a 120×36 terminal, login overlay dismissed; total PSS ≈ **13.6 MiB** |
| 200-track playlist loaded | **19.7 MiB** | 22.5 MiB | — | — | 10 / — | Public 热歌榜 (200 tracks), full song details |
| Search (50 results) | **18.6 MiB** | 21.3 MiB | — | — | 10 / — | Allocator retains playlist/search memory |
| Cache after above | — | — | — | — | — | 4 KiB; no audio cached |

Idle CPU: 0.0–0.3% of one core. No mpv process runs until playback starts.

These results are comfortably below the 100 MiB PSS target. The daemon is a
single process with one Tokio runtime; the TUI adds a second process only while
open.

## Pending real-playback measurements

To complete after QR login:

- normal playback: daemon + mpv total PSS
- TUI open during playback
- TUI closed while playback continues
- long pause
- repeated next/previous
- MPRIS-driven seek/volume
- stream URL refresh after an error
- cache growth after lyrics/recommendation use
- CPU/threads/child count under playback
- daemon restart and queue restore

They will be recorded in the final delivery report together with any deviation
from the target.
