//! Minimal, robust controller for the system `mpv` process over its JSON IPC.
//!
//! YoruPlay never links libmpv and never parses mpv's terminal output. The only
//! control protocol is the newline-delimited JSON IPC socket.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    FileLoaded,
    EndFile {
        reason: String,
        error: Option<String>,
    },
    Property {
        name: String,
        value: Value,
    },
    Exited {
        error: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MpvError {
    #[error(
        "YoruPlay requires mpv, but it was not found in PATH.\n\nOn Arch Linux, install it with:\n  sudo pacman -S mpv"
    )]
    Missing,
    #[error("mpv is not executable: {0}")]
    NotExecutable(String),
    #[error("could not start mpv: {0}")]
    Spawn(String),
    #[error("mpv IPC socket did not appear: {0}")]
    Socket(String),
    #[error("mpv IPC command failed: {0}")]
    Command(String),
    #[error("mpv IPC command timed out")]
    Timeout,
    #[error("mpv exited unexpectedly: {0}")]
    Exited(String),
    #[error("mpv I/O error: {0}")]
    Io(String),
}

impl MpvError {
    pub fn user_message(&self) -> String {
        match self {
            MpvError::Missing => self.to_string(),
            MpvError::NotExecutable(p) => {
                format!(
                    "mpv at {p} cannot be executed. Check file permissions and the installation."
                )
            }
            MpvError::Spawn(e) => format!("mpv could not start: {e}"),
            MpvError::Socket(e) => {
                format!("mpv started, but its IPC socket was not usable: {e}")
            }
            MpvError::Command(e) => format!("mpv rejected a command: {e}"),
            MpvError::Timeout => "mpv did not answer an IPC command in time.".into(),
            MpvError::Exited(e) => format!("mpv exited unexpectedly: {e}"),
            MpvError::Io(e) => format!("mpv IPC failed: {e}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MpvOptions {
    pub socket_path: PathBuf,
    /// Normalised 0.0..=1.0.
    pub volume: f64,
    /// PulseAudio is the production backend (pipewire-pulse). Tests use null.
    pub audio_output: String,
    pub extra_args: Vec<String>,
}

impl MpvOptions {
    pub fn new(socket_path: PathBuf, volume: f64) -> Self {
        Self {
            socket_path,
            volume: volume.clamp(0.0, 1.0),
            audio_output: "pulse".into(),
            extra_args: Vec::new(),
        }
    }
}

struct MpvInner {
    write_tx: mpsc::Sender<String>,
    pending: PendingMap,
    events: broadcast::Sender<MpvEvent>,
    child: Mutex<Option<Child>>,
    socket_path: PathBuf,
    next_id: AtomicU64,
}

pub struct MpvController {
    inner: Arc<MpvInner>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl std::fmt::Debug for MpvController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvController")
            .field("socket_path", &self.inner.socket_path)
            .finish_non_exhaustive()
    }
}

impl MpvController {
    /// Check the PATH executable without spawning an audio process.
    pub async fn available_version() -> Result<String, MpvError> {
        let output = timeout(
            Duration::from_secs(5),
            Command::new("mpv").arg("--version").output(),
        )
        .await
        .map_err(|_| MpvError::Timeout)?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MpvError::Missing
            } else {
                MpvError::NotExecutable(e.to_string())
            }
        })?;
        if !output.status.success() {
            return Err(MpvError::NotExecutable(output.status.to_string()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("mpv")
            .to_string())
    }

    pub async fn spawn(options: MpvOptions) -> Result<Self, MpvError> {
        if let Some(parent) = options.socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| MpvError::Spawn(e.to_string()))?;
        }
        let _ = std::fs::remove_file(&options.socket_path);
        let volume = (options.volume * 100.0).round().clamp(0.0, 100.0);
        let mut args: Vec<String> = vec![
            "--no-config".into(),
            "--no-video".into(),
            "--audio-display=no".into(),
            format!("--ao={}", options.audio_output),
            "--audio-client-name=YoruPlay".into(),
            "--input-terminal=no".into(),
            "--terminal=no".into(),
            "--idle=yes".into(),
            "--keep-open=no".into(),
            "--loop-file=no".into(),
            "--cache=yes".into(),
            "--cache-secs=120".into(),
            "--demuxer-max-bytes=64MiB".into(),
            "--demuxer-max-back-bytes=16MiB".into(),
            "--network-timeout=30".into(),
            "--ytdl=no".into(),
            "--load-scripts=no".into(),
            "--msg-level=all=warn".into(),
            format!("--volume={volume}"),
            format!("--input-ipc-server={}", options.socket_path.display()),
        ];
        args.extend(options.extra_args);
        let mut command = Command::new("mpv");
        command
            .args(&args)
            .env("DISABLE_RTKIT", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MpvError::Missing
            } else {
                MpvError::Spawn(e.to_string())
            }
        })?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "mpv", "{line}");
                }
            });
        }
        let stream = match wait_for_socket(&options.socket_path, &mut child).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = std::fs::remove_file(&options.socket_path);
                return Err(e);
            }
        };
        let (read_half, write_half) = stream.into_split();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let (write_tx, write_rx) = mpsc::channel::<String>(128);
        let reader_task = spawn_reader(read_half, pending.clone(), events.clone());
        let writer_task = spawn_writer(write_half, write_rx);
        let inner = Arc::new(MpvInner {
            write_tx,
            pending,
            events,
            child: Mutex::new(Some(child)),
            socket_path: options.socket_path.clone(),
            next_id: AtomicU64::new(1),
        });
        let controller = Self {
            inner,
            reader_task,
            writer_task,
        };
        // Observe the properties the service needs. Failures here mean the IPC
        // process is not healthy, so tear it down and report a useful error.
        for (id, name) in [
            (1u64, "pause"),
            (2, "time-pos"),
            (3, "duration"),
            (4, "volume"),
            (5, "idle-active"),
        ] {
            controller
                .command(vec![json!("observe_property"), json!(id), json!(name)])
                .await?;
        }
        controller.set_loop_file(false).await?;
        Ok(controller)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MpvEvent> {
        self.inner.events.subscribe()
    }

    pub async fn command(&self, args: Vec<Value>) -> Result<Value, MpvError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(id, tx);
        }
        let message = json!({ "command": args, "request_id": id }).to_string();
        if self.inner.write_tx.send(message).await.is_err() {
            self.inner.pending.lock().await.remove(&id);
            return Err(MpvError::Exited("IPC writer stopped".into()));
        }
        match timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(MpvError::Command(error)),
            Ok(Err(_)) => Err(MpvError::Exited("IPC reader stopped".into())),
            Err(_) => {
                self.inner.pending.lock().await.remove(&id);
                Err(MpvError::Timeout)
            }
        }
    }

    pub async fn set_property(&self, name: &str, value: Value) -> Result<(), MpvError> {
        self.command(vec![json!("set_property"), json!(name), value])
            .await?;
        Ok(())
    }

    pub async fn get_property(&self, name: &str) -> Result<Value, MpvError> {
        let value = self
            .command(vec![json!("get_property"), json!(name)])
            .await?;
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    }

    pub async fn loadfile(&self, url: &str, start_ms: u64, paused: bool) -> Result<(), MpvError> {
        self.set_property("pause", json!(paused)).await?;
        let mut args = vec![json!("loadfile"), json!(url), json!("replace")];
        if start_ms > 0 {
            let start = start_ms as f64 / 1000.0;
            args.push(json!(-1));
            args.push(json!(format!("start={start:.3}")));
        }
        self.command(args).await?;
        Ok(())
    }

    pub async fn pause(&self, paused: bool) -> Result<(), MpvError> {
        self.set_property("pause", json!(paused)).await
    }

    pub async fn set_volume(&self, volume: f64) -> Result<(), MpvError> {
        let volume = (volume.clamp(0.0, 1.0) * 100.0).round();
        self.set_property("volume", json!(volume)).await
    }

    pub async fn seek_ms(&self, position_ms: u64) -> Result<(), MpvError> {
        let seconds = position_ms as f64 / 1000.0;
        self.command(vec![json!("seek"), json!(seconds), json!("absolute")])
            .await?;
        Ok(())
    }

    pub async fn seek_relative_ms(&self, offset_ms: i64) -> Result<(), MpvError> {
        let seconds = offset_ms as f64 / 1000.0;
        self.command(vec![json!("seek"), json!(seconds), json!("relative")])
            .await?;
        Ok(())
    }

    pub async fn set_loop_file(&self, enabled: bool) -> Result<(), MpvError> {
        self.set_property("loop-file", json!(if enabled { "inf" } else { "no" }))
            .await
    }

    pub async fn stop(&self) -> Result<(), MpvError> {
        self.command(vec![json!("stop")]).await?;
        Ok(())
    }

    pub async fn shutdown(self) {
        let _ = timeout(Duration::from_secs(1), self.command(vec![json!("quit")])).await;
        let mut child = self.inner.child.lock().await.take();
        if let Some(child) = child.as_mut() {
            if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        self.reader_task.abort();
        self.writer_task.abort();
        let _ = std::fs::remove_file(&self.inner.socket_path);
    }
}

