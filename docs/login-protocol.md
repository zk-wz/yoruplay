# QR login protocol

This note records why the original YoruPlay QR login failed with
“当前设备环境异常” / “设备环境有异常”, and which protocol the daemon now uses.

Investigated on 2026-11-02 against the live `https://music.163.com/login` page.
No account was logged in and no cookie was copied during the investigation.

## Short version

The old implementation followed upstream api-enhanced's **2025** QR flow:

```text
eapi /api/login/qrcode/unikey          {type: 3}
QR content: https://music.163.com/login?codekey=...&chainId=...
poll:      eapi /api/login/qrcode/client/login {type: 3}
```

The current official web login uses a different flow:

```text
weapi /api/login/qrcode/unikey          {type: 1, noCheckToken: true}
QR content: https://music.163.com/st/platform/scanlogin
            ?codekey=...
            &chainId=v1_<sDeviceId-or-unknown-N>_web_login_<epochMs>
            &hdw_device=web
            &hdw_appid=web
            &hitExp=1
poll:      weapi /api/login/qrcode/client/login
           {type: 1, noCheckToken: true, key, ydDeviceToken}
headers:   x-os: web
           x-loginmethod: QrCode
           x-login-chain-id: <chainId>
           nm-gcore-status: 1
           x-channelsource: undefined
```

The phone reads the QR content before YoruPlay polls. When it receives the old
`/login?codekey=` URL, it rejects the login chain, so the API remains at 801 and
never sees 802/803. The missing `chainId` was not the only problem: the route,
key type, transport and web login headers all differ.

The upstream commit `4f4aa13` added `chainId` to the old `/login` URL in
September 2025. The local upstream HEAD (`984cc02`, 2026-10-25) still has that
old QR module. Following upstream alone therefore does not fix the 2026 web
login change.

## Evidence

The official page was opened in a hidden Chromium instance. Its QR canvas was
decoded and its requests were observed:

```text
QR value:
https://music.163.com/st/platform/scanlogin?codekey=<key>
&chainId=v1_unknown-486228_web_login_<timestamp>
&hdw_device=web&hdw_appid=web&hitExp=1
```

The page's JavaScript uses:

```js
// key
{ type: 1, noCheckToken: true }

// chain id
const deviceId = getCookieValue("sDeviceId") || "unknown-" + random;
`v1_${deviceId}_web_login_${Date.now()}`

// QR content
`${origin}/st/platform/scanlogin?codekey=${key}` +
`&chainId=${chainId}&hdw_device=web&hdw_appid=web&hitExp=1`

// poll
{ type: 1, noCheckToken: true, key, ydDeviceToken }
```

The official page had a `sDeviceId` cookie, but the observed QR used the
`unknown-<number>` fallback. Both forms are valid; an Android eapi `deviceId`
is not a web login device and must not be placed in `chainId`.

The page also calls the key and poll endpoints through `weapi`, not `eapi`, and
sends `x-os: web`, `x-loginmethod: QrCode`, `x-login-chain-id` and
`nm-gcore-status: 1`.

## YoruPlay implementation

`crates/api/src/client.rs` now:

1. Requests the key with **weapi**, `type: 1`, `noCheckToken: true`, and
   `x-os: web`. If the web endpoint cannot initialise (for example a restrictive
   cookie jar), it retries once with eapi type 1; it never goes back to the old
   `/login` URL.
2. Builds the chain id from `sDeviceId` when present, otherwise
   `unknown-<random>`. It never uses the eapi Android `device_id`.
3. Returns the `chainId` alongside the QR key/URL. The IPC protocol carries it
   to the TUI and CLI poll commands.
4. Builds the QR content with `/st/platform/scanlogin` and the three
   `hdw_*`/`hitExp` parameters.
5. Polls with weapi and the current web headers.
6. Captures `Set-Cookie`, plus `x-refresh-token` as a `MUSIC_U` fallback when
   the cookie is stripped by a proxy.
7. Maps `800/801/802/803/860` to expired/waiting/scanned/success/cancelled.

The IPC protocol version was bumped from 1 to 2 because `QrLogin` and
`LoginQrPoll` now carry `chain_id`. Restart both the daemon and any TUI/CLI
client after upgrading.

### `ydDeviceToken`

The official page obtains this token from NetEase's `createNEFingerprint`
JavaScript SDK. YoruPlay has no JavaScript runtime, so it sends an empty value,
which is also the fallback used by current third-party clients when their
fingerprint provider is unavailable. The key and QR generation do not require
it. If NetEase later makes a non-empty token mandatory for polling, login will
need a browser/WebView fingerprint provider; cookie import remains the fallback.

## Verification status

- Unit tests pin the chain id format, scan-login URL and status mapping.
- `cargo test --workspace` passes.
- A single live key/poll smoke on 2026-11-02 used the release binary:
  - the weapi `type=1` key request returned 200 (no eapi fallback warning);
  - the QR content was
    `/st/platform/scanlogin?...&chainId=v1_unknown-...&hdw_device=web&hdw_appid=web&hitExp=1`;
  - the first poll returned 801 “等待扫码”.
- No QR was scanned. Do not claim the real-account login path is verified until
  a QR is scanned and `auth.json` contains `MUSIC_U` with mode 0600.

## Troubleshooting

If a future scan still fails:

1. Confirm the QR text contains `/st/platform/scanlogin` and
   `hdw_device=web&hdw_appid=web&hitExp=1`.
2. Check `journalctl --user -u yoruplay` (or the CLI terminal) for the poll
   status. `8821` is risk control, not an account ban.
3. Route `music.163.com`, `interfacepc.music.163.com`, `interface3.music.163.com`
   and `*.126.net` directly if Clash/TUN is active.
4. Log in on the official web page and import `MUSIC_U`. This path was verified
   with a Firefox session on 2026-11-03:

   ```sh
   yoruplay login --cookie-stdin
   # paste MUSIC_U=...; __csrf=... then press Ctrl-D
   ```

   Firefox stores cookies in `~/.config/mozilla/firefox/<profile>/cookies.sqlite`
   on Arch. The database is locked while Firefox is running; use a read-only
   copy if extracting manually. Never paste a cookie or `MUSIC_U` into logs,
   shell arguments or chat.
