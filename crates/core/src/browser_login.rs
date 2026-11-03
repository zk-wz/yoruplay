//! Browser-assisted login for browsers whose cookie databases are readable
//! without a keyring. Firefox stores cookies in a plain SQLite database.
//!
//! Cookies are secrets: this module never logs values, copies the database to
//! a private temporary directory, queries only the NetEase cookie names it
//! needs, and removes the copy before returning.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
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
            _ => Some("Check Firefox profile permissions and retry, or use --cookie-stdin."),
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
            .unwrap_or(UNIX_EPOCH)
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
    let connection = Connection::open_with_flags(
        &copy,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| BrowserLoginError::Sqlite(e.to_string()))?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|e| BrowserLoginError::Sqlite(e.to_string()))?;

    let placeholders = COOKIE_NAMES
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT name, value FROM moz_cookies \
         WHERE host IN ('music.163.com', '.music.163.com') \
         AND path='/' AND name IN ({placeholders}) \
         ORDER BY CASE host WHEN '.music.163.com' THEN 0 ELSE 1 END"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|e| BrowserLoginError::Sqlite(e.to_string()))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(COOKIE_NAMES.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| BrowserLoginError::Sqlite(e.to_string()))?;
    let mut cookies = HashMap::new();
    for row in rows {
        let (name, value) = row.map_err(|e| BrowserLoginError::Sqlite(e.to_string()))?;
        if !value.is_empty() && !value.contains([';', '\n', '\r']) {
            cookies.entry(name).or_insert(value);
        }
    }
    if !cookies.contains_key("MUSIC_U") {
        return Ok(None);
    }
    let header = COOKIE_NAMES
        .iter()
        .filter_map(|name| cookies.get(*name).map(|value| format!("{name}={value}")))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Some(header))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_database() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cookies.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (host TEXT, name TEXT, value TEXT, path TEXT, expiry INTEGER);",
            )
            .unwrap();
        (dir, path)
    }

    #[test]
    fn reads_only_net_ease_cookies() {
        let (_dir, path) = fake_database();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO moz_cookies VALUES ('.music.163.com','MUSIC_U','abc','/',0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO moz_cookies VALUES ('.music.163.com','__csrf','csrf','/',0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO moz_cookies VALUES ('.example.com','MUSIC_U','other','/',0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO moz_cookies VALUES ('.music.163.com','session','secret','/other',0)",
                [],
            )
            .unwrap();
        drop(connection);
        let header = read_profile(&path).unwrap().unwrap();
        assert!(header.contains("MUSIC_U=abc"));
        assert!(header.contains("__csrf=csrf"));
        assert!(!header.contains("other"));
        assert!(!header.contains("secret"));
    }

    #[test]
    fn missing_music_u_is_not_a_login() {
        let (_dir, path) = fake_database();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO moz_cookies VALUES ('.music.163.com','__csrf','csrf','/',0)",
                [],
            )
            .unwrap();
        drop(connection);
        assert!(read_profile(&path).unwrap().is_none());
    }
}
