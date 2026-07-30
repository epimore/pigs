use crate::net::state::Protocol;
use crate::utils::rt::GlobalRuntime;
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use exception::{GlobalError, GlobalResult, GlobalResultExt};
use log::{debug, error, warn};
use std::io::{self, IoSlice};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

pub trait PacketDispatcher: Send + Sync + 'static {
    fn dispatch_owned(
        &self,
        data: Bytes,
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()>;

    fn close(&self, _remote_addr: SocketAddr, _protocol: Protocol) -> GlobalResult<()> {
        Ok(())
    }
}

pub trait PacketSplitter: Send + 'static {
    fn feed_owned<F>(&mut self, chunk: &mut BytesMut, f: F) -> GlobalResult<()>
    where
        F: FnMut(Bytes) -> GlobalResult<()>;
}

const MAX_BUF_SIZE: usize = 2 * 1024 * 1024;
const TCP_READ_BUF_SIZE: usize = 64 * 1024;
const TCP_MIN_READ_SPARE: usize = 4 * 1024;
const UDP_RECV_BUF_SIZE: usize = 2 * 1024;

const TCP_WRITE_QUEUE_SIZE: usize = 1024;
pub const INLINE_PREFIX_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InlinePrefix {
    len: u8,
    data: [u8; INLINE_PREFIX_CAPACITY],
}

pub trait InlinePrefixNumber: Copy {
    fn to_inline_prefix_be(self) -> InlinePrefix;
    fn to_inline_prefix_le(self) -> InlinePrefix;
    fn to_inline_prefix_ne(self) -> InlinePrefix;
}

impl InlinePrefix {
    pub fn new(prefix: &[u8]) -> GlobalResult<Self> {
        if prefix.len() > INLINE_PREFIX_CAPACITY {
            return Err(GlobalError::new_sys_error(
                "inline packet prefix exceeds capacity",
                |msg| {
                    error!(
                        "{msg}: len={}, capacity={INLINE_PREFIX_CAPACITY}",
                        prefix.len()
                    )
                },
            ));
        }
        let mut data = [0u8; INLINE_PREFIX_CAPACITY];
        for (idx, byte) in prefix.iter().enumerate() {
            data[idx] = *byte;
        }
        Ok(Self {
            len: prefix.len() as u8,
            data,
        })
    }

    pub fn from_array<const N: usize>(prefix: [u8; N]) -> GlobalResult<Self> {
        Self::new(&prefix)
    }

    pub fn from_be<T>(value: T) -> Self
    where
        T: InlinePrefixNumber,
    {
        value.to_inline_prefix_be()
    }

    pub fn from_le<T>(value: T) -> Self
    where
        T: InlinePrefixNumber,
    {
        value.to_inline_prefix_le()
    }

    pub fn from_ne<T>(value: T) -> Self
    where
        T: InlinePrefixNumber,
    {
        value.to_inline_prefix_ne()
    }

    fn from_number_bytes<const N: usize>(prefix: [u8; N]) -> Self {
        debug_assert!(N <= INLINE_PREFIX_CAPACITY);
        let mut data = [0u8; INLINE_PREFIX_CAPACITY];
        for (idx, byte) in prefix.iter().enumerate() {
            data[idx] = *byte;
        }
        Self { len: N as u8, data }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..self.len()]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

macro_rules! impl_inline_prefix_number {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl InlinePrefixNumber for $ty {
                fn to_inline_prefix_be(self) -> InlinePrefix {
                    InlinePrefix::from_number_bytes(self.to_be_bytes())
                }

                fn to_inline_prefix_le(self) -> InlinePrefix {
                    InlinePrefix::from_number_bytes(self.to_le_bytes())
                }

                fn to_inline_prefix_ne(self) -> InlinePrefix {
                    InlinePrefix::from_number_bytes(self.to_ne_bytes())
                }
            }
        )+
    };
}

impl_inline_prefix_number!(u8, u16, u32, u64, u128);

#[macro_export]
macro_rules! inline_prefix {
    ($value:expr) => {
        $crate::net::rw::InlinePrefix::from_be($value)
    };
    (be, $value:expr) => {
        $crate::net::rw::InlinePrefix::from_be($value)
    };
    (le, $value:expr) => {
        $crate::net::rw::InlinePrefix::from_le($value)
    };
    (ne, $value:expr) => {
        $crate::net::rw::InlinePrefix::from_ne($value)
    };
}

#[derive(Clone, Debug)]
pub enum EncodedPacket {
    Single(Bytes),
    InlinePrefix {
        prefix: InlinePrefix,
        payload: Bytes,
    },
}

impl EncodedPacket {
    pub fn single(data: Bytes) -> Self {
        Self::Single(data)
    }

    pub fn with_inline_prefix(prefix: InlinePrefix, payload: Bytes) -> Self {
        Self::InlinePrefix { prefix, payload }
    }
}

pub trait PacketEncoder: Send + Sync + 'static {
    /// UDP encoders must return one contiguous datagram buffer.
    fn encode_udp(&self, data: Bytes) -> GlobalResult<Bytes> {
        Ok(data)
    }

    /// TCP encoders may return multiple slices for vectored writes.
    fn encode_tcp(&self, data: Bytes) -> GlobalResult<EncodedPacket> {
        Ok(EncodedPacket::single(data))
    }
}

#[derive(Clone, Default)]
pub struct RawPacketEncoder;

impl PacketEncoder for RawPacketEncoder {}

#[derive(Clone, Default)]
pub struct U16BeLengthPrefixEncoder;

