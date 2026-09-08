use super::transport::{
    LengthDelimitedCodec, LengthPrefix, MessageTransport, TransportAddress, TransportCloseReport,
    TransportEndpoint, TransportError, TransportErrorKind, TransportFuture, TransportKind,
    TransportMessage, TransportResult,
};
use crate::utils::rt::GlobalRuntime;
use bytes::Bytes;
use std::{
    fs::Metadata,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    net::{UnixDatagram, UnixListener, UnixStream},
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_QUEUE_SIZE: usize = 128;
const DEFAULT_MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct UnixTransportConfig {
    pub socket_root: PathBuf,
    pub socket_path: PathBuf,
    pub peer_path: Option<PathBuf>,
    pub queue_size: usize,
    pub max_message_size: usize,
    pub permissions: u32,
    pub remove_stale: bool,
    pub length_prefix: LengthPrefix,
    pub connect_timeout: Duration,
}

impl UnixTransportConfig {
    pub fn new(socket_root: impl Into<PathBuf>, socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_root: socket_root.into(),
            socket_path: socket_path.into(),
            peer_path: None,
            queue_size: DEFAULT_QUEUE_SIZE,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            permissions: 0o600,
            remove_stale: true,
            length_prefix: LengthPrefix::U32Be,
            connect_timeout: Duration::from_secs(5),
        }
    }

    fn validate(&self) -> TransportResult<()> {
        if self.queue_size == 0 {
            return Err(TransportError::new(
                TransportErrorKind::InvalidConfiguration,
                "queue_size must be greater than zero",
            ));
        }
        LengthDelimitedCodec::new(self.length_prefix, self.max_message_size)?;
        if self.permissions & !0o777 != 0 {
            return Err(TransportError::new(
                TransportErrorKind::InvalidConfiguration,
                "permissions must only contain Unix permission bits",
            ));
        }
        validate_socket_path(&self.socket_root, &self.socket_path)?;
        if let Some(peer_path) = &self.peer_path {
            validate_socket_path(&self.socket_root, peer_path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnixPeerCredentials {
    pub process_id: Option<u32>,
    pub user_id: u32,
    pub group_id: u32,
}

#[derive(Debug)]
pub struct ManagedUnixStreamListener {
    listener: UnixListener,
    config: UnixTransportConfig,
    identity: SocketIdentity,
    cancel: CancellationToken,
    closed: AtomicBool,
}

impl ManagedUnixStreamListener {
    pub async fn bind(config: UnixTransportConfig) -> TransportResult<Self> {
        config.validate()?;
        prepare_socket_path(&config, TransportKind::UnixStream).await?;
        let listener = UnixListener::bind(&config.socket_path)
            .map_err(|error| TransportError::from_io("bind Unix stream listener", &error))?;
        set_socket_permissions(&config.socket_path, config.permissions)?;
        let identity = socket_identity(&config.socket_path)?;
        Ok(Self {
            listener,
            config,
            identity,
            cancel: CancellationToken::new(),
            closed: AtomicBool::new(false),
        })
    }

    pub fn endpoint(&self) -> TransportEndpoint {
        TransportEndpoint::new(
            TransportKind::UnixStream,
            TransportAddress::Unix(self.config.socket_path.clone()),
            self.config.max_message_size,
        )
        .expect("validated Unix stream endpoint")
    }

    pub async fn accept(
        &self,
        runtime: &GlobalRuntime,
        task_name: impl Into<String>,
    ) -> TransportResult<(ManagedUnixStream, UnixPeerCredentials)> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::new(
                TransportErrorKind::Closed,
                "Unix stream listener is closed",
            ));
        }
        let (stream, _) = tokio::select! {
            _ = self.cancel.cancelled() => {
                return Err(TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix stream listener is closed",
                ));
            }
            accepted = self.listener.accept() => accepted
                .map_err(|error| TransportError::from_io("accept Unix stream connection", &error))?,
        };
        let credentials = peer_credentials(&stream)?;
        let connection = ManagedUnixStream::from_stream(
            stream,
            self.endpoint(),
            &self.config,
            runtime,
            task_name.into(),
        )?;
        Ok((connection, credentials))
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    pub async fn close_and_wait(&self) -> TransportResult<TransportCloseReport> {
        let already_closed = self.closed.swap(true, Ordering::AcqRel);
        self.cancel.cancel();
        let endpoint_removed = remove_owned_socket(&self.config.socket_path, self.identity)?;
        Ok(TransportCloseReport {
            already_closed,
            root_task_joined: true,
            endpoint_removed,
        })
    }
}

impl Drop for ManagedUnixStreamListener {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = remove_owned_socket(&self.config.socket_path, self.identity);
    }
}

