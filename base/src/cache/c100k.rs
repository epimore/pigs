/*
异步缓存超时组件
存储容量固定 512k：若业务数据量超过此值，需要增加分片数或提高 128_000 上限。
内存：45~85MB;
Max(ttl):512 * 128 * 200ms ≈ 3.6 小时
2核:
稳定 250k~400k
安全 150k~300k
极限 1.5M
4核:
稳定 600k~900k
安全 400k~700k
极限 3M
8核:
稳定 1.0M~1.6M
安全 700k~1.2M
极限 5M
*/

use std::{
    collections::HashMap,
    hash::{Hash},
    mem::MaybeUninit,
    time::Duration,
};

use ahash::{RandomState};
use exception::GlobalResultExt;
use log::error;
use once_cell::sync::Lazy;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::{interval_at, Instant};
//
// ================= CONFIG =================
//

const SHARDS: usize = 4;
const WHEEL_L1: usize = 512;
const WHEEL_L2: usize = 128;

const TICK_MS: u64 = 200;
const CHANNEL: usize = 65536;
const BATCH: usize = 64;
const COMPACT_THRESHOLD: usize = 1024;
const DEFAULT_CAPACITY_PER_SHARD: usize = 128_000;

//
// ================= TRAIT =================
//

pub trait CacheKey: Hash + Eq + Clone + Send + Sync + 'static {}
impl<T: Hash + Eq + Clone + Send + Sync + 'static> CacheKey for T {}

//
// ================= HASH =================
//
static HASHER: Lazy<RandomState> = Lazy::new(RandomState::new);
#[inline]
fn hash<K: Hash>(k: &K) -> u64 {
    HASHER.hash_one(k)
}

//
// ================= EVENT =================
//

#[derive(Clone, Debug)]
pub struct CacheEvent<K> {
    pub key: K,
    pub hash: u64,
    pub version: u32,
}

//
// ================= COMMAND =================
//

enum Command<K> {
    Insert { key: K, ttl: u64 },
    Refresh { key: K },
    Delete { key: K },
}

//
// ================= ENTRY =================
//

pub struct Entry<K> {
    key: MaybeUninit<K>,
    hash: u64,
    version: u32,
    expire_tick: u64,
    ttl_tick: u64,

    next_free: u32,
    in_use: bool,
}

impl<K> Entry<K> {
    #[inline]
    fn new_empty(next_free: u32) -> Self {
        Self {
            key: MaybeUninit::uninit(),
            hash: 0,
            version: 0,
            expire_tick: 0,
            ttl_tick: 0,
            next_free,
            in_use: false,
        }
    }

    #[inline]
    fn write(&mut self, key: K, hash: u64, ttl_tick: u64, now_tick: u64) {
        self.key.write(key);
        self.hash = hash;
        self.version = 1;
        self.ttl_tick = ttl_tick;
        self.expire_tick = now_tick + ttl_tick;
        self.in_use = true;
    }

    #[inline]
    unsafe fn key(&self) -> &K {
        self.key.assume_init_ref()
    }

    #[inline]
    unsafe fn drop_key(&mut self) {
        std::ptr::drop_in_place(self.key.as_mut_ptr());
    }

    /// Move key out without clone
    #[inline]
    unsafe fn take_key(&mut self) -> K {
        self.in_use = false;
        self.key.as_ptr().read()
    }
}

impl<K> Drop for Entry<K> {
    fn drop(&mut self) {
        if self.in_use {
            unsafe {
                std::ptr::drop_in_place(self.key.as_mut_ptr());
            }
        }
    }
}

//
// ================= SHARD =================
//

struct Shard<K: CacheKey> {
    entries: Vec<Entry<K>>,
    free_head: u32,

    // hash -> collision chain
    index_map: HashMap<u64, Vec<u32>>,

    // (idx, version)
    wheel_l1: Vec<Vec<(u32, u32)>>,
    wheel_l2: Vec<Vec<(u32, u32)>>,

    tick: u64,

    cmd_rx: Receiver<Command<K>>,
    event_tx: Sender<Vec<CacheEvent<K>>>,

    batch: Vec<CacheEvent<K>>,
}

