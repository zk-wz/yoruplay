//! Authentication state and cookie handling.
//!
//! Cookies are secrets.  This module never derives `Debug` for the raw map,
//! never logs cookie values, writes auth.json with mode 0600 using an atomic
//! rename, and keeps device identity separate from the credential file.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use rand::Rng;
use serde::{Deserialize, Serialize};
use yoruplay_protocol::AuthState;

use crate::ApiError;

const AUTH_VERSION: u32 = 1;
/// Same practical behaviour as JavaScript's `encodeURIComponent` for cookie
/// names/values: unreserved characters stay literal, everything else is
/// percent-encoded. Encoding the name as `MUSIC%5FU` would create a different
/// cookie name on the server, so `_`, `-`, `.` and `~` must be preserved.
const COOKIE_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'_')
    .remove(b'-')
    .remove(b'.')
    .remove(b'~');
const EAPI_APPVER: &str = "8.20.20.231215173437";

#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    pub os: String,
    pub osver: String,
    pub appver: String,
    pub channel: String,
    pub device_id: String,
    pub nuid: String,
    pub nnid: String,
    pub wnmcid: String,
    pub buildver: String,
    pub resolution: String,
    pub versioncode: String,
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("os", &self.os)
            .field("device_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Device {
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        let now_ms = now_ms();
        let nuid = random_hex(&mut rng, 32);
        Self {
            os: "android".into(),
            osver: "14".into(),
            appver: EAPI_APPVER.into(),
            channel: "xiaomi".into(),
            device_id: random_hex(&mut rng, 32).to_uppercase(),
            nnid: format!("{nuid},{now_ms}"),
            nuid,
            wnmcid: format!("{}.{}.01.0", random_letters(&mut rng, 6), now_ms),
            buildver: (now_ms / 1000).to_string(),
            resolution: "1920x1080".into(),
            versioncode: "140".into(),
        }
    }

    pub fn refresh_timestamps(&mut self) {
        let now = now_ms();
        self.buildver = (now / 1000).to_string();
        // WNMCID embeds creation time; keep it stable for the device.
        if self.nnid.is_empty() {
            self.nnid = format!("{},{}", self.nuid, now);
        }
    }
}

impl Default for Device {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredAuth {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    device: Option<Device>,
    #[serde(default)]
    cookies: BTreeMap<String, String>,
    #[serde(default)]
    profile: Option<yoruplay_protocol::UserProfile>,
}

#[derive(Clone)]
pub struct SessionData {
    pub device: Device,
    cookies: BTreeMap<String, String>,
    pub profile: Option<yoruplay_protocol::UserProfile>,
    pub expired: bool,
    auth_path: Option<PathBuf>,
}

impl fmt::Debug for SessionData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionData")
            .field("device", &self.device)
            .field("cookie_count", &self.cookies.len())
            .field("logged_in", &self.logged_in())
            .field("profile", &self.profile)
            .field("expired", &self.expired)
            .finish()
    }
}

impl SessionData {
    /// Loads auth.json. A missing file is not an error. Corrupt auth is
    /// returned as an error so the daemon can report it without crashing.
    pub fn load(path: Option<PathBuf>) -> Result<Self, ApiError> {
        let Some(path) = path else {
            return Ok(Self::anonymous(None));
        };
        if !path.exists() {
            return Ok(Self::anonymous(Some(path)));
        }
        let text = fs::read_to_string(&path).map_err(|e| ApiError::SessionIo(e.to_string()))?;
        let stored: StoredAuth =
            serde_json::from_str(&text).map_err(|e| ApiError::SessionCorrupt(e.to_string()))?;
        if stored.version != 0 && stored.version != AUTH_VERSION {
            return Err(ApiError::SessionCorrupt(format!(
                "unsupported auth file version {}",
                stored.version
            )));
        }
        Ok(Self {
            device: stored.device.unwrap_or_default(),
            cookies: stored.cookies,
            profile: stored.profile,
            expired: false,
            auth_path: Some(path),
        })
    }

    pub fn anonymous(auth_path: Option<PathBuf>) -> Self {
        Self {
            device: Device::new(),
            cookies: BTreeMap::new(),
            profile: None,
            expired: false,
            auth_path,
        }
    }

    pub fn logged_in(&self) -> bool {
        self.cookies.get("MUSIC_U").is_some_and(|v| !v.is_empty())
    }

