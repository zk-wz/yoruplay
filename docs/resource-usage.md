# Resource usage

Measurements will be filled from the real Arch Linux / Surface Pro 6 environment
using PSS, not summed RSS.

Planned scenarios:

1. daemon idle after startup (no mpv)
2. normal playback (daemon + mpv)
3. TUI open
4. search
5. large playlist load
6. TUI closed while playback continues
7. long pause
8. repeated next/previous
9. network error and recovery

Metrics: PSS/RSS, CPU, thread count, child count, cache size, queue size,
kernel version, mpv version and PipeWire/PulseAudio server. The target is
<100 MiB total PSS during normal playback and ≤150 MiB as the accepted fallback.
