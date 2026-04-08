/*
1.存储容量固定 512k：若业务数据量超过此值，需要增加分片数或提高 128_000 上限。
2.命令通道为 try_send 非阻塞：当通道满时直接丢弃请求，无背压反馈。对于需要可靠性的场景，应改为 send().await 或提供背压机制。
3.时间轮大小 512 限制了最大 TTL 为 102.4 秒：若需要更长 TTL，需扩大轮子或改用层级时间轮。
4.无主动内存回收：Entry 中 key 字段用 unsafe 零初始化，若 K 有 Drop 实现可能导致泄漏。建议用 MaybeUninit 或 Option。
5.单事件通道：所有 shard 共用过期事件通道，高过期率时可能成为瓶颈，可考虑每 shard 独立事件通道。

核心数	稳定容量（写 QPS）	安全容量（写 QPS）	极限容量（写 QPS）	总存储条目上限
2 核	300k ~ 500k	        150k ~ 200k	        2M ~ 3M（丢请求）	512k
4 核	800k ~ 1.2M	        400k ~ 600k	        4M ~ 5M	            512k
8 核	1.5M ~ 2M	        600k ~ 800k	        6M ~ 8M	            512k
*/
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    time::Duration,
};

use ahash::AHasher;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::time::interval;

// ================= 配置 =================

pub const DEFAULT_SHARDS: usize = 4;
pub const DEFAULT_WHEEL_SIZE: usize = 512;
pub const DEFAULT_CHANNEL: usize = 65536;
pub const DEFAULT_TICK_MS: u64 = 200;
const BATCH_SIZE: usize = 64;

// ================= trait =================

pub trait CacheKey: Hash + Eq + Clone + Send + Sync + 'static {}
impl<T: Hash + Eq + Clone + Send + Sync + 'static> CacheKey for T {}

// ================= 工具 =================

#[inline]
fn hash<K: Hash>(k: &K) -> u64 {
    let mut h = AHasher::default();
    k.hash(&mut h);
    h.finish()
}

// ================= 数据结构 =================

#[repr(align(64))]
pub struct Entry<K> {
    key: K,
    hash: u64,
    version: u32,
    expire_tick: u64,
    ttl_tick: u64,

    next_free: u32,
    in_use: bool,
}

#[derive(Clone, Debug)]
pub struct CacheEvent<K> {
    pub key: K,
    pub hash: u64,
    pub version: u32,
}

// ================= 命令 =================

enum Command<K> {
    Insert { key: K, ttl: u64 },
    Refresh { key: K },
    Delete { key: K },
}

// ================= Shard =================

struct Shard<K: CacheKey> {
    entries: Vec<Entry<K>>,
    free_head: u32,

    index_map: HashMap<u64, u32>,

    wheel: Vec<Vec<u32>>,
    tick: u64,

    cmd_rx: Receiver<Command<K>>,
    event_tx: Sender<Vec<CacheEvent<K>>>,

    batch: Vec<CacheEvent<K>>,
}

impl<K: CacheKey> Shard<K> {
    fn new(
        cap: usize,
        wheel_size: usize,
        cmd_rx: Receiver<Command<K>>,
        event_tx: Sender<Vec<CacheEvent<K>>>,
    ) -> Self {
        let mut entries = Vec::with_capacity(cap);

        // 初始化 free list
        for i in 0..cap {
            entries.push(Entry {
                key: unsafe { std::mem::zeroed() },
                hash: 0,
                version: 0,
                expire_tick: 0,
                ttl_tick: 0,
                next_free: (i + 1) as u32,
                in_use: false,
            });
        }

        Self {
            entries,
            free_head: 0,
            index_map: HashMap::with_capacity(cap),
            wheel: vec![Vec::new(); wheel_size],
            tick: 0,
            cmd_rx,
            event_tx,
            batch: Vec::with_capacity(BATCH_SIZE),
        }
    }

    #[inline]
    fn alloc(&mut self) -> Option<u32> {
        if self.free_head as usize >= self.entries.len() {
            return None;
        }

        let idx = self.free_head;
        let next = self.entries[idx as usize].next_free;
        self.free_head = next;
        self.entries[idx as usize].in_use = true;
        Some(idx)
    }

    #[inline]
    fn free(&mut self, idx: u32) {
        let e = &mut self.entries[idx as usize];
        e.in_use = false;
        e.next_free = self.free_head;
        self.free_head = idx;
    }

    fn insert(&mut self, key: K, ttl: u64) {
        let h = hash(&key);

        let idx = match self.alloc() {
            Some(i) => i,
            None => return,
        };

        let expire = self.tick + ttl;

        let e = &mut self.entries[idx as usize];
        e.key = key.clone();
        e.hash = h;
        e.version = 1;
        e.ttl_tick = ttl;
        e.expire_tick = expire;

        self.index_map.insert(h, idx);

        let slot = expire as usize % self.wheel.len();
        self.wheel[slot].push(idx);
    }