pub struct ManagedUnixStream {
    endpoint: TransportEndpoint,
    encoder: LengthDelimitedCodec,
    outbound: mpsc::Sender<Bytes>,
    inbound: Mutex<mpsc::Receiver<TransportMessage>>,
    cancel: CancellationToken,
    root_task: Mutex<Option<JoinHandle<TransportResult<()>>>>,
    closed: AtomicBool,
}

impl ManagedUnixStream {
    pub async fn connect(
        config: UnixTransportConfig,
        runtime: &GlobalRuntime,
        task_name: impl Into<String>,
    ) -> TransportResult<Self> {
        config.validate()?;
        let connected = tokio::time::timeout(
            config.connect_timeout,
            UnixStream::connect(&config.socket_path),
        )
        .await
        .map_err(|_| {
            TransportError::new(
                TransportErrorKind::Timeout,
                "connect Unix stream endpoint timed out",
            )
        })?
        .map_err(|error| TransportError::from_io("connect Unix stream endpoint", &error))?;
        let endpoint = TransportEndpoint::new(
            TransportKind::UnixStream,
            TransportAddress::Unix(config.socket_path.clone()),
            config.max_message_size,
        )?;
        Self::from_stream(connected, endpoint, &config, runtime, task_name.into())
    }

    fn from_stream(
        stream: UnixStream,
        endpoint: TransportEndpoint,
        config: &UnixTransportConfig,
        runtime: &GlobalRuntime,
        task_name: String,
    ) -> TransportResult<Self> {
        let encoder = LengthDelimitedCodec::new(config.length_prefix, config.max_message_size)?;
        let decoder = LengthDelimitedCodec::new(config.length_prefix, config.max_message_size)?;
        let (outbound, outbound_rx) = mpsc::channel(config.queue_size);
        let (inbound_tx, inbound) = mpsc::channel(config.queue_size);
        let cancel = runtime.cancel.child_token();
        let task_cancel = cancel.clone();
        let root_task = runtime
            .spawn(task_name, async move {
                super::framed_stream::run(
                    stream,
                    decoder,
                    outbound_rx,
                    inbound_tx,
                    task_cancel,
                    None,
                    "Unix stream",
                )
                .await
            })
            .map_err(|error| {
                TransportError::new(
                    TransportErrorKind::Join,
                    format!("register Unix stream root task: {error}"),
                )
            })?;
        Ok(Self {
            endpoint,
            encoder,
            outbound,
            inbound: Mutex::new(inbound),
            cancel,
            root_task: Mutex::new(Some(root_task)),
            closed: AtomicBool::new(false),
        })
    }
}

impl MessageTransport for ManagedUnixStream {
    fn endpoint(&self) -> &TransportEndpoint {
        &self.endpoint
    }