impl Drop for MpvInner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn wait_for_socket(path: &Path, child: &mut Child) -> Result<UnixStream, MpvError> {
    let deadline = tokio::time::Instant::now() + SOCKET_WAIT;
    loop {
        if let Ok(stream) = UnixStream::connect(path).await {
            return Ok(stream);
        }
        if let Some(status) = child.try_wait().map_err(|e| MpvError::Io(e.to_string()))? {
            return Err(MpvError::Spawn(format!("mpv exited with {status}")));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MpvError::Socket(path.display().to_string()));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_reader(
    read_half: OwnedReadHalf,
    pending: PendingMap,
    events: broadcast::Sender<MpvEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        tracing::debug!(target: "mpv", "unparseable IPC line");
                        continue;
                    };
                    dispatch(&value, &pending, &events).await;
                }
                Err(e) => {
                    let _ = events.send(MpvEvent::Exited {
                        error: Some(e.to_string()),
                    });
                    break;
                }
            }
        }
        let mut pending = pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err("mpv IPC closed".into()));
        }
        let _ = events.send(MpvEvent::Exited { error: None });
    })
}

fn spawn_writer(mut write_half: OwnedWriteHalf, mut rx: mpsc::Receiver<String>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut message) = rx.recv().await {
            message.push('\n');
            if write_half.write_all(message.as_bytes()).await.is_err() {
                break;
            }
        }
    })
}

