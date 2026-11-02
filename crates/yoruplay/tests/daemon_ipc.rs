//! End-to-end local IPC tests: two clients, event fan-out, single instance and
//! graceful shutdown. No NetEase account or audio device is needed.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command as ProcessCommand};
use tokio::time::{sleep, timeout};
use yoruplay_ipc::IpcClient;
use yoruplay_protocol::{
    Command, CommandResult, Event, PlayMode, PlaybackStatus, PROTOCOL_VERSION,
};

struct DaemonProcess {
    child: Child,
    paths: yoruplay_protocol::Paths,
    dir: tempfile::TempDir,
}

impl DaemonProcess {
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let paths = yoruplay_protocol::Paths {
            config_dir: root.join("config/yoruplay"),
            data_dir: root.join("data/yoruplay"),
            cache_dir: root.join("cache/yoruplay"),
            runtime_dir: root.join("run/yoruplay"),
        };
        paths.ensure_dirs().unwrap();
        std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let exe = env!("CARGO_BIN_EXE_yoruplay");
        let child = ProcessCommand::new(exe)
            .arg("daemon")
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent-yoruplay-bus",
            )
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn daemon");
        let process = Self { child, paths, dir };
        process.wait_ready().await;
        process
    }

    async fn wait_ready(&self) {
        for _ in 0..100 {
            if IpcClient::connect(self.paths.socket_file()).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("daemon did not become ready");
    }

    async fn shutdown(mut self) {
        if let Ok(client) = IpcClient::connect(self.paths.socket_file()).await {
            let _ = client.request(Command::Shutdown).await;
        }
        let _ = timeout(Duration::from_secs(5), self.child.wait()).await;
        let _ = &self.dir;
    }
}

fn socket_dir(paths: &yoruplay_protocol::Paths) -> &Path {
    paths.runtime_dir.parent().unwrap()
}

#[tokio::test]
async fn two_clients_share_state_and_events() {
    let daemon = DaemonProcess::start().await;
    assert!(daemon.paths.socket_file().exists());
    let runtime_dir = &daemon.paths.runtime_dir;
    assert_eq!(
        std::fs::metadata(runtime_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(daemon.paths.socket_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let _ = socket_dir(&daemon.paths);

    let client_a = IpcClient::connect(daemon.paths.socket_file())
        .await
        .unwrap();
    let client_b = IpcClient::connect(daemon.paths.socket_file())
        .await
        .unwrap();
    assert_eq!(
        client_a.daemon_version(),
        &format!("protocol-v{PROTOCOL_VERSION}")
    );

    let state = client_a.request(Command::GetState).await.unwrap();
    match state {
        CommandResult::State(snapshot) => {
            assert!(snapshot.queue.items.is_empty());
            assert!(!snapshot.auth.logged_in);
            assert_eq!(snapshot.playback.status, PlaybackStatus::Stopped);
        }
        other => panic!("unexpected state response: {other:?}"),
    }

    let mut events = client_b.subscribe();
    client_a
        .request(Command::SetVolume { volume: 0.33 })
        .await
        .unwrap();
    let mut saw_volume = false;
    for _ in 0..20 {
        match timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Ok(Event::Playback(playback))) => {
                if (playback.volume - 0.33).abs() < 0.001 {
                    saw_volume = true;
                    break;
                }
            }
            Ok(Ok(_)) | Err(_) => continue,
            Ok(Err(_)) => break,
        }
    }
    assert!(saw_volume, "second client did not receive the volume event");

    client_a
        .request(Command::SetMode {
            mode: PlayMode::TrackLoop,
        })
        .await
        .unwrap();
    let state = client_b.request(Command::GetState).await.unwrap();
    let CommandResult::State(snapshot) = state else {
        panic!("unexpected state response")
    };
    assert_eq!(snapshot.playback.mode, PlayMode::TrackLoop);
    assert_eq!(snapshot.playback.volume, 0.33);

    let second = ProcessCommand::new(env!("CARGO_BIN_EXE_yoruplay"))
        .arg("daemon")
        .env("HOME", daemon.dir.path())
        .env("XDG_CONFIG_HOME", daemon.dir.path().join("config"))
        .env("XDG_DATA_HOME", daemon.dir.path().join("data"))
        .env("XDG_CACHE_HOME", daemon.dir.path().join("cache"))
        .env("XDG_RUNTIME_DIR", daemon.dir.path().join("run"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-yoruplay-bus",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("run second daemon");
    assert!(!second.success(), "a second daemon must not start");

    client_a.close().await;
    client_b.close().await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn empty_queue_commands_are_clear_errors() {
    let daemon = DaemonProcess::start().await;
    let client = IpcClient::connect(daemon.paths.socket_file())
        .await
        .unwrap();
    let error = client.request(Command::Play).await.unwrap_err();
    assert_eq!(error.code, "invalid_argument");
    assert!(error.message.contains("queue"));
    client.close().await;
    daemon.shutdown().await;
}

async fn spawn_daemon_for(root: &Path, paths: &yoruplay_protocol::Paths) -> Child {
    let exe = env!("CARGO_BIN_EXE_yoruplay");
    let child = ProcessCommand::new(exe)
        .arg("daemon")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-yoruplay-bus",
        )
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn daemon");
    for _ in 0..100 {
        if IpcClient::connect(paths.socket_file()).await.is_ok() {
            return child;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon did not become ready");
}

#[tokio::test]
async fn cli_shutdown_stops_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = yoruplay_protocol::Paths {
        config_dir: root.join("config/yoruplay"),
        data_dir: root.join("data/yoruplay"),
        cache_dir: root.join("cache/yoruplay"),
        runtime_dir: root.join("run/yoruplay"),
    };
    paths.ensure_dirs().unwrap();
    std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = spawn_daemon_for(root, &paths).await;

    let output = ProcessCommand::new(env!("CARGO_BIN_EXE_yoruplay"))
        .arg("shutdown")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-yoruplay-bus",
        )
        .output()
        .await
        .expect("run yoruplay shutdown");
    assert!(
        output.status.success(),
        "shutdown failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !paths.socket_file().exists(),
        "shutdown returned but the socket still exists"
    );
    let status = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("daemon did not exit")
        .expect("wait daemon");
    assert!(status.success(), "daemon exited with {status}");
}

#[tokio::test]
async fn volume_and_mode_survive_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = yoruplay_protocol::Paths {
        config_dir: root.join("config/yoruplay"),
        data_dir: root.join("data/yoruplay"),
        cache_dir: root.join("cache/yoruplay"),
        runtime_dir: root.join("run/yoruplay"),
    };
    paths.ensure_dirs().unwrap();
    std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = spawn_daemon_for(root, &paths).await;
    let client = IpcClient::connect(paths.socket_file()).await.unwrap();
    client
        .request(Command::SetVolume { volume: 0.21 })
        .await
        .unwrap();
    client
        .request(Command::SetMode {
            mode: PlayMode::Shuffle,
        })
        .await
        .unwrap();
    client.request(Command::Shutdown).await.unwrap();
    client.close().await;
    let _ = timeout(Duration::from_secs(5), child.wait()).await;

    let mut child = spawn_daemon_for(root, &paths).await;
    let client = IpcClient::connect(paths.socket_file()).await.unwrap();
    let result = client.request(Command::GetState).await.unwrap();
    let CommandResult::State(snapshot) = result else {
        panic!("unexpected state");
    };
    assert!((snapshot.playback.volume - 0.21).abs() < 0.001);
    assert_eq!(snapshot.playback.mode, PlayMode::Shuffle);
    client.request(Command::Shutdown).await.unwrap();
    client.close().await;
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}