    fn try_send(&self, payload: Bytes) -> TransportResult<()> {
        let encoded = self.encoder.encode(&payload)?;
        self.outbound
            .try_send(encoded)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TransportError::new(
                    TransportErrorKind::QueueFull,
                    "Unix stream send queue is full",
                ),
                mpsc::error::TrySendError::Closed(_) => TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix stream connection is closed",
                ),
            })
    }

    fn send<'a>(&'a self, payload: Bytes) -> TransportFuture<'a, ()> {
        Box::pin(async move {
            let encoded = self.encoder.encode(&payload)?;
            tokio::select! {
                _ = self.cancel.cancelled() => Err(TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix stream connection is closed",
                )),
                sent = self.outbound.send(encoded) => sent.map_err(|_| TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix stream connection is closed",
                )),
            }
        })
    }

    fn receive<'a>(&'a self) -> TransportFuture<'a, TransportMessage> {
        Box::pin(async move {
            let mut inbound = self.inbound.lock().await;
            tokio::select! {
                _ = self.cancel.cancelled() => Err(TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix stream connection is closed",
                )),
                message = inbound.recv() => message.ok_or_else(|| TransportError::new(
                    TransportErrorKind::PeerClosed,
                    "Unix stream peer closed the connection",
                )),
            }
        })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    fn close_and_wait<'a>(&'a self) -> TransportFuture<'a, TransportCloseReport> {
        Box::pin(async move {
            let already_closed = self.closed.swap(true, Ordering::AcqRel);
            self.cancel.cancel();
            let root_task = self.root_task.lock().await.take();
            let root_task_joined = if let Some(root_task) = root_task {
                root_task.await.map_err(|error| {
                    TransportError::new(
                        TransportErrorKind::Join,
                        format!("join Unix stream root task: {error}"),
                    )
                })??;
                true
            } else {
                false
            };
            Ok(TransportCloseReport {
                already_closed,
                root_task_joined,
                endpoint_removed: false,
            })
        })
    }
}

impl Drop for ManagedUnixStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub struct ManagedUnixDatagram {
    endpoint: TransportEndpoint,
    peer_path: Option<PathBuf>,
    outbound: mpsc::Sender<Bytes>,
    inbound: Mutex<mpsc::Receiver<TransportMessage>>,
    cancel: CancellationToken,
    root_task: Mutex<Option<JoinHandle<TransportResult<()>>>>,
    identity: SocketIdentity,
    closed: AtomicBool,
}

impl ManagedUnixDatagram {
    pub async fn bind(
        config: UnixTransportConfig,
        runtime: &GlobalRuntime,
        task_name: impl Into<String>,
    ) -> TransportResult<Self> {
        config.validate()?;
        prepare_socket_path(&config, TransportKind::UnixDatagram).await?;
        let socket = UnixDatagram::bind(&config.socket_path)
            .map_err(|error| TransportError::from_io("bind Unix datagram endpoint", &error))?;
        set_socket_permissions(&config.socket_path, config.permissions)?;
        let identity = socket_identity(&config.socket_path)?;
        let socket = Arc::new(socket);
        let (outbound, outbound_rx) = mpsc::channel(config.queue_size);
        let (inbound_tx, inbound) = mpsc::channel(config.queue_size);
        let cancel = runtime.cancel.child_token();
        let task_cancel = cancel.clone();
        let task_socket = socket.clone();
        let max_message_size = config.max_message_size;
        let peer_path = config.peer_path.clone();
        let root_task = runtime
            .spawn(task_name, async move {
                run_datagram(
                    task_socket,
                    peer_path,
                    max_message_size,
                    outbound_rx,
                    inbound_tx,
                    task_cancel,
                )
                .await
            })
            .map_err(|error| {
                TransportError::new(
                    TransportErrorKind::Join,
                    format!("register Unix datagram root task: {error}"),
                )
            })?;
        Ok(Self {
            endpoint: TransportEndpoint::new(
                TransportKind::UnixDatagram,
                TransportAddress::Unix(config.socket_path),
                config.max_message_size,
            )?,
            peer_path: config.peer_path,
            outbound,
            inbound: Mutex::new(inbound),
            cancel,
            root_task: Mutex::new(Some(root_task)),
            identity,
            closed: AtomicBool::new(false),
        })
    }
}

impl MessageTransport for ManagedUnixDatagram {
    fn endpoint(&self) -> &TransportEndpoint {
        &self.endpoint
    }

