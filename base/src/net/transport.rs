use bytes::{Bytes, BytesMut};
use std::{
    error::Error,
    fmt::{Display, Formatter},
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
};

/// The transport mechanism. Payload codecs such as RTP/JPEG/WebP are intentionally
/// outside this type: a transport only reports what delivery semantics it provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Udp,
    Tcp,
    UnixDatagram,
    UnixStream,
    NamedPipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    Datagram,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportCapabilities {
    pub reliable: bool,
    pub ordered: bool,
    pub preserves_message_boundary: bool,
    pub encrypted: bool,
    pub congestion_controlled: bool,
    pub local_only: bool,
    pub max_message_size: usize,
    pub mode: TransportMode,
}

impl TransportCapabilities {
    pub fn for_kind(kind: TransportKind, max_message_size: usize) -> Self {
        match kind {
            TransportKind::Udp => Self {
                reliable: false,
                ordered: false,
                preserves_message_boundary: true,
                encrypted: false,
                congestion_controlled: false,
                local_only: false,
                max_message_size,
                mode: TransportMode::Datagram,
            },
            TransportKind::Tcp => Self {
                reliable: true,
                ordered: true,
                preserves_message_boundary: false,
                encrypted: false,
                congestion_controlled: true,
                local_only: false,
                max_message_size,
                mode: TransportMode::Stream,
            },
            TransportKind::UnixDatagram => Self {
                reliable: true,
                ordered: true,
                preserves_message_boundary: true,
                encrypted: false,
                congestion_controlled: false,
                local_only: true,
                max_message_size,
                mode: TransportMode::Datagram,
            },
            TransportKind::UnixStream => Self {
                reliable: true,
                ordered: true,
                preserves_message_boundary: false,
                encrypted: false,
                congestion_controlled: true,
                local_only: true,
                max_message_size,
                mode: TransportMode::Stream,
            },
            TransportKind::NamedPipe => Self {
                reliable: true,
                ordered: true,
                preserves_message_boundary: false,
                encrypted: false,
                congestion_controlled: true,
                local_only: true,
                max_message_size,
                mode: TransportMode::Stream,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportAddress {
    Inet(SocketAddr),
    Unix(PathBuf),
    NamedPipe(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportEndpoint {
    pub kind: TransportKind,
    pub address: TransportAddress,
    pub capabilities: TransportCapabilities,
}

impl TransportEndpoint {
    pub fn new(
        kind: TransportKind,
        address: TransportAddress,
        max_message_size: usize,
    ) -> TransportResult<Self> {
        let valid = matches!(
            (kind, &address),
            (
                TransportKind::Udp | TransportKind::Tcp,
                TransportAddress::Inet(_)
            ) | (
                TransportKind::UnixDatagram | TransportKind::UnixStream,
                TransportAddress::Unix(_)
            ) | (TransportKind::NamedPipe, TransportAddress::NamedPipe(_))
        );
        if !valid {
            return Err(TransportError::new(
                TransportErrorKind::InvalidEndpoint,
                "transport kind does not match endpoint address",
            ));
        }
        if max_message_size == 0 {
            return Err(TransportError::new(
                TransportErrorKind::InvalidConfiguration,
                "max_message_size must be greater than zero",
            ));
        }
        Ok(Self {
            kind,
            address,
            capabilities: TransportCapabilities::for_kind(kind, max_message_size),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    Unsupported,
    InvalidConfiguration,
    InvalidEndpoint,
    PermissionDenied,
    AddressInUse,
    MessageTooLarge,
    QueueFull,
    Closed,
    PeerClosed,
    Timeout,
    Io,
    Protocol,
    Join,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    kind: TransportErrorKind,
    message: String,
}

impl TransportError {
    pub fn new(kind: TransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> TransportErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn from_io(context: &str, error: &std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::PermissionDenied => TransportErrorKind::PermissionDenied,
            std::io::ErrorKind::AddrInUse => TransportErrorKind::AddressInUse,
            std::io::ErrorKind::TimedOut => TransportErrorKind::Timeout,
            _ => TransportErrorKind::Io,
        };
        Self::new(kind, format!("{context}: {error}"))
    }
}

impl Display for TransportError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl Error for TransportError {}

pub type TransportResult<T> = Result<T, TransportError>;
pub type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = TransportResult<T>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportMessage {
    pub payload: Bytes,
    pub peer: Option<TransportAddress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransportCloseReport {
    pub already_closed: bool,
    pub root_task_joined: bool,
    pub endpoint_removed: bool,
}

/// A complete-message transport contract shared by stream and datagram adapters.
/// Stream implementations must add framing so callers never observe partial bytes.
pub trait MessageTransport: Send + Sync {
    fn endpoint(&self) -> &TransportEndpoint;
    fn try_send(&self, payload: Bytes) -> TransportResult<()>;
    fn send<'a>(&'a self, payload: Bytes) -> TransportFuture<'a, ()>;
    fn receive<'a>(&'a self) -> TransportFuture<'a, TransportMessage>;
    fn close(&self);
    fn close_and_wait<'a>(&'a self) -> TransportFuture<'a, TransportCloseReport>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthPrefix {
    U16Be,
    U32Be,
}

impl LengthPrefix {
    fn encoded_len(self) -> usize {
        match self {
            Self::U16Be => 2,
            Self::U32Be => 4,
        }
    }

    fn maximum(self) -> usize {
        match self {
            Self::U16Be => u16::MAX as usize,
            Self::U32Be => u32::MAX as usize,
        }
    }
}

/// Stateful length-delimited framing for reliable byte streams.
#[derive(Debug, Clone)]
pub struct LengthDelimitedCodec {
    prefix: LengthPrefix,
    max_message_size: usize,
    buffered: BytesMut,
}

impl LengthDelimitedCodec {
    pub fn new(prefix: LengthPrefix, max_message_size: usize) -> TransportResult<Self> {
        if max_message_size == 0 || max_message_size > prefix.maximum() {
            return Err(TransportError::new(
                TransportErrorKind::InvalidConfiguration,
                "max_message_size is outside the selected length-prefix range",
            ));
        }
        Ok(Self {
            prefix,
            max_message_size,
            buffered: BytesMut::new(),
        })
    }

    pub fn max_message_size(&self) -> usize {
        self.max_message_size
    }

    pub fn encode(&self, payload: &[u8]) -> TransportResult<Bytes> {
        if payload.len() > self.max_message_size {
            return Err(TransportError::new(
                TransportErrorKind::MessageTooLarge,
                format!(
                    "message size {} exceeds configured maximum {}",
                    payload.len(),
                    self.max_message_size
                ),
            ));
        }

        let mut encoded = BytesMut::with_capacity(self.prefix.encoded_len() + payload.len());
        match self.prefix {
            LengthPrefix::U16Be => encoded.extend_from_slice(&(payload.len() as u16).to_be_bytes()),
            LengthPrefix::U32Be => encoded.extend_from_slice(&(payload.len() as u32).to_be_bytes()),
        }
        encoded.extend_from_slice(payload);
        Ok(encoded.freeze())
    }

    pub fn push(&mut self, chunk: &[u8]) -> TransportResult<Vec<Bytes>> {
        self.buffered.extend_from_slice(chunk);
        let mut messages = Vec::new();

        loop {
            let prefix_len = self.prefix.encoded_len();
            if self.buffered.len() < prefix_len {
                break;
            }
            let message_len = match self.prefix {
                LengthPrefix::U16Be => {
                    u16::from_be_bytes([self.buffered[0], self.buffered[1]]) as usize
                }
                LengthPrefix::U32Be => u32::from_be_bytes([
                    self.buffered[0],
                    self.buffered[1],
                    self.buffered[2],
                    self.buffered[3],
                ]) as usize,
            };
            if message_len > self.max_message_size {
                return Err(TransportError::new(
                    TransportErrorKind::MessageTooLarge,
                    format!(
                        "peer message size {message_len} exceeds configured maximum {}",
                        self.max_message_size
                    ),
                ));
            }
            if self.buffered.len() < prefix_len + message_len {
                break;
            }
            let _ = self.buffered.split_to(prefix_len);
            messages.push(self.buffered.split_to(message_len).freeze());
        }

        if self.buffered.len() > self.max_message_size + self.prefix.encoded_len() {
            return Err(TransportError::new(
                TransportErrorKind::Protocol,
                "framing buffer exceeded its configured bound",
            ));
        }
        Ok(messages)
    }

    pub fn finish(self) -> TransportResult<()> {
        if self.buffered.is_empty() {
            Ok(())
        } else {
            Err(TransportError::new(
                TransportErrorKind::Protocol,
                "stream ended with a partial framed message",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_report_real_transport_semantics() {
        let udp = TransportCapabilities::for_kind(TransportKind::Udp, 1200);
        assert_eq!(udp.mode, TransportMode::Datagram);
        assert!(udp.preserves_message_boundary);
        assert!(!udp.reliable);
        assert!(!udp.local_only);

        let uds = TransportCapabilities::for_kind(TransportKind::UnixStream, 4096);
        assert_eq!(uds.mode, TransportMode::Stream);
        assert!(uds.reliable);
        assert!(uds.ordered);
        assert!(uds.local_only);
        assert!(!uds.preserves_message_boundary);

        let pipe = TransportCapabilities::for_kind(TransportKind::NamedPipe, 4096);
        assert_eq!(pipe.mode, TransportMode::Stream);
        assert!(pipe.reliable);
        assert!(pipe.ordered);
        assert!(pipe.local_only);
        assert!(!pipe.preserves_message_boundary);
    }

    #[test]
    fn length_delimited_codec_handles_fragmented_and_batched_input() {
        let encoder = LengthDelimitedCodec::new(LengthPrefix::U32Be, 1024).unwrap();
        let first = encoder.encode(b"first").unwrap();
        let second = encoder.encode(b"second").unwrap();
        let mut wire = Vec::from(first.as_ref());
        wire.extend_from_slice(&second);

        let mut decoder = LengthDelimitedCodec::new(LengthPrefix::U32Be, 1024).unwrap();
        assert!(decoder.push(&wire[..3]).unwrap().is_empty());
        let messages = decoder.push(&wire[3..]).unwrap();
        assert_eq!(
            messages,
            vec![Bytes::from_static(b"first"), Bytes::from_static(b"second")]
        );
        decoder.finish().unwrap();
    }

    #[test]
    fn length_delimited_codec_rejects_oversized_messages() {
        let codec = LengthDelimitedCodec::new(LengthPrefix::U16Be, 4).unwrap();
        let error = codec.encode(b"12345").unwrap_err();
        assert_eq!(error.kind(), TransportErrorKind::MessageTooLarge);
    }
}
