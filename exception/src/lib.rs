pub mod code;
pub mod typed;

use std::fmt::{Display, Formatter};

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
    pub fn new_biz_error<O: FnOnce(String)>(code: u16, msg: &str, op: O) -> Self {
        op(format!("biz err = [code = {code}, msg=\"{msg}\"]"));
        Self::BizErr(BizError { code, msg: msg.to_string() })
    }

    pub fn new_sys_error<O: FnOnce(String)>(msg: &str, op: O) -> Self {
        op(format!("sys err = [{msg}]"));
        Self::SysErr(anyhow::anyhow!("{}", msg))
    }
}

/// 业务错误结构体
#[derive(Debug, Clone)]
pub struct BizError {
    pub code: u16,
    pub msg: String,
}

impl std::error::Error for BizError {}
impl Display for BizError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "BizError: [code = {}, msg = \"{}\"]", self.code, self.msg)
    }
}

/// 错误处理扩展 trait
pub trait GlobalResultExt<T> {
    fn hand_log<O: FnOnce(String)>(self, op: O) -> GlobalResult<T>;
    fn hand_biz_log<O: FnOnce(String)>(self, code: u16, msg: &str, op: O) -> GlobalResult<T>;
}

impl<T, E> GlobalResultExt<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn hand_log<O: FnOnce(String)>(self, op: O) -> GlobalResult<T> {
        self.map_err(|e| {
            op(format!("source err = [{e:?}]"));
            GlobalError::SysErr(anyhow::Error::from(e))
        })
    }
    fn hand_biz_log<O: FnOnce(String)>(self, code: u16, msg: &str, op: O) -> GlobalResult<T> {
        self.map_err(|e| {
            op(format!("biz err = [code = {code}, msg=\"{msg}\"]; source = [{e:?}]"));
            GlobalError::BizErr(BizError {
                code,
                msg: msg.to_string(),
            })
        })
    }
}