    fn try_send(&self, payload: Bytes) -> TransportResult<()> {
        validate_datagram_send(
            self.peer_path.as_deref(),
            payload.len(),
            self.endpoint.capabilities.max_message_size,
        )?;
        self.outbound
            .try_send(payload)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TransportError::new(
                    TransportErrorKind::QueueFull,
                    "Unix datagram send queue is full",
                ),
                mpsc::error::TrySendError::Closed(_) => TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix datagram endpoint is closed",
                ),
            })
    }

    fn send<'a>(&'a self, payload: Bytes) -> TransportFuture<'a, ()> {
        Box::pin(async move {
            validate_datagram_send(
                self.peer_path.as_deref(),
                payload.len(),
                self.endpoint.capabilities.max_message_size,
            )?;
            tokio::select! {
                _ = self.cancel.cancelled() => Err(TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix datagram endpoint is closed",
                )),
                sent = self.outbound.send(payload) => sent.map_err(|_| TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix datagram endpoint is closed",
                )),
            }
        })
    }

    fn receive<'a>(&'a self) -> TransportFuture<'a, TransportMessage> {
        Box::pin(async move {
            let mut inbound = self.inbound.lock().await;
            tokio::select! {
                _ = self.cancel.cancelled() => Err(TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix datagram endpoint is closed",
                )),
                message = inbound.recv() => message.ok_or_else(|| TransportError::new(
                    TransportErrorKind::Closed,
                    "Unix datagram endpoint stopped",
                )),
            }
        })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    fn close_and_wait<'a>(&'a self) -> TransportFuture<'a, TransportCloseReport> {
        Box::pin(async move {
            let already_closed = self.closed.swap(true, Ordering::AcqRel);
            self.cancel.cancel();
            let root_task = self.root_task.lock().await.take();
            let root_task_joined = if let Some(root_task) = root_task {
                root_task.await.map_err(|error| {
                    TransportError::new(
                        TransportErrorKind::Join,
                        format!("join Unix datagram root task: {error}"),
                    )
                })??;
                true
            } else {
                false
            };
            let socket_path = match &self.endpoint.address {
                TransportAddress::Unix(path) => path,
                TransportAddress::Inet(_) | TransportAddress::NamedPipe(_) => {
                    unreachable!("validated Unix datagram endpoint")
                }
            };
            let endpoint_removed = remove_owned_socket(socket_path, self.identity)?;
            Ok(TransportCloseReport {
                already_closed,
                root_task_joined,
                endpoint_removed,
            })
        })
    }
}

impl Drop for ManagedUnixDatagram {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let TransportAddress::Unix(path) = &self.endpoint.address {
            let _ = remove_owned_socket(path, self.identity);
        }
    }
}

async fn run_datagram(
    socket: Arc<UnixDatagram>,
    peer_path: Option<PathBuf>,
    max_message_size: usize,
    mut outbound: mpsc::Receiver<Bytes>,
    inbound: mpsc::Sender<TransportMessage>,
    cancel: CancellationToken,
) -> TransportResult<()> {
    let mut buffer = vec![0u8; max_message_size.saturating_add(1)];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            received = socket.recv_from(&mut buffer) => {
                let (size, peer) = received
                    .map_err(|error| TransportError::from_io("receive Unix datagram", &error))?;
                if size > max_message_size {
                    return Err(TransportError::new(
                        TransportErrorKind::MessageTooLarge,
                        "received Unix datagram exceeds configured maximum",
                    ));
                }
                let peer = peer.as_pathname().map(|path| TransportAddress::Unix(path.to_path_buf()));
                let message = TransportMessage {
                    payload: Bytes::copy_from_slice(&buffer[..size]),
                    peer,
                };
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    sent = inbound.send(message) => {
                        if sent.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            payload = outbound.recv() => {
                let Some(payload) = payload else {
                    return Ok(());
                };
                let peer_path = peer_path.as_deref().ok_or_else(|| TransportError::new(
                    TransportErrorKind::InvalidEndpoint,
                    "Unix datagram endpoint has no configured peer",
                ))?;
                socket
                    .send_to(&payload, peer_path)
                    .await
                    .map_err(|error| TransportError::from_io("send Unix datagram", &error))?;
            }
        }
    }
}

fn validate_datagram_send(
    peer_path: Option<&Path>,
    message_size: usize,
    max_message_size: usize,
) -> TransportResult<()> {
    if peer_path.is_none() {
        return Err(TransportError::new(
            TransportErrorKind::InvalidEndpoint,
            "Unix datagram endpoint has no configured peer",
        ));
    }
    if message_size > max_message_size {
        return Err(TransportError::new(
            TransportErrorKind::MessageTooLarge,
            format!("message size {message_size} exceeds configured maximum {max_message_size}"),
        ));
    }
    Ok(())
}

