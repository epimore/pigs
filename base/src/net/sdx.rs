use std::net::SocketAddr;
use std::net::{TcpListener, UdpSocket};
use log::error;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};
use exception::{GlobalError, GlobalResult, GlobalResultExt};
use crate::net::state::{CHANNEL_BUFFER_SIZE, Gate, GateListener, Protocol, Zip};
use crate::net::{tcp, udp};

/*
使用std创建网络句柄：解决跨运行时、io、网络驱动绑定问题
*/

#[cfg(feature = "net")]
pub fn listen(protocol: Protocol, socket_addr: SocketAddr) -> GlobalResult<(Option<TcpListener>, Option<UdpSocket>)> {
    match protocol {
        Protocol::UDP => {
            let udp_socket = UdpSocket::bind(socket_addr).hand_log(|msg| error!("{msg}"))?;
            Ok((None, Some(udp_socket)))
        }
        Protocol::TCP => {
            let tcp_listener = TcpListener::bind(socket_addr).hand_log(|msg| error!("{msg}"))?;
            Ok((Some(tcp_listener), None))
        }
        Protocol::ALL => {
            let udp_socket = UdpSocket::bind(socket_addr).hand_log(|msg| error!("{msg}"))?;
            let tcp_listener = TcpListener::bind(socket_addr).hand_log(|msg| error!("{msg}"))?;
            Ok((Some(tcp_listener), Some(udp_socket)))
        }
    }
}

#[cfg(feature = "net")]
pub async fn run_by_tokio(tu: (Option<TcpListener>, Option<UdpSocket>)) -> GlobalResult<(Sender<Zip>, Receiver<Zip>)> {
    let (listen_tx, listen_rx) = tokio::sync::oneshot::channel();
    //socket 读数据通道 input
    let (input_tx, input_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
    //socket 写数据通道 output
    let (output_tx, output_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
    match tu {
        (Some(tl), None) => {
            let gate = Gate::new(tl.local_addr().hand_log(|msg| error!("{msg}"))?, input_tx, output_rx);
            let listener = tcp::listen_by_std(gate, tl)?;
            listen_tx.send(listener).map_err(|_err| GlobalError::new_sys_error("net io listen err:channel has drop", |msg| error!("{msg}")))?;
        }
        (None, Some(us)) => {
            let gate = Gate::new(us.local_addr().hand_log(|msg| error!("{msg}"))?, input_tx, output_rx);
            let listener = udp::listen_by_std(gate, us)?;
            listen_tx.send(listener).map_err(|_err| GlobalError::new_sys_error("net io listen err:channel has drop", |msg| error!("{msg}")))?;
        }
        (Some(tl), Some(us)) => {
            let (tw_tx, tw_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
            let tcp_gate = Gate::new(tl.local_addr().hand_log(|msg| error!("{msg}"))?, input_tx.clone(), tw_rx);
            let tcp_listener = tcp::listen_by_std(tcp_gate, tl)?;
            let (uw_tx, uw_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
            let udp_gate = Gate::new(us.local_addr().hand_log(|msg| error!("{msg}"))?, input_tx.clone(), uw_rx);
            let udp_listener = udp::listen_by_std(udp_gate, us)?;

            let gate_listener = GateListener::build_all(tcp_listener, udp_listener);
            listen_tx.send(gate_listener).map_err(|_err| GlobalError::new_sys_error("net io listen err:channel has drop", |msg| error!("{msg}")))?;
            crate::net::core::classify(output_rx, tw_tx, uw_tx);
        }
        (None, None) => {
            panic!("At least one network listener is required")
        }
    }
    let (accept_tx, accept_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
    let _ = crate::net::core::accept(listen_rx, accept_tx).await.hand_log(|msg| error!("{msg}"));
    tokio::spawn(async move {
        crate::net::core::rw(accept_rx).await;
    });
    Ok((output_tx, input_rx))
}