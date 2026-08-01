use crate::net::state::Protocol;
use crate::utils::rt::GlobalRuntime;
use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use exception::{GlobalError, GlobalResult, GlobalResultExt};
use log::{debug, error, warn};
use std::collections::{HashMap, HashSet};
use std::io::{self, IoSlice};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::select;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
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
const UDP_MAX_DATAGRAM_SIZE: usize = u16::MAX as usize;
const TCP_ACCEPT_ERROR_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_millis(10);
const TCP_ACCEPT_ERROR_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(1);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TcpWriterRegistration {
    remote_addr: SocketAddr,
    writer_id: u64,
}

struct RegisteredTcpSink<E>
where
    E: PacketEncoder,
{
    registration: TcpWriterRegistration,
    sink: TcpPacketSink<E>,
}

pub struct PacketWriter<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    udp_socket: Arc<RwLock<Option<Arc<UdpSocket>>>>,
    tcp_writers: Arc<DashMap<SocketAddr, RegisteredTcpSink<E>>>,
    tcp_writer_addrs_by_ip: Arc<DashMap<IpAddr, Vec<TcpWriterRegistration>>>,
    tcp_writer_registry_lock: Arc<StdRwLock<()>>,
    next_tcp_writer_id: Arc<AtomicU64>,
    tcp_writer_ready: Arc<Notify>,
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
            tcp_writer_registry_lock: self.tcp_writer_registry_lock.clone(),
            next_tcp_writer_id: self.next_tcp_writer_id.clone(),
            tcp_writer_ready: self.tcp_writer_ready.clone(),
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
            tcp_writer_registry_lock: Arc::new(StdRwLock::new(())),
            next_tcp_writer_id: Arc::new(AtomicU64::new(1)),
            tcp_writer_ready: Arc::new(Notify::new()),
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
            .map(|item| item.value().sink.clone())
    }

    pub fn tcp_sink_by_ip(&self, remote_ip: IpAddr) -> Option<TcpPacketSink<E>> {
        let _registry_guard = read_unpoisoned(&self.tcp_writer_registry_lock);
        let registrations = self.tcp_writer_addrs_by_ip.get(&remote_ip)?;
        registrations.iter().rev().find_map(|registration| {
            self.tcp_writers
                .get(&registration.remote_addr)
                .filter(|registered| registered.registration == *registration)
                .map(|registered| registered.sink.clone())
        })
    }

    /// Waits until the exact TCP peer writer is registered or the deadline/endpoint closes.
    pub async fn wait_tcp_sink(
        &self,
        remote_addr: SocketAddr,
        timeout: std::time::Duration,
    ) -> GlobalResult<TcpPacketSink<E>> {
        if timeout.is_zero() {
            return Err(GlobalError::new_sys_error(
                "tcp writer wait timeout must be greater than zero",
                |msg| debug!("{msg}: remote_addr={remote_addr}"),
            ));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.ensure_open()?;
            if let Some(sink) = self.tcp_sink(&remote_addr) {
                return Ok(sink);
            }
            let notified = self.tcp_writer_ready.notified();
            if let Some(sink) = self.tcp_sink(&remote_addr) {
                return Ok(sink);
            }
            select! {
                biased;
                _ = self.closed.cancelled() => return Err(packet_writer_closed()),
                result = tokio::time::timeout_at(deadline, notified) => {
                    if result.is_err() {
                        return Err(GlobalError::new_sys_error(
                            "tcp writer wait timed out",
                            |msg| debug!("{msg}: remote_addr={remote_addr}"),
                        ));
                    }
                }
            }
        }
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
        self.insert_registered_tcp_writer(remote_addr, sender, cancel);
    }

    fn insert_registered_tcp_writer(
        &self,
        remote_addr: SocketAddr,
        sender: mpsc::Sender<EncodedPacket>,
        cancel: CancellationToken,
    ) -> Option<TcpWriterRegistration> {
        let sink = TcpPacketSink::Queued(QueuedTcpSink::new(
            remote_addr,
            sender,
            self.encoder.clone(),
            cancel,
        ));
        self.insert_tcp_sink(remote_addr, sink)
    }

    pub fn insert_direct_tcp_writer(
        &self,
        remote_addr: SocketAddr,
        stream: OwnedWriteHalf,
        cancel: CancellationToken,
    ) {
        self.insert_registered_direct_tcp_writer(remote_addr, stream, cancel);
    }

    fn insert_registered_direct_tcp_writer(
        &self,
        remote_addr: SocketAddr,
        stream: OwnedWriteHalf,
        cancel: CancellationToken,
    ) -> Option<TcpWriterRegistration> {
        let sink = TcpPacketSink::Direct(DirectTcpSink::new(
            remote_addr,
            stream,
            self.encoder.clone(),
            cancel,
        ));
        self.insert_tcp_sink(remote_addr, sink)
    }

    fn insert_tcp_sink(
        &self,
        remote_addr: SocketAddr,
        sink: TcpPacketSink<E>,
    ) -> Option<TcpWriterRegistration> {
        if self.closed.is_cancelled() {
            sink.close();
            return None;
        }
        let writer_id = self.next_tcp_writer_id.fetch_add(1, Ordering::Relaxed);
        let registration = TcpWriterRegistration {
            remote_addr,
            writer_id,
        };
        let previous = {
            let _registry_guard = write_unpoisoned(&self.tcp_writer_registry_lock);
            if self.closed.is_cancelled() {
                sink.close();
                return None;
            }
            let previous = self
                .tcp_writers
                .insert(remote_addr, RegisteredTcpSink { registration, sink });
            let mut registrations = self
                .tcp_writer_addrs_by_ip
                .entry(remote_addr.ip())
                .or_default();
            registrations.retain(|item| item.remote_addr != remote_addr);
            registrations.push(registration);
            previous
        };
        if let Some(previous) = previous {
            previous.sink.close();
        }
        self.tcp_writer_ready.notify_waiters();
        Some(registration)
    }

    pub fn remove_tcp_writer(&self, remote_addr: &SocketAddr) {
        if let Some(sink) = self.remove_tcp_writer_inner(remote_addr, None) {
            sink.close();
        }
    }

    fn remove_registered_tcp_writer(&self, registration: TcpWriterRegistration) {
        if let Some(sink) =
            self.remove_tcp_writer_inner(&registration.remote_addr, Some(registration.writer_id))
        {
            sink.close();
        }
    }

    fn remove_tcp_writer_inner(
        &self,
        remote_addr: &SocketAddr,
        expected_writer_id: Option<u64>,
    ) -> Option<TcpPacketSink<E>> {
        let _registry_guard = write_unpoisoned(&self.tcp_writer_registry_lock);
        let current_writer_id = self
            .tcp_writers
            .get(remote_addr)
            .map(|registered| registered.registration.writer_id)?;
        if expected_writer_id.is_some_and(|expected| expected != current_writer_id) {
            return None;
        }
        let (_, registered) = self.tcp_writers.remove(remote_addr)?;
        let remote_ip = remote_addr.ip();
        let remove_ip_entry =
            if let Some(mut registrations) = self.tcp_writer_addrs_by_ip.get_mut(&remote_ip) {
                registrations.retain(|item| item != &registered.registration);
                registrations.is_empty()
            } else {
                false
            };
        if remove_ip_entry {
            self.tcp_writer_addrs_by_ip.remove(&remote_ip);
        }
        Some(registered.sink)
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

        let sinks = {
            let _registry_guard = write_unpoisoned(&self.tcp_writer_registry_lock);
            let sinks = self
                .tcp_writers
                .iter()
                .map(|entry| entry.value().sink.clone())
                .collect::<Vec<_>>();
            self.tcp_writers.clear();
            self.tcp_writer_addrs_by_ip.clear();
            sinks
        };

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

fn lock_unpoisoned<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_unpoisoned<T>(lock: &StdRwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_unpoisoned<T>(lock: &StdRwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// Connection parameters for a TCP peer actively opened by a managed endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedTcpConnectOptions {
    pub remote_addr: SocketAddr,
    pub local_addr: Option<SocketAddr>,
    pub timeout: std::time::Duration,
}

struct ManagedTcpConnectionCloseState {
    task: Option<JoinHandle<GlobalResult<()>>>,
    report: NetworkCloseReport,
    completed: bool,
    failure_logged: bool,
}

struct ManagedTcpConnectionInner<E>
where
    E: PacketEncoder,
{
    connection_id: u64,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    writer: PacketWriter<E>,
    cancel: CancellationToken,
    close_state: Mutex<ManagedTcpConnectionCloseState>,
    owner_registered: AtomicBool,
    owner_connections: Weak<StdMutex<HashMap<u64, ManagedTcpConnection<E>>>>,
    owner_report: Weak<StdMutex<NetworkCloseReport>>,
}

impl<E> Drop for ManagedTcpConnectionInner<E>
where
    E: PacketEncoder,
{
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A managed active TCP connection registered with a [`ManagedPacketIo`] endpoint.
#[must_use = "managed TCP connection must be retained and closed explicitly"]
pub struct ManagedTcpConnection<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    inner: Arc<ManagedTcpConnectionInner<E>>,
}

impl<E> Clone for ManagedTcpConnection<E>
where
    E: PacketEncoder,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<E> ManagedTcpConnection<E>
where
    E: PacketEncoder,
{
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.inner.remote_addr
    }

    pub async fn write(&self, data: Bytes) -> GlobalResult<()> {
        self.inner
            .writer
            .write_to(data, self.inner.remote_addr, Protocol::TCP)
            .await
    }

    /// Cancels this connection and waits for its read/write tasks to exit.
    pub async fn close_and_wait(&self) -> GlobalResult<NetworkCloseReport> {
        let mut state = self.inner.close_state.lock().await;
        if state.completed {
            let report = state.report.clone();
            drop(state);
            self.finalize_owner(&report);
            return close_report_result(report, false);
        }

        self.inner.cancel.cancel();
        let task_result = match state.task.as_mut() {
            Some(task) => Some(task.await),
            None => None,
        };
        if let Some(task_result) = task_result {
            state.task.take();
            match task_result {
                Ok(Ok(())) => state.report.completed += 1,
                Ok(Err(error)) => {
                    state.report.failed += 1;
                    debug!(
                        "managed active tcp connection failed: remote_addr={}, error={error}",
                        self.inner.remote_addr
                    );
                }
                Err(join_error) => record_join_error(&mut state.report, join_error),
            }
        }

        state.completed = true;
        let log_failure = !state.failure_logged;
        if !state.report.is_complete() {
            state.failure_logged = true;
        }
        let report = state.report.clone();
        drop(state);
        self.finalize_owner(&report);
        close_report_result(report, log_failure)
    }

    fn finalize_owner(&self, report: &NetworkCloseReport) {
        if self.inner.owner_registered.swap(false, Ordering::AcqRel) {
            if let Some(owner_report) = self.inner.owner_report.upgrade() {
                lock_unpoisoned(&owner_report).merge_network(report.clone());
            }
            if let Some(owner_connections) = self.inner.owner_connections.upgrade() {
                lock_unpoisoned(&owner_connections).remove(&self.inner.connection_id);
            }
        }
    }

    fn task_is_finished(&self) -> bool {
        self.inner.close_state.try_lock().is_ok_and(|state| {
            state.completed || state.task.as_ref().is_none_or(JoinHandle::is_finished)
        })
    }
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

    fn merge_network(&mut self, other: Self) {
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
    connecting_tcp_peers: StdMutex<HashSet<SocketAddr>>,
    active_connections: Arc<StdMutex<HashMap<u64, ManagedTcpConnection<E>>>>,
    completed_connection_report: Arc<StdMutex<NetworkCloseReport>>,
    next_connection_id: AtomicU64,
    close_state: Mutex<ManagedCloseState>,
}

struct TcpConnectReservation<'a> {
    remote_addr: SocketAddr,
    connecting_tcp_peers: &'a StdMutex<HashSet<SocketAddr>>,
}

impl Drop for TcpConnectReservation<'_> {
    fn drop(&mut self) {
        lock_unpoisoned(self.connecting_tcp_peers).remove(&self.remote_addr);
    }
}

