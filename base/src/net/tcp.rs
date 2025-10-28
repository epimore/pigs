use crate::exception::{GlobalResult, GlobalResultExt};
use crate::net::state::{
    Association, Event, Gate, GateAccept, GateListener, Package, Protocol, Zip, SOCKET_BUFFER_SIZE,
    TCP_HANDLE_MAP,
};
use bytes::Bytes;
use log::{debug, error, info, warn};
use std::io::Error;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::{io, time};

//创建tcp监听，并将监听句柄（内含读写句柄）发送出去
//卸载监听 drop listen？
pub async fn listen(gate: Gate) -> GlobalResult<GateListener> {
    let local_addr = gate.get_local_addr().clone();
    let tcp_listener = TcpListener::bind(local_addr)
        .await
        .hand_log(|msg| error!("{msg}"))?;
    debug!("开始监听 TCP 地址： {}", local_addr);
    let gate_listener = GateListener::build_tcp(gate, tcp_listener);
    Ok(gate_listener)
}
pub fn listen_by_std(
    gate: Gate,
    std_tcp_listener: std::net::TcpListener,
) -> GlobalResult<GateListener> {
    debug!("tokio监听 TCP 地址： {}", gate.get_local_addr());
    std_tcp_listener
        .set_nonblocking(true)
        .hand_log(|msg| error!("{msg}"))?;
    let tcp_listener = TcpListener::from_std(std_tcp_listener).hand_log(|msg| error!("{msg}"))?;
    let gate_listener = GateListener::build_tcp(gate, tcp_listener);
    Ok(gate_listener)
}

//将连接句柄（内含读写句柄，远端地址等）发送出去
pub async fn accept(
    gate: Gate,
    tcp_listener: &TcpListener,
    accept_tx: Sender<GateAccept>,
    lone_output_tx: Sender<Zip>,
) -> GlobalResult<()> {
    let local_addr = gate.get_local_addr().clone();
    let gate_accept = check_accept(tcp_listener)
        .await
        .map(|(tcp_stream, remote_addr)| {
            let association = Association::new(local_addr, remote_addr, Protocol::TCP);
            TCP_HANDLE_MAP.insert(association, lone_output_tx);
            GateAccept::accept_tcp(gate, remote_addr, tcp_stream)
        })
        .hand_log(|msg| {
            error!(
                "{:?} : TCP accept has failed too many times.{msg}",
                local_addr
            )
        })?;
    accept_tx
        .send(gate_accept)
        .await
        .hand_log(|msg| error!("{msg}"))?;
    Ok(())
}
const MAX_BACKOFF_MS: u64 = 3000; // 最大退避 3 秒
//连接检测
async fn check_accept(tcp_listener: &TcpListener) -> Result<(TcpStream, SocketAddr), Error> {
    let mut backoff_ms = 10;
    loop {
        match tcp_listener.accept().await {
            Ok((tcp_stream, remote_addr)) => {
                return Ok((tcp_stream, remote_addr));
            }
            Err(err) => {
                // 对于资源暂时不可用等错误进行重试
                if should_retry(&err) {
                    warn!("Accept failed (backoff {}ms): {}, retrying...", backoff_ms, err);

                    if backoff_ms > MAX_BACKOFF_MS {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("Accept failed after backoff: {}", err)
                        ));
                    }

                    time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = backoff_ms * 2;
                } else {
                    // 致命错误，立即返回
                    return Err(err);
                }
            }
        }
    }
}
fn should_retry(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock |        // 非阻塞模式下的暂时不可用
        io::ErrorKind::TimedOut |          // 超时
        io::ErrorKind::Interrupted |       // 被信号中断
        io::ErrorKind::ResourceBusy |      // 资源繁忙
        io::ErrorKind::NotConnected        // 暂时未连接
    )
}

//连接断开测试
pub async fn read(
    mut reader: io::ReadHalf<TcpStream>,
    local_addr: SocketAddr,
    remote_addr: SocketAddr,
    tx: Sender<Zip>,
) {
    loop {
        let mut buf = [0u8; SOCKET_BUFFER_SIZE];
        match reader.read(&mut buf[..]).await {
            Ok(len) => {
                if len != 0 {
                    debug!("【TCP read success】 【Local_addr = {:?}】 【Remote_addr = {:?}】 【len = {}】",
                            local_addr,
                            remote_addr,
                            len
                            );
                    let association = Association::new(local_addr, remote_addr, Protocol::TCP);
                    let zip = Zip::build_data(Package::new(
                        association,
                        Bytes::copy_from_slice(&buf[..len]),
                    ));
                    //接收端drop后，跳出循环
                    if let Err(_) = tx.send(zip).await {
                        break;
                    }
                } else {
                    debug!("【TCP connection disconnected】 【Local_addr = {:?}】 【Remote_addr = {:?}】",
                            local_addr,
                            remote_addr
                            );
                    let association = Association::new(local_addr, remote_addr, Protocol::TCP);

                    //断开连接移除持有句柄
                    TCP_HANDLE_MAP.remove(&association);
                    let zip = Zip::build_event(Event::new(association, 0u8));
                    let _ = tx.send(zip).await;
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                continue;
            }
            Err(err) => {
                info!(
                    "【TCP read failure】 【Local_addr = {}】 【err = {:?}】",
                    local_addr.to_string(),
                    err,
                );
                break;
            }
        }
    }
}

pub async fn write(mut writer: io::WriteHalf<TcpStream>, mut rx: Receiver<Zip>) {
    //发送端drop后，释放writer，并调用默认shutdown，通知对端
    while let Some(zip) = rx.recv().await {
        match zip {
            Zip::Data(package) => {
                let bytes = package.get_data();
                let local_addr = package.get_association().get_local_addr();
                let remote_addr = package.get_association().get_remote_addr();
                match writer.write(&*bytes).await {
                    Ok(len) => {
                        debug!("【TCP write success】 【Local_addr = {:?}】 【Remote_addr = {:?}】 【len = {}】",
                            local_addr,
                            remote_addr,
                            len
                            );
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(err) => {
                        error!("【TCP write failure】 【Local_addr = {:?}】 【Remote_addr = {:?}】 【err = {:?}】",
                            local_addr,
                            remote_addr,
                            err
                            );
                        TCP_HANDLE_MAP.remove(package.get_association());
                        break;
                    }
                }
            }
            Zip::Event(event) => {
                if event.type_code == 0 {
                    TCP_HANDLE_MAP.remove(event.get_association());
                    break;
                }
            }
        }
    }
    let _ = writer.shutdown();
}
