#![warn(unsafe_code)]

pub mod channel;
pub mod config;
pub mod error;
pub mod interceptor;
pub mod retry;
pub mod server;
pub mod stream_supervisor;

pub use channel::connect_channel;
pub use config::{RpcChannelConfig, RpcClientTlsConfig, RpcServerConfig, RpcServerTlsConfig};
pub use error::{RpcError, status_from_global_error};
pub use interceptor::{ClientMetadataInterceptor, RpcMetadata, extract_rpc_metadata};
pub use retry::RetryPolicy;
pub use server::build_server;
pub use stream_supervisor::{
    BoundedQueue, ConnectionReporter, ConnectionState, StreamConnector, StreamSupervisor,
    StreamSupervisorConfig, StreamSupervisorHandle,
};