impl<E> ManagedPacketIo<E>
where
    E: PacketEncoder,
{
    /// Returns a clone of the writer bound to this managed endpoint.
    pub fn writer(&self) -> PacketWriter<E> {
        self.writer.clone()
    }

    fn reserve_tcp_connect(
        &self,
        remote_addr: SocketAddr,
    ) -> GlobalResult<TcpConnectReservation<'_>> {
        let mut connecting_tcp_peers = lock_unpoisoned(&self.connecting_tcp_peers);
        if self.writer.has_tcp_writer(&remote_addr) || !connecting_tcp_peers.insert(remote_addr) {
            return Err(GlobalError::new_sys_error(
                "managed tcp connection already exists",
                |msg| debug!("{msg}: remote_addr={remote_addr}"),
            ));
        }
        drop(connecting_tcp_peers);
        Ok(TcpConnectReservation {
            remote_addr,
            connecting_tcp_peers: &self.connecting_tcp_peers,
        })
    }

    async fn reap_finished_connections(&self) {
        let finished = lock_unpoisoned(&self.active_connections)
            .values()
            .filter(|connection| connection.task_is_finished())
            .cloned()
            .collect::<Vec<_>>();
        for connection in finished {
            if let Err(error) = connection.close_and_wait().await {
                debug!(
                    "finished managed tcp connection reaped with failure: remote_addr={}, error={error}",
                    connection.remote_addr()
                );
            }
        }
    }

    /// Actively connects a TCP peer and registers its read/write tasks with this endpoint.
    ///
    /// The returned handle is READY: the exact remote-address writer is installed before this
    /// method completes. The connection shares endpoint cancellation and is also awaited by
    /// [`ManagedPacketIo::close_and_wait`].
    pub async fn connect_tcp<D, S>(
        &self,
        runtime: &GlobalRuntime,
        task_name: impl Into<String>,
        options: ManagedTcpConnectOptions,
        dispatcher: Arc<D>,
    ) -> GlobalResult<ManagedTcpConnection<E>>
    where
        D: PacketDispatcher,
        S: PacketSplitter + Default,
    {
        if options.timeout.is_zero() {
            return Err(GlobalError::new_sys_error(
                "managed tcp connect timeout must be greater than zero",
                |msg| debug!("{msg}: remote_addr={}", options.remote_addr),
            ));
        }
        if options
            .local_addr
            .is_some_and(|local| local.is_ipv4() != options.remote_addr.is_ipv4())
        {
            return Err(GlobalError::new_sys_error(
                "managed tcp local and remote address families differ",
                |msg| {
                    debug!(
                        "{msg}: local_addr={:?}, remote_addr={}",
                        options.local_addr, options.remote_addr
                    )
                },
            ));
        }

        if self.cancel.is_cancelled() {
            return Err(managed_tcp_connect_cancelled(options.remote_addr));
        }
        let _connect_reservation = self.reserve_tcp_connect(options.remote_addr)?;
        self.reap_finished_connections().await;

        let socket = if options.remote_addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .hand_log(|msg| debug!("{msg}: remote_addr={}", options.remote_addr))?;
        if let Some(local_addr) = options.local_addr {
            socket
                .bind(local_addr)
                .hand_log(|msg| debug!("{msg}: local_addr={local_addr}"))?;
        }

        let stream = select! {
            biased;
            _ = self.cancel.cancelled() => {
                return Err(managed_tcp_connect_cancelled(options.remote_addr));
            }
            result = tokio::time::timeout(options.timeout, socket.connect(options.remote_addr)) => {
                match result {
                    Ok(result) => result.hand_log(|msg| {
                        debug!("{msg}: remote_addr={}", options.remote_addr)
                    })?,
                    Err(_) => {
                        return Err(GlobalError::new_sys_error(
                            "managed tcp connect timed out",
                            |msg| debug!("{msg}: remote_addr={}", options.remote_addr),
                        ));
                    }
                }
            }
        };
        let local_addr = stream
            .local_addr()
            .hand_log(|msg| debug!("{msg}: remote_addr={}", options.remote_addr))?;
        let cancel = self.cancel.child_token();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = runtime.spawn(
            task_name,
            run_managed_tcp_connection::<D, S, E>(
                stream,
                options.remote_addr,
                cancel.clone(),
                dispatcher,
                self.writer.clone(),
                Some(ready_tx),
            ),
        )?;
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let connection = ManagedTcpConnection {
            inner: Arc::new(ManagedTcpConnectionInner {
                connection_id,
                local_addr,
                remote_addr: options.remote_addr,
                writer: self.writer.clone(),
                cancel,
                close_state: Mutex::new(ManagedTcpConnectionCloseState {
                    task: Some(task),
                    report: NetworkCloseReport::default(),
                    completed: false,
                    failure_logged: false,
                }),
                owner_registered: AtomicBool::new(false),
                owner_connections: Arc::downgrade(&self.active_connections),
                owner_report: Arc::downgrade(&self.completed_connection_report),
            }),
        };

        let ready = select! {
            biased;
            _ = self.cancel.cancelled() => false,
            ready = ready_rx => ready.is_ok(),
        };
        if !ready {
            if let Err(error) = connection.close_and_wait().await {
                debug!(
                    "unready managed tcp connection close failed: remote_addr={}, error={error}",
                    options.remote_addr
                );
            }
            return Err(managed_tcp_connect_cancelled(options.remote_addr));
        }

        let state = self.close_state.lock().await;
        if self.cancel.is_cancelled() || state.completed {
            drop(state);
            if let Err(error) = connection.close_and_wait().await {
                debug!(
                    "late managed tcp connection close failed: remote_addr={}, error={error}",
                    options.remote_addr
                );
            }
            return Err(managed_tcp_connect_cancelled(options.remote_addr));
        }
        connection
            .inner
            .owner_registered
            .store(true, Ordering::Release);
        lock_unpoisoned(&self.active_connections).insert(connection_id, connection.clone());
        Ok(connection)
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

        loop {
            let active_connections = lock_unpoisoned(&self.active_connections)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            if active_connections.is_empty() {
                break;
            }
            for connection in active_connections {
                if let Err(error) = connection.close_and_wait().await {
                    debug!(
                        "owned managed tcp connection close failed: remote_addr={}, error={error}",
                        connection.remote_addr()
                    );
                }
            }
        }
        let completed_connection_report =
            std::mem::take(&mut *lock_unpoisoned(&self.completed_connection_report));
        state.report.merge_network(completed_connection_report);

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

fn managed_tcp_connect_cancelled(remote_addr: SocketAddr) -> GlobalError {
    GlobalError::new_sys_error("managed tcp connect cancelled", |msg| {
        debug!("{msg}: remote_addr={remote_addr}")
    })
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
        connecting_tcp_peers: StdMutex::new(HashSet::new()),
        active_connections: Arc::new(StdMutex::new(HashMap::new())),
        completed_connection_report: Arc::new(StdMutex::new(NetworkCloseReport::default())),
        next_connection_id: AtomicU64::new(1),
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
        let mut accept_error_backoff = std::time::Duration::ZERO;
        loop {
            select! {
                biased;

                res = listener.accept() => {
                    match res {
                        Ok((stream, remote_addr)) => {
                            accept_error_backoff = std::time::Duration::ZERO;
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
                            accept_error_backoff = next_accept_error_backoff(accept_error_backoff);
                            warn!(
                                "accept failed; retrying after {:?}: {e}",
                                accept_error_backoff
                            );
                            select! {
                                biased;
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep(accept_error_backoff) => {}
                            }
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
        let mut accept_error_backoff = std::time::Duration::ZERO;
        loop {
            select! {
                biased;

                res = listener.accept() => {
                    match res {
                        Ok((stream, remote_addr)) => {
                            accept_error_backoff = std::time::Duration::ZERO;
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
                            accept_error_backoff = next_accept_error_backoff(accept_error_backoff);
                            warn!(
                                "accept failed; retrying after {:?}: {e}",
                                accept_error_backoff
                            );
                            select! {
                                biased;
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep(accept_error_backoff) => {}
                            }
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
    let mut accept_error_backoff = std::time::Duration::ZERO;

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
                        accept_error_backoff = std::time::Duration::ZERO;
                        connections.spawn(run_managed_tcp_connection::<D, S, E>(
                            stream,
                            remote_addr,
                            cancel.child_token(),
                            dispatcher.clone(),
                            writer.clone(),
                            None,
                        ));
                    }
                    Ok(_) => break,
                    Err(error) => {
                        accept_error_backoff = next_accept_error_backoff(accept_error_backoff);
                        warn!(
                            "tcp accept failed; retrying after {:?}: {error}",
                            accept_error_backoff
                        );
                        select! {
                            biased;
                            _ = cancel.cancelled() => {
                                report.cancelled += 1;
                                break;
                            }
                            _ = tokio::time::sleep(accept_error_backoff) => {}
                        }
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

fn next_accept_error_backoff(current: std::time::Duration) -> std::time::Duration {
    if current.is_zero() {
        TCP_ACCEPT_ERROR_BACKOFF_MIN
    } else {
        current.saturating_mul(2).min(TCP_ACCEPT_ERROR_BACKOFF_MAX)
    }
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
    let mut receive_buf = vec![0u8; UDP_MAX_DATAGRAM_SIZE];
    loop {
        select! {
            biased;

            _ = cancel.cancelled() => {
                report.cancelled += 1;
                break;
            }

            received = udp_socket_read_owned_buf(&mut receive_buf, udp_socket.as_ref()) => {
                match received {
                    Ok((size, remote_addr)) if size != 0 => {
                        let packet = Bytes::copy_from_slice(&receive_buf[..size]);
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
    ready: Option<oneshot::Sender<()>>,
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
            let Some(registration) =
                writer.insert_registered_tcp_writer(remote_addr, tx, cancel.clone())
            else {
                return Err(packet_writer_closed());
            };
            if let Some(ready) = ready {
                let _ = ready.send(());
            }
            let (read_result, write_result) = tokio::join!(
                handle_tcp_read_owned_half::<D, S, E>(
                    read_half,
                    remote_addr,
                    cancel.clone(),
                    dispatcher,
                    S::default(),
                    writer.clone(),
                    registration,
                ),
                handle_tcp_write::<E>(write_half, remote_addr, cancel, rx, writer, registration,),
            );
            read_result?;
            write_result
        }
        None => {
            let Some(registration) =
                writer.insert_registered_direct_tcp_writer(remote_addr, write_half, cancel.clone())
            else {
                return Err(packet_writer_closed());
            };
            if let Some(ready) = ready {
                let _ = ready.send(());
            }
            handle_tcp_read_owned_half::<D, S, E>(
                read_half,
                remote_addr,
                cancel,
                dispatcher,
                S::default(),
                writer,
                registration,
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
    registration: TcpWriterRegistration,
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
    writer.remove_registered_tcp_writer(registration);
    dispatcher.close(remote_addr, Protocol::TCP)?;
    Ok(())
}

async fn handle_tcp_write<E>(
    mut stream: OwnedWriteHalf,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    mut rx: mpsc::Receiver<EncodedPacket>,
    writer: PacketWriter<E>,
    registration: TcpWriterRegistration,
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
    writer.remove_registered_tcp_writer(registration);
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
        let mut receive_buf = vec![0u8; UDP_MAX_DATAGRAM_SIZE];
        loop {
            select! {
                res = udp_socket_read_owned_buf(&mut receive_buf, &socket) => {
                    match res {
                        Ok((n,addr)) if n != 0 => {
                            let pkt = Bytes::copy_from_slice(&receive_buf[..n]);
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
    buf: &mut [u8],
    socket: &UdpSocket,
) -> GlobalResult<(usize, SocketAddr)> {
    socket
        .recv_from(buf)
        .await
        .hand_log(|msg| error!("read buf failed:{msg}"))
}

#[cfg(test)]
mod tests {
    use super::{
        handle_tcp_write, into_tokio_udp_socket, managed_rw_with_tcp_write_mode, ManagedCloseState,
        ManagedPacketIo, ManagedTaskReport, ManagedTcpConnectOptions, NetworkCloseReport,
        PacketDispatcher, PacketSplitter, PacketWriter, RawPacketEncoder, TcpWriteMode,
        TcpWriterRegistration, TCP_ACCEPT_ERROR_BACKOFF_MAX, TCP_ACCEPT_ERROR_BACKOFF_MIN,
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
        udp_bytes: AtomicUsize,
        tcp_packets: AtomicUsize,
        tcp_closes: AtomicUsize,
    }

    impl PacketDispatcher for TestDispatcher {
        fn dispatch_owned(
            &self,
            data: Bytes,
            _remote_addr: SocketAddr,
            protocol: Protocol,
        ) -> GlobalResult<()> {
            if protocol == Protocol::UDP {
                self.udp_packets.fetch_add(1, Ordering::Relaxed);
                self.udp_bytes.fetch_add(data.len(), Ordering::Relaxed);
            } else if protocol == Protocol::TCP {
                self.tcp_packets.fetch_add(1, Ordering::Relaxed);
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

    #[test]
    fn tcp_accept_error_backoff_is_bounded() {
        let first = super::next_accept_error_backoff(Duration::ZERO);
        assert_eq!(first, TCP_ACCEPT_ERROR_BACKOFF_MIN);
        assert_eq!(super::next_accept_error_backoff(first), first * 2);

        let mut backoff = first;
        for _ in 0..32 {
            backoff = super::next_accept_error_backoff(backoff);
        }
        assert_eq!(backoff, TCP_ACCEPT_ERROR_BACKOFF_MAX);
    }

    #[tokio::test]
    async fn tcp_writer_generation_and_same_ip_fallback_preserve_live_sinks() {
        let writer = PacketWriter::new(
            None,
            Arc::new(RawPacketEncoder),
            TcpWriteMode::Queued { queue_size: 4 },
        );
        let first_addr: SocketAddr = "127.0.0.1:31001".parse().unwrap();
        let second_addr: SocketAddr = "127.0.0.1:31002".parse().unwrap();
        let first_cancel = CancellationToken::new();
        let second_cancel = CancellationToken::new();
        let replacement_cancel = CancellationToken::new();
        let (first_tx, _first_rx) = mpsc::channel(1);
        let (second_tx, _second_rx) = mpsc::channel(1);
        let (replacement_tx, _replacement_rx) = mpsc::channel(1);

        let first = writer
            .insert_registered_tcp_writer(first_addr, first_tx, first_cancel.clone())
            .unwrap();
        let second = writer
            .insert_registered_tcp_writer(second_addr, second_tx, second_cancel.clone())
            .unwrap();
        assert_eq!(
            writer
                .tcp_sink_by_ip(first_addr.ip())
                .unwrap()
                .remote_addr(),
            second_addr
        );

        writer.remove_registered_tcp_writer(second);
        assert!(second_cancel.is_cancelled());
        assert_eq!(
            writer
                .tcp_sink_by_ip(first_addr.ip())
                .unwrap()
                .remote_addr(),
            first_addr
        );

        let replacement = writer
            .insert_registered_tcp_writer(first_addr, replacement_tx, replacement_cancel.clone())
            .unwrap();
        assert!(first_cancel.is_cancelled());
        writer.remove_registered_tcp_writer(first);
        assert!(writer.has_tcp_writer(&first_addr));
        assert!(!replacement_cancel.is_cancelled());

        writer.remove_registered_tcp_writer(replacement);
        assert!(!writer.has_tcp_writer(&first_addr));
        assert!(replacement_cancel.is_cancelled());
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

        handle_tcp_write(
            write_half,
            remote_addr,
            cancel.clone(),
            rx,
            writer,
            TcpWriterRegistration {
                remote_addr,
                writer_id: 1,
            },
        )
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
    async fn managed_udp_receiver_preserves_large_datagram() {
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let local_addr = udp.local_addr().unwrap();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-large-udp-{local_addr}"),
                (None, Some(udp)),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = vec![0x5a; 60_000];

        assert_eq!(
            sender.send_to(&payload, local_addr).await.unwrap(),
            payload.len()
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while dispatcher.udp_packets.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(dispatcher.udp_packets.load(Ordering::Relaxed), 1);
        assert_eq!(dispatcher.udp_bytes.load(Ordering::Relaxed), payload.len());
        managed.close_and_wait().await.unwrap();
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
            connecting_tcp_peers: std::sync::Mutex::new(std::collections::HashSet::new()),
            active_connections: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            completed_connection_report: Arc::new(std::sync::Mutex::new(
                NetworkCloseReport::default(),
            )),
            next_connection_id: std::sync::atomic::AtomicU64::new(1),
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
    async fn managed_active_tcp_is_ready_for_exact_read_write_and_local_bind() {
        for (name, write_mode) in [
            ("queued", TcpWriteMode::Queued { queue_size: 4 }),
            ("direct", TcpWriteMode::Direct),
        ] {
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let remote_addr = server.local_addr().unwrap();
            let local_reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let local_addr = local_reservation.local_addr().unwrap();
            drop(local_reservation);
            let dispatcher = Arc::new(TestDispatcher::default());
            let managed =
                managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                    &GlobalRuntime::get_main_runtime(),
                    format!("managed-active-connect-{name}-{remote_addr}"),
                    (None, None),
                    CancellationToken::new(),
                    dispatcher.clone(),
                    Arc::new(RawPacketEncoder),
                    write_mode,
                )
                .unwrap();

            let connection = managed
                .connect_tcp::<TestDispatcher, DrainSplitter>(
                    &GlobalRuntime::get_main_runtime(),
                    format!("managed-active-peer-{name}-{remote_addr}"),
                    ManagedTcpConnectOptions {
                        remote_addr,
                        local_addr: Some(local_addr),
                        timeout: Duration::from_secs(1),
                    },
                    dispatcher.clone(),
                )
                .await
                .unwrap();
            let (mut peer, peer_addr) = server.accept().await.unwrap();
            assert_eq!(connection.local_addr(), local_addr);
            assert_eq!(connection.remote_addr(), remote_addr);
            assert_eq!(peer_addr, local_addr);
            assert!(managed.writer().has_tcp_writer(&remote_addr));
            assert!(managed
                .writer()
                .wait_tcp_sink(remote_addr, Duration::from_secs(1))
                .await
                .is_ok());

            connection
                .write(Bytes::from_static(b"outbound"))
                .await
                .unwrap();
            let mut outbound = [0u8; 8];
            peer.read_exact(&mut outbound).await.unwrap();
            assert_eq!(&outbound, b"outbound");

            peer.write_all(b"inbound").await.unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while dispatcher.tcp_packets.load(Ordering::Relaxed) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();

            let first = connection.close_and_wait().await.unwrap();
            let second = connection.close_and_wait().await.unwrap();
            assert_eq!(first, second);
            assert!(first.is_complete());
            assert!(super::lock_unpoisoned(&managed.active_connections).is_empty());
            assert!(!managed.writer().has_tcp_writer(&remote_addr));
            assert_eq!(dispatcher.tcp_closes.load(Ordering::Relaxed), 1);
            assert!(managed.close_and_wait().await.unwrap().is_complete());
        }
    }

    #[tokio::test]
    async fn managed_active_tcp_rejects_duplicate_peer_and_refused_connection() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = server.local_addr().unwrap();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-duplicate-{remote_addr}"),
                (None, None),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let options = ManagedTcpConnectOptions {
            remote_addr,
            local_addr: None,
            timeout: Duration::from_secs(1),
        };
        let connection = managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-duplicate-peer-{remote_addr}"),
                options,
                dispatcher.clone(),
            )
            .await
            .unwrap();
        let (_peer, _) = server.accept().await.unwrap();
        assert!(managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-duplicate-rejected",
                options,
                dispatcher.clone(),
            )
            .await
            .is_err());
        connection.close_and_wait().await.unwrap();
        managed.close_and_wait().await.unwrap();

        let refused_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused_addr = refused_listener.local_addr().unwrap();
        drop(refused_listener);
        let refused =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-refused-{refused_addr}"),
                (None, None),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        assert!(refused
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-refused-peer",
                ManagedTcpConnectOptions {
                    remote_addr: refused_addr,
                    local_addr: None,
                    timeout: Duration::from_secs(1),
                },
                dispatcher,
            )
            .await
            .is_err());
        refused.close_and_wait().await.unwrap();
    }

    #[tokio::test]
    async fn managed_tcp_connect_reservations_are_scoped_per_remote_peer() {
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                "managed-connect-reservation-scope",
                (None, None),
                CancellationToken::new(),
                dispatcher,
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let first: SocketAddr = "127.0.0.1:32001".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:32002".parse().unwrap();

        let first_reservation = managed.reserve_tcp_connect(first).unwrap();
        assert!(managed.reserve_tcp_connect(first).is_err());
        let second_reservation = managed.reserve_tcp_connect(second).unwrap();
        assert_eq!(
            super::lock_unpoisoned(&managed.connecting_tcp_peers).len(),
            2
        );

        drop((first_reservation, second_reservation));
        assert!(super::lock_unpoisoned(&managed.connecting_tcp_peers).is_empty());
        managed.close_and_wait().await.unwrap();
    }

    #[tokio::test]
    async fn managed_active_tcp_closed_handles_do_not_accumulate() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = server.local_addr().unwrap();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-reconnect-{remote_addr}"),
                (None, None),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();

        for attempt in 0..16 {
            let connection = managed
                .connect_tcp::<TestDispatcher, DrainSplitter>(
                    &GlobalRuntime::get_main_runtime(),
                    format!("managed-active-reconnect-{remote_addr}-{attempt}"),
                    ManagedTcpConnectOptions {
                        remote_addr,
                        local_addr: None,
                        timeout: Duration::from_secs(1),
                    },
                    dispatcher.clone(),
                )
                .await
                .unwrap();
            let (peer, _) = server.accept().await.unwrap();

            connection.close_and_wait().await.unwrap();
            assert!(super::lock_unpoisoned(&managed.active_connections).is_empty());
            drop(peer);
        }

        let report = managed.close_and_wait().await.unwrap();
        assert!(report.is_complete());
        assert_eq!(report.completed, 17);
    }

    #[tokio::test]
    async fn managed_active_tcp_reaps_naturally_finished_handle_before_reconnect() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = server.local_addr().unwrap();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-natural-reap-{remote_addr}"),
                (None, None),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let options = ManagedTcpConnectOptions {
            remote_addr,
            local_addr: None,
            timeout: Duration::from_secs(1),
        };

        let first = managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-natural-reap-first",
                options,
                dispatcher.clone(),
            )
            .await
            .unwrap();
        let (first_peer, _) = server.accept().await.unwrap();
        drop(first_peer);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !first.task_is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(super::lock_unpoisoned(&managed.active_connections).len(), 1);

        let second = managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-natural-reap-second",
                options,
                dispatcher,
            )
            .await
            .unwrap();
        let (second_peer, _) = server.accept().await.unwrap();
        assert_eq!(super::lock_unpoisoned(&managed.active_connections).len(), 1);
        assert!(first.close_and_wait().await.unwrap().is_complete());

        second.close_and_wait().await.unwrap();
        drop(second_peer);
        assert!(super::lock_unpoisoned(&managed.active_connections).is_empty());
        assert!(managed.close_and_wait().await.unwrap().is_complete());
    }

    #[tokio::test]
    async fn managed_active_tcp_honors_pre_cancel_and_timeout_validation() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-pre-cancelled",
                (None, None),
                cancel,
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let remote_addr = "127.0.0.1:9".parse().unwrap();
        assert!(managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-pre-cancelled-peer",
                ManagedTcpConnectOptions {
                    remote_addr,
                    local_addr: None,
                    timeout: Duration::from_secs(1),
                },
                dispatcher.clone(),
            )
            .await
            .is_err());
        assert!(managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                "managed-active-zero-timeout",
                ManagedTcpConnectOptions {
                    remote_addr,
                    local_addr: None,
                    timeout: Duration::ZERO,
                },
                dispatcher,
            )
            .await
            .is_err());
        managed.close_and_wait().await.unwrap();
    }

    #[tokio::test]
    async fn packet_writer_waits_for_exact_peer_and_times_out_without_one() {
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                "managed-wait-exact-peer",
                (
                    Some(std::net::TcpListener::bind("127.0.0.1:0").unwrap()),
                    None,
                ),
                CancellationToken::new(),
                dispatcher,
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let absent = "127.0.0.1:9".parse().unwrap();
        assert!(managed
            .writer()
            .wait_tcp_sink(absent, Duration::from_millis(10))
            .await
            .is_err());
        managed.close_and_wait().await.unwrap();
    }

    #[tokio::test]
    async fn managed_endpoint_close_waits_for_active_tcp_connection() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = server.local_addr().unwrap();
        let dispatcher = Arc::new(TestDispatcher::default());
        let managed =
            managed_rw_with_tcp_write_mode::<TestDispatcher, DrainSplitter, RawPacketEncoder>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-owner-{remote_addr}"),
                (None, None),
                CancellationToken::new(),
                dispatcher.clone(),
                Arc::new(RawPacketEncoder),
                TcpWriteMode::Direct,
            )
            .unwrap();
        let connection = managed
            .connect_tcp::<TestDispatcher, DrainSplitter>(
                &GlobalRuntime::get_main_runtime(),
                format!("managed-active-owned-peer-{remote_addr}"),
                ManagedTcpConnectOptions {
                    remote_addr,
                    local_addr: None,
                    timeout: Duration::from_secs(1),
                },
                dispatcher.clone(),
            )
            .await
            .unwrap();
        let (mut peer, _) = server.accept().await.unwrap();

        let report = managed.close_and_wait().await.unwrap();
        assert!(report.is_complete());
        assert!(report.completed >= 2, "{report:?}");
        assert_eq!(connection.close_and_wait().await.unwrap().completed, 1);
        assert_eq!(dispatcher.tcp_closes.load(Ordering::Relaxed), 1);
        let mut byte = [0u8; 1];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
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
