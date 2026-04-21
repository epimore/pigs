use crate::net::state::Protocol;
use bytes::{BufMut, Bytes, BytesMut};
use exception::{GlobalResult, GlobalResultExt};
use log::{debug, error, warn};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::select;
use tokio_util::sync::CancellationToken;

pub trait PacketDispatcher: Send + Sync + 'static {
    fn dispatch(
        &self,
        data: Bytes,
        remote_addr: SocketAddr,
        protocol: Protocol,
    ) -> GlobalResult<()>;
}

pub trait PacketSplitter: Send + 'static {
    fn feed<F>(&mut self, chunk: &mut BytesMut, f: F)-> GlobalResult<()>
    where F: FnMut(Bytes) -> GlobalResult<()>;
}
const MAX_BUF_SIZE: usize = 2 * 1024 * 1024;
pub fn reader<D, S>(
    tu: (Option<std::net::TcpListener>, Option<std::net::UdpSocket>),
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    match tu {
        (Some(tcp), None) => spawn_tcp::<D, S>(tcp, cancel, dispatcher),
        (None, Some(udp)) => spawn_udp(udp, cancel, dispatcher),
        (Some(tcp), Some(udp)) => {
            spawn_tcp::<D, S>(tcp, cancel.clone(), dispatcher.clone())?;
            spawn_udp(udp, cancel, dispatcher)
        }
        _ => Ok(()),
    }
}
fn spawn_tcp<D, S>(
    tcp: std::net::TcpListener,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter + Default,
{
    tcp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;
    let listener = tokio::net::TcpListener::from_std(tcp).hand_log(|msg| error!("{msg}"))?;

    tokio::spawn(async move {
        loop {
            select! {
                biased;

                res = listener.accept() => {
                    match res {
                        Ok((stream, remote_addr)) => {
                            let dispatcher = dispatcher.clone();
                            let cancel = cancel.clone();

                            tokio::spawn(async move {
                                let splitter = S::default();
                                if let Err(e) = handle_tcp(stream, remote_addr, cancel, dispatcher, splitter).await{
                                    debug!("TCP connection {} closed with error: {}", remote_addr, e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("accept failed: {}", e);
                        }
                    }
                }

                _ = cancel.cancelled() => break,
            }
        }
    });

    Ok(())
}
async fn handle_tcp<D, S>(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
    mut splitter: S,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
    S: PacketSplitter,
{
    let mut buf = BytesMut::with_capacity(64 * 1024);
    loop {
        select! {
            res = tcp_stream_read_buf(&mut buf,&mut stream) => {
                match res {
                    Ok(0) => break,
                    Ok(_) => if buf.len()>MAX_BUF_SIZE {
                        warn!("Rev data greater than max buf size.close the peer");
                        break;
                    },
                    Err(err) => {
                        debug!("tcp read {} failed: {}", remote_addr, err);
                        break;
                    }
                };

                // 拆包
                splitter.feed(&mut buf, |pkt| {
                    dispatcher.dispatch(pkt, remote_addr, Protocol::TCP)
                })?;
            }
            _ = cancel.cancelled() => break,
        }
    }
    Ok(())
}

async fn tcp_stream_read_buf(buf: &mut BytesMut, stream: &mut TcpStream) -> std::io::Result<usize> {
    if buf.remaining_mut() < 4096 {
        buf.reserve(64 * 1024);
    }
    //Tokio 的 read_buf 内部已经处理了 WouldBlock 并挂起任务
    stream.read_buf(buf).await
}

fn spawn_udp<D>(
    udp: std::net::UdpSocket,
    cancel: CancellationToken,
    dispatcher: Arc<D>,
) -> GlobalResult<()>
where
    D: PacketDispatcher,
{
    udp.set_nonblocking(true).hand_log(|msg| error!("{msg}"))?;

    let socket = UdpSocket::from_std(udp).hand_log(|msg| debug!("{msg}"))?;

    tokio::spawn(async move {
        let mut buf = BytesMut::with_capacity(2048);
        loop {
            select! {
                Ok((n,addr)) = udp_socket_read_buf(&mut buf,&socket)=>{
                    if n!=0{
                        let data = buf.split().freeze();
                        let _ = dispatcher.dispatch(data, addr, Protocol::UDP);
                    }
                }

                _ = cancel.cancelled() => break,
            }
        }
    });

    Ok(())
}
async fn udp_socket_read_buf(
    buf: &mut BytesMut,
    socket: &UdpSocket,
) -> GlobalResult<(usize, SocketAddr)> {
    buf.clear();
    socket
        .recv_buf_from(buf)
        .await
        .hand_log(|msg| error!("read buf failed:{}", msg))
}
