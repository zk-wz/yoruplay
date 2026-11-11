//! CLI side-effect and validation boundaries with isolated application paths.

use std::process::Command;

fn isolated_command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yoruplay"));
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"));
    command
}

#[test]
fn readonly_commands_do_not_create_any_application_paths() {
    let temporary = tempfile::tempdir().unwrap();
    for args in [
        vec!["--json", "listpath"],
        vec!["config", "list", "--json"],
        vec!["config", "get", "playback.retry_count", "--json"],
        vec!["--json", "cache", "info"],
        vec!["cache", "list", "--json"],
    ] {
        let output = isolated_command(temporary.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(value.is_object(), "{args:?}");
        for directory in ["home", "config", "data", "cache", "runtime"] {
            assert!(
                !temporary.path().join(directory).exists(),
                "{args:?} created {directory}"
            );
        }
    }
}

#[test]
fn cache_list_rejects_oversized_pages_without_creating_paths() {
    let temporary = tempfile::tempdir().unwrap();
    for limit in ["0", "101"] {
        let output = isolated_command(temporary.path())
            .args(["cache", "list", "--limit", limit])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!temporary.path().join("cache").exists());
    }
}

#[test]
fn liked_rejects_oversized_pages_without_creating_paths() {
    let temporary = tempfile::tempdir().unwrap();
    for limit in ["0", "1001"] {
        let output = isolated_command(temporary.path())
            .args(["library", "liked", "--limit", limit])
            .output()
            .unwrap();
        assert!(!output.status.success(), "limit {limit}");
        for directory in ["home", "config", "data", "cache", "runtime"] {
            assert!(
                !temporary.path().join(directory).exists(),
                "limit {limit} created {directory}"
            );
        }
    }
}

#[test]
fn configuration_keys_are_typed_and_invalid_writes_preserve_file() {
    let temporary = tempfile::tempdir().unwrap();
    let output = isolated_command(temporary.path())
        .args(["--json", "config", "set", "cache.audio.max_gb", "0.5G"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["persisted"], true);
    assert_eq!(value["value"], 0.5);
    assert_eq!(value["status"], "persisted_daemon_not_running");
    let file = temporary.path().join("config/yoruplay/config.toml");
    let before = std::fs::read(&file).unwrap();
    for (key, value) in [
        ("cache.audio.max_gb", "NaN"),
        ("api.quality", "lossles"),
        ("playback.retry_count", "-1"),
        ("cache.no_such_key", "true"),
    ] {
        let output = isolated_command(temporary.path())
            .args(["config", "set", key, value])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{key}={value}");
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
    let output = isolated_command(temporary.path())
        .args(["config", "reset", "--all"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[test]
fn default_cache_clear_preserves_audio_and_logs() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cache/yoruplay");
    std::fs::create_dir_all(root.join("metadata")).unwrap();
    std::fs::create_dir_all(root.join("audio")).unwrap();
    std::fs::write(root.join("metadata/lyrics_1.json"), "{}").unwrap();
    std::fs::write(root.join("audio/keep.mp3"), "audio").unwrap();
    std::fs::write(root.join("yoruplay.log"), "daemon").unwrap();
    std::fs::write(root.join("tui.log"), "tui").unwrap();
    let output = isolated_command(temporary.path())
        .args(["cache", "clear", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("metadata/lyrics_1.json").exists());
    assert_eq!(
        std::fs::read(root.join("audio/keep.mp3")).unwrap(),
        b"audio"
    );
    assert_eq!(std::fs::read(root.join("yoruplay.log")).unwrap(), b"daemon");
    assert_eq!(std::fs::read(root.join("tui.log")).unwrap(), b"tui");
}
