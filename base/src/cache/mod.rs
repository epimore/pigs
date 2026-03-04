use crate::utils::rt::GlobalRuntime;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use futures::future::BoxFuture;
use futures::StreamExt;
use once_cell::sync::Lazy;
use std::{
    sync::Arc,
    time::Duration,
};
use log::error;
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tokio_util::time::{delay_queue::Key, DelayQueue};
use exception::GlobalResultExt;

static COMMON_CACHE: Lazy<Cache> = Lazy::new(Cache::init);
pub struct CommonCache;
impl CommonCache {
    pub async fn insert(key: String, value: CachedValue) {
        COMMON_CACHE.insert(key, value).await;
    }
    pub fn get(&self, key: &str) -> Option<CachedValue> {
        COMMON_CACHE.get(key)
    }
    pub async fn remove(&self, key: &str) -> Option<CachedValue> {
        COMMON_CACHE.remove(key).await
    }
    pub async fn refresh(&self, key: &str) {
        COMMON_CACHE.refresh(key).await;
    }
}

pub trait Cacheable: Send + Sync + 'static {
    fn recache(&self) -> Option<Duration> {
        None
    }

    fn expire_call(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn expire_ttl(&self) -> Option<Duration>;
}

#[derive(Clone)]
pub struct CachedValue(Arc<dyn Cacheable>);

impl std::ops::Deref for CachedValue {
    type Target = dyn Cacheable;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

struct Inner {
    value: CachedValue,
    delay_key: Option<Key>,
}
struct Shared {
    map: DashMap<String, Inner>,
    delays: Mutex<DelayQueue<String>>,
    call_tx: Sender<CachedValue>,
}
pub struct Cache {
    shared: Arc<Shared>,
}
impl Cache {
    pub fn init() -> Self {
        let (tx, mut rx) = channel(10000);
        let shared = Arc::new(Shared {
            map: DashMap::new(),
            delays: Mutex::new(DelayQueue::new()),
            call_tx: tx,
        });

        let call_shared = shared.clone();
        let cache = Cache {
            shared: call_shared,
        };
        let rt = GlobalRuntime::get_main_runtime();
        rt.rt_handle
            .spawn(Self::purge_task(shared.clone(), rt.cancel.clone()));
        rt.rt_handle.spawn(async move {
            loop {
                tokio::select! {
                    _ = rt.cancel.cancelled() => break,
                    res = rx.recv() => {
                        match res{
                            None => {break}
                            Some(entity) => {
                                entity.expire_call().await;
                            }
                        }
                    }
                }
            }
            shared.map.clear();
            let mut delays = shared.delays.lock().await;
            delays.clear();
        });
        cache
    }
    async fn poll_expired(shared: &Arc<Shared>) -> Option<String> {
        let mut delays = shared.delays.lock().await;
        delays.next().await.map(|e| e.into_inner())
    }
    async fn purge_task(shared: Arc<Shared>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,

                expired = Self::poll_expired(&shared) => {
                    if let Some(key) = expired {
                        Self::handle_expired(&shared, key).await;
                    }
                }
            }
        }

        shared.map.clear();
        let mut delays = shared.delays.lock().await;
        delays.clear();
    }

    async fn handle_expired(shared: &Arc<Shared>, key: String) {
        if let Some((_, inner)) = shared.map.remove(&key) {
            let value = inner.value.clone();
            if let Some(ttl) = value.recache() {
                let mut delays = shared.delays.lock().await;
                let delay_key = delays.insert(key.clone(), ttl);

                shared.map.insert(
                    key,
                    Inner {
                        value,
                        delay_key: Some(delay_key),
                    },
                );

                return;
            }

            // 发送给 async worker
            let _ = shared.call_tx.try_send(value).hand_log(|msg|error!("Cache call channel error: {msg}"));
        }
    }
}
impl Cache {
    pub async fn insert(&self, key: String, value: CachedValue) {
        match self.shared.map.entry(key.clone()) {
            Entry::Occupied(mut occ) => {
                let inner = occ.get_mut();
                let delay_key = match inner.delay_key {
                    None => match value.expire_ttl() {
                        None => None,
                        Some(ttl) => {
                            let mut delays = self.shared.delays.lock().await;
                            Some(delays.insert(key, ttl))
                        }
                    },
                    Some(old_delay_key) => {
                        let mut delays = self.shared.delays.lock().await;
                        delays.remove(&old_delay_key);
                        match value.expire_ttl() {
                            None => None,
                            Some(ttl) => Some(delays.insert(key, ttl)),
                        }
                    }
                };
                *inner = Inner { value, delay_key };
            }
            Entry::Vacant(vac) => {
                let delay_key = match value.expire_ttl() {
                    None => None,
                    Some(ttl) => {
                        let mut delays = self.shared.delays.lock().await;
                        Some(delays.insert(key, ttl))
                    }
                };
                vac.insert(Inner { value, delay_key });
            }
        }
    }
    pub fn get(&self, key: &str) -> Option<CachedValue> {
        self.shared.map.get(key).map(|v| v.value.clone())
    }
    pub async fn remove(&self, key: &str) -> Option<CachedValue> {
        if let Some((_, entry)) = self.shared.map.remove(key) {
            if let Some(delay_key) = entry.delay_key {
                let mut delays = self.shared.delays.lock().await;
                delays.remove(&delay_key);
            }
            return Some(entry.value);
        }
        None
    }
    pub async fn refresh(&self, key: &str) {
        //使用get_mut,并发remove导致delay_key失效
        if let Some(entry) = self.shared.map.get_mut(key) {
            if let Some(delay_key) = &entry.delay_key {
                if let Some(ttl) = entry.value.expire_ttl() {
                    let mut delays = self.shared.delays.lock().await;
                    delays.reset(delay_key, ttl);
                }
            }
        }
    }
}

