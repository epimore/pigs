use std::path::PathBuf;
use std::time::Duration;

use crate::MqttError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttProtocolVersion {
    V3,
    V5,
}

impl MqttProtocolVersion {
    pub fn parse(value: &str) -> Result<Self, MqttError> {
        match value {
            "v3" => Ok(Self::V3),
            "v5" => Ok(Self::V5),
            _ => Err(MqttError::InvalidConfig(
                "protocol_version must be v3 or v5".to_string(),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::V3 => "v3",
            Self::V5 => "v5",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttQos {
    AtMostOnce,
    AtLeastOnce,
    ExactlyOnce,
}

#[derive(Debug, Clone)]
pub struct MqttTlsConfig {
    pub ca_certificate_path: Option<PathBuf>,
    pub client_certificate_path: Option<PathBuf>,
    pub client_private_key_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct MqttReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub multiplier: f64,
    pub jitter_ratio: f64,
    pub max_attempts: Option<u32>,
}

impl Default for MqttReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
            jitter_ratio: 0.2,
            max_attempts: None,
        }
    }
}

impl MqttReconnectPolicy {
    pub fn delay(&self, attempt: u32) -> Duration {
        use base::rand::Rng;

        let exponent = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
        let seconds = self.initial_delay.as_secs_f64() * self.multiplier.max(1.0).powi(exponent);
        let capped = seconds.min(self.max_delay.as_secs_f64());
        if self.jitter_ratio <= 0.0 || capped == 0.0 {
            return Duration::from_secs_f64(capped);
        }
        let jitter = capped * self.jitter_ratio.clamp(0.0, 1.0);
        let delta = base::rand::thread_rng().gen_range(-jitter..=jitter);
        Duration::from_secs_f64((capped + delta).max(0.0))
    }

    pub fn permits(&self, attempt: u32) -> bool {
        self.max_attempts.is_none_or(|max| attempt <= max)
    }
}

#[derive(Debug, Clone)]
pub struct MqttWill {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: MqttQos,
    pub retain: bool,
}

#[derive(Debug, Clone)]
pub struct MqttSubscription {
    pub topic_filter: String,
    pub qos: MqttQos,
}

#[derive(Clone)]
pub struct MqttClientConfig {
    pub protocol_version: MqttProtocolVersion,
    pub client_id: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub keep_alive: Duration,
    pub request_capacity: usize,
    pub clean_start: bool,
    pub session_expiry: Option<Duration>,
    pub tls: Option<MqttTlsConfig>,
    pub last_will: Option<MqttWill>,
    pub reconnect: MqttReconnectPolicy,
}

impl std::fmt::Debug for MqttClientConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MqttClientConfig")
            .field("protocol_version", &self.protocol_version)
            .field("client_id", &self.client_id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("keep_alive", &self.keep_alive)
            .field("request_capacity", &self.request_capacity)
            .field("clean_start", &self.clean_start)
            .field("session_expiry", &self.session_expiry)
            .field("tls", &self.tls)
            .field(
                "last_will",
                &self.last_will.as_ref().map(|_| "<configured>"),
            )
            .field("reconnect", &self.reconnect)
            .finish()
    }
}

impl MqttClientConfig {
    pub fn validate(&self) -> Result<(), MqttError> {
        if self.client_id.trim().is_empty() || self.host.trim().is_empty() || self.port == 0 {
            return Err(MqttError::InvalidConfig(
                "client_id, host, and port are required".to_string(),
            ));
        }
        if self.request_capacity == 0 || self.keep_alive.is_zero() {
            return Err(MqttError::InvalidConfig(
                "request_capacity and keep_alive must be positive".to_string(),
            ));
        }
        if self.username.is_some() != self.password.is_some() {
            return Err(MqttError::InvalidConfig(
                "username and password must be configured together".to_string(),
            ));
        }
        if self.protocol_version == MqttProtocolVersion::V3 && self.session_expiry.is_some() {
            return Err(MqttError::InvalidConfig(
                "session_expiry requires MQTT v5".to_string(),
            ));
        }
        if let Some(tls) = &self.tls
            && (tls.client_certificate_path.is_some() != tls.client_private_key_path.is_some())
        {
            return Err(MqttError::InvalidConfig(
                "client certificate and private key must be configured together".to_string(),
            ));
        }
        if let Some(tls) = &self.tls
            && tls.ca_certificate_path.is_none()
            && tls.client_certificate_path.is_some()
        {
            return Err(MqttError::InvalidConfig(
                "custom client authentication requires an explicit CA certificate".to_string(),
            ));
        }
        if let Some(will) = &self.last_will
            && will.topic.trim().is_empty()
        {
            return Err(MqttError::InvalidConfig(
                "last will topic must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> MqttClientConfig {
        MqttClientConfig {
            protocol_version: MqttProtocolVersion::V5,
            client_id: "steward-node-1".to_string(),
            host: "127.0.0.1".to_string(),
            port: 1883,
            username: None,
            password: None,
            keep_alive: Duration::from_secs(30),
            request_capacity: 32,
            clean_start: false,
            session_expiry: Some(Duration::from_secs(3600)),
            tls: None,
            last_will: None,
            reconnect: MqttReconnectPolicy::default(),
        }
    }

    #[test]
    fn validates_auth_pair() {
        let mut config = valid_config();
        config.username = Some("user".to_string());
        assert!(matches!(
            config.validate(),
            Err(MqttError::InvalidConfig(_))
        ));
    }

    #[test]
    fn rejects_v3_session_expiry() {
        let mut config = valid_config();
        config.protocol_version = MqttProtocolVersion::V3;
        assert!(matches!(
            config.validate(),
            Err(MqttError::InvalidConfig(_))
        ));
    }

    #[test]
    fn debug_redacts_password_and_last_will_payload() {
        let mut config = valid_config();
        config.username = Some("user".to_string());
        config.password = Some("mqtt-secret".to_string());
        config.last_will = Some(MqttWill {
            topic: "state".to_string(),
            payload: b"will-secret".to_vec(),
            qos: MqttQos::AtLeastOnce,
            retain: true,
        });
        let debug = format!("{config:?}");
        assert!(!debug.contains("mqtt-secret"));
        assert!(!debug.contains("will-secret"));
    }
}