impl<K: CacheKey> Shard<K> {
    fn new(
        capacity: usize,
        cmd_rx: Receiver<Command<K>>,
        event_tx: Sender<Vec<CacheEvent<K>>>,
    ) -> Self {
        let mut entries = Vec::with_capacity(capacity);

        for i in 0..capacity {
            let next = (i + 1) as u32;
            entries.push(Entry::new_empty(next));
        }

        Self {
            entries,
            free_head: 0,
            index_map: HashMap::with_capacity(capacity),

            wheel_l1: vec![Vec::new(); WHEEL_L1],
            wheel_l2: vec![Vec::new(); WHEEL_L2],

            tick: 0,

            cmd_rx,
            event_tx,

            batch: Vec::with_capacity(BATCH),
        }
    }

    //
    // ---------- slab alloc/free ----------
    //

    fn alloc(&mut self) -> Option<u32> {
        let idx = self.free_head;

        if idx as usize >= self.entries.len() {
            return None;
        }

        self.free_head = self.entries[idx as usize].next_free;
        Some(idx)
    }

    fn free(&mut self, idx: u32) {
        let e = &mut self.entries[idx as usize];

        if e.in_use {
            unsafe {
                e.drop_key();
            }
        }

        e.in_use = false;
        e.next_free = self.free_head;
        self.free_head = idx;
    }

    //
    // ---------- index map ----------
    //

    fn insert_index(&mut self, hash: u64, idx: u32) {
        self.index_map.entry(hash).or_default().push(idx);
    }

    fn remove_index(&mut self, hash: u64, idx: u32) {
        if let Some(v) = self.index_map.get_mut(&hash) {
            if let Some(pos) = v.iter().position(|&x| x == idx) {
                v.swap_remove(pos);
            }
            if v.is_empty() {
                self.index_map.remove(&hash);
            }
        }
    }

    fn find_entry(&self, key: &K, hash: u64) -> Option<u32> {
        let list = self.index_map.get(&hash)?;

        for &idx in list {
            let e = &self.entries[idx as usize];

            if e.in_use {
                unsafe {
                    if e.key() == key {
                        return Some(idx);
                    }
                }
            }
        }

        None
    }

    //
    // ---------- timing wheel ----------
    //

    fn schedule(&mut self, idx: u32) {
        let e = &self.entries[idx as usize];

        let diff = e.expire_tick - self.tick;

        if diff < WHEEL_L1 as u64 {
            let slot = e.expire_tick as usize % WHEEL_L1;
            self.wheel_l1[slot].push((idx, e.version));
        } else {
            let slot = (e.expire_tick / WHEEL_L1 as u64) as usize % WHEEL_L2;
            self.wheel_l2[slot].push((idx, e.version));
        }
    }

    fn cascade(&mut self) {
        let slot = (self.tick / WHEEL_L1 as u64) as usize % WHEEL_L2;

        let mut bucket = std::mem::take(&mut self.wheel_l2[slot]);

        for (idx, ver) in bucket.drain(..) {
            let e = &self.entries[idx as usize];

            if e.in_use && e.version == ver {
                self.schedule(idx);
            }
        }
    }

    fn compact_bucket(&mut self, bucket: &mut Vec<(u32, u32)>) {
        bucket.retain(|(idx, ver)| {
            let e = &self.entries[*idx as usize];
            e.in_use && e.version == *ver
        });
    }

    //
    // ---------- commands ----------
    //

    fn insert(&mut self, key: K, ttl_tick: u64) {
        let h = hash(&key);

        if let Some(old_idx) = self.find_entry(&key, h) {
            self.remove_index(h, old_idx);
            self.free(old_idx);
        }

        let idx = match self.alloc() {
            Some(i) => i,
            None => return,
        };

        let e = &mut self.entries[idx as usize];
        e.write(key, h, ttl_tick, self.tick);

        self.insert_index(h, idx);
        self.schedule(idx);
    }

    fn refresh(&mut self, key: K) {
        let h = hash(&key);

        if let Some(idx) = self.find_entry(&key, h) {
            let e = &mut self.entries[idx as usize];

            e.version += 1;
            e.expire_tick = self.tick + e.ttl_tick;

            self.schedule(idx);
        }
    }

    fn delete(&mut self, key: K) {
        let h = hash(&key);

        if let Some(idx) = self.find_entry(&key, h) {
            self.remove_index(h, idx);
            self.free(idx);
        }
    }