impl PacketEncoder for U16BeLengthPrefixEncoder {
    fn encode_tcp(&self, data: Bytes) -> GlobalResult<EncodedPacket> {
        if data.len() > u16::MAX as usize {
            return Err(GlobalError::new_sys_error(
                "packet length exceeds u16 max",
                |msg| error!("{msg}: len={}", data.len()),
            ));
        }
        let prefix = crate::inline_prefix!(be, data.len() as u16);
        Ok(EncodedPacket::with_inline_prefix(prefix, data))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcpWriteMode {
    Queued { queue_size: usize },
    Direct,
}

impl Default for TcpWriteMode {
    fn default() -> Self {
        Self::Queued {
            queue_size: TCP_WRITE_QUEUE_SIZE,
        }
    }
}

impl TcpWriteMode {
    fn queue_size(self) -> Option<usize> {
        match self {
            Self::Queued { queue_size } => Some(queue_size.max(1)),
            Self::Direct => None,
        }
    }
}

pub struct QueuedTcpSink<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    remote_addr: SocketAddr,
    sender: mpsc::Sender<EncodedPacket>,
    encoder: Arc<E>,
    cancel: CancellationToken,
}

impl<E> Clone for QueuedTcpSink<E>
where
    E: PacketEncoder,
{
    fn clone(&self) -> Self {
        Self {
            remote_addr: self.remote_addr,
            sender: self.sender.clone(),
            encoder: self.encoder.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

impl<E> QueuedTcpSink<E>
where
    E: PacketEncoder,
{
    fn new(
        remote_addr: SocketAddr,
        sender: mpsc::Sender<EncodedPacket>,
        encoder: Arc<E>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            remote_addr,
            sender,
            encoder,
            cancel,
        }
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    pub fn close(&self) {
        self.cancel.cancel();
    }

    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled() || self.sender.is_closed()
    }

    pub async fn write(&self, data: Bytes) -> GlobalResult<()> {
        if self.cancel.is_cancelled() {
            return Err(GlobalError::new_sys_error("tcp sink is closed", |msg| {
                debug!("{msg}: remote_addr={}", self.remote_addr)
            }));
        }
        let packet = self.encoder.encode_tcp(data)?;
        select! {
            biased;
            _ = self.cancel.cancelled() => Err(GlobalError::new_sys_error(
                "tcp sink is closed",
                |msg| debug!("{msg}: remote_addr={}", self.remote_addr),
            )),
            result = self.sender.send(packet) => result.map_err(|_| {
                GlobalError::new_sys_error("tcp write channel closed", |msg| {
                    debug!("{msg}: remote_addr={}", self.remote_addr)
                })
            }),
        }
    }

    pub fn try_write(&self, data: Bytes) -> GlobalResult<()> {
        if self.cancel.is_cancelled() {
            return Err(GlobalError::new_sys_error("tcp sink is closed", |msg| {
                error!("{msg}: remote_addr={}", self.remote_addr)
            }));
        }
        let packet = self.encoder.encode_tcp(data)?;
        match self.sender.try_send(packet) {
            Ok(_) => Ok(()),
            Err(TrySendError::Full(_)) => Err(GlobalError::new_sys_error(
                "tcp write channel is full",
                |msg| error!("{msg}: remote_addr={}", self.remote_addr),
            )),
            Err(TrySendError::Closed(_)) => Err(GlobalError::new_sys_error(
                "tcp write channel closed",
                |msg| error!("{msg}: remote_addr={}", self.remote_addr),
            )),
        }
    }
}

pub struct DirectTcpSink<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    remote_addr: SocketAddr,
    stream: Arc<Mutex<OwnedWriteHalf>>,
    encoder: Arc<E>,
    cancel: CancellationToken,
}

impl<E> Clone for DirectTcpSink<E>
where
    E: PacketEncoder,
{
    fn clone(&self) -> Self {
        Self {
            remote_addr: self.remote_addr,
            stream: self.stream.clone(),
            encoder: self.encoder.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

impl<E> DirectTcpSink<E>
where
    E: PacketEncoder,
{
    fn new(
        remote_addr: SocketAddr,
        stream: OwnedWriteHalf,
        encoder: Arc<E>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            remote_addr,
            stream: Arc::new(Mutex::new(stream)),
            encoder,
            cancel,
        }
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    pub fn close(&self) {
        self.cancel.cancel();
    }

    pub fn is_closed(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub async fn write(&self, data: Bytes) -> GlobalResult<()> {
        if self.cancel.is_cancelled() {
            return Err(GlobalError::new_sys_error("tcp sink is closed", |msg| {
                error!("{msg}: remote_addr={}", self.remote_addr)
            }));
        }
        let packet = self.encoder.encode_tcp(data)?;
        let mut stream = self.stream.lock().await;
        if self.cancel.is_cancelled() {
            return Err(GlobalError::new_sys_error("tcp sink is closed", |msg| {
                debug!("{msg}: remote_addr={}", self.remote_addr)
            }));
        }
        let result = select! {
            biased;
            _ = self.cancel.cancelled() => Err(GlobalError::new_sys_error(
                "tcp sink is closed",
                |msg| debug!("{msg}: remote_addr={}", self.remote_addr),
            )),
            result = write_encoded_packet(&mut *stream, packet) => {
                result.hand_log(|msg| error!("{msg}: remote_addr={}", self.remote_addr))
            }
        };
        if result.is_err() {
            self.cancel.cancel();
        }
        result
    }

    pub async fn shutdown(&self) -> GlobalResult<()> {
        self.cancel.cancel();
        let mut stream = self.stream.lock().await;
        stream
            .shutdown()
            .await
            .hand_log(|msg| error!("{msg}: remote_addr={}", self.remote_addr))
    }
}

pub enum TcpPacketSink<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    Queued(QueuedTcpSink<E>),
    Direct(DirectTcpSink<E>),
}

impl<E> Clone for TcpPacketSink<E>
where
    E: PacketEncoder,
{
    fn clone(&self) -> Self {
        match self {
            Self::Queued(sink) => Self::Queued(sink.clone()),
            Self::Direct(sink) => Self::Direct(sink.clone()),
        }
    }
}

impl<E> TcpPacketSink<E>
where
    E: PacketEncoder,
{
    pub fn remote_addr(&self) -> SocketAddr {
        match self {
            Self::Queued(sink) => sink.remote_addr(),
            Self::Direct(sink) => sink.remote_addr(),
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, Self::Direct(_))
    }

    pub fn close(&self) {
        match self {
            Self::Queued(sink) => sink.close(),
            Self::Direct(sink) => sink.close(),
        }
    }

    pub async fn write(&self, data: Bytes) -> GlobalResult<()> {
        match self {
            Self::Queued(sink) => sink.write(data).await,
            Self::Direct(sink) => sink.write(data).await,
        }
    }

    pub fn try_write(&self, data: Bytes) -> GlobalResult<()> {
        match self {
            Self::Queued(sink) => sink.try_write(data),
            Self::Direct(sink) => Err(GlobalError::new_sys_error(
                "direct tcp sink requires async write",
                |msg| error!("{msg}: remote_addr={}", sink.remote_addr()),
            )),
        }
    }
}

pub struct PacketWriter<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    udp_socket: Arc<RwLock<Option<Arc<UdpSocket>>>>,
    tcp_writers: Arc<DashMap<SocketAddr, TcpPacketSink<E>>>,
    tcp_writer_addrs_by_ip: Arc<DashMap<IpAddr, SocketAddr>>,
    encoder: Arc<E>,
    tcp_write_mode: TcpWriteMode,
    closed: CancellationToken,
}

impl<E> Clone for PacketWriter<E>
where
    E: PacketEncoder,
{
    fn clone(&self) -> Self {
        Self {
            udp_socket: self.udp_socket.clone(),
            tcp_writers: self.tcp_writers.clone(),
            tcp_writer_addrs_by_ip: self.tcp_writer_addrs_by_ip.clone(),
            encoder: self.encoder.clone(),
            tcp_write_mode: self.tcp_write_mode,
            closed: self.closed.clone(),
        }
    }
}

impl<E> PacketWriter<E>
where
    E: PacketEncoder,
{
    fn new(
        udp_socket: Option<Arc<UdpSocket>>,
        encoder: Arc<E>,
        tcp_write_mode: TcpWriteMode,
    ) -> Self {
        Self {
            udp_socket: Arc::new(RwLock::new(udp_socket)),
            tcp_writers: Arc::new(DashMap::new()),
            tcp_writer_addrs_by_ip: Arc::new(DashMap::new()),
            encoder,
            tcp_write_mode,
            closed: CancellationToken::new(),
        }
    }

    pub async fn write_to(
        &self,
        data: Bytes,
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()> {
        self.ensure_open()?;
        match protocol {
            Protocol::UDP => {
                let packet = self.encoder.encode_udp(data)?;
                let socket = self.udp_socket.read().await;
                let socket = socket.as_ref().ok_or_else(|| {
                    GlobalError::new_sys_error("udp socket is not available", |msg| error!("{msg}"))
                })?;
                select! {
                    biased;
                    _ = self.closed.cancelled() => {
                        return Err(packet_writer_closed());
                    }
                    result = socket.send_to(packet.as_ref(), remote_addr) => {
                        result.hand_log(|msg| error!("{msg}: remote_addr={remote_addr}"))?;
                    }
                }
                Ok(())
            }
            Protocol::TCP => self.tcp_sink_or_err(remote_addr)?.write(data).await,
            Protocol::ALL => Err(GlobalError::new_sys_error(
                "protocol ALL cannot be used to write a packet",
                |msg| error!("{msg}"),
            )),
        }
    }

    pub async fn write_slice_to(
        &self,
        data: &[u8],
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()> {
        self.ensure_open()?;
        match protocol {
            Protocol::UDP => {
                let socket = self.udp_socket.read().await;
                let socket = socket.as_ref().ok_or_else(|| {
                    GlobalError::new_sys_error("udp socket is not available", |msg| error!("{msg}"))
                })?;
                select! {
                    biased;
                    _ = self.closed.cancelled() => {
                        return Err(packet_writer_closed());
                    }
                    result = socket.send_to(data, remote_addr) => {
                        result.hand_log(|msg| error!("{msg}: remote_addr={remote_addr}"))?;
                    }
                }
                Ok(())
            }
            Protocol::TCP => Err(GlobalError::new_sys_error(
                "write_slice_to cannot write TCP without copy; use write_to with Bytes",
                |msg| error!("{msg}: remote_addr={remote_addr}"),
            )),
            Protocol::ALL => Err(GlobalError::new_sys_error(
                "protocol ALL cannot be used to write a packet",
                |msg| error!("{msg}"),
            )),
        }
    }

    pub async fn write_tcp_to_ip(&self, data: Bytes, remote_ip: IpAddr) -> GlobalResult<()> {
        self.ensure_open()?;
        self.tcp_sink_by_ip(remote_ip)
            .ok_or_else(|| {
                GlobalError::new_sys_error("tcp writer is not available", |msg| {
                    error!("{msg}: remote_ip={remote_ip}")
                })
            })?
            .write(data)
            .await
    }

    pub fn try_write_to(
        &self,
        data: Bytes,
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()> {
        self.ensure_open()?;
        match protocol {
            Protocol::UDP => {
                let packet = self.encoder.encode_udp(data)?;
                let socket = self.udp_socket.try_read().map_err(|_| {
                    GlobalError::new_sys_error("udp socket is closing", |msg| error!("{msg}"))
                })?;
                let socket = socket.as_ref().ok_or_else(|| {
                    GlobalError::new_sys_error("udp socket is not available", |msg| error!("{msg}"))
                })?;
                socket
                    .try_send_to(packet.as_ref(), remote_addr)
                    .hand_log(|msg| error!("{msg}: remote_addr={remote_addr}"))?;
                Ok(())
            }
            Protocol::TCP => self.tcp_sink_or_err(remote_addr)?.try_write(data),
            Protocol::ALL => Err(GlobalError::new_sys_error(
                "protocol ALL cannot be used to write a packet",
                |msg| error!("{msg}"),
            )),
        }
    }

    pub fn tcp_write_mode(&self) -> TcpWriteMode {
        self.tcp_write_mode
    }

    pub fn tcp_sink(&self, remote_addr: &SocketAddr) -> Option<TcpPacketSink<E>> {
        self.tcp_writers
            .get(remote_addr)
            .map(|item| item.value().clone())
    }

    pub fn tcp_sink_by_ip(&self, remote_ip: IpAddr) -> Option<TcpPacketSink<E>> {
        let remote_addr = self
            .tcp_writer_addrs_by_ip
            .get(&remote_ip)
            .map(|item| *item.value())?;
        self.tcp_sink(&remote_addr)
    }

    fn tcp_sink_or_err(&self, remote_addr: SocketAddr) -> GlobalResult<TcpPacketSink<E>> {
        self.tcp_sink(&remote_addr).ok_or_else(|| {
            GlobalError::new_sys_error("tcp writer is not available", |msg| {
                error!("{msg}: remote_addr={remote_addr}")
            })
        })
    }

    pub fn insert_tcp_writer(
        &self,
        remote_addr: SocketAddr,
        sender: mpsc::Sender<EncodedPacket>,
        cancel: CancellationToken,
    ) {
        let sink = TcpPacketSink::Queued(QueuedTcpSink::new(
            remote_addr,
            sender,
            self.encoder.clone(),
            cancel,
        ));
        self.insert_tcp_sink(remote_addr, sink);
    }

    pub fn insert_direct_tcp_writer(
        &self,
        remote_addr: SocketAddr,
        stream: OwnedWriteHalf,
        cancel: CancellationToken,
    ) {
        let sink = TcpPacketSink::Direct(DirectTcpSink::new(
            remote_addr,
            stream,
            self.encoder.clone(),
            cancel,
        ));
        self.insert_tcp_sink(remote_addr, sink);
    }

    fn insert_tcp_sink(&self, remote_addr: SocketAddr, sink: TcpPacketSink<E>) {
        if self.closed.is_cancelled() {
            sink.close();
            return;
        }
        self.tcp_writer_addrs_by_ip
            .insert(remote_addr.ip(), remote_addr);
        self.tcp_writers.insert(remote_addr, sink);
    }

    pub fn remove_tcp_writer(&self, remote_addr: &SocketAddr) {
        if let Some((_, handle)) = self.tcp_writers.remove(remote_addr) {
            let remote_ip = remote_addr.ip();
            if self
                .tcp_writer_addrs_by_ip
                .get(&remote_ip)
                .is_some_and(|item| *item.value() == *remote_addr)
            {
                self.tcp_writer_addrs_by_ip.remove(&remote_ip);
            }
            handle.close();
        }
    }

    pub fn has_tcp_writer(&self, remote_addr: &SocketAddr) -> bool {
        self.tcp_writers.contains_key(remote_addr)
    }

    fn ensure_open(&self) -> GlobalResult<()> {
        if self.closed.is_cancelled() {
            return Err(packet_writer_closed());
        }
        Ok(())
    }

    async fn close(&self) -> GlobalResult<()> {
        self.closed.cancel();

        let sinks = self
            .tcp_writers
            .iter()
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        self.tcp_writers.clear();
        self.tcp_writer_addrs_by_ip.clear();

        let mut close_error = None;
        for sink in sinks {
            sink.close();
            if let TcpPacketSink::Direct(sink) = sink {
                if let Err(error) = sink.shutdown().await {
                    close_error.get_or_insert(error);
                }
            }
        }
        self.udp_socket.write().await.take();

        match close_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn packet_writer_closed() -> GlobalError {
    GlobalError::new_sys_error("packet writer is closed", |_| {})
}

/// Aggregated result of stopping all tasks owned by a managed network endpoint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NetworkCloseReport {
    pub completed: usize,
    pub cancelled: usize,
    pub failed: usize,
    pub panicked: usize,
    pub remaining: usize,
}

impl NetworkCloseReport {
    /// Returns `true` when every owned task reached a terminal state without failure.
    pub fn is_complete(&self) -> bool {
        self.failed == 0 && self.panicked == 0 && self.remaining == 0
    }

    fn merge(&mut self, other: ManagedTaskReport) {
        self.completed += other.completed;
        self.cancelled += other.cancelled;
        self.failed += other.failed;
        self.panicked += other.panicked;
        self.remaining += other.remaining;
    }
}

#[derive(Default)]
struct ManagedTaskReport {
    completed: usize,
    cancelled: usize,
    failed: usize,
    panicked: usize,
    remaining: usize,
}

impl ManagedTaskReport {
    fn merge(&mut self, other: Self) {
        self.completed += other.completed;
        self.cancelled += other.cancelled;
        self.failed += other.failed;
        self.panicked += other.panicked;
        self.remaining += other.remaining;
    }
}

struct ManagedCloseState {
    root_task: Option<JoinHandle<ManagedTaskReport>>,
    report: NetworkCloseReport,
    writer_closed: bool,
    completed: bool,
    failure_logged: bool,
}

/// Owns a packet writer and the lifecycle of its TCP/UDP receive tasks.
///
/// Dropping this value requests cancellation but does not wait for resource release. Owners that
/// require deterministic listener release must call [`ManagedPacketIo::close_and_wait`].
#[must_use = "managed packet I/O must be retained and closed explicitly"]
pub struct ManagedPacketIo<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    writer: PacketWriter<E>,
    cancel: CancellationToken,
    close_state: Mutex<ManagedCloseState>,
}

impl<E> ManagedPacketIo<E>
where
    E: PacketEncoder,
{
    /// Returns a clone of the writer bound to this managed endpoint.
    pub fn writer(&self) -> PacketWriter<E> {
        self.writer.clone()
    }

    /// Cancels all owned work and waits until the writer and protocol tasks are closed.
    ///
    /// The operation is idempotent and cancellation-safe: if the waiting future is dropped, a
    /// later call resumes cleanup without losing the root task handle.
    pub async fn close_and_wait(&self) -> GlobalResult<NetworkCloseReport> {
        let mut state = self.close_state.lock().await;
        if state.completed {
            return close_report_result(state.report.clone(), false);
        }

        self.cancel.cancel();
        if !state.writer_closed {
            let close_result = self.writer.close().await;
            state.writer_closed = true;
            if let Err(close_error) = close_result {
                debug!("managed packet writer close failed: {close_error}");
                state.report.failed += 1;
            }
        }

        let root_result = match state.root_task.as_mut() {
            Some(root_task) => Some(root_task.await),
            None => None,
        };
        if let Some(root_result) = root_result {
            state.root_task.take();
            match root_result {
                Ok(task_report) => state.report.merge(task_report),
                Err(join_error) => record_join_error(&mut state.report, join_error),
            }
        }

        state.completed = true;
        let log_failure = !state.failure_logged;
        if !state.report.is_complete() {
            state.failure_logged = true;
        }
        close_report_result(state.report.clone(), log_failure)
    }
}

impl<E> Drop for ManagedPacketIo<E>
where
    E: PacketEncoder,
{
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn close_report_result(
    report: NetworkCloseReport,
    log_failure: bool,
) -> GlobalResult<NetworkCloseReport> {
    if report.is_complete() {
        return Ok(report);
    }
    let message = format!(
        "managed network close incomplete: completed={}, cancelled={}, failed={}, panicked={}, remaining={}",
        report.completed, report.cancelled, report.failed, report.panicked, report.remaining
    );
    Err(GlobalError::new_sys_error(&message, |msg| {
        if log_failure {
            error!("{msg}");
        }
    }))
}

fn record_join_error(report: &mut NetworkCloseReport, join_error: JoinError) {
    if join_error.is_panic() {
        report.panicked += 1;
    } else if join_error.is_cancelled() {
        report.cancelled += 1;
        report.remaining += 1;
    } else {
        report.failed += 1;
    }
}

/// Starts managed packet I/O with the default queued TCP writer.
pub fn managed_rw<D, S, E>(
    runtime: &GlobalRuntime,
    task_name: impl Into<String>,
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    encoder: Arc<E>,
) -> GlobalResult<ManagedPacketIo<E>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    managed_rw_with_tcp_write_mode::<D, S, E>(
        runtime,
        task_name,
        tu,
        cancel,
        dispatcher,
        encoder,
        TcpWriteMode::default(),
    )
}

/// Starts managed packet I/O with direct TCP writes.
pub fn managed_direct_rw<D, S, E>(
    runtime: &GlobalRuntime,
    task_name: impl Into<String>,
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    encoder: Arc<E>,
) -> GlobalResult<ManagedPacketIo<E>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    managed_rw_with_tcp_write_mode::<D, S, E>(
        runtime,
        task_name,
        tu,
        cancel,
        dispatcher,
        encoder,
        TcpWriteMode::Direct,
    )
}

/// Starts managed packet I/O with an explicit TCP write mode.
///
/// TCP accept/read work and UDP receive work run in independent protocol tasks. A single runtime
/// supervisor owns those tasks and provides deterministic shutdown through [`ManagedPacketIo`].
pub fn managed_rw_with_tcp_write_mode<D, S, E>(
    runtime: &GlobalRuntime,
    task_name: impl Into<String>,
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    external_cancel: CancellationToken,
    dispatcher: Arc<D>,
    encoder: Arc<E>,
    tcp_write_mode: TcpWriteMode,
) -> GlobalResult<ManagedPacketIo<E>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    let (tcp_listener, udp_socket, writer) = prepare_packet_io(tu, encoder, tcp_write_mode)?;
    let cancel = runtime.cancel.child_token();
    let root_task = runtime.spawn(
        task_name,
        run_managed_packet_io::<D, S, E>(
            tcp_listener,
            udp_socket,
            cancel.clone(),
            external_cancel,
            dispatcher,
            writer.clone(),
        ),
    )?;
    Ok(ManagedPacketIo {
        writer,
        cancel,
        close_state: Mutex::new(ManagedCloseState {
            root_task: Some(root_task),
            report: NetworkCloseReport::default(),
            writer_closed: false,
            completed: false,
            failure_logged: false,
        }),
    })
}

pub fn reader<D, S>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    match tu {
        (Some(tcp), None) => spawn_tcp::<D, S>(tcp, cancel, dispatcher),
        (None, Some(udp)) => spawn_udp(udp, cancel, dispatcher),
        (Some(tcp), Some(udp)) => {
            spawn_tcp::<D, S>(tcp, cancel.clone(), dispatcher.clone())?;
            spawn_udp(udp, cancel, dispatcher)
        }
        _ => Ok(()),
    }
}

pub fn owned_reader<D, S>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    match tu {
        (Some(tcp), None) => spawn_tcp_owned::<D, S>(tcp, cancel, dispatcher),
        (None, Some(udp)) => spawn_udp_owned(udp, cancel, dispatcher),
        (Some(tcp), Some(udp)) => {
            spawn_tcp_owned::<D, S>(tcp, cancel.clone(), dispatcher.clone())?;
            spawn_udp_owned(udp, cancel, dispatcher)
        }
        _ => Ok(()),
    }
}

pub fn rw<D, S, E>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    encoder: Arc<E>,
) -> GlobalResult<PacketWriter<E>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    rw_with_tcp_write_mode::<D, S, E>(tu, cancel, dispatcher, encoder, TcpWriteMode::default())
}

pub fn direct_rw<D, S, E>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    encoder: Arc<E>,
) -> GlobalResult<PacketWriter<E>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    rw_with_tcp_write_mode::<D, S, E>(tu, cancel, dispatcher, encoder, TcpWriteMode::Direct)
}

