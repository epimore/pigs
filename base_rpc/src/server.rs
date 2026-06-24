use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

use crate::config::{RpcServerConfig, RpcServerTlsConfig};
use crate::error::RpcError;

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