    //
    // ---------- tick ----------
    //

    fn on_tick(&mut self) {
        self.tick += 1;

        if self.tick % WHEEL_L1 as u64 == 0 {
            self.cascade();
        }

        let slot = self.tick as usize % WHEEL_L1;
        let mut bucket = std::mem::take(&mut self.wheel_l1[slot]);

        if bucket.len() > COMPACT_THRESHOLD {
            self.compact_bucket(&mut bucket);
        }

        for (idx, ver) in bucket.drain(..) {
            let e = &self.entries[idx as usize];

            // version validation
            if !e.in_use || e.version != ver {
                continue;
            }

            // lazy re-schedule
            if e.expire_tick > self.tick {
                self.schedule(idx);
                continue;
            }

            // expire
            let hash = e.hash;
            let version = e.version;

            let key = unsafe { self.entries[idx as usize].take_key() };

            self.remove_index(hash, idx);

            // recycle slab slot
            let e = &mut self.entries[idx as usize];
            e.next_free = self.free_head;
            self.free_head = idx;

            self.batch.push(CacheEvent { key, hash, version });

            if self.batch.len() >= BATCH {
                let _ = self
                    .event_tx
                    .try_send(std::mem::take(&mut self.batch))
                    .hand_log(|msg| error!("{msg}"));
            }
        }

        if !self.batch.is_empty() {
            let _ = self
                .event_tx
                .try_send(std::mem::take(&mut self.batch))
                .hand_log(|msg| error!("{msg}"));
        }
    }

    fn handle(&mut self, cmd: Command<K>) {
        match cmd {
            Command::Insert { key, ttl } => self.insert(key, ttl),
            Command::Refresh { key } => self.refresh(key),
            Command::Delete { key } => self.delete(key),
        }
    }
    //
    // ---------- worker ----------
    //

    async fn run(mut self) {
        let mut ticker = interval_at(
            Instant::now() + Duration::from_millis(TICK_MS),
            Duration::from_millis(TICK_MS),
        );

        loop {
            tokio::select! {
                _ = ticker.tick() => self.on_tick(),

                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle(cmd);
                    for _ in 0..256 {
                        match self.cmd_rx.try_recv() {
                            Ok(cmd) => self.handle(cmd),
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }
}

//
// ================= CACHE =================
//

pub struct Cache<K: CacheKey> {
    shards: Vec<Sender<Command<K>>>,
    event_rx: Mutex<Receiver<Vec<CacheEvent<K>>>>,
}

impl<K: CacheKey> Default for Cache<K> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY_PER_SHARD, SHARDS)
    }
}

impl<K: CacheKey> Cache<K> {
    #[inline]
    fn shard(&self, hash: u64) -> &Sender<Command<K>> {
        &self.shards[(hash as usize) & (self.shards.len() - 1)]
    }

    #[inline]
    fn ttl_to_tick(ttl: Duration) -> u64 {
        ((ttl.as_millis() as u64) + TICK_MS - 1) / TICK_MS
    }

    //
    // ---------- public api ----------
    //

    //shards.len() 必须是 2^n
    pub fn with_capacity(capacity_per_shard: usize, shard_len: usize) -> Self {
        assert!(shard_len.is_power_of_two());
        let (event_tx, event_rx) = channel(CHANNEL);

        let mut shards = Vec::new();

        for _ in 0..shard_len {
            let (tx, rx) = channel(CHANNEL);

            let shard = Shard::new(capacity_per_shard, rx, event_tx.clone());

            tokio::spawn(shard.run());

            shards.push(tx);
        }

        Self {
            shards,
            event_rx: Mutex::new(event_rx),
        }
    }

    pub fn insert(&self, key: K, ttl: Duration) {
        let h = hash(&key);
        let ttl_tick = Self::ttl_to_tick(ttl);

        let _ = self
            .shard(h)
            .try_send(Command::Insert { key, ttl: ttl_tick })
            .hand_log(|msg| error!("{msg}"));
    }

    pub fn refresh(&self, key: K) {
        let h = hash(&key);

        let _ = self
            .shard(h)
            .try_send(Command::Refresh { key })
            .hand_log(|msg| error!("{msg}"));
    }

