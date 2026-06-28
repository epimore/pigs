use std::fs;
use std::net::TcpListener as StdTcpListener;

use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use crate::config::{ClientAuthMode, RpcServerConfig, RpcServerTlsConfig, TlsFileConfig};
use crate::error::RpcError;

pub fn tcp_incoming_from_std(
    listener: StdTcpListener,
) -> Result<tonic::transport::server::TcpIncoming, RpcError> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    Ok(tonic::transport::server::TcpIncoming::from(listener))
}

pub fn load_server_tls_from_files(config: &TlsFileConfig) -> Result<RpcServerTlsConfig, RpcError> {
    let certificate_path = config.certificate_path.as_ref().ok_or_else(|| {
        RpcError::InvalidEndpoint("server TLS certificate_path is required".to_string())
    })?;
    let private_key_path = config.private_key_path.as_ref().ok_or_else(|| {
        RpcError::InvalidEndpoint("server TLS private_key_path is required".to_string())
    })?;
    Ok(RpcServerTlsConfig {
        certificate_pem: fs::read(certificate_path)?,
        private_key_pem: fs::read(private_key_path)?,
        client_ca_pem: read_optional(&config.ca_certificate_path)?,
        client_auth_optional: config.client_auth == ClientAuthMode::Optional,
        handshake_timeout: config.handshake_timeout,
    })
}

fn read_optional(path: &Option<std::path::PathBuf>) -> Result<Option<Vec<u8>>, RpcError> {
    path.as_ref()
        .map(fs::read)
        .transpose()
        .map_err(RpcError::from)
}

pub fn build_server(config: &RpcServerConfig) -> Result<Server, RpcError> {
    let mut server = Server::builder()
        .concurrency_limit_per_connection(config.concurrency_limit_per_connection)
        .tcp_keepalive(config.tcp_keepalive)
        .tcp_keepalive_interval(config.tcp_keepalive_interval)
        .tcp_keepalive_retries(config.tcp_keepalive_retries)
        .http2_keepalive_interval(config.http2_keepalive_interval)
        .http2_keepalive_timeout(config.http2_keepalive_timeout);

    if let Some(tls) = &config.tls {
        server = server.tls_config(build_server_tls(tls))?;
    }

    Ok(server)
}

fn build_server_tls(config: &RpcServerTlsConfig) -> ServerTlsConfig {
    let identity = Identity::from_pem(
        config.certificate_pem.clone(),
        config.private_key_pem.clone(),
    );
    let mut tls = ServerTlsConfig::new()
        .identity(identity)
        .timeout(config.handshake_timeout);
    if let Some(client_ca) = &config.client_ca_pem {
        tls = tls
            .client_ca_root(Certificate::from_pem(client_ca.clone()))
            .client_auth_optional(config.client_auth_optional);
    }
    tls
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn builds_plain_server() {
        build_server(&RpcServerConfig::default()).unwrap();
    }

    #[test]
    fn builds_tls_and_optional_mtls_servers() {
        let certificate = include_bytes!("../tests/fixtures/server.pem").to_vec();
        let private_key = include_bytes!("../tests/fixtures/server-key.pem").to_vec();
        let mut config = RpcServerConfig {
            tls: Some(RpcServerTlsConfig {
                certificate_pem: certificate.clone(),
                private_key_pem: private_key.clone(),
                client_ca_pem: None,
                client_auth_optional: false,
                handshake_timeout: Duration::from_secs(5),
            }),
            ..RpcServerConfig::default()
        };
        build_server(&config).unwrap();
        config.tls = Some(RpcServerTlsConfig {
            certificate_pem: certificate.clone(),
            private_key_pem: private_key,
            client_ca_pem: Some(certificate),
            client_auth_optional: true,
            handshake_timeout: Duration::from_secs(5),
        });
        build_server(&config).unwrap();
    }
}
