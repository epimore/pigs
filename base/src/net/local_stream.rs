#[cfg(windows)]
pub use super::named_pipe::{
    ManagedNamedPipeListener as ManagedLocalStreamListener,
    ManagedNamedPipeStream as ManagedLocalStream, NamedPipeTransportConfig as LocalStreamConfig,
};
#[cfg(unix)]
pub use super::uds::{
    ManagedUnixStream as ManagedLocalStream,
    ManagedUnixStreamListener as ManagedLocalStreamListener,
    UnixTransportConfig as LocalStreamConfig,
};
