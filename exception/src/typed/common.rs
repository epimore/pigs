use thiserror::Error;

#[derive(Debug, Error)]
pub enum MessageBusError {
    #[error("通道已满")]
    Full,
    #[error("等待消息超时")]
    Timeout,
    #[error("通道已关闭")]
    ChannelClosed,
    #[error("类型不匹配")]
    TypeMismatch,
    #[error("消息滞后")]
    Lagged,
    #[error("通道不存在")]
    NotFound,
    #[error("通道已存在")]
    AlreadyExists,
}

#[derive(Error, Debug)]
pub enum RpcError {
    #[error("io error: {0}")]
    Io(String),
    #[error("timeout")]
    Timeout,
    #[error("remote error: {0}")]
    Remote(String),
    #[error("unknown error")]
    Unknown,
}