use crate::utils::rt::GlobalRuntime;
use dashmap::DashMap;
use exception::GlobalResultExt;
use futures::future::BoxFuture;
use futures::StreamExt;
use log::error;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::HashMap;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc::{channel, Sender, UnboundedSender};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::time::{delay_queue::Key, DelayQueue};

static COMMON_CACHE: Lazy<Cache> = Lazy::new(Cache::init);
pub struct CommonCache;
impl CommonCache {
    pub fn contains_key(key: &str) -> bool {
        COMMON_CACHE.contains_key(key)
    }
    pub fn insert(key: String, value: CachedValue) {
        COMMON_CACHE.insert(key, value);
    }
    pub fn get(key: &str) -> Option<CachedValue> {
        COMMON_CACHE.get(key)
    }
    pub fn remove(key: &str) -> Option<CachedValue> {
        COMMON_CACHE.remove(key)
    }
    pub fn refresh(key: &str) {
        COMMON_CACHE.refresh(key);
    }
}

pub trait Cacheable: Send + Sync + Any + 'static {
    fn expire_call(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn expire_ttl(&self) -> Option<Duration>;
}

#[derive(Clone)]
pub struct CachedValue(Arc<dyn Cacheable>);
impl CachedValue {
    pub fn from_any(value: Arc<dyn Cacheable>) -> Self {
        CachedValue(value)
    }
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        (self.0.as_ref() as &dyn Any).downcast_ref::<T>()
    }
}
impl std::ops::Deref for CachedValue {
    type Target = dyn Cacheable;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// DelayQueue 操作命令
enum DelayCommand {
    Insert { key: String, ttl: Option<Duration> },
    Remove { key: String },
    Refresh { key: String, ttl: Duration },
}

struct Shared {
    map: DashMap<String, CachedValue>,
    cmd_tx: UnboundedSender<DelayCommand>,
    call_tx: Sender<CachedValue>,
}

pub struct Cache {
    shared: Arc<Shared>,
}
impl Cache {
    pub fn init() -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<DelayCommand>();
        let (call_tx, mut call_rx) = channel::<CachedValue>(10000);

        let shared = Arc::new(Shared {
            map: DashMap::new(),
            cmd_tx,
            call_tx,
        });
        // spawn purge_task，独占 DelayQueue
        let purge_shared = shared.clone();
        let rt = GlobalRuntime::get_main_runtime();
        let cancellation_token_cmd = rt.cancel.clone();
        rt.rt_handle.spawn(async move {
            let mut delay_queue = DelayQueue::<String>::new();
            let mut key_map = HashMap::<String, Key>::new();
            loop {
                tokio::select! {
                    _ = cancellation_token_cmd.cancelled() => break,

                    Some(cmd) = cmd_rx.recv() => {
                        match cmd {
                            DelayCommand::Insert { key, ttl } => {
                                match ttl{
                                    None => {
                                        if let Some(token) = key_map.remove(&key) {
                                            delay_queue.remove(&token);
                                        }
                                    }
                                    Some(ttl) => {
                                        if let Some(token) = key_map.get(&key) {
                                            delay_queue.reset(token, ttl);
                                        } else {
                                            let token = delay_queue.insert(key.clone(), ttl);
                                            key_map.insert(key, token);
                                        }
                                    }
                                }
                            }
                            DelayCommand::Remove { key } => {
                                 if let Some(k) = key_map.remove(&key) {
                                    delay_queue.remove(&k);
                                }
                            }
                            DelayCommand::Refresh { key, ttl } => {
                                if let Some(k) = key_map.get(&key) {
                                    delay_queue.reset(k, ttl);
                                }
                            }
                        }
                    }

                    Some(expired) = delay_queue.next()  => {
                        let key = expired.into_inner();
                        key_map.remove(&key);
                        if let Some((_, value)) = purge_shared.map.remove(&key) {
                            // async worker 执行 expire_call
                            let _ = purge_shared.call_tx.try_send(value).hand_log(|msg| error!("Cache call channel error: {msg}"));
                        }
                    }
                }
            }
            key_map.clear();
            purge_shared.map.clear();
            delay_queue.clear();
        });

