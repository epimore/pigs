use crate::net::state::Protocol;
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
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
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

#[derive(Clone)]
struct TcpWriterHandle {
    sender: mpsc::Sender<EncodedPacket>,
    cancel: CancellationToken,
}

pub struct PacketWriter<E = RawPacketEncoder>
where
    E: PacketEncoder,
{
    udp_socket: Option<Arc<UdpSocket>>,
    tcp_writers: Arc<DashMap<SocketAddr, TcpWriterHandle>>,
    tcp_writer_addrs_by_ip: Arc<DashMap<IpAddr, SocketAddr>>,
    encoder: Arc<E>,
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
        }
    }
}

impl<E> PacketWriter<E>
where
    E: PacketEncoder,
{
    fn new(udp_socket: Option<Arc<UdpSocket>>, encoder: Arc<E>) -> Self {
        Self {
            udp_socket,
            tcp_writers: Arc::new(DashMap::new()),
            tcp_writer_addrs_by_ip: Arc::new(DashMap::new()),
            encoder,
        }
    }

    pub async fn write_to(
        &self,
        data: Bytes,
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()> {
        match protocol {
            Protocol::UDP => {
                let packet = self.encoder.encode_udp(data)?;
                let socket = self.udp_socket.as_ref().ok_or_else(|| {
                    GlobalError::new_sys_error("udp socket is not available", |msg| error!("{msg}"))
                })?;
                socket
                    .send_to(packet.as_ref(), remote_addr)
                    .await
                    .hand_log(|msg| error!("{msg}: remote_addr={remote_addr}"))?;
                Ok(())
            }
            Protocol::TCP => {
                let packet = self.encoder.encode_tcp(data)?;
                let sender = self
                    .tcp_writers
                    .get(&remote_addr)
                    .map(|handle| handle.value().sender.clone())
                    .ok_or_else(|| {
                        GlobalError::new_sys_error("tcp writer is not available", |msg| {
                            error!("{msg}: remote_addr={remote_addr}")
                        })
                    })?;
                sender.send(packet).await.map_err(|_| {
                    GlobalError::new_sys_error("tcp write channel closed", |msg| {
                        error!("{msg}: remote_addr={remote_addr}")
                    })
                })?;
                Ok(())
            }
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
        match protocol {
            Protocol::UDP => {
                let socket = self.udp_socket.as_ref().ok_or_else(|| {
                    GlobalError::new_sys_error("udp socket is not available", |msg| error!("{msg}"))
                })?;
                socket
                    .send_to(data, remote_addr)
                    .await
                    .hand_log(|msg| error!("{msg}: remote_addr={remote_addr}"))?;
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
        let packet = self.encoder.encode_tcp(data)?;
        let remote_addr = self
            .tcp_writer_addrs_by_ip
            .get(&remote_ip)
            .map(|item| *item.value())
            .ok_or_else(|| {
                GlobalError::new_sys_error("tcp writer is not available", |msg| {
                    error!("{msg}: remote_ip={remote_ip}")
                })
            })?;
        let sender = self
            .tcp_writers
            .get(&remote_addr)
            .map(|handle| handle.value().sender.clone())
            .ok_or_else(|| {
                GlobalError::new_sys_error("tcp writer is not available", |msg| {
                    error!("{msg}: remote_addr={remote_addr}")
                })
            })?;
        sender.send(packet).await.map_err(|_| {
            GlobalError::new_sys_error("tcp write channel closed", |msg| {
                error!("{msg}: remote_ip={remote_ip}")
            })
        })?;
        Ok(())
    }

    pub fn try_write_to(
        &self,
        data: Bytes,
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()> {
        match protocol {
            Protocol::UDP => {
                let packet = self.encoder.encode_udp(data)?;
                let socket = self.udp_socket.as_ref().ok_or_else(|| {
                    GlobalError::new_sys_error("udp socket is not available", |msg| error!("{msg}"))
                })?;
                socket
                    .try_send_to(packet.as_ref(), remote_addr)
                    .hand_log(|msg| error!("{msg}: remote_addr={remote_addr}"))?;
                Ok(())
            }
            Protocol::TCP => {
                let packet = self.encoder.encode_tcp(data)?;
                let sender = self
                    .tcp_writers
                    .get(&remote_addr)
                    .map(|handle| handle.value().sender.clone())
                    .ok_or_else(|| {
                        GlobalError::new_sys_error("tcp writer is not available", |msg| {
                            error!("{msg}: remote_addr={remote_addr}")
                        })
                    })?;
                match sender.try_send(packet) {
                    Ok(_) => Ok(()),
                    Err(TrySendError::Full(_)) => Err(GlobalError::new_sys_error(
                        "tcp write channel is full",
                        |msg| error!("{msg}: remote_addr={remote_addr}"),
                    )),
                    Err(TrySendError::Closed(_)) => Err(GlobalError::new_sys_error(
                        "tcp write channel closed",
                        |msg| error!("{msg}: remote_addr={remote_addr}"),
                    )),
                }
            }
            Protocol::ALL => Err(GlobalError::new_sys_error(
                "protocol ALL cannot be used to write a packet",
                |msg| error!("{msg}"),
            )),
        }
    }

    pub fn insert_tcp_writer(
        &self,
        remote_addr: SocketAddr,
        sender: mpsc::Sender<EncodedPacket>,
        cancel: CancellationToken,
    ) {
        self.tcp_writer_addrs_by_ip
            .insert(remote_addr.ip(), remote_addr);
        self.tcp_writers
            .insert(remote_addr, TcpWriterHandle { sender, cancel });
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
            handle.cancel.cancel();
        }
    }

    pub fn has_tcp_writer(&self, remote_addr: &SocketAddr) -> bool {
        self.tcp_writers.contains_key(remote_addr)
    }
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
    let (tcp, udp) = tu;
    let udp_socket = match udp {
        Some(udp) => Some(into_tokio_udp_socket(udp)?),
        None => None,
    };
    let writer = PacketWriter::new(udp_socket.clone(), encoder);
    if let Some(tcp) = tcp {
        spawn_tcp_rw::<D, S, E>(tcp, cancel.clone(), dispatcher.clone(), writer.clone())?;
    }
    if let Some(udp_socket) = udp_socket {
        spawn_udp_rw(udp_socket, cancel, dispatcher)?;
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

fn spawn_tcp_rw<D, S, E>(
    tcp: std::net::TcpListener,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    writer: PacketWriter<E>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
    E: PacketEncoder,
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
                            let conn_cancel = cancel.child_token();
                            let writer = writer.clone();
                            let (read_half, write_half) = stream.into_split();
                            let (tx, rx) = mpsc::channel(TCP_WRITE_QUEUE_SIZE);
                            writer.insert_tcp_writer(remote_addr, tx, conn_cancel.clone());

                            tokio::spawn(handle_tcp_read_owned_half::<D, S, E>(
                                read_half,
                                remote_addr,
                                conn_cancel.clone(),
                                dispatcher,
                                S::default(),
                                writer.clone(),
                            ));
                            tokio::spawn(handle_tcp_write::<E>(
                                write_half,
                                remote_addr,
                                conn_cancel,
                                rx,
                                writer,
                            ));
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
) where
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
    writer.remove_tcp_writer(&remote_addr);
    let _ = stream.shutdown().await;
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

fn spawn_udp_rw<D>(
    socket: Arc<UdpSocket>,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
{
    tokio::spawn(async move {
        loop {
            let mut buf = BytesMut::with_capacity(UDP_RECV_BUF_SIZE);
            select! {
                res = udp_socket_read_owned_buf(&mut buf, socket.as_ref()) => {
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
}
