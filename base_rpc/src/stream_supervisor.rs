use std::error::Error;
use std::sync::Arc;

use base::tokio::sync::{mpsc, watch};
use base::tokio::task::JoinHandle;
use base::tokio::time::sleep;
use base::tokio_util::sync::CancellationToken;
use tonic::async_trait;

use crate::retry::RetryPolicy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Connecting { generation: u64, attempt: u32 },
    Connected { generation: u64 },
    Disconnected { generation: u64, reason: String },
    Stopped,
}

#[derive(Debug, Clone, Default)]
pub struct StreamSupervisorConfig {
    pub retry: RetryPolicy,
}

#[derive(Clone)]
pub struct ConnectionReporter {
    generation: u64,
    state: watch::Sender<ConnectionState>,
}

impl ConnectionReporter {
    pub fn connected(&self) {
        let current = self.state.borrow().clone();
        let is_current = match current {
            ConnectionState::Connecting { generation, .. }
            | ConnectionState::Connected { generation } => generation == self.generation,
            _ => false,
        };
        if is_current {
            let _ = self.state.send(ConnectionState::Connected {
                generation: self.generation,
            });
        }
    }
}

#[async_trait]
pub trait StreamConnector: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn connect_and_run(
        &self,
        generation: u64,
        reporter: ConnectionReporter,
        cancel: CancellationToken,
    ) -> Result<(), Self::Error>;
}

pub struct StreamSupervisor<C> {
    config: StreamSupervisorConfig,
    connector: Arc<C>,
}

impl<C> StreamSupervisor<C>
where
    C: StreamConnector,
{
    pub fn new(config: StreamSupervisorConfig, connector: C) -> Self {
        Self {
            config,
            connector: Arc::new(connector),
        }
    }

    pub fn spawn(self) -> StreamSupervisorHandle {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (state_tx, state_rx) = watch::channel(ConnectionState::Idle);
        let join = base::tokio::spawn(async move {
            let mut generation = 0u64;
            let mut attempt = 0u32;
            loop {
                if task_cancel.is_cancelled() {
                    break;
                }
                attempt = attempt.saturating_add(1);
                if !self.config.retry.permits(attempt) {
                    break;
                }
                generation = generation.saturating_add(1);
                let _ = state_tx.send(ConnectionState::Connecting {
                    generation,
                    attempt,
                });
                let connection_cancel = task_cancel.child_token();
                let reporter = ConnectionReporter {
                    generation,
                    state: state_tx.clone(),
                };
                let result = self
                    .connector
                    .connect_and_run(generation, reporter, connection_cancel.clone())
                    .await;
                connection_cancel.cancel();
                if task_cancel.is_cancelled() {
                    break;
                }
                let reason = result
                    .err()
                    .map_or_else(|| "connection ended".to_string(), |error| error.to_string());
                let _ = state_tx.send(ConnectionState::Disconnected { generation, reason });
                let delay = self.config.retry.delay(attempt);
                base::tokio::select! {
                    _ = sleep(delay) => {}
                    _ = task_cancel.cancelled() => break,
                }
            }
            let _ = state_tx.send(ConnectionState::Stopped);
        });
        StreamSupervisorHandle {
            cancel,
            state: state_rx,
            join,
        }
    }
}

pub struct StreamSupervisorHandle {
    cancel: CancellationToken,
    state: watch::Receiver<ConnectionState>,
    join: JoinHandle<()>,
}

impl StreamSupervisorHandle {
    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.join.await;
    }
}

pub struct BoundedQueue<T> {
    sender: mpsc::Sender<T>,
    receiver: mpsc::Receiver<T>,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "queue capacity must be positive");
        let (sender, receiver) = mpsc::channel(capacity);
        Self { sender, receiver }
    }

    pub fn sender(&self) -> mpsc::Sender<T> {
        self.sender.clone()
    }

    pub fn into_receiver(self) -> mpsc::Receiver<T> {
        self.receiver
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;

    struct TestConnector {
        calls: AtomicU32,
        cancelled_generations: Arc<AtomicU32>,
    }

    #[async_trait]
    impl StreamConnector for TestConnector {
        type Error = io::Error;

        async fn connect_and_run(
            &self,
            _generation: u64,
            reporter: ConnectionReporter,
            cancel: CancellationToken,
        ) -> Result<(), Self::Error> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            reporter.connected();
            if call == 0 {
                let cancelled_generations = self.cancelled_generations.clone();
                base::tokio::spawn(async move {
                    cancel.cancelled().await;
                    cancelled_generations.fetch_add(1, Ordering::SeqCst);
                });
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset"));
            }
            cancel.cancelled().await;
            self.cancelled_generations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn reconnects_with_new_generation_and_stops() {
        let cancelled_generations = Arc::new(AtomicU32::new(0));
        let supervisor = StreamSupervisor::new(
            StreamSupervisorConfig {
                retry: RetryPolicy {
                    initial_delay: Duration::from_millis(1),
                    max_delay: Duration::from_millis(1),
                    multiplier: 1.0,
                    jitter_ratio: 0.0,
                    max_attempts: Some(3),
                },
            },
            TestConnector {
                calls: AtomicU32::new(0),
                cancelled_generations: cancelled_generations.clone(),
            },
        );
        let handle = supervisor.spawn();
        let mut state = handle.state();
        base::tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                state.changed().await.unwrap();
                if matches!(
                    *state.borrow(),
                    ConnectionState::Connected { generation: 2 }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        base::tokio::time::timeout(Duration::from_secs(1), async {
            while cancelled_generations.load(Ordering::SeqCst) < 1 {
                base::tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.shutdown().await;
        assert_eq!(cancelled_generations.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bounded_queue_rejects_when_full() {
        let queue = BoundedQueue::new(1);
        let sender = queue.sender();
        sender.try_send(1).unwrap();
        assert!(matches!(
            sender.try_send(2),
            Err(mpsc::error::TrySendError::Full(2))
        ));
    }
}
