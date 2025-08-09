use std::net::SocketAddr;
use log::error;

use tokio::sync::mpsc::{Receiver, Sender};

use crate::exception::{GlobalResult, GlobalResultExt};
use crate::net::state::{Zip};

mod udp;
mod tcp;
mod core;
pub mod state;
pub mod sdx;

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