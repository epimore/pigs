use super::transport::{
    LengthDelimitedCodec, LengthPrefix, MessageTransport, TransportAddress, TransportCloseReport,
    TransportEndpoint, TransportError, TransportErrorKind, TransportFuture, TransportKind,
    TransportMessage, TransportResult,
};
use crate::utils::rt::GlobalRuntime;
use bytes::Bytes;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{
    net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
    },
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_QUEUE_SIZE: usize = 128;
const DEFAULT_MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_INSTANCES: usize = 16;
const ERROR_PIPE_BUSY: i32 = 231;

#[derive(Debug, Clone)]
pub struct NamedPipeTransportConfig {
    namespace: String,
    name: String,
    pub queue_size: usize,
    pub max_message_size: usize,
    pub max_instances: usize,
    pub length_prefix: LengthPrefix,
    pub connect_timeout: Duration,
}

impl NamedPipeTransportConfig {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            queue_size: DEFAULT_QUEUE_SIZE,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            max_instances: DEFAULT_MAX_INSTANCES,
            length_prefix: LengthPrefix::U32Be,
            connect_timeout: Duration::from_secs(5),
        }
    }

    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\{}-{}", self.namespace, self.name)
    }

    fn validate(&self) -> TransportResult<()> {
        validate_name_part("namespace", &self.namespace)?;
        validate_name_part("name", &self.name)?;
        if self.queue_size == 0 || self.max_instances == 0 || self.max_instances > 254 {
            return Err(TransportError::new(
                TransportErrorKind::InvalidConfiguration,
                "queue_size and max_instances must be within their supported bounds",
            ));
        }
        LengthDelimitedCodec::new(self.length_prefix, self.max_message_size)?;
        Ok(())
    }
}

