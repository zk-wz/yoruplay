# Resource usage and verification measurements

## Rust playback cutover

Environment: Arch Linux, linux-surface 7.2.14, x86_64 Surface Pro 6; system
PipeWire through the ALSA plugin (ALSA 1.2.16.1). Release build uses thin LTO,
one codegen unit and stripped symbols. Rust 1.87 compatibility is checked with
the committed lockfile, separately from the host stable compiler build.

PSS/RSS come from `/proc/<pid>/smaps_rollup`; threads and FDs from `/proc`.
These are the daemon's figures: there is no separate player process to add.
Filesystem page cache is **not** process PSS; disk-backed staging remains
reclaimable filesystem cache, not free memory or a whole-track heap buffer.

### Personally measured release samples

| Scenario | PSS (KiB) | RSS (KiB) | Threads | FDs |
|---|---:|---:|---:|---:|
| Saved account, restored one-track queue, stopped | 12,568 | 15,240 | 4 | 14 |
| Real NetEase exhigh MP3, actual shared-device playback | 19,484 | 24,780 | 9 | 40 |
| Cached FLAC looping, following the large-queue exercise | 16,654 | 21,720 | 9 | 39 |
| 1,500-item queue, one unread IPC client, before edits | 12,966 | 15,644 | 4 | 16 |
| Same queue/client after 32 tail removals | 13,136 | 15,808 | 4 | 16 |
| Paged TUI open at item 1,500 (separate process) | 5,014 | 10,336 | 4 | 17 |
| Cached source after rapid Next/Previous switching | 16,787 | 21,756 | 9 | 39 |

The live MP3 sample was taken after playback entered Playing and advanced for
1.5 seconds, not after a whole track. The cached FLAC is a generated one-second
48 kHz stereo track in repeat mode. Its short 0.5-second CPU sample was 2.0% of
one core; that sampling resolution is not a sustained CPU benchmark. The queue
exercise used isolated XDG paths, working private D-Bus, a draining controller
and a second client that stopped reading. The TUI paged to item 1,500 before the
memory sample; it was closed during the before/after queue measurements.

### Historical references, not a paired benchmark

| Pre-cutover mpv scenario | PSS |
|---|---:|
| Real stream, daemon + mpv | about 67.2 MiB total |
| 1,500-item queue + unread client, before 32 removals | 11,018 KiB |
| Same old queue/client, after removals | 27,038 KiB |

Those earlier measurements used the old executable, kernel/environment and
startup conditions. They establish the old process/backlog cost, not a precise
percentage speed or memory comparison against an identical audio input. The old
queue experiment had no working private D-Bus. Allocator retention also means a
short post-disconnect sample does not prove a permanent leak. Detailed old
playback evidence remains in the explicitly historical `final-report.md`.
No pigma binary or alternative-backend PSS benchmark was run.

## Actual output, formats and transport

- Actual rodio/cpal output was exercised, not only decoder unit tests. Native
  PipeWire routing was checked with `pw-link`; a null-sink monitor captured
  nonzero PCM (peak 275 in signed 16-bit capture, 761,104 nonzero samples during
  the isolated repeat probe). `PIPEWIRE_PROPS` selected the target: `PULSE_SINK`
  alone does not route a native ALSA/PipeWire stream.
- Generated WAV/PCM, MP3, FLAC, M4A/AAC, common ADTS AAC and Ogg/Vorbis each played
  to End, published a complete native-format file and replayed locally. A counting
  HTTP server observed **one media GET per first acquisition and zero on each
  local replay**. Independent ffprobe recognized the published codecs, 48 kHz
  stereo and valid durations; no transcoding or application decryption was used.
- A saved real NetEase session returned exhigh MP3. The new decoder/output
  advanced, paused at a stable position and stopped without publishing the
  incompletely heard object. Full live lossless/hi-res entitlement was unavailable;
  the FLAC/AAC evidence is generated-fixture evidence, not a live entitlement claim.
- A forward seek to a sparse tail and EOF did not publish a complete entry.
- A deliberately interrupted body resumed at a nonzero Range offset: two media
  requests, one shared retry consumed, then a complete verified publication.
- Default service recovery waited at least ten seconds after the first failed
  API request. Exhaustion produced exactly four total attempts with retry_count=3.
  Stop during the retry wait returned within the smoke's one-second bound; no
  delayed retry occurred during the following eleven seconds.
- Trial `freeTrialInfo` caused no media GET. A one-second full track still played.
- Small quotas proved completion-time FIFO, skipping pins, and trimming when the
  pin released/limit shrank. Explicit upgrade switched the preferred complete
  file; lower requested quality and a server fallback preserved the superior old
  file without fetching replacement media. The latter has a permanent service
  regression in addition to the failing-before/passing-after runtime probe.

## CLI, TUI, MPRIS and zero-network boundary

The real release executable was exercised under a private D-Bus session and
isolated HOME/XDG paths. Both daemon and actual PTY TUI were traced with strace's
network syscall set. Opening the TUI, paging its 1,500-item queue, cached playback,
pause/seek/repeat/resume, config reload, path inventory, MPRIS and cache clear made
**no AF_INET/AF_INET6 calls**. Neither trace contained nscd/systemd-resolver proxy
markers. This checks attempts, not just successful HTTP responses.

