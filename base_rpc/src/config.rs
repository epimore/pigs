use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RpcServerTlsConfig {
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub client_ca_pem: Option<Vec<u8>>,
    pub client_auth_optional: bool,
    pub handshake_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct RpcClientTlsConfig {
    pub domain_name: Option<String>,
    pub ca_certificate_pem: Option<Vec<u8>>,
    pub client_certificate_pem: Option<Vec<u8>>,
    pub client_private_key_pem: Option<Vec<u8>>,
    pub use_native_roots: bool,
    pub handshake_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct RpcServerConfig {
    pub concurrency_limit_per_connection: usize,
    pub tcp_keepalive: Option<Duration>,
    pub tcp_keepalive_interval: Option<Duration>,
    pub tcp_keepalive_retries: Option<u32>,
    pub http2_keepalive_interval: Option<Duration>,
    pub http2_keepalive_timeout: Option<Duration>,
    pub tls: Option<RpcServerTlsConfig>,
}

impl Default for RpcServerConfig {
    fn default() -> Self {
        Self {
            concurrency_limit_per_connection: 256,
            tcp_keepalive: Some(Duration::from_secs(60)),
            tcp_keepalive_interval: Some(Duration::from_secs(30)),
            tcp_keepalive_retries: Some(3),
            http2_keepalive_interval: Some(Duration::from_secs(30)),
            http2_keepalive_timeout: Some(Duration::from_secs(10)),
            tls: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RpcChannelConfig {
    pub endpoint: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub tcp_keepalive: Option<Duration>,
    pub tcp_keepalive_interval: Option<Duration>,
    pub tcp_keepalive_retries: Option<u32>,
    pub http2_keepalive_interval: Duration,
    pub http2_keepalive_timeout: Duration,
    pub keep_alive_while_idle: bool,
    pub tls: Option<RpcClientTlsConfig>,
}

impl RpcChannelConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            tcp_keepalive: Some(Duration::from_secs(60)),
            tcp_keepalive_interval: Some(Duration::from_secs(30)),
            tcp_keepalive_retries: Some(3),
            http2_keepalive_interval: Duration::from_secs(30),
            http2_keepalive_timeout: Duration::from_secs(10),
            keep_alive_while_idle: true,
            tls: None,
        }
    }
}