fn validate_socket_path(root: &Path, path: &Path) -> TransportResult<()> {
    let canonical_root = root.canonicalize().map_err(|error| {
        TransportError::from_io("canonicalize configured Unix socket root", &error)
    })?;
    let parent = path.parent().ok_or_else(|| {
        TransportError::new(
            TransportErrorKind::InvalidEndpoint,
            "Unix socket path has no parent directory",
        )
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| TransportError::from_io("canonicalize Unix socket parent", &error))?;
    if !canonical_parent.starts_with(&canonical_root) || path.file_name().is_none() {
        return Err(TransportError::new(
            TransportErrorKind::InvalidEndpoint,
            "Unix socket path must be inside the configured socket root",
        ));
    }
    Ok(())
}

async fn prepare_socket_path(
    config: &UnixTransportConfig,
    kind: TransportKind,
) -> TransportResult<()> {
    let metadata = match std::fs::symlink_metadata(&config.socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(TransportError::from_io(
                "inspect existing Unix socket path",
                &error,
            ))
        }
    };
    if !metadata.file_type().is_socket() {
        return Err(TransportError::new(
            TransportErrorKind::InvalidEndpoint,
            "existing Unix endpoint path is not a socket",
        ));
    }
    if !config.remove_stale {
        return Err(TransportError::new(
            TransportErrorKind::AddressInUse,
            "Unix socket path already exists",
        ));
    }
    let parent = config.socket_path.parent().expect("validated parent");
    let parent_metadata = std::fs::metadata(parent)
        .map_err(|error| TransportError::from_io("inspect Unix socket parent", &error))?;
    if metadata.uid() != parent_metadata.uid() {
        return Err(TransportError::new(
            TransportErrorKind::PermissionDenied,
            "refusing to remove a Unix socket owned by another user",
        ));
    }

    let active = match kind {
        TransportKind::UnixStream => match UnixStream::connect(&config.socket_path).await {
            Ok(_) => true,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                false
            }
            Err(error) => {
                return Err(TransportError::from_io(
                    "probe existing Unix stream endpoint",
                    &error,
                ))
            }
        },
        TransportKind::UnixDatagram => {
            let probe = UnixDatagram::unbound()
                .map_err(|error| TransportError::from_io("create Unix datagram probe", &error))?;
            match probe.send_to(&[], &config.socket_path).await {
                Ok(_) => true,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    false
                }
                Err(error) => {
                    return Err(TransportError::from_io(
                        "probe existing Unix datagram endpoint",
                        &error,
                    ))
                }
            }
        }
        TransportKind::Udp | TransportKind::Tcp | TransportKind::NamedPipe => {
            unreachable!("Unix endpoint kind")
        }
    };
    if active {
        return Err(TransportError::new(
            TransportErrorKind::AddressInUse,
            "Unix socket path belongs to an active endpoint",
        ));
    }
    std::fs::remove_file(&config.socket_path)
        .map_err(|error| TransportError::from_io("remove stale Unix socket", &error))?;
    Ok(())
}

fn set_socket_permissions(path: &Path, mode: u32) -> TransportResult<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| TransportError::from_io("set Unix socket permissions", &error))
}

fn socket_identity(path: &Path) -> TransportResult<SocketIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| TransportError::from_io("inspect bound Unix socket", &error))?;
    if !metadata.file_type().is_socket() {
        return Err(TransportError::new(
            TransportErrorKind::InvalidEndpoint,
            "bound Unix endpoint path is not a socket",
        ));
    }
    Ok(SocketIdentity::from_metadata(&metadata))
}

fn remove_owned_socket(path: &Path, expected: SocketIdentity) -> TransportResult<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(TransportError::from_io(
                "inspect Unix socket during close",
                &error,
            ))
        }
    };
    if !metadata.file_type().is_socket() || SocketIdentity::from_metadata(&metadata) != expected {
        return Err(TransportError::new(
            TransportErrorKind::InvalidEndpoint,
            "refusing to remove a replaced Unix socket path",
        ));
    }
    std::fs::remove_file(path)
        .map_err(|error| TransportError::from_io("remove owned Unix socket", &error))?;
    Ok(true)
}

