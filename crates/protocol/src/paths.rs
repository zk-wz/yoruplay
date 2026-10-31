use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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
            // Keep a private fallback for unusual sessions (TTY login, tests).
            _ => {
                let uid = unsafe { libc::geteuid() };
                PathBuf::from(format!("/tmp/yoruplay-{uid}"))
            }
        };
        Ok(Self { config_dir, data_dir, cache_dir, runtime_dir })
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

    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join("yoruplay.lock")
    }

    pub fn mpv_socket_file(&self) -> PathBuf {
        self.runtime_dir.join("mpv.sock")
    }

    pub fn log_file(&self) -> PathBuf {
        self.cache_dir.join("yoruplay.log")
    }

    pub fn tui_log_file(&self) -> PathBuf {
        self.cache_dir.join("tui.log")
    }

    /// Create all directories with owner-only access where appropriate.
    pub fn ensure_dirs(&self) -> io::Result<()> {
        for dir in [&self.config_dir, &self.data_dir, &self.cache_dir, &self.runtime_dir] {
            fs::create_dir_all(dir)?;
        }
        for dir in [&self.data_dir, &self.runtime_dir] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
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
        assert_eq!(paths.socket_file(), PathBuf::from("/run/user/1000/yoruplay/yoruplay.sock"));
    }

    #[test]
    fn falls_back_to_home_directories() {
        let env: HashMap<&str, &str> = HashMap::from([("HOME", "/home/u")]);
        let paths = Paths::from_env(|k| env.get(k).map(|v| (*v).into())).unwrap();
        assert_eq!(paths.config_dir, PathBuf::from("/home/u/.config/yoruplay"));
        assert_eq!(paths.cache_dir, PathBuf::from("/home/u/.cache/yoruplay"));
        assert!(paths.runtime_dir.to_string_lossy().starts_with("/tmp/yoruplay-"));
    }
}
