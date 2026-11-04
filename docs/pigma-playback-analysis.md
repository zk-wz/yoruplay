# pigma playback analysis

Source-only investigation of `upstream/pigma`, focused on playback, decoding,
NetEase compatibility and memory trade-offs. No pigma build or runtime
measurement was performed. This is a reference for a possible future YoruPlay
playback refactor; it is not an implementation plan.

## Conclusion

pigma does **not** implement its own audio parser or decoder. It uses the Rust
ecosystem:

- `rodio 0.22.2` for the player and output graph
- `symphonia` decoders via rodio features
- `cpal` for the audio device (through rodio)
- `stream-download 0.24.4` for HTTP streaming/downloading
- `biquad` for the equalizer
- `sonar` for third-party source search/fallback, not decoding
- `lofty` for metadata/cloud upload, not playback

pigma's own playback code is mostly orchestration plus two DSP pieces:

- `src/playback/stretch.rs`: pitch-preserving WSOLA time stretching
- `src/playback/equalizer.rs`: biquad equalizer

The decoder itself is `rodio::Decoder`, which dispatches to Symphonia.

## Enabled decode formats

From `upstream/pigma/Cargo.toml`:

```toml
rodio = { version = "0.22.2", default-features = false, features = [
  "playback",
  "symphonia-aac",
  "symphonia-flac",
  "symphonia-isomp4",
  "symphonia-mp3",
  "symphonia-vorbis",
  "symphonia-wav",
] }
```

So the guaranteed set is:

- AAC
- FLAC
- MP4/M4A container
- MP3
- Vorbis/Ogg
- WAV

Not enabled by these features:

- AC3/EAC3
- DTS
- ALAC (not among the enabled Symphonia codecs)
- Opus (not among the enabled features)
- arbitrary containers that FFmpeg would normally tolerate

## Playback pipeline

```text
resolve(song)
  ├─ local cached file        -> File
  ├─ third-party result       -> sonar provider URL
  ├─ free local path          -> File
  └─ NetEase stream URL
          ↓
    stream-download
      (TempStorageProvider or CacheFileProvider)
          ↓
    SharedReader: Arc<Mutex<Box<dyn Read + Seek>>>
          ↓
    rodio::Decoder (Symphonia)
          ↓
    Stretch (WSOLA) -> EqSource (biquad)
          ↓
    rodio::Player -> MixerDeviceSink -> cpal -> system audio
```

Relevant source:

- `src/playback/source.rs`: resolve order, prebuffer, cache/local/stream paths
- `src/playback/player.rs`: persistent blocking player thread, `rodio::Decoder`,
  `MixerDeviceSink`, cpal fixed buffer
- `src/playback/stretch.rs`: custom WSOLA
- `src/playback/equalizer.rs`: biquad EQ
- `src/cache/audio.rs`: disk cache provider and cache index

## Streaming and memory behaviour

Important constants and mechanics:

- `PREBUFFER_BYTES = 512 * 1024`
- `PREBUFFER_TIMEOUT = 8s`
- `stream-download` writes to a temporary file or the on-disk cache; it does
  not retain the complete track in RAM.
- `SharedReader` lets the decoder and seek path share a `Read + Seek` source.
- A persistent blocking player thread is reused across songs.
- cpal output uses a fixed buffer of 8192 frames.
- Cache is on disk; the default cache budget is 2 GiB (a cap, not a
  preallocation). Cache eviction is a disk policy, not an in-memory cache.
- `src/playback/engine.rs` calls `trim_heap()` (`malloc_trim(0)` on glibc)
  after resolving a song and after failures; `src/app/cover.rs` does the same
  after cover handling.

This is a streaming design: the audio bytes are not held as one in-memory
buffer. Memory is still used by:

- the TUI and its rendered state
- cover images and image decoding
- lyrics and content pages
- rodio/cpal ring buffers and the persistent audio thread
- the active queue (other queues are stored on disk)

pigma does **not** solve memory use by simply being a Rust program. It avoids a
separate mpv process but puts the TUI, image handling, decoder and audio thread
in one process.

