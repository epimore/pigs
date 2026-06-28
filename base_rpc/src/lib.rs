#![warn(unsafe_code)]

pub mod channel;
pub mod config;
pub mod error;
pub mod interceptor;
pub mod retry;
pub mod server;
pub mod stream_supervisor;

pub use channel::{connect_channel, load_client_tls_from_files, rpc_endpoint_uri, rpc_scheme};
pub use config::{
    ClientAuthMode, RpcChannelConfig, RpcClientTlsConfig, RpcServerConfig, RpcServerTlsConfig,
    TlsFileConfig,
};
pub use error::{RpcError, status_from_global_error};
pub use interceptor::{ClientMetadataInterceptor, RpcMetadata, extract_rpc_metadata};
pub use retry::RetryPolicy;
pub use server::{build_server, load_server_tls_from_files, tcp_incoming_from_std};
pub use stream_supervisor::{
    BoundedQueue, ConnectionReporter, ConnectionState, StreamConnector, StreamSupervisor,
    StreamSupervisorConfig, StreamSupervisorHandle,
};
