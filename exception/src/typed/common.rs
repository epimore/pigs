use thiserror::Error;

#[derive(Debug, Error)]
pub enum MessageBusError {
    #[error("通道已满")]
    Full,
    #[error("等待消息超时")]
    Timeout,
    #[error("消息通道已关闭")]
    ChannelClosed,
    #[error("消息类型不匹配")]
    TypeMismatch,
    #[error("消息滞后")]
    Lagged,
    #[error("通道不存在")]
    NotFound,
}