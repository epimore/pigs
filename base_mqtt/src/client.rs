use base::tokio::sync::mpsc;
use base::tokio_util::sync::CancellationToken;
use rumqttc::v5::mqttbytes::QoS as QoSV5;
use rumqttc::v5::{
    AsyncClient as AsyncClientV5, Event as EventV5, EventLoop as EventLoopV5,
    MqttOptions as MqttOptionsV5,
};
use rumqttc::{
    AsyncClient, Event, EventLoop, LastWill, MqttOptions, Outgoing, Packet, QoS, Transport,
};
use std::fs;

use crate::{
    MqttClientConfig, MqttError, MqttProtocolVersion, MqttQos, MqttReconnectPolicy,
    MqttSubscription, MqttTlsConfig, MqttWill,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MqttEvent {
    Connected,
    Disconnected(String),
    Publish {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
}

#[derive(Clone)]
enum PublishClient {
    V3(AsyncClient),
    V5(AsyncClientV5),
}

#[derive(Clone)]
pub struct MqttPublisher {
    client: PublishClient,
}

impl MqttPublisher {
    pub async fn publish(
        &self,
        topic: impl Into<String>,
        payload: impl Into<Vec<u8>>,
        qos: MqttQos,
        retain: bool,
    ) -> Result<(), MqttError> {
        let topic = topic.into();
        let payload = payload.into();
        match &self.client {
            PublishClient::V3(client) => client
                .publish(topic, qos_v3(qos), retain, payload)
                .await
                .map_err(|error| MqttError::Client(error.to_string())),
            PublishClient::V5(client) => client
                .publish(topic, qos_v5(qos), retain, payload)
                .await
                .map_err(|error| MqttError::Client(error.to_string())),
        }
    }

    pub async fn disconnect(&self) -> Result<(), MqttError> {
        match &self.client {
            PublishClient::V3(client) => client
                .disconnect()
                .await
                .map_err(|error| MqttError::Client(error.to_string())),
            PublishClient::V5(client) => client
                .disconnect()
                .await
                .map_err(|error| MqttError::Client(error.to_string())),
        }
    }
}

#[derive(Clone)]
enum RuntimeClient {
    V3(AsyncClient),
    V5(AsyncClientV5),
}

enum RuntimeEventLoop {
    V3(Box<EventLoop>),
    V5(Box<EventLoopV5>),
}

pub struct MqttRuntime {
    publisher: MqttPublisher,
    client: RuntimeClient,
    event_loop: RuntimeEventLoop,
    subscriptions: Vec<MqttSubscription>,
    reconnect: MqttReconnectPolicy,
}

impl MqttRuntime {
    pub fn new(
        config: MqttClientConfig,
        subscriptions: Vec<MqttSubscription>,
    ) -> Result<Self, MqttError> {
        config.validate()?;
        for subscription in &subscriptions {
            if subscription.topic_filter.trim().is_empty() {
                return Err(MqttError::InvalidConfig(
                    "subscription topic filter must not be empty".to_string(),
                ));
            }
        }

        let tls_transport = config.tls.as_ref().map(load_tls).transpose()?;
        let (client, event_loop, publish_client) = match config.protocol_version {
            MqttProtocolVersion::V3 => {
                let mut options = MqttOptions::new(&config.client_id, &config.host, config.port);
                options.set_keep_alive(config.keep_alive);
                options.set_clean_session(config.clean_start);
                if let Some(transport) = tls_transport.clone() {
                    options.set_transport(transport);
                }
                if let (Some(username), Some(password)) = (&config.username, &config.password) {
                    options.set_credentials(username, password);
                }
                if let Some(will) = &config.last_will {
                    options.set_last_will(last_will_v3(will));
                }
                let (client, event_loop) = AsyncClient::new(options, config.request_capacity);
                (
                    RuntimeClient::V3(client.clone()),
                    RuntimeEventLoop::V3(Box::new(event_loop)),
                    PublishClient::V3(client),
                )
            }
            MqttProtocolVersion::V5 => {
                let mut options = MqttOptionsV5::new(&config.client_id, &config.host, config.port);
                options.set_keep_alive(config.keep_alive);
                options.set_clean_start(config.clean_start);
                options.set_session_expiry_interval(
                    config
                        .session_expiry
                        .map(|duration| duration.as_secs().min(u64::from(u32::MAX)) as u32),
                );
                if let Some(transport) = tls_transport {
                    options.set_transport(transport);
                }
                if let (Some(username), Some(password)) = (&config.username, &config.password) {
                    options.set_credentials(username, password);
                }
                if let Some(will) = &config.last_will {
                    options.set_last_will(last_will_v5(will));
                }
                let (client, event_loop) = AsyncClientV5::new(options, config.request_capacity);
                (
                    RuntimeClient::V5(client.clone()),
                    RuntimeEventLoop::V5(Box::new(event_loop)),
                    PublishClient::V5(client),
                )
            }
        };

        Ok(Self {
            publisher: MqttPublisher {
                client: publish_client,
            },
            client,
            event_loop,
            subscriptions,
            reconnect: config.reconnect,
        })
    }

    pub fn publisher(&self) -> MqttPublisher {
        self.publisher.clone()
    }

    pub fn set_subscriptions(
        &mut self,
        subscriptions: Vec<MqttSubscription>,
    ) -> Result<(), MqttError> {
        if subscriptions
            .iter()
            .any(|subscription| subscription.topic_filter.trim().is_empty())
        {
            return Err(MqttError::InvalidConfig(
                "subscription topic filter must not be empty".to_string(),
            ));
        }
        self.subscriptions = subscriptions;
        Ok(())
    }

    pub async fn run(
        mut self,
        cancel: CancellationToken,
        events: mpsc::Sender<MqttEvent>,
    ) -> Result<(), MqttError> {
        let mut connected = false;
        let mut attempt = 0_u32;
        loop {
            let polled = base::tokio::select! {
                _ = cancel.cancelled() => {
                    self.disconnect_and_flush().await;
                    return Ok(());
                }
                result = self.poll() => result,
            };
            match polled {
                Ok(Some(event)) => {
                    if matches!(event, MqttEvent::Connected) {
                        subscribe_all(self.client.clone(), self.subscriptions.clone()).await?;
                        connected = true;
                        attempt = 0;
                    }
                    if events.send(event).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    attempt = attempt.saturating_add(1);
                    if connected {
                        connected = false;
                        if events
                            .send(MqttEvent::Disconnected(error.to_string()))
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    if !self.reconnect.permits(attempt) {
                        return Err(error);
                    }
                    let delay = self.reconnect.delay(attempt);
                    base::tokio::select! {
                        _ = cancel.cancelled() => return Ok(()),
                        _ = base::tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn poll(&mut self) -> Result<Option<MqttEvent>, MqttError> {
        match &mut self.event_loop {
            RuntimeEventLoop::V3(event_loop) => match event_loop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => Ok(Some(MqttEvent::Connected)),
                Ok(Event::Incoming(Packet::Publish(publish))) => Ok(Some(MqttEvent::Publish {
                    topic: publish.topic,
                    payload: publish.payload.to_vec(),
                    retain: publish.retain,
                })),
                Ok(_) => Ok(None),
                Err(error) => Err(MqttError::Connection(error.to_string())),
            },
            RuntimeEventLoop::V5(event_loop) => match event_loop.poll().await {
                Ok(EventV5::Incoming(rumqttc::v5::mqttbytes::v5::Packet::ConnAck(_))) => {
                    Ok(Some(MqttEvent::Connected))
                }
                Ok(EventV5::Incoming(rumqttc::v5::mqttbytes::v5::Packet::Publish(publish))) => {
                    Ok(Some(MqttEvent::Publish {
                        topic: String::from_utf8_lossy(&publish.topic).into_owned(),
                        payload: publish.payload.to_vec(),
                        retain: publish.retain,
                    }))
                }
                Ok(_) => Ok(None),
                Err(error) => Err(MqttError::Connection(error.to_string())),
            },
        }
    }

    async fn disconnect_and_flush(&mut self) {
        let queued = match &self.client {
            RuntimeClient::V3(client) => client.try_disconnect().is_ok(),
            RuntimeClient::V5(client) => client.try_disconnect().is_ok(),
        };
        if !queued {
            return;
        }
        let flush = async {
            loop {
                let disconnected = match &mut self.event_loop {
                    RuntimeEventLoop::V3(event_loop) => {
                        matches!(
                            event_loop.poll().await,
                            Ok(Event::Outgoing(Outgoing::Disconnect))
                        )
                    }
                    RuntimeEventLoop::V5(event_loop) => matches!(
                        event_loop.poll().await,
                        Ok(EventV5::Outgoing(Outgoing::Disconnect))
                    ),
                };
                if disconnected {
                    return;
                }
            }
        };
        let _ = base::tokio::time::timeout(std::time::Duration::from_secs(1), flush).await;
    }
}

async fn subscribe_all(
    client: RuntimeClient,
    subscriptions: Vec<MqttSubscription>,
) -> Result<(), MqttError> {
    for subscription in subscriptions {
        match &client {
            RuntimeClient::V3(client) => client
                .subscribe(subscription.topic_filter, qos_v3(subscription.qos))
                .await
                .map_err(|error| MqttError::Client(error.to_string()))?,
            RuntimeClient::V5(client) => client
                .subscribe(subscription.topic_filter, qos_v5(subscription.qos))
                .await
                .map_err(|error| MqttError::Client(error.to_string()))?,
        }
    }
    Ok(())
}

fn load_tls(config: &MqttTlsConfig) -> Result<Transport, MqttError> {
    let Some(ca_path) = &config.ca_certificate_path else {
        return Ok(Transport::tls_with_default_config());
    };
    let ca = read_tls_file(ca_path)?;
    let client_auth = match (
        &config.client_certificate_path,
        &config.client_private_key_path,
    ) {
        (Some(certificate), Some(private_key)) => {
            Some((read_tls_file(certificate)?, read_tls_file(private_key)?))
        }
        _ => None,
    };
    Ok(Transport::tls(ca, client_auth, None))
}

fn read_tls_file(path: &std::path::Path) -> Result<Vec<u8>, MqttError> {
    fs::read(path).map_err(|source| MqttError::TlsFile {
        path: path.display().to_string(),
        source,
    })
}

fn qos_v3(qos: MqttQos) -> QoS {
    match qos {
        MqttQos::AtMostOnce => QoS::AtMostOnce,
        MqttQos::AtLeastOnce => QoS::AtLeastOnce,
        MqttQos::ExactlyOnce => QoS::ExactlyOnce,
    }
}

fn qos_v5(qos: MqttQos) -> QoSV5 {
    match qos {
        MqttQos::AtMostOnce => QoSV5::AtMostOnce,
        MqttQos::AtLeastOnce => QoSV5::AtLeastOnce,
        MqttQos::ExactlyOnce => QoSV5::ExactlyOnce,
    }
}

fn last_will_v3(will: &MqttWill) -> LastWill {
    LastWill::new(
        &will.topic,
        will.payload.clone(),
        qos_v3(will.qos),
        will.retain,
    )
}

fn last_will_v5(will: &MqttWill) -> rumqttc::v5::mqttbytes::v5::LastWill {
    rumqttc::v5::mqttbytes::v5::LastWill::new(
        &will.topic,
        will.payload.clone(),
        qos_v5(will.qos),
        will.retain,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_closes_an_unconnected_runtime() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let runtime = MqttRuntime::new(
            MqttClientConfig {
                protocol_version: MqttProtocolVersion::V5,
                client_id: "base-mqtt-cancel-test".to_string(),
                host: "127.0.0.1".to_string(),
                port,
                username: None,
                password: None,
                keep_alive: std::time::Duration::from_secs(5),
                request_capacity: 4,
                clean_start: true,
                session_expiry: None,
                tls: None,
                last_will: None,
                reconnect: MqttReconnectPolicy::default(),
            },
            Vec::new(),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (events, _receiver) = mpsc::channel(1);
        let result = base::tokio::time::timeout(
            std::time::Duration::from_secs(2),
            runtime.run(cancel, events),
        )
        .await;
        assert!(matches!(result, Ok(Ok(()))));
    }
}
