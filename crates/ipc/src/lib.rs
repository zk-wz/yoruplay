//! Async client for the YoruPlay daemon Unix-socket protocol.
//!
//! The TUI and CLI share this implementation so the wire contract is decoded in
//! exactly one place.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use yoruplay_protocol::{
    ClientMessage, Command, CommandResult, ErrorInfo, Event, ServerMessage, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<CommandResult, ErrorInfo>>>>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("could not connect to the YoruPlay daemon: {0}")]
    Connect(String),
    #[error("the daemon did not complete the protocol handshake in time")]
    HandshakeTimeout,
    #[error("protocol version mismatch: daemon speaks v{daemon}, client speaks v{client}")]
    VersionMismatch { daemon: u32, client: u32 },
    #[error("the daemon connection closed")]
    Closed,
    #[error("daemon request timed out")]
    Timeout,
    #[error("could not encode a request: {0}")]
    Encode(String),
    #[error("invalid daemon response: {0}")]
    Decode(String),
    #[error("daemon rejected shutdown: {0}")]
    Daemon(String),
}

impl IpcError {
    pub fn to_error_info(&self) -> ErrorInfo {
        let hint = match self {
            IpcError::Connect(_) => Some(
                "Start the daemon with `yoruplay daemon` or open the TUI, which auto-starts it.",
            ),
            IpcError::VersionMismatch { .. } => {
                Some("Restart the daemon and TUI from the same YoruPlay build.")
            }
            _ => None,
        };
        ErrorInfo {
            code: match self {
                IpcError::Connect(_) => "daemon_unavailable",
                IpcError::HandshakeTimeout => "daemon_handshake_timeout",
                IpcError::VersionMismatch { .. } => "protocol_mismatch",
                IpcError::Closed => "daemon_disconnected",
                IpcError::Timeout => "daemon_timeout",
                IpcError::Encode(_) => "encode_error",
                IpcError::Decode(_) => "decode_error",
                IpcError::Daemon(_) => "daemon_error",
            }
            .into(),
            message: self.to_string(),
            hint: hint.map(str::to_string),
        }
    }
}

/// Send a shutdown request even when the daemon speaks a different protocol
/// version. This is used by `yoruplay shutdown` during an upgrade: the normal
/// client refuses a version mismatch, but the `shutdown` command itself has a
/// stable wire shape. The socket is owner-only, so this does not broaden access.
pub async fn shutdown_any_version(path: impl AsRef<Path>) -> Result<(), IpcError> {
    let path = path.as_ref();
    // The server waits for the client Hello before sending its own Hello. A
    // wrong guess still reveals the server version, but the server closes the
    // connection, so reconnect with the reported version before sending
    // Shutdown.
    let (reader, writer, server_version) = open_shutdown_connection(path, PROTOCOL_VERSION).await?;
    let (mut reader, mut writer) = if server_version != PROTOCOL_VERSION {
        drop(reader);
        drop(writer);
        let (reader, writer, _) = open_shutdown_connection(path, server_version).await?;
        (reader, writer)
    } else {
        (reader, writer)
    };

    write_shutdown_message(
        &mut writer,
        &ClientMessage::Request {
            id: 1,
            command: Command::Shutdown,
        },
    )
    .await?;
    let mut line = String::new();
    for _ in 0..32 {
        match read_shutdown_line(&mut reader, &mut line).await {
            Ok(()) => {}
            // The daemon may close immediately after scheduling shutdown.
            Err(IpcError::Closed) => return Ok(()),
            Err(error) => return Err(error),
        }
        let message: ServerMessage =
            serde_json::from_str(line.trim()).map_err(|e| IpcError::Decode(e.to_string()))?;
        if let ServerMessage::Response {
            id: 1,
            result,
            error,
        } = message
        {
            if error.is_some() {
                return Err(IpcError::Daemon(
                    error.map(|e| e.message).unwrap_or_default(),
                ));
            }
            if result.is_some() {
                return Ok(());
            }
        }
    }
    Err(IpcError::Decode(
        "daemon did not answer the shutdown request".into(),
    ))
}

type ShutdownReader = BufReader<tokio::net::unix::OwnedReadHalf>;
type ShutdownWriter = tokio::net::unix::OwnedWriteHalf;

async fn open_shutdown_connection(
    path: &Path,
    guess: u32,
) -> Result<(ShutdownReader, ShutdownWriter, u32), IpcError> {
    let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| IpcError::Connect("connection timed out".into()))?
        .map_err(|e| IpcError::Connect(e.to_string()))?;
    let (read_half, mut write_half) = stream.into_split();
    write_shutdown_message(&mut write_half, &ClientMessage::Hello { version: guess }).await?;
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    read_shutdown_line(&mut reader, &mut line).await?;
    let hello: ServerMessage =
        serde_json::from_str(line.trim()).map_err(|e| IpcError::Decode(e.to_string()))?;
    let version = match hello {
        ServerMessage::Hello { version, .. } => version,
        _ => return Err(IpcError::Decode("daemon did not start with a hello".into())),
    };
    Ok((reader, write_half, version))
}