    pub fn delete(&self, key: K) {
        let h = hash(&key);

        let _ = self
            .shard(h)
            .try_send(Command::Delete { key })
            .hand_log(|msg| error!("{msg}"));
    }

    pub async fn next_batch(&self) -> Option<Vec<CacheEvent<K>>> {
        let mut rx = self.event_rx.lock().await;
        rx.recv().await
    }
}

// ================= TEST =================
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{sleep, timeout};

    #[tokio::test]
    async fn test_expire() {
        let c = Cache::default();
        c.insert("a".to_string(), Duration::from_millis(2000));
        assert!(timeout(Duration::from_millis(1000), c.next_batch())
            .await
            .is_err());
        sleep(Duration::from_millis(1200)).await;

        assert!(timeout(Duration::from_millis(1000), c.next_batch())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_refresh() {
        let c = Cache::default();

        c.insert("a".to_string(), Duration::from_millis(500));
        c.insert("b".to_string(), Duration::from_millis(1200));
        c.insert("c".to_string(), Duration::from_millis(1200));

        sleep(Duration::from_millis(1000)).await;
        assert_eq!(
            timeout(Duration::from_millis(150), c.next_batch())
                .await
                .unwrap()
                .unwrap()[0]
                .key,
            "a".to_string()
        );

        c.refresh("b".to_string());
        sleep(Duration::from_millis(800)).await;
        assert_eq!(
            timeout(Duration::from_millis(100), c.next_batch())
                .await
                .unwrap()
                .unwrap()[0]
                .key,
            "c".to_string()
        );
        sleep(Duration::from_millis(500)).await;
        // println!("{:?}",c.next_batch().await);
        assert_eq!(
            timeout(Duration::from_millis(100), c.next_batch())
                .await
                .unwrap()
                .unwrap()[0]
                .key,
            "b".to_string()
        );
        // let x = c.next_batch().await;
        // println!("{:?}",x);
    }

    #[tokio::test]
    async fn test_long_ttl() {
        let c = Cache::default();

        c.insert("b".to_string(), Duration::from_secs(2));

        sleep(Duration::from_secs(3)).await;

        let e = c.next_batch().await.unwrap();
        assert_eq!(e[0].key, "b");
    }

    #[tokio::test]
    async fn test_delete() {
        let cache = Cache::<String>::default();

        cache.insert("dead".to_string(), Duration::from_secs(1));
        cache.delete("dead".to_string());

        let r = timeout(Duration::from_millis(1200), cache.next_batch()).await;
        assert!(r.is_err());
    }
    use once_cell::sync::Lazy;
    #[derive(Hash, Eq, PartialEq, Clone, Debug)]
    pub enum ExpireKey {
        Session(u64),
        Stream(u64),
        Device(String),
    }
    pub static TTL_CACHE: Lazy<Cache<ExpireKey>> = Lazy::new(|| Cache::default());
    #[tokio::test]
    async fn single_instance() {
        TTL_CACHE.insert(ExpireKey::Session(123), Duration::from_secs(2));
        TTL_CACHE.insert(ExpireKey::Stream(456), Duration::from_secs(2));
        TTL_CACHE.insert(ExpireKey::Device("abc".to_string()), Duration::from_secs(2));
        sleep(Duration::from_secs(1)).await;
        TTL_CACHE.delete(ExpireKey::Stream(456));
        TTL_CACHE.refresh(ExpireKey::Device("abc".to_string()));
        assert!(timeout(Duration::from_secs(5), async {
            loop {
                if let Some(batch) = TTL_CACHE.next_batch().await {
                    for ev in batch {
                        println!("expired {:?},version {}", ev.key, ev.version);
                    }
                }
            }
        })
        .await
        .is_err());
    }

    #[tokio::test]
    async fn test_duplicate_insert_overwrite() {
        let c = Cache::default();

        c.insert("dup".to_string(), Duration::from_millis(500));
        c.insert("dup".to_string(), Duration::from_millis(1200));

        // 600ms 后不应过期（第一次已被覆盖）
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(tokio::time::timeout(Duration::from_millis(100), c.next_batch())
            .await
            .is_err());

        // 再等足够时间，应只收到一次
        tokio::time::sleep(Duration::from_millis(700)).await;

        let batch = c.next_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].key, "dup");
    }
}
