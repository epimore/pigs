use std::{
    any::{Any, TypeId},
    sync::Arc,
};

use dashmap::DashMap;
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};
use exception::typed::common::MessageBusError;

const DEFAULT_CHANNEL_SIZE: usize = 64;

#[derive(Clone)]
pub struct TypedMessageBus {
    channels: Arc<DashMap<TypeId, broadcast::Sender<Arc<dyn Any + Send + Sync>>>>,
}

impl TypedMessageBus {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(DashMap::new()),
        }
    }

    pub fn publish<T>(&self, msg: T) -> Result<usize, broadcast::error::SendError<Arc<dyn Any + Send + Sync>>>
    where
        T: Send + Sync + 'static + Clone,
    {
        let type_id = TypeId::of::<T>();
        let entry = self.channels.entry(type_id).or_insert_with(|| {
            let (tx, _) = broadcast::channel(DEFAULT_CHANNEL_SIZE);
            tx
        });
        entry.send(Arc::new(msg))
    }

    pub fn publish_ref<T>(&self, msg: &T) -> Result<usize, broadcast::error::SendError<Arc<dyn Any + Send + Sync>>>
    where
        T: Send + Sync + 'static + Clone,
    {
        self.publish(msg.clone())
    }

    pub fn sub_type_channel<T>(&self) -> TypedReceiver<T>
    where
        T: Send + Sync + 'static + Clone,
    {
        let type_id = TypeId::of::<T>();
        let tx = self
            .channels
            .entry(type_id)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(DEFAULT_CHANNEL_SIZE);
                tx
            })
            .clone();

        TypedReceiver {
            inner: tx.subscribe(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn spawn_cleanup_task(&self) {
        let map = self.channels.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let to_remove: Vec<_> = map
                    .iter()
                    .filter(|entry| entry.value().receiver_count() == 0)
                    .map(|entry| *entry.key())
                    .collect();
                for id in to_remove {
                    map.remove(&id);
                }
            }
        });
    }
}

pub struct TypedReceiver<T>
where
    T: Send + Sync + 'static + Clone,
{
    inner: broadcast::Receiver<Arc<dyn Any + Send + Sync>>,
    _marker: std::marker::PhantomData<T>,
}

impl<T> TypedReceiver<T>
where
    T: Send + Sync + 'static + Clone,
{
    pub fn try_recv(&mut self) -> Result<T, MessageBusError> {
        match self.inner.try_recv() {
            Ok(arc) => Self::try_cast(arc),
            Err(broadcast::error::TryRecvError::Closed) => Err(MessageBusError::ChannelClosed),
            Err(broadcast::error::TryRecvError::Empty) => Err(MessageBusError::Empty),
            Err(broadcast::error::TryRecvError::Lagged(_)) => Err(MessageBusError::Lagged),
        }
    }

    pub async fn recv(&mut self) -> Result<T, MessageBusError> {
        match self.inner.recv().await {
            Ok(arc) => Self::try_cast(arc),
            Err(broadcast::error::RecvError::Closed) => Err(MessageBusError::ChannelClosed),
            Err(broadcast::error::RecvError::Lagged(_)) => Err(MessageBusError::Lagged),
        }
    }

    pub async fn recv_with_timeout(&mut self, dur: Duration) -> Result<T, MessageBusError> {
        match timeout(dur, self.inner.recv()).await {
            Ok(Ok(arc)) => Self::try_cast(arc),
            Ok(Err(broadcast::error::RecvError::Closed)) => Err(MessageBusError::ChannelClosed),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => Err(MessageBusError::Lagged),
            Err(_) => Err(MessageBusError::Timeout),
        }
    }

    fn try_cast(arc: Arc<dyn Any + Send + Sync>) -> Result<T, MessageBusError> {
        match Arc::downcast::<T>(arc) {
            Ok(val) => Ok((*val).clone()),
            Err(_) => Err(MessageBusError::TypeMismatch),
        }
    }
}