pub fn rw_with_tcp_write_mode<D, S, E>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    encoder: Arc<E>,
    tcp_write_mode: TcpWriteMode,
) -> GlobalResult<PacketWriter<E>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    let (tcp_listener, udp_socket, writer) = prepare_packet_io(tu, encoder, tcp_write_mode)?;
    if let Some(tcp_listener) = tcp_listener {
        drop(tokio::spawn(run_tcp_listener::<D, S, E>(
            tcp_listener,
            cancel.clone(),
            dispatcher.clone(),
            writer.clone(),
        )));
    }
    if let Some(udp_socket) = udp_socket {
        drop(tokio::spawn(run_udp_receiver(
            udp_socket, cancel, dispatcher,
        )));
    }
    Ok(writer)
}

pub fn raw_rw<D, S>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<PacketWriter<RawPacketEncoder>>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    rw::<D, S, RawPacketEncoder>(tu, cancel, dispatcher, Arc::new(RawPacketEncoder))
}

type PreparedPacketIo<E> = (
    Option<tokio::net::TcpListener>,
    Option<Arc<UdpSocket>>,
    PacketWriter<E>,
);

fn prepare_packet_io<E>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    encoder: Arc<E>,
    tcp_write_mode: TcpWriteMode,
) -> GlobalResult<PreparedPacketIo<E>>
where
    E: PacketEncoder,
{
    let (tcp, udp) = tu;
    let tcp_listener = tcp.map(into_tokio_tcp_listener).transpose()?;
    let udp_socket = udp.map(into_tokio_udp_socket).transpose()?;
    let writer = PacketWriter::new(udp_socket.clone(), encoder, tcp_write_mode);
    Ok((tcp_listener, udp_socket, writer))
}

