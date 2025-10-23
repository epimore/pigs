use crate::daemon::signal::Signal;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use exception::{GlobalError, GlobalResult};
use log::error;
use once_cell::sync::Lazy;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Handle, Runtime};
use tokio_util::sync::CancellationToken;

///协议类型	      推荐线程数	运行时类型	理由
///HTTP API	      2-4	    多线程	    短连接，高并发，I/O等待多
///WebSocket	  4-8	    多线程	    长连接，状态维护，中等并发
///TCP Server	  6-12	    多线程	    重量级连接，复杂协议处理
///UDP Service	  1	        当前线程     无连接，高吞吐，单线程高效
///RPC Service	  4-8	    多线程	    中等负载，序列化开销
///Proxy Service  8+	    多线程	    高吞吐，数据转发

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RuntimeType {
    // 网络I/O密集型 4
    CommonNetwork, // 通用网络
    HttpApi,       // HTTP API 服务
    WebSocket,     // WebSocket 连接
    RpcService,    // RPC 服务
    MessageQueue,  // 消息队列消费

    // 磁盘I/O密集型 4
    CommonIO,       // 通用IO
    FileProcessing, // 文件处理
    Database,       // 数据库操作
    CacheService,   // 缓存服务

    // 计算密集型 8
    CommonCompute,   // 通用计算
    DataProcessing,  // 数据处理
    ImageProcessing, // 图像处理
    MachineLearning, // 机器学习

    //自定义
    Custom(String),
}
impl RuntimeType {
    pub fn as_thread_name(&self) -> String {
        match self {
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
            RuntimeType::Custom(s) => format!("custom-{}", s),
        }
    }
}

#[macro_export]
macro_rules! create_default_runtime {
    // 仅 RuntimeType
    ($rt_type:expr) => {{
        let name = $rt_type.as_thread_name();
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name(&name)
            .build()
            .unwrap()
    }};
    // RuntimeType + worker_threads
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

static GLOBAL_RUNTIMES: Lazy<DashMap<RuntimeType, (Runtime, Arc<CancellationToken>)>> =
    Lazy::new(|| DashMap::new());

pub struct GlobalRuntime {
    pub rt_handle: Handle,
    pub cancel: Arc<CancellationToken>,
}

impl GlobalRuntime {
    pub fn register(rt_type: RuntimeType, rt: Runtime) -> GlobalResult<Self> {
        match GLOBAL_RUNTIMES.entry(rt_type.clone()) {
            Entry::Occupied(_) => Err(GlobalError::new_sys_error("运行时已存在", |msg| {
                error!("{:?} {}", rt_type, msg)
            })),
            Entry::Vacant(vac) => {
                let cancel = Arc::new(CancellationToken::new());
                let grt = Self {
                    rt_handle: rt.handle().clone(),
                    cancel: cancel.clone(),
                };
                vac.insert((rt, cancel));
                Ok(grt)
            }
        }
    }

    pub fn register_default(rt_type: RuntimeType, threads: Option<usize>) -> GlobalResult<Self> {
        let rt = match threads {
            None => {
                create_default_runtime!(rt_type)
            }
            Some(num) => {
                create_default_runtime!(rt_type, num)
            }
        };
        match Self::register(rt_type, rt) {
            Ok(grt) => Ok(grt),
            Err(err) => Err(err),
        }
    }

    pub fn get_runtime(rt_type: &RuntimeType) -> Option<Self> {
        GLOBAL_RUNTIMES.get(rt_type).map(|r| {
            let (rt, cancel) = r.value();
            Self {
                rt_handle: rt.handle().clone(),
                cancel: cancel.clone(),
            }
        })
    }

    pub async fn order_shutdown(order: &[RuntimeType]) {
        Signal::exit().await;
        for rt_type in order {
            GLOBAL_RUNTIMES.remove(rt_type).map(|(_, (rt, cancel))| {
                cancel.cancel();
                rt.shutdown_timeout(Duration::from_secs(2));
            });
        }
    }
}