fn peer_credentials(stream: &UnixStream) -> TransportResult<UnixPeerCredentials> {
    let credentials = stream
        .peer_cred()
        .map_err(|error| TransportError::from_io("read Unix peer credentials", &error))?;
    Ok(UnixPeerCredentials {
        process_id: credentials.pid().and_then(|pid| u32::try_from(pid).ok()),
        user_id: credentials.uid(),
        group_id: credentials.gid(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::rt::{GlobalRuntime, RuntimeType};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn test_root(name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("pigs-uds-{name}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_runtime(name: &str) -> GlobalRuntime {
        GlobalRuntime::register_default(RuntimeType::Custom(format!(
            "uds-test-{name}-{}",
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        )))
        .unwrap()
    }

    #[tokio::test]
    async fn unix_stream_delivers_complete_bidirectional_messages_and_cleans_path() {
        let root = test_root("stream");
        let path = root.join("service.sock");
        let config = UnixTransportConfig::new(&root, &path);
        let listener = ManagedUnixStreamListener::bind(config.clone())
            .await
            .unwrap();
        let runtime = test_runtime("stream");

        let (client, accepted) = tokio::join!(
            ManagedUnixStream::connect(config, &runtime, "uds-stream-client"),
            listener.accept(&runtime, "uds-stream-server")
        );
        let client = client.unwrap();
        let (server, credentials) = accepted.unwrap();
        assert_eq!(credentials.user_id, users::get_current_uid());

        client.send(Bytes::from_static(b"request")).await.unwrap();
        assert_eq!(server.receive().await.unwrap().payload, b"request"[..]);
        server.send(Bytes::from_static(b"response")).await.unwrap();
        assert_eq!(client.receive().await.unwrap().payload, b"response"[..]);

        client.close_and_wait().await.unwrap();
        server.close_and_wait().await.unwrap();
        let report = listener.close_and_wait().await.unwrap();
        assert!(report.endpoint_removed);
        assert!(!path.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn unix_datagram_preserves_messages_and_cleans_paths() {
        let root = test_root("datagram");
        let left_path = root.join("left.sock");
        let right_path = root.join("right.sock");
        let runtime = test_runtime("datagram");
        let mut left_config = UnixTransportConfig::new(&root, &left_path);
        left_config.peer_path = Some(right_path.clone());
        let mut right_config = UnixTransportConfig::new(&root, &right_path);
        right_config.peer_path = Some(left_path.clone());

        let left = ManagedUnixDatagram::bind(left_config, &runtime, "uds-dgram-left")
            .await
            .unwrap();
        let right = ManagedUnixDatagram::bind(right_config, &runtime, "uds-dgram-right")
            .await
            .unwrap();
        left.send(Bytes::from_static(b"one-datagram"))
            .await
            .unwrap();
        let received = right.receive().await.unwrap();
        assert_eq!(received.payload, b"one-datagram"[..]);
        assert_eq!(
            received.peer,
            Some(TransportAddress::Unix(left_path.clone()))
        );

        left.close_and_wait().await.unwrap();
        right.close_and_wait().await.unwrap();
        assert!(!left_path.exists());
        assert!(!right_path.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn listener_removes_only_a_stale_socket() {
        let root = test_root("stale");
        let path = root.join("stale.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());

        let listener = ManagedUnixStreamListener::bind(UnixTransportConfig::new(&root, &path))
            .await
            .unwrap();
        listener.close_and_wait().await.unwrap();
        assert!(!path.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[tokio::test]
    async fn listener_rejects_an_active_socket() {
        let root = test_root("active");
        let path = root.join("active.sock");
        let listener = ManagedUnixStreamListener::bind(UnixTransportConfig::new(&root, &path))
            .await
            .unwrap();
        let error = ManagedUnixStreamListener::bind(UnixTransportConfig::new(&root, &path))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), TransportErrorKind::AddressInUse);
        listener.close_and_wait().await.unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
