use tokio::sync::mpsc::{Sender, Receiver};
use log::{debug, error, info, warn};
use crate::exception::{GlobalResult, GlobalResultExt};
use crate::net::state::{Zip, Gate, GateListener, GateAccept, SOCKET_BUFFER_SIZE, Association, Protocol, Package};
use tokio::net::UdpSocket;
use std::net::SocketAddr;
use bytes::Bytes;
use tokio::io;

//监听，将socket句柄发送出去
pub async fn listen(gate: Gate) -> GlobalResult<GateListener> {
    let local_addr = gate.get_local_addr().clone();
    let socket = UdpSocket::bind(local_addr).await.hand_log(|msg| error!("{msg}"))?;
    let gate_listener = GateListener::build_udp(gate, socket);
    debug!("开始监听 UDP 地址： {}", local_addr);
    Ok(gate_listener)
}
pub fn listen_by_std(gate: Gate,std_udp_socket:std::net::UdpSocket) -> GlobalResult<GateListener> {
    debug!("tokio监听 UDP 地址： {}", gate.get_local_addr());
    std_udp_socket.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;
    let socket = UdpSocket::from_std(std_udp_socket).hand_log(|msg| error!("{msg}"))?;
    let gate_listener = GateListener::build_udp(gate, socket);
    Ok(gate_listener)
}

//将socket句柄包装发送出去
pub async fn accept(gate: Gate, udp_socket: UdpSocket, accept_tx: Sender<GateAccept>) -> GlobalResult<()> {
    let gate_accept = GateAccept::accept_udp(gate, udp_socket);
    accept_tx.send(gate_accept).await.hand_log(|msg| error!("{msg}"))?;
    Ok(())
}

pub async fn read(local_addr: SocketAddr, udp_socket: &UdpSocket, tx: Sender<Zip>) {
    loop {
        let _ = udp_socket.readable().await;
        let mut buf = [0u8; SOCKET_BUFFER_SIZE];
        match udp_socket.try_recv_from(&mut buf) {
            Ok((len, remote_addr)) => {
                if len != 0 {
                    debug!("【UDP read success】 【Local_addr = {}】 【Remote_addr = {}】 【len = {}】",
                            local_addr.to_string(),
                            remote_addr.to_string(),
                            len
                            );
                    let association = Association::new(local_addr, remote_addr, Protocol::UDP);
                    let zip = Zip::build_data(Package::new(association, Bytes::copy_from_slice(&buf[..len])));
                    //接收端drop后，跳出循环
                    if let Err(_) = tx.send(zip).await {
                       break;
                    }
                }
            }

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(err) => {
                warn!("【UDP read failure】 【Local_addr = {}】 【err = {:?}】",
                            local_addr.to_string(),
                            err,
                            );
                break;
            }
        }
    }
}

pub async fn write(udp_socket: &UdpSocket, mut rx: Receiver<Zip>) {
    //发送端drop后，跳出循环
    while let Some(zip) = rx.recv().await {
        let _ = udp_socket.writable().await;
        match zip {
            Zip::Data(package) => {
                let bytes = package.get_data();
                let local_addr = package.get_association().get_local_addr();
                let remote_addr = package.get_association().get_remote_addr();
                match udp_socket.try_send_to(&*bytes, *package.get_association().get_remote_addr()) {
                    Ok(len) => {
                        debug!("【UDP write success】 【Local_addr = {:?}】 【Remote_addr = {:?}】 【len = {}】",
                            local_addr,
                            remote_addr,
                            len
                            );
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(err) => {
                        error!("【UDP write failure】 【Local_addr = {:?}】 【Remote_addr = {:?}】 【err = {:?}】",
                            local_addr,
                            remote_addr,
                            err
                            );
                        break;
                    }
                }
            }
            Zip::Event(_event) => { info!("UDP Events are not supported") }
        }
    }
}