    pub fn auth_state(&self) -> AuthState {
        AuthState {
            logged_in: self.logged_in() && !self.expired,
            profile: if self.expired {
                None
            } else {
                self.profile.clone()
            },
            session_expired: self.expired,
        }
    }

    pub fn csrf(&self) -> String {
        self.cookies.get("__csrf").cloned().unwrap_or_default()
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(String::as_str)
    }

    pub fn merge_cookies<'a>(&mut self, values: impl IntoIterator<Item = &'a str>) -> bool {
        let mut changed = false;
        for value in values {
            if let Some((name, val)) = parse_set_cookie(value) {
                if name.is_empty() {
                    continue;
                }
                if self.cookies.get(&name) != Some(&val) {
                    self.cookies.insert(name, val);
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn import_cookie_string(&mut self, raw: &str) -> Result<(), ApiError> {
        let mut any = false;
        for part in raw.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((name, value)) = part.split_once('=') else {
                return Err(ApiError::InvalidCookie(format!("missing '=' in `{part}`")));
            };
            let name = name.trim();
            let value = value.trim();
            if name.is_empty() {
                return Err(ApiError::InvalidCookie("empty cookie name".into()));
            }
            self.cookies.insert(name.to_string(), value.to_string());
            any = true;
        }
        if !any {
            return Err(ApiError::InvalidCookie("empty cookie string".into()));
        }
        if !self.logged_in() {
            return Err(ApiError::InvalidCookie(
                "cookie does not contain MUSIC_U".into(),
            ));
        }
        self.expired = false;
        Ok(())
    }

    pub fn set_profile(&mut self, profile: yoruplay_protocol::UserProfile) {
        self.profile = Some(profile);
        self.expired = false;
    }

    pub fn clear_auth(&mut self) {
        self.cookies.clear();
        self.profile = None;
        self.expired = false;
    }

    pub fn mark_expired(&mut self) {
        self.expired = true;
        self.profile = None;
    }

    /// Persist only when a real login exists. The file is written atomically
    /// with 0600 permissions; the parent directory is forced to 0700.
    pub fn persist(&self) -> Result<(), ApiError> {
        let Some(path) = &self.auth_path else {
            return Ok(());
        };
        if !self.logged_in() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ApiError::SessionIo(e.to_string()))?;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
        let stored = StoredAuth {
            version: AUTH_VERSION,
            device: Some(self.device.clone()),
            cookies: self.cookies.clone(),
            profile: self.profile.clone(),
        };
        let text =
            serde_json::to_vec_pretty(&stored).map_err(|e| ApiError::SessionIo(e.to_string()))?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| ApiError::SessionIo(e.to_string()))?;
        file.write_all(&text)
            .map_err(|e| ApiError::SessionIo(e.to_string()))?;
        file.sync_all()
            .map_err(|e| ApiError::SessionIo(e.to_string()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .map_err(|e| ApiError::SessionIo(e.to_string()))?;
        fs::rename(&tmp, path).map_err(|e| ApiError::SessionIo(e.to_string()))?;
        Ok(())
    }

    pub fn remove_persisted(&self) -> Result<(), ApiError> {
        let Some(path) = &self.auth_path else {
            return Ok(());
        };
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::SessionIo(e.to_string())),
        }
    }

    pub fn auth_path(&self) -> Option<&Path> {
        self.auth_path.as_deref()
    }

    /// Base cookie set used by both Web API and eapi. Stored server cookies
    /// take precedence over generated defaults.
    pub fn base_cookies(&self) -> BTreeMap<String, String> {
        let d = &self.device;
        let mut map = BTreeMap::new();
        for (k, v) in [
            ("__remember_me", "true".to_string()),
            ("ntes_kaola_ad", "1".to_string()),
            ("_ntes_nuid", d.nuid.clone()),
            ("_ntes_nnid", d.nnid.clone()),
            ("WNMCID", d.wnmcid.clone()),
            ("WEVNSM", "1.0.0".to_string()),
            ("osver", d.osver.clone()),
            ("os", d.os.clone()),
            ("channel", d.channel.clone()),
            ("appver", d.appver.clone()),
            ("deviceId", d.device_id.clone()),
        ] {
            map.insert(k.to_string(), v);
        }
        for (k, v) in &self.cookies {
            map.insert(k.clone(), v.clone());
        }
        map
    }

    pub fn cookie_header(&self) -> String {
        cookie_map_to_string(&self.base_cookies())
    }

    pub fn eapi_header(&self, request_id: &str) -> BTreeMap<String, String> {
        let d = &self.device;
        let mut header = BTreeMap::new();
        for (k, v) in [
            ("osver", d.osver.clone()),
            ("deviceId", d.device_id.clone()),
            ("os", d.os.clone()),
            ("appver", d.appver.clone()),
            ("versioncode", d.versioncode.clone()),
            ("mobilename", String::new()),
            ("buildver", (now_ms() / 1000).to_string()),
            ("resolution", d.resolution.clone()),
            ("__csrf", self.csrf()),
            ("channel", d.channel.clone()),
            ("requestId", request_id.to_string()),
        ] {
            header.insert(k.to_string(), v);
        }
        if let Some(v) = self.cookies.get("MUSIC_U") {
            header.insert("MUSIC_U".into(), v.clone());
        }
        if let Some(v) = self.cookies.get("MUSIC_A") {
            header.insert("MUSIC_A".into(), v.clone());
        }
        if let Some(v) = self.cookies.get("NMTID") {
            header.insert("NMTID".into(), v.clone());
        }
        header
    }
}

