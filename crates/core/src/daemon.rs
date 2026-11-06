//! Daemon entry point: single-instance lock, protected Unix socket, client
//! protocol, MPRIS startup and graceful shutdown.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use yoruplay_protocol::{ClientMessage, Config, Paths, ServerMessage, PROTOCOL_VERSION};

use crate::cache::FileCache;
use crate::mpris;
use crate::service::{request_owner, Daemon};

/// Run the background service until shutdown. Must be called from a Tokio
/// runtime. The caller owns logging setup.
pub async fn run(paths: Paths, config: Config) -> Result<()> {
    paths.ensure_dirs().context("create XDG directories")?;
    let _lock = acquire_instance_lock(&paths.lock_file())?;
    let socket_path = paths.socket_file();
    remove_socket_if_present(&socket_path)?;

    let api = yoruplay_api::NeteaseClient::new(&config.api, Some(paths.auth_file()))
        .map_err(|e| anyhow::anyhow!("cannot initialise NetEase client: {}", e.user_message()))?;
    let cache = FileCache::new(paths.metadata_dir(), config.cache.max_mb);
    cache
        .migrate_legacy()
        .context("migrate legacy metadata cache")?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let daemon = Daemon::build(config, paths.clone(), api, cache.clone(), shutdown_tx).await;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
        .context("set IPC socket permissions")?;
    let _socket_guard = SocketGuard(socket_path.clone());

    let ticker = daemon.start_ticker();
    let mpris_handle = match mpris::start(daemon.clone()).await {
        Ok(handle) => {
            tracing::info!("MPRIS registered as org.mpris.MediaPlayer2.yoruplay");
            Some(handle)
        }
        Err(e) => {
            daemon
                .add_warning(format!("MPRIS registration failed: {e}"))
                .await;
            tracing::warn!(error = %e, "MPRIS registration failed");
            None
        }
    };

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tracing::info!(socket = %socket_path.display(), "YoruPlay daemon ready");
    let limits = Arc::new(IpcLimits::new());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received; shutting down");
                daemon.request_shutdown();
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received; shutting down");
                daemon.request_shutdown();
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        if let Ok(permit) = limits.connections.clone().try_acquire_owned() {
                            let daemon = daemon.clone();
                            let limits = limits.clone();
                            connections.spawn(async move {
                                let _permit = permit;
                                if let Err(e) = handle_connection(daemon, stream, limits).await {
                                    tracing::debug!(error = %e, "IPC connection ended with an error");
                                }
                            });
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "IPC accept failed"),
                }
            }
        }
    }

    drop(listener);
    daemon.request_shutdown();
    while connections.join_next().await.is_some() {}
    ticker.abort();
    daemon.shutdown().await;
    if let Some(handle) = mpris_handle {
        handle.shutdown().await;
    }
    let _ = fs::remove_file(&socket_path);
    tracing::info!("YoruPlay daemon stopped");
    Ok(())
}

fn acquire_instance_lock(path: &Path) -> Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    file.try_lock_exclusive().map_err(|_| {
        anyhow::anyhow!(
            "another YoruPlay daemon is already running (lock: {})",
            path.display()
        )
    })?;
    Ok(file)
}

fn remove_socket_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove stale socket {}", path.display())),
    }
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

const CONNECTIONS: usize = 16;
const REQUESTS_PER_CLIENT: usize = 4;
const REQUEST_BYTES: usize = 16 * 1024 * 1024;
const OUTBOUND_BYTES: usize = 32 * 1024 * 1024;
const CLIENT_OUTBOUND_BYTES: usize = 8 * 1024 * 1024;

struct IpcLimits {
    connections: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    controls: Arc<Semaphore>,
    urgent: Arc<Semaphore>,
    request_bytes: Arc<Semaphore>,
    outbound_bytes: Arc<Semaphore>,
}

impl IpcLimits {
    fn new() -> Self {
        Self {
            connections: Arc::new(Semaphore::new(CONNECTIONS)),
            requests: Arc::new(Semaphore::new(16)),
            controls: Arc::new(Semaphore::new(8)),
            urgent: Arc::new(Semaphore::new(2)),
            request_bytes: Arc::new(Semaphore::new(REQUEST_BYTES)),
            outbound_bytes: Arc::new(Semaphore::new(OUTBOUND_BYTES)),
        }
    }
}