fn validate_name_part(label: &str, value: &str) -> TransportResult<()> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(TransportError::new(
            TransportErrorKind::InvalidConfiguration,
            format!("named pipe {label} must use 1-64 ASCII letters, digits, '.', '-' or '_'"),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowsPeerIdentity {
    pub process_id: Option<u32>,
}

pub struct ManagedNamedPipeListener {
    config: NamedPipeTransportConfig,
    pending: Mutex<Option<NamedPipeServer>>,
    cancel: CancellationToken,
    closed: AtomicBool,
}

impl ManagedNamedPipeListener {
    pub async fn bind(config: NamedPipeTransportConfig) -> TransportResult<Self> {
        config.validate()?;
        let pending = create_server(&config, true)?;
        Ok(Self {
            config,
            pending: Mutex::new(Some(pending)),
            cancel: CancellationToken::new(),
            closed: AtomicBool::new(false),
        })
    }

    pub fn endpoint(&self) -> TransportEndpoint {
        TransportEndpoint::new(
            TransportKind::NamedPipe,
            TransportAddress::NamedPipe(self.config.pipe_name()),
            self.config.max_message_size,
        )
        .expect("validated named pipe endpoint")
    }

    pub async fn accept(
        &self,
        runtime: &GlobalRuntime,
        task_name: impl Into<String>,
    ) -> TransportResult<(ManagedNamedPipeStream, WindowsPeerIdentity)> {
        if self.closed.load(Ordering::Acquire) {
            return Err(closed_error("named pipe listener"));
        }
        let mut pending = self.pending.lock().await;
        let server = pending
            .take()
            .ok_or_else(|| closed_error("named pipe listener"))?;
        tokio::select! {
            _ = self.cancel.cancelled() => return Err(closed_error("named pipe listener")),
            connected = server.connect() => connected
                .map_err(|error| TransportError::from_io("accept named pipe connection", &error))?,
        }
        if !self.closed.load(Ordering::Acquire) {
            *pending = Some(create_server(&self.config, false)?);
        }
        drop(pending);
        let stream = ManagedNamedPipeStream::from_server(
            server,
            self.endpoint(),
            &self.config,
            runtime,
            task_name.into(),
        )?;
        Ok((stream, WindowsPeerIdentity::default()))
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel.cancel();
    }

    pub async fn close_and_wait(&self) -> TransportResult<TransportCloseReport> {
        let already_closed = self.closed.swap(true, Ordering::AcqRel);
        self.cancel.cancel();
        self.pending.lock().await.take();
        Ok(TransportCloseReport {
            already_closed,
            root_task_joined: true,
            endpoint_removed: false,
        })
    }
}

impl Drop for ManagedNamedPipeListener {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub struct ManagedNamedPipeStream {
    endpoint: TransportEndpoint,
    encoder: LengthDelimitedCodec,
    outbound: mpsc::Sender<Bytes>,
    inbound: Mutex<mpsc::Receiver<TransportMessage>>,
    cancel: CancellationToken,
    root_task: Mutex<Option<JoinHandle<TransportResult<()>>>>,
    closed: AtomicBool,
}

impl ManagedNamedPipeStream {
    pub async fn connect(
        config: NamedPipeTransportConfig,
        runtime: &GlobalRuntime,
        task_name: impl Into<String>,
    ) -> TransportResult<Self> {
        config.validate()?;
        let pipe_name = config.pipe_name();
        let client = connect_client(&pipe_name, config.connect_timeout).await?;
        let endpoint = TransportEndpoint::new(
            TransportKind::NamedPipe,
            TransportAddress::NamedPipe(pipe_name),
            config.max_message_size,
        )?;
        Self::from_client(client, endpoint, &config, runtime, task_name.into())
    }

    fn from_server(
        server: NamedPipeServer,
        endpoint: TransportEndpoint,
        config: &NamedPipeTransportConfig,
        runtime: &GlobalRuntime,
        task_name: String,
    ) -> TransportResult<Self> {
        Self::from_stream(server, endpoint, config, runtime, task_name)
    }

    fn from_client(
        client: NamedPipeClient,
        endpoint: TransportEndpoint,
        config: &NamedPipeTransportConfig,
        runtime: &GlobalRuntime,
        task_name: String,
    ) -> TransportResult<Self> {
        Self::from_stream(client, endpoint, config, runtime, task_name)
    }

    fn from_stream<S>(
        stream: S,
        endpoint: TransportEndpoint,
        config: &NamedPipeTransportConfig,
        runtime: &GlobalRuntime,
        task_name: String,
    ) -> TransportResult<Self>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
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
                    "named pipe",
                )
                .await
            })
            .map_err(|error| {
                TransportError::new(
                    TransportErrorKind::Join,
                    format!("register named pipe root task: {error}"),
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

impl MessageTransport for ManagedNamedPipeStream {
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
                    "named pipe send queue is full",
                ),
                mpsc::error::TrySendError::Closed(_) => closed_error("named pipe connection"),
            })
    }

    fn send<'a>(&'a self, payload: Bytes) -> TransportFuture<'a, ()> {
        Box::pin(async move {
            let encoded = self.encoder.encode(&payload)?;
            tokio::select! {
                _ = self.cancel.cancelled() => Err(closed_error("named pipe connection")),
                sent = self.outbound.send(encoded) => sent
                    .map_err(|_| closed_error("named pipe connection")),
            }
        })
    }

    fn receive<'a>(&'a self) -> TransportFuture<'a, TransportMessage> {
        Box::pin(async move {
            let mut inbound = self.inbound.lock().await;
            tokio::select! {
                _ = self.cancel.cancelled() => Err(closed_error("named pipe connection")),
                message = inbound.recv() => message.ok_or_else(|| TransportError::new(
                    TransportErrorKind::PeerClosed,
                    "named pipe peer closed the connection",
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
                        format!("join named pipe root task: {error}"),
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

impl Drop for ManagedNamedPipeStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn create_server(
    config: &NamedPipeTransportConfig,
    first: bool,
) -> TransportResult<NamedPipeServer> {
    ServerOptions::new()
        .pipe_mode(PipeMode::Byte)
        .reject_remote_clients(true)
        .max_instances(config.max_instances)
        .first_pipe_instance(first)
        .create(config.pipe_name())
        .map_err(|error| TransportError::from_io("create named pipe listener", &error))
}

async fn connect_client(pipe_name: &str, timeout: Duration) -> TransportResult<NamedPipeClient> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match ClientOptions::new()
            .pipe_mode(PipeMode::Byte)
            .open(pipe_name)
        {
            Ok(client) => return Ok(client),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    || error.kind() == std::io::ErrorKind::NotFound =>
            {
                if tokio::time::Instant::now() >= deadline {
                    return Err(TransportError::new(
                        TransportErrorKind::Timeout,
                        "connect named pipe endpoint timed out",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => {
                return Err(TransportError::from_io(
                    "connect named pipe endpoint",
                    &error,
                ))
            }
        }
    }
}

fn closed_error(target: &str) -> TransportError {
    TransportError::new(TransportErrorKind::Closed, format!("{target} is closed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::rt::{GlobalRuntime, RuntimeType};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn test_config(name: &str) -> NamedPipeTransportConfig {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        NamedPipeTransportConfig::new("pigs-test", format!("{name}-{}-{id}", std::process::id()))
    }

    fn test_runtime(name: &str) -> GlobalRuntime {
        GlobalRuntime::register_default(RuntimeType::Custom(format!(
            "pipe-test-{name}-{}",
            NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
        )))
        .unwrap()
    }

    #[tokio::test]
    async fn named_pipe_delivers_complete_bidirectional_messages() {
        let config = test_config("stream");
        let listener = ManagedNamedPipeListener::bind(config.clone())
            .await
            .unwrap();
        let runtime = test_runtime("stream");
        let (client, accepted) = tokio::join!(
            ManagedNamedPipeStream::connect(config, &runtime, "pipe-client"),
            listener.accept(&runtime, "pipe-server")
        );
        let client = client.unwrap();
        let (server, _) = accepted.unwrap();

        client.send(Bytes::from_static(b"request")).await.unwrap();
        assert_eq!(server.receive().await.unwrap().payload, b"request"[..]);
        server.send(Bytes::from_static(b"response")).await.unwrap();
        assert_eq!(client.receive().await.unwrap().payload, b"response"[..]);

        client.close_and_wait().await.unwrap();
        server.close_and_wait().await.unwrap();
        listener.close_and_wait().await.unwrap();
    }

    #[test]
    fn pipe_name_is_confined_to_validated_namespace() {
        let error = NamedPipeTransportConfig::new("pigs", r"..\outside")
            .validate()
            .unwrap_err();
        assert_eq!(error.kind(), TransportErrorKind::InvalidConfiguration);
    }
}