- The rendered 140×36 TUI showed global queue rows through 1,500 with the Rust
  audio header; its terminal screen was inspected, not inferred from a mock.
- `config list/get`, `listpath`, `cache info/list` and `doctor` on missing paths
  did not create application directories or auto-start a daemon.
- CLI set accepted GiB/G units and hot-reloaded retries; invalid values left the
  configuration file unchanged. Permanent config tests also cover concurrent
  setters/comment preservation and explicit all-reset confirmation.
- MPRIS Play/Pause, Next/Previous, Seek, SetPosition and Volume were exercised
  against the actual daemon, including two final runs of 100 consecutive cached
  source changes without RPC disconnection or IP socket calls. The second run
  recovered complete files/manifests into an empty audio index before playback.
  Cached lyrics were returned with no remote HTTP artwork URL; FDs/threads remained
  bounded. A transient-full notification queue no longer disconnects the client.
- Metadata-only clear preserved audio and logs. Explicit audio clear removed the
  preferred entries while the pinned source resumed and looped through its open
  inode. Stop then reported zero retained/pinned audio bytes.
- A separate CLI scenario configured audio and metadata to the same directory:
  hot metadata trimming and metadata-only clear preserved native audio, manifests,
  the live index and logs. Namespace ownership also has a failing-before/passing-
  after filesystem regression; a metadata budget cannot become an audio budget.
- A real partial online source exposed an anonymous staging inode in `/proc/fd`.
  Stop removed it before acknowledgement, with zero retained entry/reservation/
  scratch accounting. A second partial source was SIGKILLed while staging existed:
  its process/FDs disappeared immediately, with no named partial or published
  audio entry. This did not depend on next-start cleanup.

## Personally traced memory changes

- Full-queue broadcasts were replaced by small revision notifications and paged
  retrieval; outbound byte budgets and bounded admission prevent slow-client
  serialized-snapshot accumulation. MPRIS reads scalar playback state.
- The canonical queue is shared/COW; unchanged volume/position saves stream compact
  state rather than copy/serialize every song again. Queue content uses separate
  revision files, with crash-orphan cleanup before revision reuse.
- TUI browse ownership is bounded/cancellable, including discarding stale queued
  responses. Playlist pages start by selected ID, not by forwarding a large song
  vector or a pre-filter page offset.
- API playlist cache ownership is shared, large replies are bounded/typed, song
  details reconcile per batch, shuffle avoids a full candidate vector, and log
  tail reads only its bounded suffix.
- The default 100 ms / 48 kHz / stereo ring payload is **153,600 bytes**: each
  `PcmSample` carries a generation and occupies 16 bytes on x86_64, not just a
  four-byte float. Decoder packets, conversion, HTTP/device buffers and index
  cache must still be counted. Redb's page cache is explicitly 1 MiB.
- Input channels are capped at 32, PCM ring at 64 MiB, decoded interleaved packet
  at 2,097,152 samples. These are safety ceilings, not expected/default usage or
  a promise that arbitrary configured multichannel playback fits 30 MiB.
- Cache data goes on the configured filesystem. tmpfs/ramfs is detected and
  warned about: anonymous files there still consume RAM. No malloc_trim ritual,
  whole-song heap buffer or whole-audio mmap is used to manufacture a low PSS claim.

### Liked paging memory check (2026-11-11)

Release binaries, isolated XDG paths, real read-only session. The TUI opened
Liked Songs and then played the first track; PSS came from
`/proc/<pid>/smaps_rollup`. Both runs rendered the liked list (`add all` footer
present).

| Build | TUI PSS (KiB) | Daemon browse peak (KiB) | Daemon play peak (KiB) |
|---|---:|---:|---:|
| Before paging | 4,419 | 13,062 | 16,549 |
| After paging | 3,512 | 10,965 | 14,136 |

Paging holds 100 songs instead of the full library in the TUI/daemon browse
path. Playback still builds the complete queue, but the after-build is lower in
this run because the browse allocation is smaller and no song-detail cache was
added.

## Build checks

- `cargo test --workspace --locked`: 102 passed; three opt-in live tests ignored.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed.
- `cargo fmt --all --check` and `cargo build --release --locked`: passed.
- The cutover/dependency lock was also checked using Rust 1.87.0 with
  `cargo +1.87.0 check --workspace --all-targets --locked`.
- Notification-lag recovery has a permanent regression: stale buffered events
  must be replaced by authoritative state while a one-slot consumer applies
  backpressure. A transient full notification queue is not a disconnected RPC
  client; stalled socket writes have a deadline.

## Limits of this verification

No Bluetooth unplug/replug, suspend/resume, real long-pause URL expiry, live
lossless/hi-res entitlement, maximum-rate multichannel device or visual Noctalia
widget run was performed. Local native format/transport fault coverage does not
claim all codecs accepted by mpv. See `known-issues.md` for ADTS variants, native
container/tag limits and restart requirements. Historical live API/account checks
are not automatically rerun by the default suite.