struct EncodedMessage {
    line: String,
    _global_bytes: OwnedSemaphorePermit,
    _client_bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct Outbound {
    tx: mpsc::Sender<EncodedMessage>,
    global_bytes: Arc<Semaphore>,
    client_bytes: Arc<Semaphore>,
}

impl Outbound {
    async fn send(&self, message: ServerMessage) -> Result<()> {
        let reservation = (yoruplay_protocol::MAX_FRAME_BYTES + 1) as u32;
        // Reserve BEFORE serialization, including tasks waiting to enqueue.
        let mut global = self
            .global_bytes
            .clone()
            .acquire_many_owned(reservation)
            .await?;
        let mut client = self
            .client_bytes
            .clone()
            .acquire_many_owned(reservation)
            .await?;
        let line = encode_message(&message)?;
        drop(message);
        let bytes = line.len() + 1;
        let global_bytes = global
            .split(bytes)
            .context("reserve encoded response bytes")?;
        let client_bytes = client
            .split(bytes)
            .context("reserve client response bytes")?;
        drop(global);
        drop(client);
        self.tx
            .send(EncodedMessage {
                line,
                _global_bytes: global_bytes,
                _client_bytes: client_bytes,
            })
            .await
            .context("queue IPC response")
    }

    fn try_send(&self, message: ServerMessage) -> Result<()> {
        let reservation = (yoruplay_protocol::MAX_FRAME_BYTES + 1) as u32;
        let mut global = self
            .global_bytes
            .clone()
            .try_acquire_many_owned(reservation)?;
        let mut client = self
            .client_bytes
            .clone()
            .try_acquire_many_owned(reservation)?;
        let line = encode_message(&message)?;
        drop(message);
        let bytes = line.len() + 1;
        let global_bytes = global.split(bytes).context("reserve event bytes")?;
        let client_bytes = client.split(bytes).context("reserve client event bytes")?;
        drop(global);
        drop(client);
        self.tx
            .try_send(EncodedMessage {
                line,
                _global_bytes: global_bytes,
                _client_bytes: client_bytes,
            })
            .context("slow IPC subscriber")
    }
}

struct BoundedJson(Vec<u8>);

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > yoruplay_protocol::MAX_FRAME_BYTES {
            return Err(std::io::Error::other(
                "IPC response exceeds the frame limit",
            ));
        }
        let needed = self.0.len() + bytes.len();
        if needed > self.0.capacity() {
            let capacity = needed
                .max(self.0.capacity().saturating_mul(2))
                .clamp(256, yoruplay_protocol::MAX_FRAME_BYTES);
            self.0.reserve_exact(capacity - self.0.len());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_message(message: &ServerMessage) -> Result<String> {
    let mut writer = BoundedJson(Vec::new());
    if let Err(error) = serde_json::to_writer(&mut writer, message) {
        if let ServerMessage::Response { id, .. } = message {
            return Ok(serde_json::to_string(&ServerMessage::err(
                *id,
                yoruplay_protocol::ErrorInfo::new(
                    "response_too_large",
                    "Response exceeds the IPC byte limit; request a smaller page.",
                ),
            ))?);
        }
        return Err(error.into());
    }
    Ok(String::from_utf8(writer.0)?)
}

fn is_control_request(command: &yoruplay_protocol::Command) -> bool {
    use yoruplay_protocol::Command;
    matches!(
        command,
        Command::PlaySongs { .. }
            | Command::AddSongs { .. }
            | Command::QueueRemove { .. }
            | Command::QueueClear
            | Command::QueuePlay { .. }
            | Command::Play
            | Command::Pause
            | Command::PlayPause
            | Command::Next
            | Command::Previous
            | Command::Stop
            | Command::SeekRelative { .. }
            | Command::SetPosition { .. }
            | Command::SetVolume { .. }
            | Command::SetMode { .. }
            | Command::CycleMode
            | Command::RefreshCurrent
            | Command::PlayCached { .. }
            | Command::ReloadConfig
            | Command::Shutdown
            | Command::ClearCache
            | Command::ClearAudioCache { .. }
    )
}

struct RequestTask {
    task: tokio::task::JoinHandle<()>,
    owner: u64,
    upgrade: bool,
}

async fn cancel_request(daemon: &Daemon, request: RequestTask) {
    request.task.abort();
    let _ = request.task.await;
    if request.upgrade {
        daemon.cancel_owned_upgrade(request.owner).await;
    }
}

async fn handle_connection(
    daemon: Daemon,
    stream: tokio::net::UnixStream,
    limits: Arc<IpcLimits>,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut read = FramedRead::new(
        read_half,
        LinesCodec::new_with_max_length(yoruplay_protocol::MAX_FRAME_BYTES),
    );
    let first = match timeout(Duration::from_secs(5), read.next()).await {
        Ok(Some(Ok(line))) => line,
        Ok(None) => return Ok(()),
        _ => return Err(anyhow::anyhow!("invalid or timed-out IPC handshake")),
    };
    let hello: ClientMessage = serde_json::from_str(&first).context("invalid hello")?;
    let ClientMessage::Hello { version } = hello else {
        return Err(anyhow::anyhow!("first message must be hello"));
    };
    drop(first);
    let (tx, rx) = mpsc::channel(8);
    let outbound = Outbound {
        tx,
        global_bytes: limits.outbound_bytes.clone(),
        client_bytes: Arc::new(Semaphore::new(CLIENT_OUTBOUND_BYTES)),
    };
    let mut writer = tokio::spawn(write_loop(write_half, rx));
    outbound
        .send(ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await?;
    if version != PROTOCOL_VERSION {
        outbound.send(ServerMessage::err(0, yoruplay_protocol::ErrorInfo::new("protocol_mismatch", format!("Protocol mismatch: daemon v{PROTOCOL_VERSION}, client v{version}. Restart both.")))).await?;
        drop(outbound);
        if timeout(Duration::from_secs(1), &mut writer).await.is_err() {
            writer.abort();
            let _ = writer.await;
        }
        return Ok(());
    }
    let mut forwarder = tokio::spawn(forward_events(
        daemon.clone(),
        daemon.subscribe(),
        outbound.clone(),
    ));
    let permits = Arc::new(Semaphore::new(REQUESTS_PER_CLIENT));
    let control_permits = Arc::new(Semaphore::new(REQUESTS_PER_CLIENT));
    let urgent_permits = Arc::new(Semaphore::new(2));
    let client_request_bytes = Arc::new(Semaphore::new(8 * 1024 * 1024));
    let (completed_tx, mut completed_rx) = mpsc::channel::<(u64, u64)>(REQUESTS_PER_CLIENT * 2 + 2);
    let mut requests: HashMap<u64, RequestTask> = HashMap::new();
    let mut writer_done = false;
    let mut forwarder_done = false;
    let mut shutdown = daemon.subscribe_shutdown();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let line = tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = &mut writer => { writer_done = true; break; },
            _ = &mut forwarder => { forwarder_done = true; break; },
            completed = completed_rx.recv() => {
                if let Some((id, owner)) = completed {
                    if requests.get(&id).is_some_and(|request| request.owner == owner) { requests.remove(&id); }
                }
                continue;
            }
            line = read.next() => match line { Some(Ok(line)) => line, _ => break },
        };
        let bytes = line.len().max(1) as u32;
        let global_bytes = match limits.request_bytes.clone().try_acquire_many_owned(bytes) {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let client_bytes = match client_request_bytes.clone().try_acquire_many_owned(bytes) {
            Ok(permit) => permit,
            Err(_) => break,
        };
        let message: ClientMessage = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                if outbound
                    .try_send(ServerMessage::err(
                        0,
                        yoruplay_protocol::ErrorInfo::new(
                            "invalid_request",
                            "The daemon could not parse the request.",
                        ),
                    ))
                    .is_err()
                {
                    break;
                }
                continue;
            }
        };
        drop(line);
        match message {
            ClientMessage::Hello { .. } => {}
            ClientMessage::Cancel { id } => {
                if let Some(request) = requests.remove(&id) {
                    cancel_request(&daemon, request).await;
                }
            }
            ClientMessage::Request { id, command } => {
                if requests.contains_key(&id) {
                    if outbound
                        .try_send(ServerMessage::err(
                            id,
                            yoruplay_protocol::ErrorInfo::new(
                                "duplicate_request",
                                "Request ID is already in flight",
                            ),
                        ))
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                let urgent = matches!(
                    &command,
                    yoruplay_protocol::Command::Stop | yoruplay_protocol::Command::Next
                );
                let control = is_control_request(&command);
                let client_slots = if urgent {
                    &urgent_permits
                } else if control {
                    &control_permits
                } else {
                    &permits
                };
                let global_slots = if urgent {
                    &limits.urgent
                } else if control {
                    &limits.controls
                } else {
                    &limits.requests
                };
                let permit = match client_slots.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        if outbound
                            .try_send(ServerMessage::err(
                                id,
                                yoruplay_protocol::ErrorInfo::new(
                                    "busy",
                                    "Client request class limit reached",
                                ),
                            ))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let global_permit = match global_slots.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        if outbound
                            .try_send(ServerMessage::err(
                                id,
                                yoruplay_protocol::ErrorInfo::new(
                                    "busy",
                                    "Daemon request limit reached",
                                ),
                            ))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let owner = request_owner();
                let upgrade = matches!(&command, yoruplay_protocol::Command::UpgradeAudio { .. });
                let worker = daemon.clone();
                let sender = outbound.clone();
                let completed = completed_tx.clone();
                let task = tokio::spawn(async move {
                    let _client_slot = permit;
                    let request_bytes = (global_bytes, client_bytes);
                    let response = match worker.handle_owned_command(command, owner).await {
                        Ok(result) => ServerMessage::ok(id, result),
                        Err(error) => ServerMessage::err(id, error),
                    };
                    drop(request_bytes);
                    let _query_slot = if control {
                        drop(global_permit);
                        None
                    } else {
                        Some(global_permit)
                    };
                    let _ = sender.send(response).await;
                    let _ = completed.send((id, owner)).await;
                });
                requests.insert(
                    id,
                    RequestTask {
                        task,
                        owner,
                        upgrade,
                    },
                );
            }
        }
    }
    if !forwarder_done {
        forwarder.abort();
        let _ = forwarder.await;
    }
    for (_, request) in requests {
        cancel_request(&daemon, request).await;
    }
    drop(outbound);
    if !writer_done && timeout(Duration::from_secs(2), &mut writer).await.is_err() {
        writer.abort();
        let _ = writer.await;
    }
    Ok(())
}

async fn forward_events(
    daemon: Daemon,
    mut events: tokio::sync::broadcast::Receiver<yoruplay_protocol::Event>,
    outbound: Outbound,
) {
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    loop {
        let first = match events.recv().await {
            Ok(event) => Some(event),
            Err(RecvError::Lagged(_)) => None,
            Err(RecvError::Closed) => return,
        };
        let mut resync = first.is_none();
        let mut next = first;
        let mut latest_queue = None;
        let mut latest_playback = None;
        for _ in 0..32 {
            if resync {
                break;
            }
            let event = match next.take() {
                Some(event) => event,
                None => match events.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => {
                        resync = true;
                        break;
                    }
                    Err(TryRecvError::Closed) => return,
                },
            };
            match event {
                event @ yoruplay_protocol::Event::QueueChanged { .. } => latest_queue = Some(event),
                event @ yoruplay_protocol::Event::Playback(_) => latest_playback = Some(event),
                event => {
                    if outbound.send(ServerMessage::Event { event }).await.is_err() {
                        return;
                    }
                }
            }
        }
        if resync {
            // Subscribe before taking the snapshot so no newer state can be lost.
            // One bounded forwarder waits for capacity; a transient full queue is
            // not a disconnected client and must not tear down accepted RPCs.
            events = daemon.subscribe();
            for event in daemon.state_events().await {
                if outbound.send(ServerMessage::Event { event }).await.is_err() {
                    return;
                }
            }
            continue;
        }
        for event in [latest_queue, latest_playback].into_iter().flatten() {
            if outbound.send(ServerMessage::Event { event }).await.is_err() {
                return;
            }
        }
    }
}

