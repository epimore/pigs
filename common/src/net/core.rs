use std::net::SocketAddr;
use std::sync::{Arc};
use tokio::{io};
use crate::net::state::{Zip, Gate, GateListener, GateAccept, Protocol, CHANNEL_BUFFER_SIZE, TCP_HANDLE_MAP};
use crate::net::{tcp, udp};
use log::{error, warn};
use crate::exception::{GlobalResult, GlobalResultExt};
use tokio::sync::{mpsc, oneshot};
use tokio::sync::mpsc::{Receiver, Sender};
use exception::GlobalError;

//启动监听并返回读写句柄
pub async fn listen(protocol: Protocol, local_addr: SocketAddr, tx: oneshot::Sender<GateListener>) -> GlobalResult<(Sender<Zip>, Receiver<Zip>)> {
    //socket 读数据通道 input
    let (input_tx, input_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
    //socket 写数据通道 output
    let (output_tx, output_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
    match protocol {
        Protocol::TCP => {
            let gate = Gate::new(local_addr, input_tx, output_rx);
            let listener = tcp::listen(gate).await?;
            tx.send(listener).map_err(|_err|GlobalError::new_sys_error("net io listen err:channel has drop",|msg| error!("{msg}")))?;
        }
        Protocol::UDP => {
            let gate = Gate::new(local_addr, input_tx, output_rx);
            let listener = udp::listen(gate).await?;
            tx.send(listener).map_err(|_err|GlobalError::new_sys_error("net io listen err:channel has drop",|msg| error!("{msg}")))?;
        }
        Protocol::ALL => {
            let (tw_tx, tw_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
            let tgate = Gate::new(local_addr, input_tx.clone(), tw_rx);
            let tcp_listener = tcp::listen(tgate).await?;
            let (uw_tx, uw_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
            let ugate = Gate::new(local_addr, input_tx.clone(), uw_rx);
            let udp_listener = udp::listen(ugate).await?;
            let gate_listener = GateListener::build_all(tcp_listener,udp_listener);
            tx.send(gate_listener).map_err(|_err|GlobalError::new_sys_error("net io listen err:channel has drop",|msg| error!("{msg}")))?;
            classify(output_rx, tw_tx, uw_tx);
        }
    }
    Ok((output_tx, input_rx))
}

pub fn classify(mut output: Receiver<Zip>, tw_tx: Sender<Zip>, uw_tx: Sender<Zip>) {
    tokio::spawn(async move {
        while let Some(zip) = output.recv().await {
            match zip.get_association_protocol() {
                &Protocol::UDP => { let _ = uw_tx.clone().send(zip).await.hand_log(|msg| error!("{msg}")); }
                &Protocol::TCP => { let _ = tw_tx.clone().send(zip).await.hand_log(|msg| error!("{msg}")); }
                &Protocol::ALL => { warn!("全协议发送????") }
            }
        }
    });
}

//接收监听句柄，开启socket接入，将tcp/udp连接发送出去
pub async fn accept(rx: oneshot::Receiver<GateListener>, tx: Sender<GateAccept>) -> GlobalResult<()> {
    match rx.await {
        Ok(gate_listener) => {
            match gate_listener {
                GateListener::Tcp(gate, listener) => {
                    let sender = tx.clone();
                    let local_addr = gate.get_local_addr().clone();
                    let input = gate.get_input().clone();
                    tokio::spawn(async move {
                        loop {
                            //给予每个对外发送数据tcp连接一个接收句柄，并将其对应的发送句柄保存起来
                            let (lone_output_tx, lone_output_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
                            let gate1 = Gate::new(local_addr, input.clone(), lone_output_rx);
                            let _ = tcp::accept(gate1, &listener, sender.clone(), lone_output_tx).await.hand_log(|msg| error!("{msg}"));
                        }
                    });
                    tokio::spawn(async move {
                        //接收对外输出信息，并根据Zip上的账单信息，发送到对应的TCP发送通道
                        let mut receiver = gate.get_owned_output();
                        while let Some(zip) = receiver.recv().await {
                            let association = zip.get_association();
                            match TCP_HANDLE_MAP.clone().get(&association) {
                                None => {
                                    warn!("【TCP】连接不存在 => {:?}",&association);
                                }
                                Some(lone_output_tx) => {
                                    let _ = lone_output_tx.send(zip).await.hand_log(|msg| error!("{msg}"));
                                }
                            }
                        }
                    });
                }
                GateListener::Udp(gate, socket) => {
                    udp::accept(gate, socket, tx.clone()).await?;
                }
                GateListener::All((tcp_gate, tcp_listener), (udp_gate, udp_socket)) => {
                    let sender = tx.clone();
                    let local_addr = tcp_gate.get_local_addr().clone();
                    let input = tcp_gate.get_input().clone();
                    tokio::spawn(async move {
                        loop {
                            //给予每个对外发送数据tcp连接一个接收句柄，并将其对应的发送句柄保存起来
                            let (lone_output_tx, lone_output_rx) = mpsc::channel(CHANNEL_BUFFER_SIZE);
                            let gate1 = Gate::new(local_addr, input.clone(), lone_output_rx);
                            let _ = tcp::accept(gate1, &tcp_listener, sender.clone(), lone_output_tx).await.hand_log(|msg| error!("{msg}"));
                        }
                    });
                    tokio::spawn(async move {
                        //接收对外输出信息，并根据Zip上的账单信息，发送到对应的TCP发送通道
                        let mut receiver = tcp_gate.get_owned_output();
                        while let Some(zip) = receiver.recv().await {
                            let association = zip.get_association();
                            match TCP_HANDLE_MAP.clone().get(&association) {
                                None => {
                                    warn!("【TCP】连接不存在 => {:?}",&association);
                                }
                                Some(lone_output_tx) => {
                                    let _ = lone_output_tx.send(zip).await.hand_log(|msg| error!("{msg}"));
                                }
                            }
                        }
                    });
                    udp::accept(udp_gate, udp_socket, tx).await?;
                }
            }
        }
        Err(e) => {
            Err(GlobalError::new_sys_error(&format!("net io: {}",e),|msg| error!("{msg}")))?
        }
    }
    Ok(())
}

pub async fn rw(mut rx: Receiver<GateAccept>) {
    while let Some(gate_accept) = rx.recv().await {
        match gate_accept {
            GateAccept::Tcp(gate, remote_addr, tcp_stream) => {
                let (read, write) = io::split(tcp_stream);
                let local_addr = gate.get_local_addr().clone();
                let sender = gate.get_input().clone();
                tokio::spawn(async move {
                    let _ = tcp::read(read, local_addr, remote_addr, sender).await;
                });
                let receiver = gate.get_owned_output();
                tokio::spawn(async move {
                    let _ = tcp::write(write, receiver).await;
                });
            }
            GateAccept::Udp(gate, udp_socket) => {
                let local_addr = gate.get_local_addr().clone();
                let sender = gate.get_input().clone();
                let receiver = gate.get_owned_output();
                let aus = Arc::new(udp_socket);
                let ausc = aus.clone();
                tokio::spawn(async move {
                    let _ = udp::read(local_addr, &*aus, sender).await;
                });
                tokio::spawn(async move {
                    let _ = udp::write(&*ausc, receiver).await;
                });
            }
        }
    }
}