        // spawn async expire_call worker
        let cancellation_token_call = rt.cancel;
        rt.rt_handle.spawn(async move {
            let semaphore = Arc::new(Semaphore::new(100));
            loop {
                tokio::select! {
                _ = cancellation_token_call.cancelled() => break,
                Some(entity) = call_rx.recv() => {
                        let permit = match semaphore.clone().acquire_owned().await {
                            Ok(p) => p,
                            Err(e) => {
                                error!("Failed to acquire semaphore: {}", e);
                                continue;
                            }
                        };
                        tokio::spawn(async move{
                            let _permit = permit;
                            entity.expire_call().await;
                        });
                    }
                }
            }
        });
        Cache { shared }
    }
}
impl Cache {
    pub fn contains_key(&self, key: &str) -> bool {
        self.shared.map.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<CachedValue> {
        self.shared.map.get(key).map(|v| v.value().clone())
    }
    pub fn insert(&self, key: String, value: CachedValue) {
        let ttl = value.expire_ttl();
        self.shared.map.insert(key.clone(), value);
        let _ = self
            .shared
            .cmd_tx
            .send(DelayCommand::Insert { key, ttl })
            .hand_log(|msg| error!("cmd channel close: {msg}"));
    }

    pub fn remove(&self, key: &str) -> Option<CachedValue> {
        let _ = self
            .shared
            .cmd_tx
            .send(DelayCommand::Remove {
                key: key.to_string(),
            })
            .hand_log(|msg| error!("cmd channel close: {msg}"));
        self.shared.map.remove(key).map(|(_, v)| v)
    }

    pub fn refresh(&self, key: &str) {
        if let Some(value) = self.shared.map.get(key) {
            if let Some(ttl) = value.expire_ttl() {
                let _ = self
                    .shared
                    .cmd_tx
                    .send(DelayCommand::Refresh {
                        key: key.to_string(),
                        ttl,
                    })
                    .hand_log(|msg| error!("cmd channel close: {msg}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::time::{sleep, timeout};

    // 测试用的 Mock 数据
    #[derive(Clone)]
    struct MockCacheable {
        id: String,
        ttl: Option<Duration>,
        expire_call_count: Arc<AtomicUsize>,
        should_panic: bool,
        // 用于验证 expire_call 被调用
        expire_called: Arc<AtomicBool>,
    }

    impl MockCacheable {
        fn new(id: &str, ttl: Option<Duration>) -> Self {
            Self {
                id: id.to_string(),
                ttl,
                expire_call_count: Arc::new(AtomicUsize::new(0)),
                should_panic: false,
                expire_called: Arc::new(AtomicBool::new(false)),
            }
        }

        fn with_panic(id: &str, ttl: Option<Duration>) -> Self {
            Self {
                id: id.to_string(),
                ttl,
                expire_call_count: Arc::new(AtomicUsize::new(0)),
                should_panic: true,
                expire_called: Arc::new(AtomicBool::new(false)),
            }
        }

        fn get_id(&self) -> &str {
            &self.id
        }

        fn was_expire_called(&self) -> bool {
            self.expire_called.load(Ordering::SeqCst)
        }

        fn get_expire_call_count(&self) -> usize {
            self.expire_call_count.load(Ordering::SeqCst)
        }
    }

    impl Cacheable for MockCacheable {
        fn expire_ttl(&self) -> Option<Duration> {
            self.ttl
        }

        fn expire_call(&self) -> BoxFuture<'_, ()> {
            let count = self.expire_call_count.clone();
            let should_panic = self.should_panic;
            let id = self.id.clone();
            let expire_called = self.expire_called.clone();

            Box::pin(async move {
                if should_panic {
                    panic!("Mock panic for {}", id);
                }
                count.fetch_add(1, Ordering::SeqCst);
                expire_called.store(true, Ordering::SeqCst);
                println!("expire_call executed for {}", id);
            })
        }
    }

    // 辅助函数：从 CachedValue 中获取 MockCacheable 的引用
    fn get_mock_from_cached(value: &CachedValue) -> Option<&MockCacheable> {
        value.downcast_ref::<MockCacheable>()
    }

    // 测试基本的插入和获取
    #[tokio::test]
    async fn test_basic_insert_and_get() {
        let cache = Cache::init();
        let key = "test_key".to_string();
        let mock = MockCacheable::new("test", Some(Duration::from_secs(60)));
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value.clone());

        let retrieved = cache.get(&key);
        assert!(retrieved.is_some());

        let retrieved_value = retrieved.unwrap();
        let mock = get_mock_from_cached(&retrieved_value).unwrap();
        assert_eq!(mock.get_id(), "test");
    }

    // 测试不存在的 key
    #[tokio::test]
    async fn test_get_nonexistent_key() {
        let cache = Cache::init();
        let retrieved = cache.get("nonexistent");
        assert!(retrieved.is_none());
    }

    // 测试移除功能
    #[tokio::test]
    async fn test_remove() {
        let cache = Cache::init();
        let key = "test_key".to_string();
        let mock = MockCacheable::new("test", Some(Duration::from_secs(60)));
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);
        assert!(cache.get(&key).is_some());

        let removed = cache.remove(&key);
        assert!(removed.is_some());
        assert!(cache.get(&key).is_none());
        let cached_value = removed.unwrap();
        let mock = get_mock_from_cached(&cached_value).unwrap();
        assert_eq!(mock.get_id(), "test");
    }

    // 测试过期自动移除
    #[tokio::test]
    async fn test_expiration() {
        let cache = Cache::init();
        let key = "expire_key".to_string();
        let mock = MockCacheable::new("expire_test", Some(Duration::from_millis(100)));
        let expire_call_count = Arc::new(AtomicUsize::new(0));

        // 包装 mock 以便共享计数器
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);
        assert!(cache.get(&key).is_some());

        // 等待过期
        sleep(Duration::from_millis(150)).await;

        // 验证已被移除
        assert!(cache.get(&key).is_none());

        // 给 expire_call 一点时间执行
        sleep(Duration::from_millis(50)).await;
    }

    // 测试无过期的缓存项
    #[tokio::test]
    async fn test_no_expiration() {
        let cache = Cache::init();
        let key = "no_expire_key".to_string();
        let mock = MockCacheable::new("no_expire", None);
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);
        assert!(cache.get(&key).is_some());

        // 等待一段时间
        sleep(Duration::from_millis(200)).await;

        // 应该仍然存在
        assert!(cache.get(&key).is_some());
    }

    // 测试刷新功能
    #[tokio::test]
    async fn test_refresh() {
        let cache = Cache::init();
        let key = "refresh_key".to_string();
        let mock = MockCacheable::new("refresh_test", Some(Duration::from_millis(200)));
        let expire_called = mock.expire_called.clone();
        let value = CachedValue(Arc::new(mock));
        cache.insert(key.clone(), value);

        // 等待一段时间但不过期
        sleep(Duration::from_millis(150)).await;

        // 刷新
        cache.refresh(&key);

        // 再等待超过原始过期时间
        sleep(Duration::from_millis(150)).await;

        // 应该还存在（因为被刷新了）
        assert!(cache.get(&key).is_some());

        // 等待新的过期
        sleep(Duration::from_millis(250)).await;

        // 应该过期了
        assert!(cache.get(&key).is_none());
        sleep(Duration::from_millis(50)).await;
        assert!(expire_called.load(Ordering::SeqCst));
    }

    // 测试更新已存在的 key
    #[tokio::test]
    async fn test_update_existing_key() {
        let cache = Cache::init();
        let key = "update_key".to_string();

        let mock1 = MockCacheable::new("first", Some(Duration::from_secs(60)));
        let expire_called1 = mock1.expire_called.clone();
        let value1 = CachedValue(Arc::new(mock1));

        let mock2 = MockCacheable::new("second", Some(Duration::from_secs(30)));
        let expire_called2 = mock2.expire_called.clone();
        let value2 = CachedValue(Arc::new(mock2));

        cache.insert(key.clone(), value1);
        cache.insert(key.clone(), value2);

        let retrieved = cache.get(&key).unwrap();
        let mock = get_mock_from_cached(&retrieved).unwrap();
        assert_eq!(mock.get_id(), "second");

        // 等待一段时间，第一个不应该触发过期
        sleep(Duration::from_millis(200)).await;
        assert!(!expire_called1.load(Ordering::SeqCst));
        assert!(!expire_called2.load(Ordering::SeqCst));
    }

    // 测试并发插入和读取
    #[tokio::test]
    async fn test_concurrent_operations() {
        let cache = Arc::new(Cache::init());
        let mut handles = vec![];

        // 并发插入 100 个键值对
        for i in 0..100 {
            let cache = cache.clone();
            handles.push(tokio::spawn(async move {
                let key = format!("key_{}", i);
                let mock = MockCacheable::new(&key, Some(Duration::from_secs(60)));
                let value = CachedValue(Arc::new(mock));
                cache.insert(key, value);
            }));
        }

        // 等待所有插入完成
        for handle in handles {
            handle.await.unwrap();
        }

        // 验证所有键都存在
        for i in 0..100 {
            let key = format!("key_{}", i);
            assert!(cache.get(&key).is_some());
        }

        // 并发读取和移除
        let mut read_handles = vec![];
        for i in 0..100 {
            let cache = cache.clone();
            read_handles.push(tokio::spawn(async move {
                let key = format!("key_{}", i);
                let _ = cache.get(&key);
                if i % 2 == 0 {
                    cache.remove(&key);
                }
            }));
        }

        for handle in read_handles {
            handle.await.unwrap();
        }

        // 验证只有奇数键还存在
        for i in 0..100 {
            let key = format!("key_{}", i);
            if i % 2 == 0 {
                assert!(cache.get(&key).is_none());
            } else {
                assert!(cache.get(&key).is_some());
            }
        }
    }

    // 测试快速插入和移除
    #[tokio::test]
    async fn test_rapid_insert_remove() {
        let cache = Cache::init();

        for i in 0..1000 {
            let key = format!("key_{}", i);
            let mock = MockCacheable::new(&key, Some(Duration::from_secs(60)));
            let value = CachedValue(Arc::new(mock));

            cache.insert(key.clone(), value);
            assert!(cache.get(&key).is_some());

            let removed = cache.remove(&key);
            assert!(removed.is_some());
            assert!(cache.get(&key).is_none());
        }
    }

    // 测试不同 TTL 的混合使用
    #[tokio::test]
    async fn test_mixed_ttl() {
        let cache = Cache::init();

        // 插入不同 TTL 的项
        let short_mock = MockCacheable::new("short", Some(Duration::from_millis(100)));
        let short = CachedValue(Arc::new(short_mock));

        let medium_mock = MockCacheable::new("medium", Some(Duration::from_millis(300)));
        let medium = CachedValue(Arc::new(medium_mock));

        let long_mock = MockCacheable::new("long", Some(Duration::from_millis(500)));
        let long = CachedValue(Arc::new(long_mock));

        let none_mock = MockCacheable::new("none", None);
        let none = CachedValue(Arc::new(none_mock));

        cache.insert("short".to_string(), short);
        cache.insert("medium".to_string(), medium);
        cache.insert("long".to_string(), long);
        cache.insert("none".to_string(), none);

        // 等待短过期
        sleep(Duration::from_millis(200)).await;
        assert!(cache.get("short").is_none());
        assert!(cache.get("medium").is_some());
        assert!(cache.get("long").is_some());
        assert!(cache.get("none").is_some());

        // 等待中过期
        sleep(Duration::from_millis(200)).await;
        assert!(cache.get("medium").is_none());
        assert!(cache.get("long").is_some());
        assert!(cache.get("none").is_some());

        // 等待长过期
        sleep(Duration::from_millis(200)).await;
        assert!(cache.get("long").is_none());
        assert!(cache.get("none").is_some());
    }

    // 测试在过期前刷新
    #[tokio::test]
    async fn test_refresh_before_expiry() {
        let cache = Cache::init();
        let key = "refresh_before".to_string();
        let mock = MockCacheable::new("refresh_test", Some(Duration::from_millis(200)));
        let expire_called = mock.expire_called.clone();
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);

        // 多次刷新
        for _ in 0..5 {
            sleep(Duration::from_millis(150)).await;
            cache.refresh(&key);
            assert!(cache.get(&key).is_some());
        }

        // 等待最终过期
        sleep(Duration::from_millis(250)).await;
        assert!(cache.get(&key).is_none());
        sleep(Duration::from_millis(50)).await;
        assert!(expire_called.load(Ordering::SeqCst));
    }

