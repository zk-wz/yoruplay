# Song cache design (not implemented)

Status: approved design, intentionally not implemented yet.

## Goal

Cache played songs as normal audio files that can be played by other players and
by YoruPlay when offline. The cache is user-managed; no size limit, no automatic
cleanup and no cache settings page.

## Location and naming

```text
~/Music/YoruPlay/<song-id> - <artist> - <title>.<ext>
```

- `~/Music` is the user's Music directory; YoruPlay uses a subdirectory so files
  can be removed or backed up as a unit.
- Song ID makes lookup and deduplication stable.
- `/`, control characters and path separators in titles are replaced by `_`.
- Downloads write `<name>.part`; only a completed file is atomically renamed to
  the final name.
- No cache index on disk is required; scan the directory at startup or resolve a
  song by its ID prefix.

## Audio format

Use the stream's native container; no transcoding:

| Stream | File | Metadata |
|---|---|---|
| FLAC | `.flac` | Vorbis comments, PICTURE |
| MP3 | `.mp3` | ID3v2.4, APIC, USLT |
| AAC/M4A | `.m4a` | MP4 ilst, cover, lyrics |

FLAC is preferred when the account's `api.quality` and the stream actually
provide it. YoruPlay does **not** issue an extra high-quality request just for
caching: the cache uses the same stream URL that is used for playback, avoiding
extra API traffic and risk-control exposure. Users who want FLAC can set
`api.quality = "lossless"`/`"hires"` if their account supports it.

## Metadata

Write with the pure-Rust `lofty` library (no ffmpeg subprocess):

- title
- artists
- album
- cover (embedded picture)
- lyrics (original; translation when available)
- custom YoruPlay song ID / quality fields

A failed cover or lyric fetch must not prevent the audio file from being
cached. A failed tag write leaves the audio as a valid untagged file or discards
the temporary file; it never corrupts an existing cache entry.

## Download timing

Playback is not blocked by caching:

1. Resolve the stream URL and start mpv immediately.
2. If the stream is not a trial/preview and the cache file does not exist,
   enqueue one background cache job.
3. A single worker downloads sequentially with a small streaming buffer.
4. Write tags, atomically rename, then discard the worker state.

Only tracks that actually start playing are cached. One song ID is deduplicated
in-flight. A trial/30-second stream is never cached.

## Offline playback

Before asking the API for a stream URL:

1. Look for a completed file whose name starts with the song ID.
2. If readable, pass the local path to mpv; no network request is made.
3. If the file is corrupt/unreadable, ignore it and fall back to the stream.
4. On a later successful download, replace a corrupt entry atomically.

## Resource policy

- Stream to disk in chunks; never hold a whole song in memory.
- One cache worker; no parallel downloads.
- No audio bytes retained in a memory cache.
- No disk quota, no automatic eviction, no background polling.
- Disk use is entirely user-controlled.

## Verification plan

- Unit tests: filename sanitization, ID lookup, tag read/write on a tiny audio
  fixture, duplicate-job suppression.
- Integration: play a track with network, wait for the completed file, inspect
  tags/cover/lyrics with a tag reader.
- Offline: disable network, replay the same track, verify mpv opens the local
  file and no stream URL request is made.
- Memory: compare PSS during playback with caching enabled/disabled; the
  download buffer must remain small and bounded.

## Explicitly out of scope now

- No cache UI/settings page.
- No cache size limit or eviction.
- No prefetch of future queue entries.
- No cache management screen.
