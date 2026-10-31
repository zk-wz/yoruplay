//! Daemon entry point: single-instance lock, protected Unix socket, client
//! protocol, MPRIS startup and graceful shutdown.

use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use yoruplay_protocol::{ClientMessage, Config, Paths, ServerMessage, PROTOCOL_VERSION};

use crate::cache::FileCache;
use crate::mpris;
use crate::service::Daemon;

/// Run the background service until shutdown. Must be called from a Tokio
/// runtime. The caller owns logging setup.
pub async fn run(paths: Paths, config: Config) -> Result<()> {
    paths.ensure_dirs().context("create XDG directories")?;
    let _lock = acquire_instance_lock(&paths.lock_file())?;
    let socket_path = paths.socket_file();
    remove_socket_if_present(&socket_path)?;

    let api = yoruplay_api::NeteaseClient::new(&config.api, Some(paths.auth_file()))
        .map_err(|e| anyhow::anyhow!("cannot initialise NetEase client: {}", e.user_message()))?;
    let cache = FileCache::new(paths.cache_dir.clone(), config.cache.max_mb);
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

    let auth_daemon = daemon.clone();
    tokio::spawn(async move {
        auth_daemon.initialise_auth().await;
    });

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tracing::info!(socket = %socket_path.display(), "YoruPlay daemon ready");
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
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let daemon = daemon.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(daemon, stream).await {
                                tracing::debug!(error = %e, "IPC connection ended with an error");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "IPC accept failed"),
                }
            }
        }
    }

    drop(listener);
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

async fn handle_connection(daemon: Daemon, stream: tokio::net::UnixStream) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut read = FramedRead::new(
        read_half,
        LinesCodec::new_with_max_length(yoruplay_protocol::MAX_FRAME_BYTES),
    );
    let first = match timeout(Duration::from_secs(5), read.next()).await {
        Ok(Some(Ok(line))) => line,
        Ok(Some(Err(e))) => return Err(anyhow::anyhow!("invalid first frame: {e}")),
        Ok(None) => return Ok(()),
        Err(_) => return Err(anyhow::anyhow!("protocol handshake timed out")),
    };
    let hello: ClientMessage =
        serde_json::from_str(&first).map_err(|e| anyhow::anyhow!("invalid hello: {e}"))?;
    let version = match hello {
        ClientMessage::Hello { version } => version,
        _ => return Err(anyhow::anyhow!("first message must be hello")),
    };

    let (tx, rx) = mpsc::channel::<ServerMessage>(256);
    let writer = tokio::spawn(write_loop(write_half, rx));
    tx.send(ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").into(),
    })
    .await
    .context("send hello")?;
    if version != PROTOCOL_VERSION {
        let _ = tx
            .send(ServerMessage::err(
                0,
                yoruplay_protocol::ErrorInfo::new(
                    "protocol_mismatch",
                    format!(
                        "Protocol mismatch: daemon v{PROTOCOL_VERSION}, client v{version}. Restart both."
                    ),
                ),
            ))
            .await;
        drop(tx);
        let _ = timeout(Duration::from_secs(1), writer).await;
        return Ok(());
    }

    let mut events = daemon.subscribe();
    let event_sender = tx.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if event_sender
                        .try_send(ServerMessage::Event { event })
                        .is_err()
                    {
                        // A slow client loses events, not playback. It can call
                        // GetState to resynchronise.
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let permits = Arc::new(Semaphore::new(32));
    while let Some(line) = read.next().await {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                tracing::debug!(error = %e, "invalid IPC frame");
                break;
            }
        };
        let message: ClientMessage = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                let _ = tx
                    .send(ServerMessage::err(
                        0,
                        yoruplay_protocol::ErrorInfo::new(
                            "invalid_request",
                            "The daemon could not parse the request.",
                        ),
                    ))
                    .await;
                continue;
            }
        };
        match message {
            ClientMessage::Hello { .. } => {}
            ClientMessage::Request { id, command } => {
                let permit = match permits.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let daemon = daemon.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let response = match daemon.handle_command(command).await {
                        Ok(result) => ServerMessage::ok(id, result),
                        Err(error) => ServerMessage::err(id, error),
                    };
                    let _ = tx.send(response).await;
                });
            }
        }
    }
    forwarder.abort();
    drop(tx);
    let _ = timeout(Duration::from_secs(2), writer).await;
    Ok(())
}

async fn write_loop(
    write_half: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::Receiver<ServerMessage>,
) {
    let mut write = FramedWrite::new(
        write_half,
        LinesCodec::new_with_max_length(yoruplay_protocol::MAX_FRAME_BYTES),
    );
    while let Some(message) = rx.recv().await {
        let line = match serde_json::to_string(&message) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(error = %e, "could not encode IPC response");
                continue;
            }
        };
        if write.send(line).await.is_err() {
            break;
        }
    }
}