pub fn cookie_map_to_string(map: &BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, COOKIE_ENCODE_SET),
                utf8_percent_encode(v, COOKIE_ENCODE_SET)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn parse_set_cookie(value: &str) -> Option<(String, String)> {
    let first = value.split(';').next()?.trim();
    let (name, val) = first.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), val.trim().to_string()))
}

fn random_hex<R: Rng>(rng: &mut R, n: usize) -> String {
    (0..n)
        .map(|_| format!("{:x}", rng.gen_range(0..16)))
        .collect()
}

fn random_letters<R: Rng>(rng: &mut R, n: usize) -> String {
    (0..n)
        .map(|_| {
            let idx = rng.gen_range(0..26);
            (b'a' + idx) as char
        })
        .collect()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yoruplay_protocol::UserProfile;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yoruplay-auth-{}-{name}", std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn parses_set_cookie_first_pair_only() {
        assert_eq!(
            parse_set_cookie("MUSIC_U=abc==; Path=/; HttpOnly; Max-Age=315360000"),
            Some(("MUSIC_U".into(), "abc==".into()))
        );
        assert_eq!(parse_set_cookie("bad"), None);
    }

    #[test]
    fn cookie_roundtrip_is_encoded() {
        let map = BTreeMap::from([
            ("MUSIC_U".to_string(), "a+b/c=".to_string()),
            ("x".to_string(), "空格".to_string()),
        ]);
        let header = cookie_map_to_string(&map);
        assert!(header.contains("MUSIC_U=a%2Bb%2Fc%3D"), "{header}");
        assert!(header.contains("x=%E7%A9%BA%E6%A0%BC"));
    }

    #[test]
    fn imports_cookie_and_rejects_missing_music_u() {
        let mut s = SessionData::anonymous(None);
        assert!(s.import_cookie_string("a=b").is_err());
        s.import_cookie_string("MUSIC_U=secret; __csrf=token")
            .unwrap();
        assert!(s.logged_in());
        assert_eq!(s.csrf(), "token");
    }

    #[test]
    fn persists_with_private_permissions_and_loads() {
        let path = temp_path("persist.json");
        let mut s = SessionData::anonymous(Some(path.clone()));
        s.import_cookie_string("MUSIC_U=secret; __csrf=token")
            .unwrap();
        s.set_profile(UserProfile {
            user_id: 7,
            nickname: "tester".into(),
            avatar_url: None,
            signature: None,
            background_url: None,
        });
        s.persist().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let loaded = SessionData::load(Some(path.clone())).unwrap();
        assert!(loaded.logged_in());
        assert_eq!(loaded.profile.as_ref().unwrap().user_id, 7);
        assert_eq!(loaded.device.device_id, s.device.device_id);
        loaded.remove_persisted().unwrap();
    }

    #[test]
    fn debug_does_not_leak_cookie() {
        let mut s = SessionData::anonymous(None);
        s.import_cookie_string("MUSIC_U=super-secret-token")
            .unwrap();
        let text = format!("{s:?}");
        assert!(!text.contains("super-secret-token"));
        assert!(text.contains("logged_in: true"));
    }

    #[test]
    fn expired_state_is_not_logged_in() {
        let mut s = SessionData::anonymous(None);
        s.import_cookie_string("MUSIC_U=secret").unwrap();
        s.mark_expired();
        let state = s.auth_state();
        assert!(!state.logged_in);
        assert!(state.session_expired);
    }
}
