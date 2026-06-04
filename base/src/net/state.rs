use bytes::Bytes;
use constructor::New;
use std::fmt::{Display, Formatter};
use std::net::SocketAddr;

pub const CHANNEL_BUFFER_SIZE: usize = 10000;
pub const UDP: &str = "UDP";
pub const TCP: &str = "TCP";
pub const ALL: &str = "ALL";

#[derive(Debug)]
pub enum IoEventType {
    Close = 0,
}

/// type_code = 0 means connection closed.
#[derive(Debug, New)]
pub struct Event {
    pub association: Association,
    pub type_code: IoEventType,
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
            type_code: IoEventType::Close,
        }
    }
}

#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy)]
pub enum Protocol {
    UDP,
    TCP,
    ALL,
}

impl Display for Protocol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::UDP => write!(f, "UDP"),
            Protocol::TCP => write!(f, "TCP"),
            Protocol::ALL => write!(f, "ALL"),
        }
    }
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

#[derive(Debug, Eq, Hash, PartialEq, New, Clone)]
pub struct Association {
    pub local_addr: SocketAddr,
    pub remote_addr: SocketAddr,
    pub protocol: Protocol,
}

impl Display for Association {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}/{}",
            self.protocol, self.local_addr, self.remote_addr
        )
    }
}

#[derive(Debug)]
pub enum Zip {
    Data(Package),
    Event(Event),
}

impl Zip {
    pub fn build_shutdown_zip(association: Option<Association>) -> Self {
        Self::Event(Event::build_shutdown_event(association))
    }

    pub fn is_shutdown_event(&self) -> bool {
        matches!(
            self,
            Self::Event(Event {
                type_code: IoEventType::Close,
                ..
            })
        )
    }

    pub fn build_data(package: Package) -> Self {
        Self::Data(package)
    }

    pub fn build_event(event: Event) -> Self {
        Self::Event(event)
    }

    pub fn get_association(&self) -> Association {
        match self {
            Self::Data(Package { association, .. }) => association.clone(),
            Self::Event(Event { association, .. }) => association.clone(),
        }
    }

    pub fn get_association_protocol(&self) -> &Protocol {
        match self {
            Self::Data(Package {
                association: Association { protocol, .. },
                ..
            }) => protocol,
            Self::Event(Event {
                association: Association { protocol, .. },
                ..
            }) => protocol,
        }
    }
}

#[derive(Debug, New)]
pub struct Package {
    pub association: Association,
    pub data: Bytes,
}
