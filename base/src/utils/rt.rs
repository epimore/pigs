use crate::daemon::signal::{ExitSignal, Signal};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use exception::{GlobalError, GlobalResult};
use futures::FutureExt;
use log::{error, info, warn};
use once_cell::sync::Lazy;
use std::fmt::Debug;
use std::future::Future;
use std::panic::{resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub const APPLICATION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(8);
pub const DAEMON_STOP_TIMEOUT_SECS: u64 = 10;

/// | 协议类型 | 推荐线程数 | 运行时类型 | 理由 |
/// | --- | --- | --- | --- |
/// | HTTP API | 2-4 | 多线程 | 短连接，高并发，I/O 等待多 |
/// | WebSocket | 4-8 | 多线程 | 长连接，状态维护，中等并发 |
/// | TCP Server | 6-12 | 多线程 | 重量级连接，复杂协议处理 |
/// | UDP Service | 1 | 当前线程 | 无连接，高吞吐，单线程高效 |
/// | RPC Service | 4-8 | 多线程 | 中等负载，序列化开销 |
/// | Proxy Service | 8+ | 多线程 | 高吞吐，数据转发 |
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RuntimeType {
    Main,
    CommonNetwork,
    HttpApi,
    WebSocket,
    RpcService,
    MessageQueue,
    CommonIO,
    FileProcessing,
    Database,
    CacheService,
    CommonCompute,
    DataProcessing,
    ImageProcessing,
    MachineLearning,
    Custom(String),
}

impl RuntimeType {
    pub fn as_thread_name(&self) -> String {
        match self {
            RuntimeType::Main => "main".to_string(),
            RuntimeType::CommonNetwork => "common-network".to_string(),
            RuntimeType::HttpApi => "http-api".to_string(),
            RuntimeType::WebSocket => "websocket".to_string(),
            RuntimeType::RpcService => "rpc-service".to_string(),
            RuntimeType::MessageQueue => "message-queue".to_string(),
            RuntimeType::CommonIO => "common-io".to_string(),
            RuntimeType::FileProcessing => "file-processing".to_string(),
            RuntimeType::Database => "database".to_string(),
            RuntimeType::CacheService => "cache-service".to_string(),
            RuntimeType::CommonCompute => "common-compute".to_string(),
            RuntimeType::DataProcessing => "data-processing".to_string(),
            RuntimeType::ImageProcessing => "image-processing".to_string(),
            RuntimeType::MachineLearning => "machine-learning".to_string(),
            RuntimeType::Custom(s) => format!("custom-{s}"),
        }
    }
}

#[macro_export]
macro_rules! create_default_runtime {
    ($rt_type:expr) => {{
        let name = $rt_type.as_thread_name();
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name(&name)
            .build()
            .unwrap()
    }};
    ($rt_type:expr, $threads:expr) => {{
        let name = $rt_type.as_thread_name();
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads($threads)
            .thread_name(&name)
            .build()
            .unwrap()
    }};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Graceful,
    TimedOut,
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct RuntimeShutdownReport {
    pub runtime_type: RuntimeType,
    pub outcome: ShutdownOutcome,
    pub elapsed: Duration,
    pub completed_tasks: usize,
    pub cancelled_tasks: usize,
    pub panicked_tasks: usize,
    pub remaining_tasks: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ShutdownReport {
    pub signal: ExitSignal,
    pub outcome: ShutdownOutcome,
    pub elapsed: Duration,
    pub runtimes: Vec<RuntimeShutdownReport>,
}

#[derive(Default)]
struct TaskState {
    gate: Mutex<()>,
    accepting: AtomicBool,
    next_id: AtomicU64,
    active: DashMap<u64, String>,
    completed: AtomicUsize,
    cancelled: AtomicUsize,
    panicked: AtomicUsize,
}

impl TaskState {
    fn new() -> Self {
        Self {
            accepting: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn start_locked(self: &Arc<Self>, name: String) -> TaskGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active.insert(id, name);
        TaskGuard {
            id,
            state: self.clone(),
            finished: false,
        }
    }

    fn close(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    fn active_names(&self) -> Vec<String> {
        let mut names = self
            .active
            .iter()
            .map(|entry| entry.value().clone())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

struct TaskGuard {
    id: u64,
    state: Arc<TaskState>,
    finished: bool,
}

impl TaskGuard {
    fn complete(&mut self, cancelled: bool) {
        if cancelled {
            self.state.cancelled.fetch_add(1, Ordering::Relaxed);
        } else {
            self.state.completed.fetch_add(1, Ordering::Relaxed);
        }
        self.finished = true;
        self.state.active.remove(&self.id);
    }

    fn panic(&mut self) {
        self.state.panicked.fetch_add(1, Ordering::Relaxed);
        self.finished = true;
        self.state.active.remove(&self.id);
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.state.cancelled.fetch_add(1, Ordering::Relaxed);
            self.state.active.remove(&self.id);
        }
    }
}

struct RuntimeEntry {
    runtime: Runtime,
    cancel: CancellationToken,
    tracker: TaskTracker,
    tasks: Arc<TaskState>,
}

impl RuntimeEntry {
    fn new(runtime: Runtime) -> Self {
        Self {
            runtime,
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            tasks: Arc::new(TaskState::new()),
        }
    }

    fn handle(
        &self,
        runtime_type: RuntimeType,
        failed: Arc<AtomicBool>,
        shutting_down: Arc<AtomicBool>,
        shutdown_requested: CancellationToken,
    ) -> GlobalRuntime {
        GlobalRuntime {
            runtime_type,
            rt_handle: self.runtime.handle().clone(),
            cancel: self.cancel.clone(),
            tracker: self.tracker.clone(),
            tasks: self.tasks.clone(),
            failed,
            shutting_down,
            shutdown_requested,
        }
    }
}

struct RuntimeRegistry {
    gate: Mutex<()>,
    runtimes: DashMap<RuntimeType, RuntimeEntry>,
    shutting_down: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    shutdown_requested: CancellationToken,
}

impl RuntimeRegistry {
    fn new() -> Self {
        let runtimes = DashMap::new();
        runtimes.insert(
            RuntimeType::Main,
            RuntimeEntry::new(
                create_runtime(&RuntimeType::Main, None).expect("create main runtime"),
            ),
        );
        Self {
            gate: Mutex::new(()),
            runtimes,
            shutting_down: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            shutdown_requested: CancellationToken::new(),
        }
    }

    fn register(&self, runtime_type: RuntimeType, runtime: Runtime) -> GlobalResult<GlobalRuntime> {
        let _gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(global_runtime_error("application is shutting down"));
        }
        match self.runtimes.entry(runtime_type.clone()) {
            Entry::Occupied(_) => Err(global_runtime_error(&format!(
                "runtime already exists: {}",
                runtime_type.as_thread_name()
            ))),
            Entry::Vacant(vacant) => {
                let entry = RuntimeEntry::new(runtime);
                let handle = entry.handle(
                    runtime_type,
                    self.failed.clone(),
                    self.shutting_down.clone(),
                    self.shutdown_requested.clone(),
                );
                vacant.insert(entry);
                Ok(handle)
            }
        }
    }

    fn get(&self, runtime_type: &RuntimeType) -> Option<GlobalRuntime> {
        self.runtimes.get(runtime_type).map(|entry| {
            entry.handle(
                runtime_type.clone(),
                self.failed.clone(),
                self.shutting_down.clone(),
                self.shutdown_requested.clone(),
            )
        })
    }

    fn runtime_types(&self) -> Vec<RuntimeType> {
        self.runtimes
            .iter()
            .filter_map(|entry| (entry.key() != &RuntimeType::Main).then(|| entry.key().clone()))
            .collect()
    }

    async fn shutdown(
        &self,
        orders: &[RuntimeType],
        total_timeout: Duration,
    ) -> Vec<RuntimeShutdownReport> {
        {
            let _gate = self
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.shutting_down.store(true, Ordering::Release);
        }
        let deadline = Instant::now() + total_timeout;
        let mut ordered = Vec::new();
        for runtime_type in orders {
            if runtime_type != &RuntimeType::Main && !ordered.contains(runtime_type) {
                ordered.push(runtime_type.clone());
            }
        }
        let mut remaining = self.runtime_types();
        remaining.sort_by_key(RuntimeType::as_thread_name);
        for runtime_type in remaining {
            if !ordered.contains(&runtime_type) {
                warn!(
                    "runtime omitted from shutdown order; appending final stage: runtime_type={}",
                    runtime_type.as_thread_name()
                );
                ordered.push(runtime_type);
            }
        }

        let mut reports = Vec::with_capacity(ordered.len() + 1);
        for (index, runtime_type) in ordered.iter().enumerate() {
            let stages_left = (ordered.len() - index + 1) as u32;
            let stage_timeout = deadline
                .saturating_duration_since(Instant::now())
                .checked_div(stages_left)
                .unwrap_or_default();
            if let Some(report) = self.shutdown_runtime(runtime_type, stage_timeout).await {
                log_runtime_report(&report);
                reports.push(report);
            }
        }

        let main_timeout = deadline.saturating_duration_since(Instant::now());
        if let Some(report) = self.prepare_main_shutdown(main_timeout).await {
            log_runtime_report(&report);
            reports.push(report);
        }
        reports
    }

    async fn shutdown_runtime(
        &self,
        runtime_type: &RuntimeType,
        timeout: Duration,
    ) -> Option<RuntimeShutdownReport> {
        let started = Instant::now();
        let handle = self.get(runtime_type)?;
        {
            let _gate = handle
                .tasks
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handle.tasks.close();
            handle.tracker.close();
        }
        handle.cancel.cancel();
        let wait_outcome = tokio::time::timeout(timeout, handle.tracker.wait()).await;
        let remaining_before_shutdown = handle.tasks.active_names();
        let (_, entry) = self.runtimes.remove(runtime_type)?;
        let runtime_timeout = timeout.saturating_sub(started.elapsed());
        let shutdown = tokio::task::spawn_blocking(move || {
            entry.runtime.shutdown_timeout(runtime_timeout);
        })
        .await;
        let remaining_tasks = handle.tasks.active_names();
        let panicked_tasks = handle.tasks.panicked.load(Ordering::Relaxed);
        let outcome = if wait_outcome.is_err() {
            ShutdownOutcome::TimedOut
        } else if shutdown.is_err() || !remaining_tasks.is_empty() || panicked_tasks > 0 {
            ShutdownOutcome::Incomplete
        } else {
            ShutdownOutcome::Graceful
        };
        Some(RuntimeShutdownReport {
            runtime_type: runtime_type.clone(),
            outcome,
            elapsed: started.elapsed(),
            completed_tasks: handle.tasks.completed.load(Ordering::Relaxed),
            cancelled_tasks: handle.tasks.cancelled.load(Ordering::Relaxed),
            panicked_tasks,
            remaining_tasks: if remaining_tasks.is_empty() {
                remaining_before_shutdown
                    .into_iter()
                    .filter(|_| outcome != ShutdownOutcome::Graceful)
                    .collect()
            } else {
                remaining_tasks
            },
        })
    }

    async fn prepare_main_shutdown(&self, timeout: Duration) -> Option<RuntimeShutdownReport> {
        let started = Instant::now();
        let handle = self.get(&RuntimeType::Main)?;
        {
            let _gate = handle
                .tasks
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handle.tasks.close();
            handle.tracker.close();
        }
        handle.cancel.cancel();
        let wait_outcome = tokio::time::timeout(timeout, handle.tracker.wait()).await;
        let remaining_tasks = handle.tasks.active_names();
        let panicked_tasks = handle.tasks.panicked.load(Ordering::Relaxed);
        let outcome = if wait_outcome.is_err() {
            ShutdownOutcome::TimedOut
        } else if remaining_tasks.is_empty() && panicked_tasks == 0 {
            ShutdownOutcome::Graceful
        } else {
            ShutdownOutcome::Incomplete
        };
        Some(RuntimeShutdownReport {
            runtime_type: RuntimeType::Main,
            outcome,
            elapsed: started.elapsed(),
            completed_tasks: handle.tasks.completed.load(Ordering::Relaxed),
            cancelled_tasks: handle.tasks.cancelled.load(Ordering::Relaxed),
            panicked_tasks,
            remaining_tasks,
        })
    }

    fn take_main_runtime(&self) -> Option<Runtime> {
        self.runtimes
            .remove(&RuntimeType::Main)
            .map(|(_, entry)| entry.runtime)
    }

    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
        self.shutdown_requested.cancel();
    }

    fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

static GLOBAL_RUNTIMES: Lazy<RuntimeRegistry> = Lazy::new(RuntimeRegistry::new);

#[derive(Clone)]
pub struct GlobalRuntime {
    runtime_type: RuntimeType,
    pub rt_handle: Handle,
    pub cancel: CancellationToken,
    tracker: TaskTracker,
    tasks: Arc<TaskState>,
    failed: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    shutdown_requested: CancellationToken,
}

impl GlobalRuntime {
    pub fn register(runtime_type: RuntimeType, runtime: Runtime) -> GlobalResult<Self> {
        GLOBAL_RUNTIMES.register(runtime_type, runtime)
    }

    pub fn register_default(runtime_type: RuntimeType) -> GlobalResult<Self> {
        let runtime = create_runtime(&runtime_type, None)?;
        Self::register(runtime_type, runtime)
    }

    pub fn register_threads_default(
        runtime_type: RuntimeType,
        threads: usize,
    ) -> GlobalResult<Self> {
        let runtime = create_runtime(&runtime_type, Some(threads))?;
        Self::register(runtime_type, runtime)
    }

    pub fn get_main_runtime() -> Self {
        Self::get_runtime(&RuntimeType::Main).expect("main runtime is unavailable")
    }

    pub fn get_runtime(runtime_type: &RuntimeType) -> Option<Self> {
        GLOBAL_RUNTIMES.get(runtime_type)
    }

    pub fn runtime_type(&self) -> &RuntimeType {
        &self.runtime_type
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    pub fn spawn<F>(
        &self,
        name: impl Into<String>,
        future: F,
    ) -> GlobalResult<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let name = name.into();
        let gate = self
            .tasks
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.tasks.accepting.load(Ordering::Acquire) {
            return Err(runtime_task_rejected(&format!(
                "runtime no longer accepts tasks: runtime_type={}, task={name}",
                self.runtime_type.as_thread_name()
            )));
        }
        let mut guard = self.tasks.start_locked(name.clone());
        let cancel = self.cancel.clone();
        let failed = self.failed.clone();
        let shutdown_requested = self.shutdown_requested.clone();
        let handle = self.tracker.spawn_on(
            async move {
                let result = AssertUnwindSafe(future).catch_unwind().await;
                match result {
                    Ok(output) => {
                        guard.complete(cancel.is_cancelled());
                        output
                    }
                    Err(payload) => {
                        guard.panic();
                        error!("managed async task panicked: task={name}");
                        failed.store(true, Ordering::Release);
                        shutdown_requested.cancel();
                        resume_unwind(payload)
                    }
                }
            },
            &self.rt_handle,
        );
        drop(gate);
        Ok(handle)
    }

    pub fn spawn_blocking<F, T>(
        &self,
        name: impl Into<String>,
        task: F,
    ) -> GlobalResult<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let name = name.into();
        let gate = self
            .tasks
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.tasks.accepting.load(Ordering::Acquire) {
            return Err(runtime_task_rejected(&format!(
                "runtime no longer accepts tasks: runtime_type={}, task={name}",
                self.runtime_type.as_thread_name()
            )));
        }
        let mut guard = self.tasks.start_locked(name.clone());
        let cancel = self.cancel.clone();
        let failed = self.failed.clone();
        let shutdown_requested = self.shutdown_requested.clone();
        let handle = self.tracker.spawn_blocking_on(
            move || match std::panic::catch_unwind(AssertUnwindSafe(task)) {
                Ok(output) => {
                    guard.complete(cancel.is_cancelled());
                    output
                }
                Err(payload) => {
                    guard.panic();
                    error!("managed blocking task panicked: task={name}");
                    failed.store(true, Ordering::Release);
                    shutdown_requested.cancel();
                    resume_unwind(payload)
                }
            },
            &self.rt_handle,
        );
        drop(gate);
        Ok(handle)
    }

    pub fn request_shutdown() {
        Signal::request_shutdown();
    }

    pub fn request_shutdown_with_error() {
        GLOBAL_RUNTIMES.fail();
        Signal::request_shutdown();
    }

    pub fn order_shutdown(orders: &[RuntimeType]) -> ShutdownReport {
        let main = Self::get_main_runtime();
        let shutdown_requested = main.shutdown_requested.clone();
        let signal = main
            .rt_handle
            .block_on(async move { wait_for_exit_signal(shutdown_requested).await });
        let started = Instant::now();
        info!(
            "application shutdown requested: signal={}, stage_count={}, total_timeout_ms={}",
            signal.as_str(),
            orders.len(),
            APPLICATION_SHUTDOWN_TIMEOUT.as_millis()
        );
        let runtimes = main
            .rt_handle
            .block_on(GLOBAL_RUNTIMES.shutdown(orders, APPLICATION_SHUTDOWN_TIMEOUT));
        let main_runtime = GLOBAL_RUNTIMES.take_main_runtime();
        if let Some(runtime) = main_runtime {
            runtime
                .shutdown_timeout(APPLICATION_SHUTDOWN_TIMEOUT.saturating_sub(started.elapsed()));
        }
        let outcome = if GLOBAL_RUNTIMES.has_failed() {
            ShutdownOutcome::Incomplete
        } else if runtimes
            .iter()
            .all(|report| report.outcome == ShutdownOutcome::Graceful)
        {
            ShutdownOutcome::Graceful
        } else if runtimes
            .iter()
            .any(|report| report.outcome == ShutdownOutcome::TimedOut)
        {
            ShutdownOutcome::TimedOut
        } else {
            ShutdownOutcome::Incomplete
        };
        let report = ShutdownReport {
            signal,
            outcome,
            elapsed: started.elapsed(),
            runtimes,
        };
        match report.outcome {
            ShutdownOutcome::Graceful => info!(
                "application shutdown completed: outcome=graceful, elapsed_ms={}",
                report.elapsed.as_millis()
            ),
            ShutdownOutcome::TimedOut => warn!(
                "application shutdown completed: outcome=timeout, elapsed_ms={}",
                report.elapsed.as_millis()
            ),
            ShutdownOutcome::Incomplete => error!(
                "application shutdown completed: outcome=incomplete, elapsed_ms={}",
                report.elapsed.as_millis()
            ),
        }
        report
    }
}

impl ShutdownReport {
    pub fn is_graceful(&self) -> bool {
        self.outcome == ShutdownOutcome::Graceful
    }
}

fn create_runtime(runtime_type: &RuntimeType, threads: Option<usize>) -> GlobalResult<Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .enable_all()
        .thread_name(runtime_type.as_thread_name());
    if let Some(threads) = threads {
        builder.worker_threads(threads);
    }
    builder.build().map_err(|err| {
        global_runtime_error(&format!(
            "create runtime failed: runtime_type={}, reason={err}",
            runtime_type.as_thread_name()
        ))
    })
}

async fn wait_for_exit_signal(shutdown_requested: CancellationToken) -> ExitSignal {
    tokio::select! {
        signal = Signal::wait_exit_signal() => signal,
        _ = shutdown_requested.cancelled() => ExitSignal::Requested,
    }
}

fn global_runtime_error(message: &str) -> GlobalError {
    GlobalError::new_sys_error(message, |msg| error!("{msg}"))
}

fn runtime_task_rejected(message: &str) -> GlobalError {
    GlobalError::new_sys_error(message, |_| {})
}

fn log_runtime_report(report: &RuntimeShutdownReport) {
    let runtime_type = report.runtime_type.as_thread_name();
    match report.outcome {
        ShutdownOutcome::Graceful => info!(
            "runtime shutdown completed: runtime_type={runtime_type}, outcome=graceful, elapsed_ms={}, completed_tasks={}, cancelled_tasks={}, panicked_tasks=0, remaining_tasks=0",
            report.elapsed.as_millis(),
            report.completed_tasks,
            report.cancelled_tasks
        ),
        ShutdownOutcome::TimedOut => warn!(
            "runtime shutdown completed: runtime_type={runtime_type}, outcome=timeout, elapsed_ms={}, completed_tasks={}, cancelled_tasks={}, panicked_tasks={}, remaining_tasks={}, task_names={:?}",
            report.elapsed.as_millis(),
            report.completed_tasks,
            report.cancelled_tasks,
            report.panicked_tasks,
            report.remaining_tasks.len(),
            report.remaining_tasks
        ),
        ShutdownOutcome::Incomplete => error!(
            "runtime shutdown completed: runtime_type={runtime_type}, outcome=incomplete, elapsed_ms={}, completed_tasks={}, cancelled_tasks={}, panicked_tasks={}, remaining_tasks={}, task_names={:?}",
            report.elapsed.as_millis(),
            report.completed_tasks,
            report.cancelled_tasks,
            report.panicked_tasks,
            report.remaining_tasks.len(),
            report.remaining_tasks
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn test_runtime(runtime_type: RuntimeType) -> Runtime {
        create_runtime(&runtime_type, Some(1)).expect("create test runtime")
    }

    #[test]
    fn rejects_duplicate_runtime_registration() {
        let registry = RuntimeRegistry::new();
        registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("first registration");
        let duplicate = registry.register(
            RuntimeType::CommonNetwork,
            test_runtime(RuntimeType::CommonNetwork),
        );
        assert!(duplicate.is_err());
    }

    #[test]
    fn shuts_down_runtimes_in_declared_order() {
        let registry = RuntimeRegistry::new();
        let network = registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        let compute = registry
            .register(
                RuntimeType::CommonCompute,
                test_runtime(RuntimeType::CommonCompute),
            )
            .expect("compute runtime");
        let events = Arc::new(Mutex::new(Vec::new()));
        let network_events = events.clone();
        let network_cancel = network.cancel.clone();
        network
            .spawn("network", async move {
                network_cancel.cancelled().await;
                network_events.lock().unwrap().push("network");
            })
            .expect("network task");
        let compute_events = events.clone();
        let compute_cancel = compute.cancel.clone();
        compute
            .spawn("compute", async move {
                compute_cancel.cancelled().await;
                compute_events.lock().unwrap().push("compute");
            })
            .expect("compute task");

        let main = registry.get(&RuntimeType::Main).expect("main runtime");
        let main_events = events.clone();
        let main_cancel = main.cancel.clone();
        main.spawn("main", async move {
            main_cancel.cancelled().await;
            main_events.lock().unwrap().push("main");
        })
        .expect("main task");
        let reports = main.rt_handle.block_on(registry.shutdown(
            &[RuntimeType::CommonNetwork, RuntimeType::CommonCompute],
            Duration::from_secs(2),
        ));
        let main_runtime = registry.take_main_runtime().expect("take main runtime");
        main_runtime.shutdown_timeout(Duration::from_secs(1));

        assert_eq!(*events.lock().unwrap(), ["network", "compute", "main"]);
        assert!(network.is_shutting_down());
        assert!(reports
            .iter()
            .all(|report| report.outcome == ShutdownOutcome::Graceful));
    }

    #[test]
    fn reports_non_cooperative_task_timeout() {
        let registry = RuntimeRegistry::new();
        let network = registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        network
            .spawn("pending", std::future::pending::<()>())
            .expect("pending task");
        let main = registry.get(&RuntimeType::Main).expect("main runtime");
        let reports = main
            .rt_handle
            .block_on(registry.shutdown(&[RuntimeType::CommonNetwork], Duration::from_millis(100)));
        let main_runtime = registry.take_main_runtime().expect("take main runtime");
        main_runtime.shutdown_timeout(Duration::from_secs(1));
        let report = reports
            .iter()
            .find(|report| report.runtime_type == RuntimeType::CommonNetwork)
            .expect("network report");
        assert_eq!(report.outcome, ShutdownOutcome::TimedOut);
        assert_eq!(report.remaining_tasks, ["pending"]);
    }

    #[test]
    fn waits_for_cooperative_blocking_task() {
        let registry = RuntimeRegistry::new();
        let network = registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        let cancel = network.cancel.clone();
        network
            .spawn_blocking("blocking", move || {
                while !cancel.is_cancelled() {
                    std::thread::yield_now();
                }
            })
            .expect("blocking task");
        let main = registry.get(&RuntimeType::Main).expect("main runtime");
        let reports = main
            .rt_handle
            .block_on(registry.shutdown(&[RuntimeType::CommonNetwork], Duration::from_secs(1)));
        let main_runtime = registry.take_main_runtime().expect("take main runtime");
        main_runtime.shutdown_timeout(Duration::from_secs(1));
        let report = reports
            .iter()
            .find(|report| report.runtime_type == RuntimeType::CommonNetwork)
            .expect("network report");
        assert_eq!(report.outcome, ShutdownOutcome::Graceful);
        assert_eq!(report.cancelled_tasks, 1);
    }

    #[test]
    fn reports_non_cooperative_blocking_task_timeout() {
        let registry = RuntimeRegistry::new();
        let network = registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        network
            .spawn_blocking("blocking-pending", || {
                std::thread::sleep(Duration::from_millis(200));
            })
            .expect("blocking task");
        let main = registry.get(&RuntimeType::Main).expect("main runtime");
        let reports = main
            .rt_handle
            .block_on(registry.shutdown(&[RuntimeType::CommonNetwork], Duration::from_millis(20)));
        let main_runtime = registry.take_main_runtime().expect("take main runtime");
        main_runtime.shutdown_timeout(Duration::from_secs(1));
        let report = reports
            .iter()
            .find(|report| report.runtime_type == RuntimeType::CommonNetwork)
            .expect("network report");
        assert_eq!(report.outcome, ShutdownOutcome::TimedOut);
        assert_eq!(report.remaining_tasks, ["blocking-pending"]);
    }

    #[test]
    fn reports_managed_task_panic_as_incomplete() {
        let registry = RuntimeRegistry::new();
        let network = registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        let task = network
            .spawn("panicking", async { panic!("managed task panic") })
            .expect("panicking task");
        let main = registry.get(&RuntimeType::Main).expect("main runtime");
        assert!(main
            .rt_handle
            .block_on(task)
            .expect_err("task panic")
            .is_panic());
        let reports = main
            .rt_handle
            .block_on(registry.shutdown(&[RuntimeType::CommonNetwork], Duration::from_secs(1)));
        let main_runtime = registry.take_main_runtime().expect("take main runtime");
        main_runtime.shutdown_timeout(Duration::from_secs(1));
        let report = reports
            .iter()
            .find(|report| report.runtime_type == RuntimeType::CommonNetwork)
            .expect("network report");
        assert_eq!(report.outcome, ShutdownOutcome::Incomplete);
        assert_eq!(report.panicked_tasks, 1);
        assert!(registry.has_failed());
    }

    #[test]
    fn repeated_registry_shutdown_is_safe() {
        let registry = RuntimeRegistry::new();
        registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        let main = registry.get(&RuntimeType::Main).expect("main runtime");
        let first = main
            .rt_handle
            .block_on(registry.shutdown(&[RuntimeType::CommonNetwork], Duration::from_secs(1)));
        let second = main
            .rt_handle
            .block_on(registry.shutdown(&[RuntimeType::CommonNetwork], Duration::from_secs(1)));
        let main_runtime = registry.take_main_runtime().expect("take main runtime");
        main_runtime.shutdown_timeout(Duration::from_secs(1));
        assert!(first
            .iter()
            .any(|report| report.runtime_type == RuntimeType::CommonNetwork));
        assert!(!second
            .iter()
            .any(|report| report.runtime_type == RuntimeType::CommonNetwork));
    }

    #[test]
    fn rejects_tasks_after_runtime_shutdown_starts() {
        let registry = RuntimeRegistry::new();
        let network = registry
            .register(
                RuntimeType::CommonNetwork,
                test_runtime(RuntimeType::CommonNetwork),
            )
            .expect("network runtime");
        network.tasks.close();
        assert!(network.spawn("late", async {}).is_err());
    }
}
