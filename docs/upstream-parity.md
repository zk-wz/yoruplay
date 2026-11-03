# pigma comparison

`upstream/pigma` is a Rust TUI/player with a headless daemon mode. It is useful
as a second reference because it uses a native Rust NetEase client and has its
own cookie/session handling. This note records the differences that matter for
login and request risk; it is not a feature-parity checklist.

## Architecture

| | YoruPlay | pigma |
|---|---|---|
| Playback | external `mpv` over JSON IPC | embedded `rodio` + Symphonia |
| Process model | daemon + TUI/CLI/MPRIS clients | TUI or `-d` headless process |
| Cache policy | no audio cache; 32 MiB metadata cache | runtime audio/content/cover disk-cache **budget** of 2 GiB (a cap, not preallocated; pigma source itself is only a few MiB) |
| Login | official web page + Firefox cookie | QR code or password/captcha in `ncm-api` |
| Persistent session | `auth.json` 0600 | `cookies.json` |

pigma's embedded decoder avoids an mpv child but keeps image/cover rendering,
an equalizer, download cache and content pages in the same process. YoruPlay's
TUI is virtualized and does not decode covers; mpv is a separate process.

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

- Idle daemon: about 10 MiB PSS with a restored account/queue; the earlier
  6.7 MiB measurement was an empty, anonymous state.
- Playback: mpv about 56 MiB PSS after reducing its cache to 8 MiB forward /
  2 MiB back / 30 s; daemon about 11 MiB. Total around 67 MiB.
- pigma has a much larger default **disk-cache budget** (2 GiB, not allocated
  up front) and keeps cover/image/decoder state in the same process. A direct
  pigma PSS measurement is not part of the YoruPlay test suite; do not claim a
  playback-memory win without measuring the exact pigma release.
- YoruPlay's playlist metadata cache is bounded to 8 entries and 20,000
  tracks/entry; browsing very large playlists no longer grows memory without
  limit.

## What YoruPlay does not copy

- pigma's random UA, embedded decoder and 2 GiB disk-cache budget.
- QR login, password login and captcha flows.
- pigma's `MUSIC_U || __csrf` logged-in heuristic; YoruPlay requires `MUSIC_U`.
- pigma's request-level CSRF query duplication; the payload form works.
