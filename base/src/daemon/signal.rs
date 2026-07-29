use log::debug;
use once_cell::sync::Lazy;
use tokio::signal;
use tokio_util::sync::CancellationToken;
/// 全局退出信号，如有需要可将此信号在各处持有【包括发送给分布式其他系统以表示该程序退出】
static SHUTDOWN: Lazy<CancellationToken> = Lazy::new(CancellationToken::new);

pub struct Signal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitSignal {
    Requested,
    CtrlC,
    Terminate,
}

impl ExitSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::CtrlC => "ctrl_c",
            Self::Terminate => "sigterm",
        }
    }
}

impl Signal {
    pub fn get_cancel() -> CancellationToken {
        SHUTDOWN.clone()
    }

    pub fn request_shutdown() {
        SHUTDOWN.cancel();
    }

    /// 此函数在程序生命周期只调用一次
    pub async fn wait_exit_signal() -> ExitSignal {
        // 监听 Ctrl+C
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
            debug!("收到 Ctrl+C 信号");
            ExitSignal::CtrlC
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
            debug!("收到 TERM 信号");
            ExitSignal::Terminate
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<ExitSignal>();

        let signal = tokio::select! {
            _ = SHUTDOWN.cancelled() => ExitSignal::Requested,
            signal = ctrl_c => signal,
            signal = terminate => signal,
        };
        SHUTDOWN.cancel();
        signal
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{ExitSignal, Signal};
    use std::time::Duration;

    #[test]
    fn observes_sigterm() {
        let runtime = tokio::runtime::Runtime::new().expect("create runtime");
        runtime.block_on(async {
            let waiter = tokio::spawn(Signal::wait_exit_signal());
            tokio::time::sleep(Duration::from_millis(50)).await;
            let status = unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) };
            assert_eq!(status, 0);
            assert_eq!(waiter.await.expect("signal waiter"), ExitSignal::Terminate);
        });
    }
}
