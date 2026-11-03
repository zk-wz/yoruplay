# Login

YoruPlay no longer implements NetEase QR-code login. The QR flow was removed
because NetEase's device-fingerprint checks can reject the scan before the
server emits the "scanned" state. Login now uses the official web page and the
browser's existing session.

## Default: Firefox

```sh
yoruplay login
```

The command uses the system `sqlite3` binary to read the copied Firefox
database. Install the Arch `sqlite` package if it is missing.

It:

1. first reads Firefox's existing `MUSIC_U` session and imports it **without
   opening a browser**;
2. only if no valid session exists, opens `https://music.163.com/login` with
   `xdg-open` and waits for login;
3. copies only the NetEase cookies it needs to a private temporary database;
4. validates the session with `/weapi/nuser/account/get`;
5. writes `auth.json` with mode `0600`.

To switch accounts even when the current Firefox session is valid:

```sh
yoruplay login --switch-account   # alias: --force
```

This always opens the browser and waits until Firefox holds a different
`MUSIC_U` than the current YoruPlay session.

Firefox profiles are discovered under:

```text
~/.config/mozilla/firefox/*/cookies.sqlite
~/.mozilla/firefox/*/cookies.sqlite
~/snap/firefox/common/.mozilla/firefox/*/cookies.sqlite
~/.var/app/org.mozilla.firefox/.mozilla/firefox/*/cookies.sqlite
```

The newest profile containing a non-empty `MUSIC_U` is used. Cookie values are
never printed or logged, and the temporary database is deleted immediately.

The command polls for up to five minutes. Log in to the official page with
Firefox while it waits. If Firefox is not installed or its profile is not
readable, use the manual path below.

## TUI

Press `L` to open the login panel. Inside it:

- `b` opens the official NetEase login page;
- `i` imports the Firefox cookie;
- `o` logs out;
- `Esc` closes the panel.

## Manual cookie

Other browsers (Chrome, Chromium, Brave, Edge, Flatpak browsers) store cookies
encrypted through a keyring or platform-specific encryption, so YoruPlay does
not read them automatically yet. Log in on the official web page and pipe the
Cookie header through stdin:

```sh
yoruplay login --cookie-stdin
# paste MUSIC_U=...; __csrf=... and press Ctrl-D
```

Or from a protected file:

```sh
chmod 600 ~/netease-cookie.txt
yoruplay login --cookie-stdin < ~/netease-cookie.txt
```

Requirements:

- the header must contain `MUSIC_U=...`;
- `__csrf=...` is recommended;
- copy the header from a `music.163.com` request, not
  `interfacepc.music.163.com`;
- do not paste the `Cookie:` prefix, and do not put cookies in shell history or
  chat.

The cookie is validated before the old session is replaced. An invalid cookie
rolls back and leaves the current session untouched.

## Session storage

```text
~/.local/share/yoruplay/auth.json   # mode 0600
```

`auth.json` contains the session cookies and the cached account profile. Do not
share it. `yoruplay logout` removes it; `yoruplay shutdown` stops the daemon
without touching credentials.

## Verifying an imported session

```sh
yoruplay status
cargo test -p yoruplay-api --test live_account -- --ignored --nocapture
```

The live test is read-only and covers profile, playlists, liked songs, daily
recommendations, Personal FM, recent history, search, stream URL and lyrics.
It prints metadata, never cookies or stream URLs.
