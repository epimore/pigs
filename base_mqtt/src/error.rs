#[derive(Debug, thiserror::Error)]
pub enum MqttError {
    #[error("invalid MQTT configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to read MQTT TLS file {path}: {source}")]
    TlsFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("MQTT client request failed: {0}")]
    Client(String),
    #[error("MQTT connection failed: {0}")]
    Connection(String),
}
