use log::debug;
use once_cell::sync::{Lazy};
use std::sync::Arc;
use tokio::signal;
use tokio_util::sync::CancellationToken;
/// 全局退出信号，如有需要可将此信号在各处持有【包括发送给分布式其他系统以表示该程序退出】
static SHUTDOWN: Lazy<Arc<CancellationToken>> = Lazy::new(|| Arc::new(CancellationToken::new()));

pub struct Signal;
impl Signal {
    pub fn get_cancel() -> Arc<CancellationToken> {
        SHUTDOWN.clone()
    }

    /// 此函数在程序生命周期只调用一次
    pub async fn exit() {
        // 监听 Ctrl+C
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
            debug!("\n收到 Ctrl+C 信号");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
            debug!("收到 TERM 信号");
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
        SHUTDOWN.cancel();
    }
}
