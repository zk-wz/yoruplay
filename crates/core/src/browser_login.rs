//! Browser-assisted login for browsers whose cookie databases are readable
//! without a keyring. Firefox stores cookies in a plain SQLite database.
//!
//! Cookies are secrets: this module never logs values, copies the database to
//! a private temporary directory, queries only the NetEase cookie names it
//! needs, and removes the copy before returning. The query is executed by the
//! system `sqlite3` binary so the daemon does not link a SQLite engine.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;
use yoruplay_protocol::LOGIN_URL;

const COOKIE_NAMES: &[&str] = &[
    "MUSIC_U",
    "__csrf",
    "NMTID",
    "sDeviceId",
    "WNMCID",
    "WEVNSM",
    "_iuqxldmzr_",
    "ntes_kaola_ad",
];

#[derive(Debug, Error)]
pub enum BrowserLoginError {
    #[error("no Firefox profile with cookies.sqlite was found")]
    NoProfile,
    #[error("Firefox has no valid MUSIC_U cookie; log in at {LOGIN_URL}")]
    NoCookie,
    #[error("Firefox is still logged in as the current account; switch accounts there")]
    SameAccount,
    #[error("could not read Firefox cookies: {0}")]
    Io(String),
    #[error("could not query Firefox cookies: {0}")]
    Sqlite(String),
}

impl BrowserLoginError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoProfile => "browser_profile_missing",
            Self::NoCookie => "browser_cookie_missing",
            Self::SameAccount => "browser_same_account",
            Self::Io(_) => "browser_cookie_io",
            Self::Sqlite(_) => "browser_cookie_sqlite",
        }
    }

    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::NoProfile => Some(
                "Open https://music.163.com/login in Firefox, log in, then retry. \
                 Other browsers are not supported yet; use --cookie-stdin.",
            ),
            Self::NoCookie => {
                Some("Finish logging in at https://music.163.com/login in Firefox, then retry.")
            }
            _ => Some(
                "Install the `sqlite` package, check Firefox profile permissions, \
                 or use --cookie-stdin.",
            ),
        }
    }
}

/// Read the current Firefox session as a Cookie header. Profiles are searched
/// across the Arch, upstream, Snap and Flatpak Firefox locations; the newest
/// `cookies.sqlite` with a `MUSIC_U` cookie wins.
pub fn firefox_cookie_header() -> Result<String, BrowserLoginError> {
    let databases = firefox_cookie_databases();
    if databases.is_empty() {
        return Err(BrowserLoginError::NoProfile);
    }
    let mut last_error = None;
    for database in &databases {
        match read_profile(database) {
            Ok(Some(header)) => return Ok(header),
            Ok(None) => {}
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or(BrowserLoginError::NoCookie))
}

fn firefox_cookie_databases() -> Vec<PathBuf> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let roots = [
        home.join(".config/mozilla/firefox"),
        home.join(".mozilla/firefox"),
        home.join("snap/firefox/common/.mozilla/firefox"),
        home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
    ];
    let mut found = Vec::new();
    for root in roots {
        find_cookie_databases(&root, 0, &mut found);
    }
    found.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    found.reverse();
    found
}

fn find_cookie_databases(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_cookie_databases(&path, depth + 1, found);
        } else if path
            .file_name()
            .is_some_and(|name| name == "cookies.sqlite")
        {
            found.push(path);
        }
    }
}

fn read_profile(database: &Path) -> Result<Option<String>, BrowserLoginError> {
    let temp = tempfile::Builder::new()
        .prefix("yoruplay-firefox-")
        .tempdir()
        .map_err(|e| BrowserLoginError::Io(e.to_string()))?;
    let copy = temp.path().join("cookies.sqlite");
    fs::copy(database, &copy).map_err(|e| BrowserLoginError::Io(e.to_string()))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", database.display()));
        if sidecar.exists() {
            let destination = temp.path().join(format!("cookies.sqlite{suffix}"));
            fs::copy(&sidecar, &destination).map_err(|e| BrowserLoginError::Io(e.to_string()))?;
        }
    }

    let names = COOKIE_NAMES
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT name, value FROM moz_cookies \
         WHERE host IN ('music.163.com', '.music.163.com') \
         AND path='/' AND name IN ({names}) \
         ORDER BY CASE host WHEN '.music.163.com' THEN 0 ELSE 1 END"
    );
    let output = Command::new("sqlite3")
        .arg("-readonly")
        .arg("-separator")
        .arg("\u{1f}")
        .arg(&copy)
        .arg(query)
        .output()
        .map_err(|e| BrowserLoginError::Io(format!("run sqlite3: {e}")))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(BrowserLoginError::Sqlite(message.trim().to_string()));
    }
    Ok(parse_cookie_output(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub fn music_u_from_header(header: &str) -> Option<&str> {
    header.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("MUSIC_U=")
            .filter(|value| !value.is_empty())
    })
}

fn parse_cookie_output(output: &str) -> Option<String> {
    let mut cookies = HashMap::new();
    for line in output.lines() {
        let Some((name, value)) = line.split_once('\u{1f}') else {
            continue;
        };
        if !value.is_empty() && !value.contains([';', '\n', '\r']) {
            cookies.entry(name).or_insert(value);
        }
    }
    if !cookies.contains_key("MUSIC_U") {
        return None;
    }
    Some(
        COOKIE_NAMES
            .iter()
            .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_music_u_and_csrf() {
        let header =
            parse_cookie_output("MUSIC_U\u{1f}abc\n__csrf\u{1f}csrf\nsession\u{1f}secret\n")
                .unwrap();
        assert!(header.contains("MUSIC_U=abc"));
        assert!(header.contains("__csrf=csrf"));
        assert!(!header.contains("session=secret"));
    }

    #[test]
    fn missing_music_u_is_not_a_login() {
        assert!(parse_cookie_output("__csrf\u{1f}csrf\n").is_none());
    }

    #[test]
    fn rejects_cookie_values_with_separators() {
        assert!(parse_cookie_output("MUSIC_U\u{1f}a;b\n").is_none());
    }
}
