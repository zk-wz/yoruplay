use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::Config;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct PathEntry {
    pub path: PathBuf,
    pub kind: &'static str,
    pub purpose: &'static str,
    pub exists: Option<bool>,
    pub lifecycle: &'static str,
}

/// XDG directories used by YoruPlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> io::Result<Self> {
        Self::from_env(|key| env::var_os(key))
    }

    /// Testable variant. `lookup` is only used for XDG/HOME variables.
    pub fn from_env<F>(lookup: F) -> io::Result<Self>
    where
        F: Fn(&str) -> Option<std::ffi::OsString>,
    {
        let home = lookup("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        let config_dir = lookup("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("yoruplay");
        let data_dir = lookup("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("yoruplay");
        let cache_dir = lookup("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("yoruplay");
        let runtime_dir = match lookup("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("yoruplay"),
            // Prefer the normal per-user runtime directory when a shell did not
            // export XDG_RUNTIME_DIR. The /tmp fallback is only for systems
            // without a usable /run/user/<uid>.
            _ => {
                let uid = unsafe { libc::geteuid() };
                fallback_runtime_dir(uid, Path::new("/run/user"))
            }
        };
        Ok(Self {
            config_dir: absolute(config_dir)?,
            data_dir: absolute(data_dir)?,
            cache_dir: absolute(cache_dir)?,
            runtime_dir: absolute(runtime_dir)?,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn auth_file(&self) -> PathBuf {
        self.data_dir.join("auth.json")
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    pub fn socket_file(&self) -> PathBuf {
        self.runtime_dir.join("yoruplay.sock")
    }

    pub fn daemon_lock_file(&self) -> PathBuf {
        self.data_dir.join("daemon.lock")
    }

    pub fn config_lock_file(&self) -> PathBuf {
        Config::lock_path(&self.config_file())
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.cache_dir.join("metadata")
    }

    pub fn audio_dir(&self, config: &Config) -> PathBuf {
        if config.cache.audio.dir.is_empty() {
            self.cache_dir.join("audio")
        } else {
            PathBuf::from(&config.cache.audio.dir)
        }
    }

    pub fn audio_index_file(&self, config: &Config) -> PathBuf {
        self.audio_dir(config).join("index.redb")
    }

    pub fn cache_lock_file(&self, config: &Config) -> PathBuf {
        self.audio_dir(config).join("cache.lock")
    }

    pub fn queue_file(&self, revision: u64) -> PathBuf {
        self.data_dir.join(format!("queue-{revision}.json"))
    }

    /// Inventory only: no directories, lock files or temporary files are opened.
    pub fn inventory(&self, config: &Config) -> Vec<PathEntry> {
        let audio = self.audio_dir(config);
        let mut entries = Vec::new();
        let mut add = |path: PathBuf, kind, purpose, lifecycle| {
            let exists = if kind == "pattern" {
                None
            } else {
                Some(path.exists())
            };
            entries.push(PathEntry {
                path,
                kind,
                purpose,
                exists,
                lifecycle,
            });
        };
        add(
            self.config_dir.clone(),
            "directory",
            "Configuration root",
            "preserved",
        );
        add(
            self.config_file(),
            "file",
            "Typed TOML settings",
            "preserved; atomic replacement",
        );
        add(
            self.config_lock_file(),
            "file",
            "Configuration update ownership",
            "persistent lock inode",
        );
        add(
            self.config_dir.join(".config.toml.tmp-*-*"),
            "pattern",
            "Atomic configuration writes",
            "removed after commit or failed write; crash remnants may remain",
        );
        add(
            self.data_dir.clone(),
            "directory",
            "Private account and playback state",
            "preserved",
        );
        add(
            self.auth_file(),
            "file",
            "Saved credentials (contents never listed)",
            "removed only by explicit logout",
        );
        add(
            self.daemon_lock_file(),
            "file",
            "Global daemon ownership lock",
            "daemon ownership lifetime; shared across runtime directories",
        );
        add(
            self.data_dir.join("auth.tmp-*"),
            "pattern",
            "Atomic credential updates",
            "renamed on commit",
        );
        add(
            self.state_file(),
            "file",
            "Compact playback state (v2)",
            "v1 inline queues migrate on next save: publish queue, then state pointer",
        );
        add(
            self.data_dir.join("state.tmp-*"),
            "pattern",
            "Atomic state writes",
            "renamed on commit",
        );
        add(
            self.data_dir.join("state.json.corrupt-*"),
            "pattern",
            "Quarantined invalid state",
            "retained for recovery",
        );
        add(self.data_dir.join("queue-*.json"), "pattern", "Versioned queue revisions", "current revision retained; obsolete revisions removed after state commit; invalid/missing queue quarantines state, not queue");
        add(
            self.data_dir.join("queue-*.tmp-*"),
            "pattern",
            "Atomic queue revision writes",
            "renamed on commit",
        );
        add(
            self.cache_dir.clone(),
            "directory",
            "Cache root and logs",
            "never recursively cleared by cache clear",
        );
        add(
            self.metadata_dir(),
            "directory",
            "Metadata TTL cache",
            "metadata-only clear and budget trimming",
        );
        add(
            self.metadata_dir().join("*.json"),
            "pattern",
            "Cached metadata objects",
            "TTL/budget/metadata clear",
        );
        add(
            self.metadata_dir().join("*.tmp-*"),
            "pattern",
            "Atomic metadata writes",
            "renamed on commit",
        );
        add(
            audio.clone(),
            "directory",
            "Configured audio cache directory",
            "managed audio only; directory changes require restart",
        );
        add(
            audio.join("<id> - <artist> - <title> [<quality>]*.<ext>"),
            "pattern",
            "Complete native-format audio",
            "FIFO eviction; pinned readers retain their inode",
        );
        add(
            audio.join("*.<ext>.entry.json"),
            "pattern",
            "Complete-entry recovery manifests",
            "streamed offline inspection; removed with managed audio",
        );
        add(
            audio.join("<id>-<version>.<png|jpg>"),
            "pattern",
            "Bounded local artwork",
            "removed with its owning cache entry",
        );
        add(
            audio.join(".capability-*-*"),
            "pattern",
            "Explicit doctor filesystem capability probe",
            "brief publication link removed immediately after probe",
        );
        add(
            self.audio_index_file(config),
            "file",
            "Complete-entry disk index",
            "owned by the audio cache",
        );
        add(
            self.cache_lock_file(config),
            "file",
            "Shared cache ownership lock",
            "persistent lock inode; daemon/offline mutations serialize",
        );
        add(
            audio,
            "unnamed_inode_backing",
            "Anonymous staging and bounded disk scratch; no pathname",
            "O_TMPFILE; last handle close/process death releases incomplete data",
        );
        add(
            self.log_file(),
            "file",
            "Daemon log",
            "never cache-cleared; no rotation implemented",
        );
        add(
            self.tui_log_file(),
            "file",
            "TUI log",
            "never cache-cleared; no rotation implemented",
        );
        add(
            self.runtime_dir.clone(),
            "directory",
            "IPC runtime (UID-specific /tmp fallback without XDG_RUNTIME_DIR)",
            "session/runtime lifetime",
        );
        add(
            self.socket_file(),
            "socket",
            "Local daemon IPC",
            "daemon shutdown removes socket",
        );

        add(
            std::env::temp_dir().join("yoruplay-firefox-*"),
            "pattern",
            "Firefox cookie import snapshot directories",
            "private; deleted when import finishes",
        );
        for name in ["cookies.sqlite", "cookies.sqlite-wal", "cookies.sqlite-shm"] {
            add(
                std::env::temp_dir().join("yoruplay-firefox-*").join(name),
                "pattern",
                "Firefox cookie import database snapshot",
                "private; deleted with import temporary directory",
            );
        }
        entries
    }

    pub fn log_file(&self) -> PathBuf {
        self.cache_dir.join("yoruplay.log")
    }

    pub fn tui_log_file(&self) -> PathBuf {
        self.cache_dir.join("tui.log")
    }

    /// Create all directories with owner-only access where appropriate.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [
            &self.config_dir,
            &self.data_dir,
            &self.cache_dir,
            &self.runtime_dir,
        ] {
            fs::create_dir_all(dir)?;
        }
        for dir in [&self.data_dir, &self.runtime_dir] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn absolute(path: PathBuf) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn fallback_runtime_dir(uid: u32, run_user_root: &Path) -> PathBuf {
    let user_runtime = run_user_root.join(uid.to_string());
    if user_runtime.is_dir() {
        user_runtime.join("yoruplay")
    } else {
        PathBuf::from(format!("/tmp/yoruplay-{uid}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn uses_xdg_variables() {
        let env: HashMap<&str, &str> = HashMap::from([
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_CACHE_HOME", "/cache"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]);
        let paths = Paths::from_env(|k| env.get(k).map(|v| (*v).into())).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/cfg/yoruplay"));
        assert_eq!(paths.data_dir, PathBuf::from("/data/yoruplay"));
        assert_eq!(
            paths.socket_file(),
            PathBuf::from("/run/user/1000/yoruplay/yoruplay.sock")
        );
    }

    #[test]
    fn falls_back_to_home_directories() {
        let env: HashMap<&str, &str> = HashMap::from([("HOME", "/home/u")]);
        let paths = Paths::from_env(|k| env.get(k).map(|v| (*v).into())).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/home/u/.config/yoruplay"));
        assert_eq!(paths.cache_dir, PathBuf::from("/home/u/.cache/yoruplay"));
        let uid = unsafe { libc::geteuid() };
        assert_eq!(
            paths.runtime_dir,
            fallback_runtime_dir(uid, Path::new("/run/user"))
        );
    }

    #[test]
    fn fallback_runtime_prefers_run_user() {
        let root = std::env::temp_dir().join(format!("yoruplay-paths-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("1000")).unwrap();
        assert_eq!(
            fallback_runtime_dir(1000, &root),
            root.join("1000/yoruplay")
        );
        assert_eq!(
            fallback_runtime_dir(1000, &root.join("missing")),
            PathBuf::from("/tmp/yoruplay-1000")
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