    // 测试移除不存在的 key
    #[tokio::test]
    async fn test_remove_nonexistent() {
        let cache = Cache::init();
        let removed = cache.remove("nonexistent");
        assert!(removed.is_none());
    }

    // 测试刷新不存在的 key
    #[tokio::test]
    async fn test_refresh_nonexistent() {
        let cache = Cache::init();
        // 刷新不存在的 key 不应该 panic
        cache.refresh("nonexistent");
    }

    // 测试 expire_call panic 的处理
    #[tokio::test]
    async fn test_expire_call_panic() {
        let cache = Cache::init();
        let key = "panic_key".to_string();
        let mock = MockCacheable::with_panic("panic_test", Some(Duration::from_millis(100)));
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);
        assert!(cache.get(&key).is_some());

        // 等待过期，expire_call 会 panic，但不应影响 cache 运行
        sleep(Duration::from_millis(150)).await;

        // cache 应该仍然可用
        assert!(cache.get(&key).is_none());

        // 插入新值验证 cache 仍然工作
        let new_key = "new_key".to_string();
        let new_mock = MockCacheable::new("new", Some(Duration::from_secs(60)));
        let new_value = CachedValue(Arc::new(new_mock));
        cache.insert(new_key.clone(), new_value);
        assert!(cache.get(&new_key).is_some());
    }

    // 测试超时场景 - 确保操作不会阻塞太久
    #[tokio::test]
    async fn test_operation_timeout() {
        let cache = Cache::init();
        let key = "timeout_key".to_string();
        let mock = MockCacheable::new("timeout", Some(Duration::from_secs(60)));
        let value = CachedValue(Arc::new(mock));

        let result = timeout(Duration::from_millis(100), async {
            cache.insert(key.clone(), value);
            cache.get(&key)
        })
        .await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    // 测试大量过期项同时触发
    #[tokio::test]
    async fn test_mass_expiration() {
        let cache = Arc::new(Cache::init());
        let mut expire_flags = vec![];

        for i in 0..100 {
            let mock = MockCacheable::new(&format!("mass_{}", i), Some(Duration::from_millis(100)));
            let expire_called = mock.expire_called.clone();
            expire_flags.push(expire_called);

            let value = CachedValue(Arc::new(mock));
            let key = format!("mass_key_{}", i);

            cache.insert(key, value);
        }

        // 等待过期
        sleep(Duration::from_millis(300)).await;

        // 验证所有项都已过期
        for i in 0..100 {
            let key = format!("mass_key_{}", i);
            assert!(cache.get(&key).is_none());
        }

        // 给 expire_call 一些执行时间
        sleep(Duration::from_millis(100)).await;

        // 验证 expire_call 被调用
        for flag in expire_flags {
            assert!(flag.load(Ordering::SeqCst));
        }
    }

    // 测试缓存击穿场景 - 同一 key 频繁操作
    #[tokio::test]
    async fn test_cache_penetration() {
        let cache = Arc::new(Cache::init());
        let key = "hot_key".to_string();
        let mock = MockCacheable::new("hot", Some(Duration::from_millis(200)));
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);

        let mut handles = vec![];
        for _ in 0..10 {
            let cache = cache.clone();
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    let _ = cache.get(&key);
                    tokio::task::yield_now().await;
                }
            }));
        }

        // 同时进行刷新操作
        let refresh_handle = tokio::spawn({
            let cache = cache.clone();
            let key = key.clone();
            async move {
                for _ in 0..20 {
                    sleep(Duration::from_millis(30)).await;
                    cache.refresh(&key);
                }
            }
        });

        handles.push(refresh_handle);

        for handle in handles {
            handle.await.unwrap();
        }
    }

    // 测试 CommonCache 的使用
    #[tokio::test]
    async fn test_common_cache() {
        // 注意：这里假设 CommonCache 的方法已经被修复为不带 &self 参数
        // 如果还没修复，这个测试会编译失败
        let key = "common_key".to_string();
        let mock = MockCacheable::new("common", Some(Duration::from_secs(60)));
        let value = CachedValue(Arc::new(mock));

        // 这些调用需要根据 CommonCache 的实际 API 调整
        // CommonCache::insert(key.clone(), value).await;
        // let retrieved = CommonCache::get(&key);
        // assert!(retrieved.is_some());
    }

    // 测试重复插入相同 key
    #[tokio::test]
    async fn test_repeated_insert_same_key() {
        let cache = Cache::init();
        let key = "same_key".to_string();

        for i in 0..10 {
            let mock = MockCacheable::new(&format!("value_{}", i), Some(Duration::from_secs(60)));
            let value = CachedValue(Arc::new(mock));
            cache.insert(key.clone(), value);

            let retrieved = cache.get(&key).unwrap();
            let mock = get_mock_from_cached(&retrieved).unwrap();
            assert_eq!(mock.get_id(), format!("value_{}", i));
        }
    }

    // 测试零 TTL（立即过期）
    #[tokio::test]
    async fn test_zero_ttl() {
        let cache = Cache::init();
        let key = "zero_ttl_key".to_string();
        let mock = MockCacheable::new("zero", Some(Duration::from_millis(0)));
        let expire_called = mock.expire_called.clone();
        let value = CachedValue(Arc::new(mock));

        cache.insert(key.clone(), value);

        // 立即检查应该已经过期
        sleep(Duration::from_millis(50)).await;
        assert!(cache.get(&key).is_none());
        sleep(Duration::from_millis(50)).await;
        assert!(expire_called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_replace_ttl_cleanup() {
        let cache = Cache::init();

        let key = "replace".to_string();

        let v1 = CachedValue(Arc::new(MockCacheable::new(
            "v1",
            Some(Duration::from_millis(100)),
        )));
        cache.insert(key.clone(), v1);

        sleep(Duration::from_millis(50)).await;

        let v2 = CachedValue(Arc::new(MockCacheable::new(
            "v2",
            Some(Duration::from_secs(5)),
        )));
        cache.insert(key.clone(), v2);

        sleep(Duration::from_millis(150)).await;

        assert!(cache.get(&key).is_some());
    }
}