async fn write_loop(
    write_half: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<EncodedMessage>,
) {
    let mut write = FramedWrite::new(
        write_half,
        LinesCodec::new_with_max_length(yoruplay_protocol::MAX_FRAME_BYTES),
    );
    while let Some(message) = rx.recv().await {
        if !matches!(
            timeout(Duration::from_secs(3), write.send(message.line)).await,
            Ok(Ok(()))
        ) {
            break;
        }
        // Byte permits include the frame being written, not only queued frames.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_responses_return_a_bounded_error_with_the_request_id() {
        let message = ServerMessage::ok(
            42,
            yoruplay_protocol::CommandResult::Data(serde_json::json!(
                "x".repeat(yoruplay_protocol::MAX_FRAME_BYTES)
            )),
        );
        let line = encode_message(&message).unwrap();
        assert!(line.len() < 1024);
        let decoded: ServerMessage = serde_json::from_str(&line).unwrap();
        match decoded {
            ServerMessage::Response {
                id,
                result: None,
                error: Some(error),
            } => {
                assert_eq!(id, 42);
                assert_eq!(error.code, "response_too_large");
            }
            other => panic!("expected a bounded error, got {other:?}"),
        }
    }

    #[test]
    fn serializer_rejects_oversized_chunks_before_retaining_them() {
        let mut writer = BoundedJson(Vec::new());
        assert!(writer
            .write_all(&vec![0; yoruplay_protocol::MAX_FRAME_BYTES + 1])
            .is_err());
        assert!(writer.0.is_empty());
        assert_eq!(writer.0.capacity(), 0);
    }

    #[tokio::test]
    async fn notification_lag_resynchronizes_current_state_through_bounded_backpressure() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: root.path().join("config"),
            data_dir: root.path().join("data"),
            cache_dir: root.path().join("cache"),
            runtime_dir: root.path().join("run"),
        };
        let config = Config::default();
        let api = yoruplay_api::NeteaseClient::new(&config.api, None).unwrap();
        let (shutdown, _) = watch::channel(false);
        let daemon = Daemon::build(
            config,
            paths.clone(),
            api,
            FileCache::new(paths.metadata_dir(), 32),
            shutdown,
        )
        .await;
        daemon
            .handle_command(yoruplay_protocol::Command::SetVolume { volume: 0.25 })
            .await
            .unwrap();
        let (burst, events) = tokio::sync::broadcast::channel(2);
        let mut stale = daemon.playback().await;
        stale.volume = 0.9;
        for _ in 0..8 {
            burst
                .send(yoruplay_protocol::Event::Playback(Box::new(stale.clone())))
                .unwrap();
        }
        let (tx, mut received) = mpsc::channel(1);
        let outbound = Outbound {
            tx,
            global_bytes: Arc::new(Semaphore::new(OUTBOUND_BYTES)),
            client_bytes: Arc::new(Semaphore::new(CLIENT_OUTBOUND_BYTES)),
        };
        let forwarder = tokio::spawn(forward_events(daemon.clone(), events, outbound));
        let mut recovered = false;
        for _ in 0..3 {
            let frame = timeout(Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .expect("notification lag disconnected a healthy subscriber");
            if let ServerMessage::Event {
                event: yoruplay_protocol::Event::Playback(state),
            } = serde_json::from_str(&frame.line).unwrap()
            {
                assert_eq!(state.volume, 0.25);
                recovered = true;
            }
        }
        assert!(
            recovered,
            "authoritative playback state was not resynchronized"
        );
        daemon
            .handle_command(yoruplay_protocol::Command::SetVolume { volume: 0.5 })
            .await
            .unwrap();
        let frame = timeout(Duration::from_secs(3), received.recv())
            .await
            .unwrap()
            .unwrap();
        match serde_json::from_str(&frame.line).unwrap() {
            ServerMessage::Event {
                event: yoruplay_protocol::Event::Playback(state),
            } => assert_eq!(state.volume, 0.5),
            other => panic!("unexpected state after recovery: {other:?}"),
        }
        forwarder.abort();
        let _ = forwarder.await;
        daemon.shutdown().await;
    }
}