fn into_tokio_tcp_listener(tcp: std::net::TcpListener) -> GlobalResult<tokio::net::TcpListener> {
    tcp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;
    tokio::net::TcpListener::from_std(tcp).hand_log(|msg| error!("{msg}"))
}

fn spawn_tcp<D, S>(
    tcp: std::net::TcpListener,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    tcp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;
    let listener = tokio::net::TcpListener::from_std(tcp).hand_log(|msg| error!("{msg}"))?;

    tokio::spawn(async move {
        loop {
            select! {
                biased;

                res = listener.accept() => {
                    match res {
                        Ok((stream, remote_addr)) => {
                            let dispatcher = dispatcher.clone();
                            let cancel = cancel.clone();

                            tokio::spawn(async move {
                                let splitter = S::default();
                                if let Err(e) = handle_tcp(stream, remote_addr, cancel, dispatcher, splitter).await {
                                    debug!("TCP connection {remote_addr} closed with error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept failed: {e}");
                        }
                    }
                }

                _ = cancel.cancelled() => break,
            }
        }
    });

    Ok(())
}

fn spawn_tcp_owned<D, S>(
    tcp: std::net::TcpListener,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    tcp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;
    let listener = tokio::net::TcpListener::from_std(tcp).hand_log(|msg| error!("{msg}"))?;

    tokio::spawn(async move {
        loop {
            select! {
                biased;

                res = listener.accept() => {
                    match res {
                        Ok((stream, remote_addr)) => {
                            let dispatcher = dispatcher.clone();
                            let cancel = cancel.clone();

                            tokio::spawn(async move {
                                let splitter = S::default();
                                if let Err(e) = handle_tcp_owned(stream, remote_addr, cancel, dispatcher, splitter).await {
                                    debug!("TCP connection {remote_addr} closed with error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept failed: {e}");
                        }
                    }
                }

                _ = cancel.cancelled() => break,
            }
        }
    });

    Ok(())
}

async fn run_managed_packet_io<D, S, E>(
    tcp_listener: Option<tokio::net::TcpListener>,
    udp_socket: Option<Arc<UdpSocket>>,
    cancel: CancellationToken,
    external_cancel: CancellationToken,
    dispatcher: Arc<D>,
    writer: PacketWriter<E>,
) -> ManagedTaskReport
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    let mut report = ManagedTaskReport::default();
    let mut protocol_tasks = JoinSet::new();

    if let Some(tcp_listener) = tcp_listener {
        protocol_tasks.spawn(run_tcp_listener::<D, S, E>(
            tcp_listener,
            cancel.clone(),
            dispatcher.clone(),
            writer,
        ));
    }
    if let Some(udp_socket) = udp_socket {
        protocol_tasks.spawn(run_udp_receiver(udp_socket, cancel.clone(), dispatcher));
    }

    if protocol_tasks.is_empty() {
        report.completed += 1;
        return report;
    }

    let mut watch_external_cancel = true;
    while !protocol_tasks.is_empty() {
        select! {
            biased;

            _ = external_cancel.cancelled(), if watch_external_cancel => {
                watch_external_cancel = false;
                cancel.cancel();
            }

            joined = protocol_tasks.join_next() => {
                if let Some(joined) = joined {
                    record_protocol_join(&mut report, joined);
                }
            }
        }
    }
    report
}

async fn run_tcp_listener<D, S, E>(
    tcp_listener: tokio::net::TcpListener,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    writer: PacketWriter<E>,
) -> ManagedTaskReport
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    let mut report = ManagedTaskReport::default();
    let mut connections = JoinSet::new();

    loop {
        select! {
            biased;

            _ = cancel.cancelled() => {
                report.cancelled += 1;
                break;
            }

            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(joined) = joined {
                    record_connection_join(&mut report, joined);
                }
            }

            accepted = tcp_listener.accept() => {
                match accepted {
                    Ok((stream, remote_addr)) if !cancel.is_cancelled() => {
                        connections.spawn(run_managed_tcp_connection::<D, S, E>(
                            stream,
                            remote_addr,
                            cancel.child_token(),
                            dispatcher.clone(),
                            writer.clone(),
                        ));
                    }
                    Ok(_) => break,
                    Err(error) => {
                        report.failed += 1;
                        debug!("tcp accept failed: {error}");
                    }
                }
            }
        }
    }

    cancel.cancel();
    while let Some(joined) = connections.join_next().await {
        record_connection_join(&mut report, joined);
    }
    report
}

async fn run_udp_receiver<D>(
    udp_socket: Arc<UdpSocket>,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> ManagedTaskReport
where
    D: PacketDispatcher,
{
    let mut report = ManagedTaskReport::default();
    loop {
        let mut buf = BytesMut::with_capacity(UDP_RECV_BUF_SIZE);
        select! {
            biased;

            _ = cancel.cancelled() => {
                report.cancelled += 1;
                break;
            }

            received = udp_socket_read_owned_buf(&mut buf, udp_socket.as_ref()) => {
                match received {
                    Ok((size, remote_addr)) if size != 0 => {
                        let packet = buf.split_to(size).freeze();
                        if let Err(error) = dispatcher.dispatch_owned(
                            packet,
                            remote_addr,
                            Protocol::UDP,
                        ) {
                            debug!("udp dispatch {remote_addr} failed: {error}");
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        report.failed += 1;
                        debug!("udp read failed: {error}");
                        break;
                    }
                }
            }
        }
    }
    report
}

fn record_protocol_join(
    report: &mut ManagedTaskReport,
    joined: Result<ManagedTaskReport, JoinError>,
) {
    match joined {
        Ok(task_report) => report.merge(task_report),
        Err(join_error) if join_error.is_panic() => report.panicked += 1,
        Err(join_error) if join_error.is_cancelled() => {
            report.cancelled += 1;
            report.remaining += 1;
        }
        Err(_) => report.failed += 1,
    }
}

async fn run_managed_tcp_connection<D, S, E>(
    stream: TcpStream,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    writer: PacketWriter<E>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
{
    let (read_half, write_half) = stream.into_split();
    match writer.tcp_write_mode().queue_size() {
        Some(queue_size) => {
            let (tx, rx) = mpsc::channel(queue_size);
            writer.insert_tcp_writer(remote_addr, tx, cancel.clone());
            let (read_result, write_result) = tokio::join!(
                handle_tcp_read_owned_half::<D, S, E>(
                    read_half,
                    remote_addr,
                    cancel.clone(),
                    dispatcher,
                    S::default(),
                    writer.clone(),
                ),
                handle_tcp_write::<E>(write_half, remote_addr, cancel, rx, writer),
            );
            read_result?;
            write_result
        }
        None => {
            writer.insert_direct_tcp_writer(remote_addr, write_half, cancel.clone());
            handle_tcp_read_owned_half::<D, S, E>(
                read_half,
                remote_addr,
                cancel,
                dispatcher,
                S::default(),
                writer,
            )
            .await
        }
    }
}

fn record_connection_join(
    report: &mut ManagedTaskReport,
    joined: Result<GlobalResult<()>, JoinError>,
) {
    match joined {
        Ok(Ok(())) => report.completed += 1,
        Ok(Err(error)) => {
            report.failed += 1;
            debug!("managed tcp connection failed: {error}");
        }
        Err(join_error) if join_error.is_panic() => {
            report.panicked += 1;
            error!("managed tcp connection panicked: {join_error}");
        }
        Err(join_error) if join_error.is_cancelled() => {
            report.cancelled += 1;
            report.remaining += 1;
        }
        Err(join_error) => {
            report.failed += 1;
            error!("managed tcp connection join failed: {join_error}");
        }
    }
}

async fn handle_tcp<D, S>(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    mut splitter: S,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter,
{
    let mut buf = BytesMut::with_capacity(TCP_READ_BUF_SIZE);
    loop {
        select! {
            res = tcp_stream_read_buf(&mut buf, &mut stream) => {
                match res {
                    Ok(0) => break,
                    Ok(_) if buf.len() > MAX_BUF_SIZE => {
                        warn!("recv data greater than max buf size; close peer");
                        break;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!("tcp read {remote_addr} failed: {err}");
                        break;
                    }
                }

                splitter.feed_owned(&mut buf, |pkt| {
                    dispatcher.dispatch_owned(pkt, remote_addr, Protocol::TCP)
                })?;
            }
            _ = cancel.cancelled() => break,
        }
    }
    dispatcher.close(remote_addr, Protocol::TCP)?;
    Ok(())
}

async fn handle_tcp_owned<D, S>(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    mut splitter: S,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter,
{
    let mut buf = BytesMut::with_capacity(TCP_READ_BUF_SIZE);
    loop {
        select! {
            res = tcp_stream_read_buf(&mut buf, &mut stream) => {
                match res {
                    Ok(0) => break,
                    Ok(_) if buf.len() > MAX_BUF_SIZE => {
                        warn!("recv data greater than max buf size; close peer");
                        break;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!("tcp read {remote_addr} failed: {err}");
                        break;
                    }
                }

                splitter.feed_owned(&mut buf, |pkt| {
                    dispatcher.dispatch_owned(pkt, remote_addr, Protocol::TCP)
                })?;
            }
            _ = cancel.cancelled() => break,
        }
    }
    dispatcher.close(remote_addr, Protocol::TCP)?;
    Ok(())
}

async fn handle_tcp_read_owned_half<D, S, E>(
    mut stream: OwnedReadHalf,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    mut splitter: S,
    writer: PacketWriter<E>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter,
    E: PacketEncoder,
{
    let mut buf = BytesMut::with_capacity(TCP_READ_BUF_SIZE);
    loop {
        select! {
            res = tcp_stream_read_buf(&mut buf, &mut stream) => {
                match res {
                    Ok(0) => break,
                    Ok(_) if buf.len() > MAX_BUF_SIZE => {
                        warn!("recv data greater than max buf size; close peer");
                        break;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        debug!("tcp read {remote_addr} failed: {err}");
                        break;
                    }
                }

                splitter.feed_owned(&mut buf, |pkt| {
                    dispatcher.dispatch_owned(pkt, remote_addr, Protocol::TCP)
                })?;
            }
            _ = cancel.cancelled() => break,
        }
    }
    writer.remove_tcp_writer(&remote_addr);
    dispatcher.close(remote_addr, Protocol::TCP)?;
    Ok(())
}

async fn handle_tcp_write<E>(
    mut stream: OwnedWriteHalf,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    mut rx: mpsc::Receiver<EncodedPacket>,
    writer: PacketWriter<E>,
) -> GlobalResult<()>
where
    E: PacketEncoder,
{
    loop {
        select! {
            item = rx.recv() => {
                let Some(data) = item else {
                    break;
                };
                if let Err(err) = write_encoded_packet(&mut stream, data).await {
                    debug!("tcp write {remote_addr} failed: {err}");
                    break;
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
    cancel.cancel();
    writer.remove_tcp_writer(&remote_addr);
    stream
        .shutdown()
        .await
        .hand_log(|msg| debug!("{msg}: remote_addr={remote_addr}"))
}

async fn write_encoded_packet<W>(stream: &mut W, packet: EncodedPacket) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match packet {
        EncodedPacket::Single(data) => stream.write_all(data.as_ref()).await,
        EncodedPacket::InlinePrefix { prefix, payload } => {
            write_all_vectored_2(stream, prefix.as_slice(), payload.as_ref()).await
        }
    }
}

async fn write_all_vectored_2<W>(
    stream: &mut W,
    mut first: &[u8],
    mut second: &[u8],
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while !first.is_empty() || !second.is_empty() {
        let written = if first.is_empty() {
            stream.write(second).await?
        } else if second.is_empty() {
            stream.write(first).await?
        } else {
            let bufs = [IoSlice::new(first), IoSlice::new(second)];
            stream.write_vectored(&bufs).await?
        };
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write packet",
            ));
        }
        if written < first.len() {
            first = &first[written..];
        } else {
            let second_written = written - first.len();
            first = &[];
            second = &second[second_written.min(second.len())..];
        }
    }
    Ok(())
}

fn ensure_spare_capacity(buf: &mut BytesMut, min_spare: usize, reserve_size: usize) {
    if buf.capacity().saturating_sub(buf.len()) < min_spare {
        buf.reserve(reserve_size);
    }
}

async fn tcp_stream_read_buf<R>(buf: &mut BytesMut, stream: &mut R) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    ensure_spare_capacity(buf, TCP_MIN_READ_SPARE, TCP_READ_BUF_SIZE);
    stream.read_buf(buf).await
}

fn into_tokio_udp_socket(udp: std::net::UdpSocket) -> GlobalResult<Arc<UdpSocket>> {
    udp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;
    UdpSocket::from_std(udp)
        .map(Arc::new)
        .hand_log(|msg| debug!("{msg}"))
}

fn spawn_udp<D>(
    udp: std::net::UdpSocket,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
{
    spawn_udp_owned(udp, cancel, dispatcher)
}
fn spawn_udp_owned<D>(
    udp: std::net::UdpSocket,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
{
    udp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;

    let socket = UdpSocket::from_std(udp).hand_log(|msg| debug!("{msg}"))?;

    tokio::spawn(async move {
        loop {
            let mut buf = BytesMut::with_capacity(UDP_RECV_BUF_SIZE);
            select! {
                res = udp_socket_read_owned_buf(&mut buf, &socket) => {
                    match res {
                        Ok((n,addr)) if n != 0 => {
                            let pkt = buf.split_to(n).freeze();
                            if let Err(err) = dispatcher.dispatch_owned(pkt, addr, Protocol::UDP) {
                                debug!("udp dispatch {addr} failed: {err}");
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            debug!("udp read failed: {err}");
                            break;
                        }
                    }
                }

                _ = cancel.cancelled() => break,
            }
        }
    });

    Ok(())
}

async fn udp_socket_read_owned_buf(
    buf: &mut BytesMut,
    socket: &UdpSocket,
) -> GlobalResult<(usize, SocketAddr)> {
    socket
        .recv_buf_from(buf)
        .await
        .hand_log(|msg| error!("read buf failed:{msg}"))
}

#[cfg(test)]
mod tests {
    use super::{
        handle_tcp_write, into_tokio_udp_socket, managed_rw_with_tcp_write_mode, ManagedCloseState,
        ManagedPacketIo, ManagedTaskReport, NetworkCloseReport, PacketDispatcher, PacketSplitter,
        PacketWriter, RawPacketEncoder, TcpWriteMode,
    };
    use crate::net::state::Protocol;
    use crate::tokio;
    use crate::tokio::net::{TcpListener, TcpStream};
    use crate::tokio::sync::{mpsc, Mutex, Notify};
    use crate::tokio_util::sync::CancellationToken;
    use crate::utils::rt::GlobalRuntime;
    use bytes::{Bytes, BytesMut};
    use exception::GlobalResult;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct DrainSplitter;

    impl PacketSplitter for DrainSplitter {
        fn feed_owned<F>(&mut self, chunk: &mut BytesMut, mut dispatch: F) -> GlobalResult<()>
        where
            F: FnMut(Bytes) -> GlobalResult<()>,
        {
            if !chunk.is_empty() {
                dispatch(chunk.split().freeze())?;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct TestDispatcher {
        udp_packets: AtomicUsize,
        tcp_closes: AtomicUsize,
    }

    impl PacketDispatcher for TestDispatcher {
        fn dispatch_owned(
            &self,
            _data: Bytes,
            _remote_addr: SocketAddr,
            protocol: Protocol,
        ) -> GlobalResult<()> {
            if protocol == Protocol::UDP {
                self.udp_packets.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }

        fn close(&self, _remote_addr: SocketAddr, protocol: Protocol) -> GlobalResult<()> {
            if protocol == Protocol::TCP {
                self.tcp_closes.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    #[test]
    fn inline_prefix_macro_uses_numeric_width_and_endian() {
        assert_eq!(
            crate::inline_prefix!(be, 0x1234u16).as_slice(),
            &[0x12, 0x34]
        );
        assert_eq!(
            crate::inline_prefix!(le, 0x1234u16).as_slice(),
            &[0x34, 0x12]
        );
        assert_eq!(
            crate::inline_prefix!(0x01020304u32).as_slice(),
            &[0x01, 0x02, 0x03, 0x04]
        );
        assert_eq!(crate::inline_prefix!(ne, 0x12u8).as_slice(), &[0x12]);
        assert_eq!(
            crate::inline_prefix!(be, 0x0102u16).as_slice(),
            &[0x01, 0x02]
        );
        assert_eq!(crate::inline_prefix!(be, 0x0102030405060708u64).len(), 8);
    }

    #[tokio::test]
    async fn tcp_write_task_exit_cancels_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::spawn(TcpStream::connect(addr));
        let (server, remote_addr) = listener.accept().await.unwrap();
        let client = connect.await.unwrap().unwrap();
        let (_read_half, write_half) = server.into_split();
        let cancel = CancellationToken::new();
        let writer = PacketWriter::new(None, Arc::new(RawPacketEncoder), TcpWriteMode::default());
        let (tx, rx) = mpsc::channel(1);
        drop(tx);

        handle_tcp_write(write_half, remote_addr, cancel.clone(), rx, writer)
            .await
            .unwrap();

        assert!(cancel.is_cancelled());
        drop(client);
    }

    #[tokio::test]
    async fn packet_writer_close_finishes_cleanup_after_prior_cancellation() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_addr = udp.local_addr().unwrap();
        let writer = PacketWriter::new(
            Some(into_tokio_udp_socket(udp).unwrap()),
            Arc::new(RawPacketEncoder),
            TcpWriteMode::Direct,
        );

        writer.closed.cancel();
        writer.close().await.unwrap();

        let rebound = std::net::UdpSocket::bind(local_addr).unwrap();
        drop(rebound);
    }

    #[tokio::test]
    async fn managed_close_resumes_after_waiter_timeout() {
        let release = Arc::new(Notify::new());
        let task_release = release.clone();
        let root_task = tokio::spawn(async move {
            task_release.notified().await;
            ManagedTaskReport {
                completed: 1,
                ..ManagedTaskReport::default()
            }
        });
        let managed = ManagedPacketIo {
            writer: PacketWriter::new(None, Arc::new(RawPacketEncoder), TcpWriteMode::Direct),
            cancel: CancellationToken::new(),
            close_state: Mutex::new(ManagedCloseState {
                root_task: Some(root_task),
                report: NetworkCloseReport::default(),
                writer_closed: false,
                completed: false,
                failure_logged: false,
            }),
        };

        assert!(
            tokio::time::timeout(Duration::from_millis(10), managed.close_and_wait())
                .await
                .is_err()
        );
        {
            let state = managed.close_state.lock().await;
            assert!(state.root_task.is_some());
            assert!(state.writer_closed);
            assert!(!state.completed);
        }

        release.notify_one();
        let report = tokio::time::timeout(Duration::from_secs(1), managed.close_and_wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.completed, 1);
        assert!(report.is_complete());
    }

    #[tokio::test]
    async fn managed_close_is_idempotent_and_releases_tcp_udp_listeners() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();
        let udp = std::net::UdpSocket::bind(local_addr).unwrap();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-listener-release-{local_addr}"),
                (Some(tcp), Some(udp)),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let writer = managed.writer();
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"packet", local_addr).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while dispatcher.udp_packets.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let first = managed.close_and_wait().await.unwrap();
        let second = managed.close_and_wait().await.unwrap();

        assert_eq!(first, second);
        assert!(first.is_complete());
        assert_eq!(first.cancelled, 2);
        assert!(writer
            .write_slice_to(b"closed", sender.local_addr().unwrap(), Protocol::UDP)
            .await
            .is_err());
        let rebound_tcp = std::net::TcpListener::bind(local_addr).unwrap();
        let rebound_udp = std::net::UdpSocket::bind(local_addr).unwrap();
        drop((rebound_tcp, rebound_udp));
    }

    #[tokio::test]
    async fn managed_close_waits_for_active_tcp_read_and_writer_tasks() {
        for (name, write_mode) in [
            ("queued", TcpWriteMode::Queued { queue_size: 4 }),
            ("direct", TcpWriteMode::Direct),
        ] {
            let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let local_addr = tcp.local_addr().unwrap();
            let dispatcher = Arc::new(TestDispatcher::default());
            let managed =
                managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                    &GlobalRuntime::get_main_runtime(),
                    format!("managed-active-{name}-{local_addr}"),
                    (Some(tcp), None),
                    CancellationToken::new(),
                    dispatcher.clone(),
                    Arc::new(RawPacketEncoder),
                    write_mode,
                )
                .unwrap();
            let writer = managed.writer();
            let mut client = TcpStream::connect(local_addr).await.unwrap();
            let remote_addr = client.local_addr().unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while !writer.has_tcp_writer(&remote_addr) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            client.write_all(b"packet").await.unwrap();

            let report = managed.close_and_wait().await.unwrap();

            assert!(report.is_complete(), "write_mode={name}: {report:?}");
            assert_eq!(report.cancelled, 1, "write_mode={name}");
            assert!(report.completed >= 1, "write_mode={name}: {report:?}");
            assert!(!writer.has_tcp_writer(&remote_addr));
            assert_eq!(dispatcher.tcp_closes.load(Ordering::Relaxed), 1);
            let mut byte = [0u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(read, 0, "write_mode={name}");
        }
    }

    #[tokio::test]
    async fn managed_close_rejects_an_aborted_root_task() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_addr = udp.local_addr().unwrap();
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-aborted-root-{local_addr}"),
                (None, Some(udp)),
                CancellationToken::new(),
                Arc::new(TestDispatcher::default()),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        {
            let state = managed.close_state.lock().await;
            state.root_task.as_ref().unwrap().abort();
        }

        assert!(managed.close_and_wait().await.is_err());
        let state = managed.close_state.lock().await;
        let report = &state.report;
        assert_eq!(report.cancelled, 1);
        assert_eq!(report.remaining, 1);
        assert!(!report.is_complete());
    }
}
