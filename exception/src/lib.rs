pub mod typed;

use std::fmt::{Arguments, Display, Formatter};

use anyhow::{self, Error as AnyhowError};
use thiserror::Error;

/// 通用返回类型
pub type GlobalResult<T> = Result<T, GlobalError>;

/// 全局错误枚举
#[derive(Debug, Error)]
pub enum GlobalError {
    #[error(transparent)]
    BizErr(#[from] BizError),
    #[error(transparent)]
    SysErr(#[from] AnyhowError),
}

impl GlobalError {
    pub fn new_biz_error<O: FnOnce(Arguments)>(code: u16, msg: &str, op: O) -> Self {
        op(format_args!("biz err = [code = {code}, msg=\"{msg}\"]"));
        Self::BizErr(BizError {
            code,
            msg: msg.to_string(),
        })
    }

    pub fn new_sys_error<O: FnOnce(Arguments)>(msg: &str, op: O) -> Self {
        op(format_args!("sys err = [{msg}]"));
        Self::SysErr(anyhow::anyhow!("{}", msg))
    }

    pub fn from_external_error<E, O>(error: E, op: O) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
        O: FnOnce(Arguments),
    {
        op(format_args!("Trace = [{error:?}]"));
        Self::SysErr(anyhow::Error::from(error))
    }

    pub fn from_external_biz_error<E, O>(error: E, code: u16, msg: &str, op: O) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
        O: FnOnce(&str),
    {
        op(&format!(
            "Trace = [code = {code}, msg=\"{msg}\"]; source = [{error:?}]"
        ));
        Self::BizErr(BizError {
            code,
            msg: msg.to_string(),
        })
    }
}

/// 业务错误结构体
/// A保留：0..999;B对外暴露:1000..9999；C系统自用:10000..65535；
/// 1000..1099网络异常
/// 1100..1199数据异常
#[derive(Debug, Clone)]
pub struct BizError {
    pub code: u16,
    pub msg: String,
}

impl std::error::Error for BizError {}
impl Display for BizError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BizError: [code = {}, msg = \"{}\"]",
            self.code, self.msg
        )
    }
}

/// 错误处理扩展 trait
impl<T, E> GlobalResultExt<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn hand_log<O: FnOnce(Arguments)>(self, op: O) -> GlobalResult<T> {
        self.map_err(|e| GlobalError::from_external_error(e, op))
    }

    fn hand_log_str<O: FnOnce(&str)>(self, op: O) -> GlobalResult<T> {
        self.map_err(|e| {
            op(&format!("Trace = [{e:?}]"));
            GlobalError::SysErr(anyhow::Error::from(e))
        })
    }

    fn hand_biz_log<O: FnOnce(&str)>(self, code: u16, msg: &str, op: O) -> GlobalResult<T> {
        self.map_err(|e| GlobalError::from_external_biz_error(e, code, msg, op))
    }
}

pub trait GlobalResultExt<T> {
    fn hand_log<O: FnOnce(std::fmt::Arguments)>(self, op: O) -> GlobalResult<T>;
    fn hand_log_str<O: FnOnce(&str)>(self, op: O) -> GlobalResult<T>;
    fn hand_biz_log<O: FnOnce(&str)>(self, code: u16, msg: &str, op: O) -> GlobalResult<T>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_error_conversion_logs_once_at_conversion_point() {
        let mut log_count = 0;
        let error = std::io::Error::other("network down");
        let converted = GlobalError::from_external_error(error, |_| log_count += 1);

        assert_eq!(log_count, 1);
        assert!(matches!(converted, GlobalError::SysErr(_)));
    }

    #[test]
    fn external_biz_conversion_keeps_code_and_message() {
        let mut log_count = 0;
        let error = std::io::Error::other("node rejected");
        let converted =
            GlobalError::from_external_biz_error(error, 3202, "node rpc timeout", |_| {
                log_count += 1
            });

        assert_eq!(log_count, 1);
        let GlobalError::BizErr(BizError { code, msg }) = converted else {
            panic!("expected biz error");
        };
        assert_eq!(code, 3202);
        assert_eq!(msg, "node rpc timeout");
    }
}