## NetEase compatibility

`src/playback/source.rs` resolves NetEase URLs through `ncm-api`:

- `songs_url_v1` requests a quality level.
- `standard` / `higher` / `exhigh` request AAC (`encodeType=aac`).
- `lossless` / `hires` and higher levels request FLAC (`encodeType=flac`).
- `AudioVivid` additionally sends `immerseType=c51`.

The resolver explicitly chooses a URL where `!free_trial`:

```rust
.find(|u| !u.url.is_empty() && !u.free_trial)
```

It also canonicalises cache extensions:

```rust
"flac" => "flac",
"ogg" => "ogg",
"wav" => "wav",
"m4a" | "mp4" => "m4a",
_ => "mp3",
```

Unknown extensions are labelled `.mp3` for cache filenames; Rodio generally
sniffs the content, but this is not a guarantee for every container.

### Matrix

| NetEase source / quality | pigma | Notes |
|---|---|---|
| standard / higher / exhigh | Yes | MP3/AAC; Symphonia has both |
| lossless / hires | Yes | FLAC |
| ordinary MP3 320 kbps | Yes | |
| AAC / M4A | Yes | AAC + isomp4 |
| Vorbis / WAV | Yes | Rare for NetEase streams |
| local/free file | Yes | Opened as a file and decoded |
| HTTP range/seek | Usually | Depends on CDN Range support; mpv is more tolerant |
| 30-second trial stream | No/limited | `free_trial` URLs are filtered out; pigma looks for a full URL instead |
| jyeffect surround | Not guaranteed | The server may return AC3/EAC3/MP4 variants; Rodio features do not include AC3/EAC3 |
| jymaster / sky (immersive) | Not guaranteed | Same container/codec risk; `sky` uses an immersive request |
| ALAC / Opus / AC3 / EAC3 / DTS | No/not enabled | Not in the enabled Symphonia features |
| corrupt/odd containers | Weaker than mpv | FFmpeg is much more tolerant |

The important distinction: pigma can expose a `SongQuality` enum with
`jyeffect`, `jymaster` and `sky`, and can request those levels, but that does
not mean Rodio/Symphonia can decode every container the server returns for
them.

## Comparison with the current YoruPlay + mpv design

YoruPlay currently uses system `mpv` over JSON IPC:

- mpv/FFmpeg covers essentially every NetEase container/codec.
- mpv handles trial streams, odd CDN behaviour, seeking and audio-device changes.
- Measured release state: daemon about 10.9 MiB PSS, mpv about 56.4 MiB with
  8 MiB forward / 2 MiB back / 30 s cache; total about 67.6 MiB.
- mpv is a separate process, so its memory is visible and it can be reaped
  independently.

pigma's embedded design:

- Avoids the mpv process and its FFmpeg player context.
- Uses a small 512 KiB prebuffer and disk-backed streaming.
- But the TUI, cover images, decoder and audio thread share one process.
- No pigma PSS was measured, so no claim is made that pigma uses less memory
  than mpv in total.

## Possible directions if YoruPlay ever changes playback

These are options, not commitments:

1. Keep mpv, reduce cache further (for example 4 MiB forward / 1 MiB back) and
   measure under the same playback scenarios. The current 8/2 setting is a
   conservative balance.
2. Keep mpv and borrow hygiene ideas such as `malloc_trim` after large API
   response/cover handling, if profiling shows retained heap.
3. Replace mpv with rodio/Symphonia only if full control of seek, device
   switching, EQ, speed and cache is implemented. This is a major rewrite, and
   it would reduce codec compatibility unless extra Symphonia features are
   added. It is not justified by memory alone.
4. Keep mpv as the compatibility floor for unusual NetEase qualities; an
   embedded decoder would be a second backend, increasing complexity.

## Verification status

- Source paths and crate versions were inspected directly.
- No pigma binary was built, and no pigma memory number is claimed.
- YoruPlay's mpv/daemon numbers are from the release build measured in this
  repository.