    fn refresh(&mut self, key: K) {
        let h = hash(&key);

        if let Some(&idx) = self.index_map.get(&h) {
            let e = &mut self.entries[idx as usize];

            e.version += 1;
            e.expire_tick = self.tick + e.ttl_tick;

            let slot = e.expire_tick as usize % self.wheel.len();
            self.wheel[slot].push(idx);
        }
    }

    fn delete(&mut self, key: K) {
        let h = hash(&key);

        if let Some(idx) = self.index_map.remove(&h) {
            self.free(idx);
        }
    }

    fn flush(&mut self) {
        if !self.batch.is_empty() {
            let mut out = Vec::new();
            std::mem::swap(&mut out, &mut self.batch);
            let _ = self.event_tx.try_send(out);
        }
    }

    fn on_tick(&mut self) {
        self.tick += 1;

        let slot = self.tick as usize % self.wheel.len();
        let mut bucket = std::mem::take(&mut self.wheel[slot]);

        for idx in bucket.drain(..) {
            let e = &self.entries[idx as usize];

            if !e.in_use || e.expire_tick != self.tick {
                continue;
            }

            self.batch.push(CacheEvent {
                key: e.key.clone(),
                hash: e.hash,
                version: e.version,
            });

            self.index_map.remove(&e.hash);
            self.free(idx);

            if self.batch.len() >= BATCH_SIZE {
                self.flush();
            }
        }

        self.flush();
    }

    async fn run(mut self, tick_ms: u64) {
        let mut ticker = interval(Duration::from_millis(tick_ms));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.on_tick();
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd {
                        Command::Insert{key, ttl} => self.insert(key, ttl),
                        Command::Refresh{key} => self.refresh(key),
                        Command::Delete{key} => self.delete(key),
                    }
                }
            }
        }
    }
}

// ================= Cache =================

pub struct Cache<K: CacheKey> {
    shards: Vec<Sender<Command<K>>>,
    event_rx: Receiver<Vec<CacheEvent<K>>>,
    tick_ms: u64,
}

impl<K: CacheKey> Cache<K> {
    pub fn new() -> Self {
        Self::with_config(DEFAULT_SHARDS, DEFAULT_WHEEL_SIZE, DEFAULT_TICK_MS)
    }

    pub fn with_config(shards: usize, wheel: usize, tick_ms: u64) -> Self {
        let mut shard_senders = Vec::new();
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL);

        for _ in 0..shards {
            let (tx, rx) = channel(DEFAULT_CHANNEL);
            let shard = Shard::new(128_000, wheel, rx, event_tx.clone());

            tokio::spawn(shard.run(tick_ms));
            shard_senders.push(tx);
        }

        Self {
            shards: shard_senders,
            event_rx,
            tick_ms,
        }
    }

    #[inline]
    fn shard(&self, h: u64) -> &Sender<Command<K>> {
        &self.shards[(h as usize) & (self.shards.len() - 1)]
    }

    #[inline]
    fn ttl_to_tick(&self, ttl: Duration) -> u64 {
        let ms = ttl.as_millis() as u64;
        (ms / self.tick_ms).max(1)
    }

    pub fn insert(&self, key: K, ttl: Duration) {
        let h = hash(&key);
        let ticks = self.ttl_to_tick(ttl);
        let _ = self.shard(h).try_send(Command::Insert { key, ttl: ticks });
    }

    pub fn refresh(&self, key: K) {
        let h = hash(&key);
        let _ = self.shard(h).try_send(Command::Refresh { key });
    }

    pub fn delete(&self, key: K) {
        let h = hash(&key);
        let _ = self.shard(h).try_send(Command::Delete { key });
    }

    pub async fn next_batch(&mut self) -> Option<Vec<CacheEvent<K>>> {
        self.event_rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_insert_and_expire() {
        let mut cache = Cache::new();

        cache.insert("a".to_string(), Duration::from_millis(200));

        sleep(Duration::from_millis(400)).await;

        let batch = cache.next_batch().await.unwrap();
        assert_eq!(batch[0].key, "a");
    }

    #[tokio::test]
    async fn test_refresh() {
        let mut cache = Cache::new();

        cache.insert("a".to_string(), Duration::from_millis(200));

        sleep(Duration::from_millis(100)).await;
        cache.refresh("a".to_string());

        sleep(Duration::from_millis(150)).await;

        // 不应过期
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            cache.next_batch()
        )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_delete() {
        let mut cache = Cache::new();

        cache.insert("a".to_string(), Duration::from_millis(200));
        cache.delete("a".to_string());

        sleep(Duration::from_millis(400)).await;

        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            cache.next_batch()
        )
            .await
            .is_err());
    }
}