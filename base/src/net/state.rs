use bytes::Bytes;
use constructor::{Get, New, Set};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::{Receiver, Sender};

//TCP连接有状态，需要持有每个连接的句柄
pub static TCP_HANDLE_MAP: Lazy<DashMap<Association, Sender<Zip>>> =
    Lazy::new(|| DashMap::new());
pub const SOCKET_BUFFER_SIZE: usize = 4096;
pub const CHANNEL_BUFFER_SIZE: usize = 10000;
pub const UDP: &str = "UDP";
pub const TCP: &str = "TCP";
pub const ALL: &str = "ALL";

///type_code = 0 为连接断开
#[derive(Debug, New, Set, Get)]
pub struct Event {
    pub association: Association,
    pub type_code: u8,
}

impl Event {
    fn build_shutdown_event(association: Option<Association>) -> Self {
        let association = association.unwrap_or_else(|| Association {
            local_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            remote_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            protocol: Protocol::ALL,
        });
        Self {
            association,
            type_code: 0,
        }
    }
}

#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy)]
pub enum Protocol {
    UDP,
    TCP,
    ALL,
}

impl Protocol {
    pub fn get_value(&self) -> &str {
        match self {
            Protocol::UDP => UDP,
            Protocol::TCP => TCP,
            Protocol::ALL => ALL,
        }
    }
}

/// 网络相关(协议，本地地址，本地端口号，远地地址，远地端口号）
#[derive(Debug, Eq, Hash, PartialEq, New, Set, Get, Clone)]
pub struct Association {
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
    pub protocol: Protocol,
}

///EVENT:
/// 0-TCP链接断开；input->对端断开连接；output->主动断开连接
#[derive(Debug)]
pub enum Zip {
    Data(Package),
    //网络事件
    Event(Event),
}

impl Zip {
    pub fn build_shutdown_zip(association: Option<Association>) -> Self {
        Self::Event(Event::build_shutdown_event(association))
    }

    pub fn is_shutdown_event(&self) -> bool {
        if let Self::Event(Event { type_code: 0, .. }) = self {
            return true;
        }
        false
    }
    pub fn build_data(package: Package) -> Self {
        Self::Data(package)
    }

    pub fn build_event(event: Event) -> Self {
        Self::Event(event)
    }

    pub fn get_association(&self) -> Association {
        match &self {
            Zip::Data(Package { association, .. }) => association.clone(),
            Zip::Event(Event { association, .. }) => association.clone(),
        }
    }

    pub fn get_association_protocol(&self) -> &Protocol {
        match self {
            Zip::Data(Package {
                association: Association { protocol, .. },
                ..
            }) => protocol,
            Zip::Event(Event {
                association: Association { protocol, .. },
                ..
            }) => protocol,
        }
    }
}

#[derive(Debug, New, Set, Get)]
pub struct Package {
    pub association: Association,
    pub data: Bytes,
}

impl Package {
    pub fn get_owned_data(self) -> Bytes {
        self.data
    }
}

#[derive(Debug, New, Set, Get)]
pub struct Gate {
    //监听地址
    local_addr: SocketAddr,
    //从socket读取数据向程序发送
    input: Sender<Zip>,
    //从程序中接收数据向socket写入
    output: Receiver<Zip>,
}

impl Gate {
    pub fn get_owned_output(self) -> Receiver<Zip> {
        self.output
    }
}

#[derive(Debug)]
pub enum GateListener {
    Tcp(Gate, TcpListener),
    Udp(Gate, UdpSocket),
    All((Gate, TcpListener), (Gate, UdpSocket)),
}

impl GateListener {
    pub fn build_tcp(gate: Gate, tcp_listener: TcpListener) -> Self {
        Self::Tcp(gate, tcp_listener)
    }
    pub fn build_udp(gate: Gate, udp_socket: UdpSocket) -> Self {
        Self::Udp(gate, udp_socket)
    }
    pub fn build_all(tg: GateListener, ug: GateListener) -> Self {
        match (tg, ug) {
            (GateListener::Tcp(t_gate, tcp_listener), GateListener::Udp(u_gate, udp_socket)) => {
                Self::All((t_gate, tcp_listener), (u_gate, udp_socket))
            }
            _ => panic!("build_all requires a Tcp and Udp listener"),
        }
    }
}

#[derive(Debug)]
pub enum GateAccept {
    //SocketAddr:remote_addr
    Tcp(Gate, SocketAddr, TcpStream),
    Udp(Gate, UdpSocket),
}

impl GateAccept {
    pub fn accept_tcp(gate: Gate, remote_addr: SocketAddr, tcp_stream: TcpStream) -> Self {
        Self::Tcp(gate, remote_addr, tcp_stream)
    }
    pub fn accept_udp(gate: Gate, udp_socket: UdpSocket) -> Self {
        Self::Udp(gate, udp_socket)
    }
}
