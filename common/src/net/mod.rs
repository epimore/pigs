use std::net::SocketAddr;
use log::error;

use tokio::sync::mpsc::{Receiver, Sender};

use crate::exception::{GlobalResult, TransError};
use crate::net::state::{Zip};

mod udp;
mod tcp;
mod core;
pub mod state;
pub mod sdx;

///todo 主动断开清理连接;创建事件句柄?封装数据枚举：EVENT-DATA
// static RUNTIME: Lazy<Runtime> = Lazy::new(|| {
//     tokio::runtime::Builder::new_multi_thread()
//         .thread_name_fn(|| {
//             static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
//             let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
//             format!("net-pool-{}", id)
//         })
//         .enable_all()
//         .build()
//         .hand_err(|msg| error!("net-pool Runtime build failed {msg}")).unwrap()
// });
#[cfg(feature = "net")]
pub async fn init_net(protocol: state::Protocol, socket_addr: SocketAddr) -> GlobalResult<(Sender<Zip>, Receiver<Zip>)> {
    net_run(protocol, socket_addr).await
}

async fn net_run(protocol: state::Protocol, socket_addr: SocketAddr) -> GlobalResult<(Sender<Zip>, Receiver<Zip>)> {
    let (listen_tx, listen_rx) = tokio::sync::oneshot::channel();
    let rw = core::listen(protocol, socket_addr, listen_tx).await?;
    let (accept_tx, accept_rx) = tokio::sync::mpsc::channel(state::CHANNEL_BUFFER_SIZE);
    let _ = core::accept(listen_rx, accept_tx).await.hand_log(|msg| error!("{msg}"));
    tokio::spawn(async move {
        core::rw(accept_rx).await;
    });
    Ok(rw)
}