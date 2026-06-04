use crate::exception::{GlobalResult, GlobalResultExt};
use crate::net::state::Protocol;
use log::error;
use std::net::{SocketAddr, TcpListener, UdpSocket};

#[cfg(feature = "net")]
pub fn listen(
    protocol: Protocol,
    socket_addr: SocketAddr,
) -> GlobalResult<(Option<TcpListener>, Option<UdpSocket>)> {
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
