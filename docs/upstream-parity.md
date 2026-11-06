# pigma comparison

`upstream/pigma` is a Rust TUI/player with a headless daemon mode. It is useful
as a second reference because it uses a native Rust NetEase client and has its
own cookie/session handling. This note records the differences that matter for
login and request risk; it is not a feature-parity checklist.

## Architecture

| | YoruPlay | pigma |
|---|---|---|
| Playback | Symphonia worker + bounded PCM ring + rodio/cpal | embedded `rodio` + Symphonia |
| Process model | daemon + TUI/CLI/MPRIS clients | TUI or `-d` headless process |
| Cache policy | playback-acquired native audio, 10 GiB FIFO; separate 32 MiB metadata cache | runtime audio/content/cover disk-cache **budget** of 2 GiB (not preallocated) |
| Login | official web page + Firefox cookie | QR code or password/captcha in `ncm-api` |
| Persistent session | `auth.json` 0600 | `cookies.json` |

Both players now embed decoding. YoruPlay keeps its authoritative background
daemon, paged TUI/CLI clients and MPRIS, but does not copy pigma's cover rendering,
equalizer, WSOLA or fallback download machinery. Cache and playback share one
demand-driven transfer rather than an independent downloader.

## Login

pigma's `ncm-api` QR path is:

```text
weapi /weapi/login/qrcode/unikey       {type:1}
QR URL: https://music.163.com/login?codekey=<key>
poll:   weapi /weapi/login/qrcode/client/login {type:1,key}
```

It does **not** add the official web `chainId`, `scanlogin` path or
`ydDeviceToken`. It also does not call `register_anonimous` from the app login
flow. Its QR implementation therefore should not be assumed to be a verified
working reference; the code is evidence of a different request context, not a
guarantee that NetEase accepts the QR.

YoruPlay removed QR login and now uses the official browser session. That avoids
the scan-time device check entirely.

## Request/cookie differences

| Detail | pigma | YoruPlay |
|---|---|---|
| deviceId | persisted, 52 uppercase hex | persisted; new devices now 52 hex |
| Session cookies | regenerated each process: `WNMCID`, `_ntes_nnid`, `_ntes_nuid`, `NMTID`, `os`, `appver`, `osver`, `channel` | stable device values; server `NMTID` when received |
| Browser `sDeviceId` | only if supplied in `cookies.json` | imported with Firefox cookie |
| User-Agent | random from 4 Linux/Mac/Windows/iPhone strings | fixed Edge/Chrome web UA, iPhone eapi UA |
| weapi CSRF | payload and `?csrf_token=` URL query | payload only |
| eapi host | `music.163.com` + `Host: interface.music.163.com` | `interfacepc.music.163.com` |
| eapi identity | iPhone OS/9.0.90 header | Android 14/8.20 header |
| Cookie serialization | raw `k=v` joined by `; ` | percent-encoded names/values |
| Login check | `MUSIC_U` **or** `__csrf` | `MUSIC_U` only |

YoruPlay's fixed UA and header set are deliberate: one consistent client
identity is easier to diagnose and avoids randomizing into an inconsistent
fingerprint. The imported browser cookie supplies the web `sDeviceId` and
`NMTID`, which is the context that mattered for the failed QR attempts.

## Resource expectations

- Pre-cutover historical baseline: roughly 11 MiB daemon + 56 MiB mpv, about
  67 MiB PSS during playback. These are not current Rust-backend measurements.
- Current measured release scenarios are in [resource-usage.md](resource-usage.md);
  a library/language choice alone is not evidence of resource savings.
- pigma has a much larger default **disk-cache budget** (2 GiB, not allocated
  up front) and keeps cover/image/decoder state in the same process. A direct
  pigma PSS measurement is not part of the YoruPlay test suite; do not claim a
  playback-memory win without measuring the exact pigma release.
- YoruPlay's playlist metadata cache is bounded to 8 entries and 20,000
  tracks/entry; browsing very large playlists no longer grows memory without
  limit.

## What YoruPlay does not copy

- pigma's random UA, eager downloader or 2 GiB disk-cache policy. The decoder
  separation is reused with YoruPlay's own ownership/backpressure constraints.
- QR login, password login and captcha flows.
- pigma's `MUSIC_U || __csrf` logged-in heuristic; YoruPlay requires `MUSIC_U`.
- pigma's request-level CSRF query duplication; the payload form works.

## Detailed playback analysis

See [`pigma-playback-analysis.md`](pigma-playback-analysis.md) for the decoder
stack, NetEase compatibility matrix and memory trade-offs.