async fn write_shutdown_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &ClientMessage,
) -> Result<(), IpcError> {
    let mut encoded =
        serde_json::to_string(message).map_err(|e| IpcError::Encode(e.to_string()))?;
    encoded.push('\n');
    timeout(SHUTDOWN_TIMEOUT, writer.write_all(encoded.as_bytes()))
        .await
        .map_err(|_| IpcError::Timeout)?
        .map_err(|_| IpcError::Closed)?;
    timeout(SHUTDOWN_TIMEOUT, writer.flush())
        .await
        .map_err(|_| IpcError::Timeout)?
        .map_err(|_| IpcError::Closed)
}

async fn read_shutdown_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut String,
) -> Result<(), IpcError> {
    line.clear();
    let count = timeout(SHUTDOWN_TIMEOUT, reader.read_line(line))
        .await
        .map_err(|_| IpcError::Timeout)?
        .map_err(|_| IpcError::Closed)?;
    if count == 0 {
        return Err(IpcError::Closed);
    }
    Ok(())
}

pub struct IpcClient {
    tx: mpsc::Sender<ClientMessage>,
    pending: Pending,
    events: broadcast::Sender<Event>,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
    daemon_version: String,
}

impl std::fmt::Debug for IpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcClient")
            .field("daemon_version", &self.daemon_version)
            .finish_non_exhaustive()
    }
}

impl Drop for IpcClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

impl IpcClient {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, IpcError> {
        let path = path.as_ref();
        let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(path))
            .await
            .map_err(|_| IpcError::Connect("connection timed out".into()))?
            .map_err(|e| IpcError::Connect(e.to_string()))?;
        let (read_half, write_half) = stream.into_split();
        let (tx, write_rx) = mpsc::channel::<ClientMessage>(64);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(512);
        let (hello_tx, hello_rx) = oneshot::channel::<u32>();
        let reader_task = {
            let pending = pending.clone();
            let events = events.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut stream =
                    FramedRead::new(read_half, LinesCodec::new_with_max_length(MAX_FRAME_BYTES));
                let mut hello_tx = Some(hello_tx);
                while let Some(line) = stream.next().await {
                    let Ok(line) = line else { break };
                    let msg: ServerMessage = match serde_json::from_str(&line) {
                        Ok(msg) => msg,
                        Err(_) => continue,
                    };
                    match msg {
                        ServerMessage::Hello { version, .. } => {
                            if let Some(tx) = hello_tx.take() {
                                let _ = tx.send(version);
                            }
                        }
                        ServerMessage::Response { id, result, error } => {
                            if let Some(sender) = pending.lock().await.remove(&id) {
                                let outcome = match (result, error) {
                                    (Some(result), _) => Ok(result),
                                    (None, Some(error)) => Err(error),
                                    (None, None) => Err(ErrorInfo::new(
                                        "empty_response",
                                        "The daemon returned an empty response.",
                                    )),
                                };
                                let _ = sender.send(outcome);
                            }
                        }
                        ServerMessage::Event { event } => {
                            let _ = events.send(event);
                        }
                    }
                }
                let mut pending = pending.lock().await;
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(ErrorInfo::new(
                        "daemon_disconnected",
                        "The daemon connection closed.",
                    )));
                }
                let _ = tx.send(ClientMessage::Hello { version: 0 }).await;
            })
        };
        let writer_task = tokio::spawn(async move {
            let mut stream =
                FramedWrite::new(write_half, LinesCodec::new_with_max_length(MAX_FRAME_BYTES));
            let mut write_rx = write_rx;
            while let Some(msg) = write_rx.recv().await {
                let line = match serde_json::to_string(&msg) {
                    Ok(line) => line,
                    Err(_) => continue,
                };
                if stream.send(line).await.is_err() {
                    break;
                }
            }
        });
        tx.send(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })
        .await
        .map_err(|_| IpcError::Closed)?;
        let daemon_version = match timeout(Duration::from_secs(5), hello_rx).await {
            Ok(Ok(version)) => {
                if version != PROTOCOL_VERSION {
                    reader_task.abort();
                    writer_task.abort();
                    return Err(IpcError::VersionMismatch {
                        daemon: version,
                        client: PROTOCOL_VERSION,
                    });
                }
                version.to_string()
            }
            Ok(Err(_)) => {
                reader_task.abort();
                writer_task.abort();
                return Err(IpcError::Closed);
            }
            Err(_) => {
                reader_task.abort();
                writer_task.abort();
                return Err(IpcError::HandshakeTimeout);
            }
        };
        // We do not have the daemon's package version in Hello yet; bump the
        // protocol version whenever the contract changes. Keeping the field
        // populated makes future negotiation possible without a breaking wire
        // change.
        let client = Self {
            tx,
            pending,
            events,
            next_id: AtomicU64::new(1),
            reader_task,
            writer_task,
            daemon_version: format!("protocol-v{daemon_version}"),
        };
        Ok(client)
    }

    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn request(&self, command: Command) -> Result<CommandResult, ErrorInfo> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if self
            .tx
            .send(ClientMessage::Request { id, command })
            .await
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return Err(IpcError::Closed.to_error_info());
        }
        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(IpcError::Closed.to_error_info()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(IpcError::Timeout.to_error_info())
            }
        }
    }

    pub async fn close(self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}
