use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::config::{RpcChannelConfig, RpcClientTlsConfig};
use crate::error::RpcError;

pub async fn connect_channel(config: &RpcChannelConfig) -> Result<Channel, RpcError> {
    let mut endpoint = Endpoint::from_shared(config.endpoint.clone())
        .map_err(|error| RpcError::InvalidEndpoint(error.to_string()))?
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout)
        .tcp_keepalive(config.tcp_keepalive)
        .tcp_keepalive_interval(config.tcp_keepalive_interval)
        .tcp_keepalive_retries(config.tcp_keepalive_retries)
        .http2_keep_alive_interval(config.http2_keepalive_interval)
        .keep_alive_timeout(config.http2_keepalive_timeout)
        .keep_alive_while_idle(config.keep_alive_while_idle);

    if let Some(tls) = &config.tls {
        endpoint = endpoint.tls_config(build_client_tls(tls))?;
    }

    endpoint.connect().await.map_err(RpcError::from)
}

fn build_client_tls(config: &RpcClientTlsConfig) -> ClientTlsConfig {
    let mut tls = ClientTlsConfig::new().timeout(config.handshake_timeout);
    if config.use_native_roots {
        tls = tls.with_native_roots();
    }
    if let Some(domain_name) = &config.domain_name {
        tls = tls.domain_name(domain_name.clone());
    }
    if let Some(ca) = &config.ca_certificate_pem {
        tls = tls.ca_certificate(Certificate::from_pem(ca.clone()));
    }
    if let (Some(certificate), Some(private_key)) = (
        &config.client_certificate_pem,
        &config.client_private_key_pem,
    ) {
        tls = tls.identity(Identity::from_pem(certificate.clone(), private_key.clone()));
    }
    tls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_endpoint() {
        let config = RpcChannelConfig::new("not a uri");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime.block_on(connect_channel(&config)).unwrap_err();
        assert!(matches!(error, RpcError::InvalidEndpoint(_)));
    }
}