async fn dispatch(value: &Value, pending: &PendingMap, events: &broadcast::Sender<MpvEvent>) {
    if let Some(request_id) = value.get("request_id").and_then(Value::as_u64) {
        let sender = pending.lock().await.remove(&request_id);
        if let Some(sender) = sender {
            let result = match value.get("error").and_then(Value::as_str) {
                Some("success") | None => Ok(value.clone()),
                Some(error) => Err(error.to_string()),
            };
            let _ = sender.send(result);
        }
        return;
    }
    let Some(event) = value.get("event").and_then(Value::as_str) else {
        return;
    };
    match event {
        "file-loaded" => {
            let _ = events.send(MpvEvent::FileLoaded);
        }
        "end-file" => {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty())
                .map(str::to_string);
            let _ = events.send(MpvEvent::EndFile { reason, error });
        }
        "property-change" => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let data = value.get("data").cloned().unwrap_or(Value::Null);
            let _ = events.send(MpvEvent::Property { name, value: data });
        }
        "shutdown" => {
            let _ = events.send(MpvEvent::Exited { error: None });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_silent_wav(path: &Path, milliseconds: u32) {
        let sample_rate = 44_100u32;
        let samples = sample_rate * milliseconds / 1000;
        let data_len = samples * 2;
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        file.write_all(&2u16.to_le_bytes()).unwrap();
        file.write_all(&16u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_len.to_le_bytes()).unwrap();
        file.write_all(&vec![0u8; data_len as usize]).unwrap();
    }

    #[test]
    fn controller_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<MpvController>();
        let future = MpvController::spawn(MpvOptions::new(PathBuf::from("/tmp/x"), 0.1));
        assert_send_future(future);
        fn assert_send_future<F: std::future::Future + Send>(_: F) {}
    }

    #[tokio::test]
    async fn real_mpv_ipc_if_installed() {
        if MpvController::available_version().await.is_err() {
            eprintln!("mpv not installed; skipping IPC integration test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let wav = dir.path().join("silence.wav");
        write_silent_wav(&wav, 1500);
        let mut options = MpvOptions::new(socket.clone(), 0.5);
        options.audio_output = "null".into();
        let mpv = MpvController::spawn(options).await.expect("spawn mpv");
        let mut events = mpv.subscribe();
        mpv.loadfile(wav.to_str().unwrap(), 0, false).await.unwrap();
        let mut loaded = false;
        for _ in 0..40 {
            match timeout(Duration::from_millis(250), events.recv()).await {
                Ok(Ok(MpvEvent::FileLoaded)) => {
                    loaded = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
        assert!(loaded, "mpv never reported file-loaded");
        mpv.pause(true).await.unwrap();
        let paused = mpv.get_property("pause").await.unwrap();
        assert_eq!(paused, json!(true));
        mpv.set_volume(0.25).await.unwrap();
        let volume = mpv.get_property("volume").await.unwrap();
        assert_eq!(volume, json!(25.0));
        mpv.stop().await.unwrap();
        mpv.shutdown().await;
        assert!(!socket.exists());
    }
}
