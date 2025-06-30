use std::{
    any::{Any, TypeId},
    sync::Arc,
};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use exception::typed::common::MessageBusError;

const DEFAULT_CHANNEL_SIZE: usize = 64;

pub struct TypedMessageBus {
    channels: Arc<DashMap<TypeId, mpsc::Sender<Box<dyn Any + Send + Sync>>>>,
}

impl TypedMessageBus {
    pub fn new() -> Self {
        Self {
            channels: Arc::new(DashMap::new()),
        }
    }

    pub fn try_publish<T>(&self, msg: T) -> Result<(), MessageBusError>
    where
        T: Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        let entry = self.channels.entry(type_id).or_insert_with(|| {
            let (tx, _) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
            tx
        });
        entry
            .try_send(Box::new(msg))
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => MessageBusError::Full,
                mpsc::error::TrySendError::Closed(_) => MessageBusError::ChannelClosed,
            })
    }

    pub async fn publish<T>(&self, msg: T) -> Result<(), MessageBusError>
    where
        T: Send + Sync + 'static,
    {
        let type_id = TypeId::of::<T>();
        let entry = self.channels.entry(type_id).or_insert_with(|| {
            let (tx, _) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
            tx
        });
        entry
            .send(Box::new(msg))
            .await
            .map_err(|_| MessageBusError::ChannelClosed)
    }
}

pub struct TypedReceiver<T>
where
    T: Send + Sync + 'static,
{
    inner: mpsc::Receiver<Box<dyn Any + Send + Sync>>,
    _marker: std::marker::PhantomData<T>,
}

impl<T> TypedReceiver<T>
where
    T: Send + Sync + 'static,
{
    pub fn new(inner: mpsc::Receiver<Box<dyn Any + Send + Sync>>) -> Self {
        Self { inner, _marker: std::marker::PhantomData }
    }
    pub fn try_recv(&mut self) -> Result<T, MessageBusError> {
        match self.inner.try_recv() {
            Ok(bo) => Self::try_cast(bo),
            Err(e) => match e {
                mpsc::error::TryRecvError::Empty => Err(MessageBusError::Timeout),
                mpsc::error::TryRecvError::Disconnected => Err(MessageBusError::ChannelClosed),
            },
        }
    }

    pub async fn recv(&mut self) -> Result<T, MessageBusError> {
        match self.inner.recv().await {
            Some(bo) => Self::try_cast(bo),
            None => Err(MessageBusError::ChannelClosed),
        }
    }

    pub async fn recv_with_timeout(&mut self, dur: Duration) -> Result<T, MessageBusError> {
        match timeout(dur, self.inner.recv()).await {
            Ok(Some(bo)) => Self::try_cast(bo),
            Ok(None) => Err(MessageBusError::ChannelClosed),
            Err(_) => Err(MessageBusError::Timeout),
        }
    }

    fn try_cast(bo: Box<dyn Any + Send + Sync>) -> Result<T, MessageBusError> {
        bo.downcast::<T>()
            .map(|boxed| *boxed)
            .map_err(|_| MessageBusError::TypeMismatch)
    }
}