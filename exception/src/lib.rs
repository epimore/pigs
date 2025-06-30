pub mod code;
pub mod typed;

use std::error::Error;
use std::fmt::{Display, Formatter};
pub use anyhow;
pub use thiserror;

///全局错误：
/// 1.错误分为业务错误与系统错误
/// 2.错误发生时，内置日志记录
/// 3.新建业务错误、系统错误转换为业务错误，内置日志记录
/// 4.强烈建议面向顶级调用方时，暴露错误为业务错误
pub type GlobalResult<T> = Result<T, GlobalError>;

pub trait TransGlobalError {
    fn fmt_err<O: FnOnce(String)>(self, code: u16, msg: &str, op: O) -> Self;
}

//系统错误转换为业务错误
impl<T> TransGlobalError for GlobalResult<T> {
    ///将SysErr格式化为BizErr;向顶层调用方暴露错误信息
    fn fmt_err<O: FnOnce(String)>(self, code: u16, msg: &str, op: O) -> Self {
        match self {
            Err(GlobalError::SysErr(e)) => {
                Err(GlobalError::BizErr(BizError::build_biz_error(code, &msg, op, Some(&format!("{e:?}")))))
            }
            other => {
                other
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GlobalError {
    #[error(transparent)]
    BizErr(#[from] BizError),
    // #[error("{0}")]
    #[error(transparent)]
    SysErr(#[from] anyhow::Error),
}

impl GlobalError {
    //新建业务错误
    #[allow(dead_code)]
    pub fn new_biz_error<O: FnOnce(String)>(code: u16, msg: &str, op: O) -> Self {
        Self::BizErr(BizError::build_biz_error(code, msg, op, None))
    }

    #[allow(dead_code)]
    pub fn new_sys_error<O: FnOnce(String)>(msg:&str,op: O)->Self{
        op(format!("sys err = [{msg}]"));
        Self::SysErr(anyhow::anyhow!("{msg}"))
    }
}

#[derive(Debug)]
pub struct BizError {
    /// A保留：0..999;B对外暴露:1000..9999；C系统自用:10000..65535；
    /// 1000..1099网络异常
    /// 1100..1199数据异常
    /// ...
    pub code: u16,
    pub msg: String,
}

impl Error for BizError {}

impl Display for BizError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "BizError: [code = {},msg=\"{}\"]", self.code, self.msg)
    }
}

impl BizError {
    fn build_biz_error<O: FnOnce(String)>(code: u16, msg: &str, op: O, source: Option<&str>) -> Self {
        match source {
            None => { op(format!("biz err = [code = {code},msg=\"{msg}\"]")) }
            Some(s) => { op(format!("biz err = [code = {code},msg=\"{msg}\"] [fmt] source err = [{s}]")) }
        }
        Self { code, msg: String::from(msg) }
    }
}

///其他(三方/标准库...)错误转换为全局错误:系统错误
pub trait TransError<T> {
    type F;
    fn hand_log<O: FnOnce(String)>(self, op: O) -> Result<T, Self::F>;
}

impl<T, E: Send + Sync + 'static + Error> TransError<T> for Result<T, E> {
    type F = anyhow::Error;
    ///example
    /// 将socket创建绑定异常转换为GlobalError::SysErr,并记录为error日志
    /// let socket = UdpSocket::bind(addr).await.hand_err(|msg|error!("{msg}"))?;
    fn hand_log<O: FnOnce(String)>(self, op: O) -> Result<T, Self::F> {
        match self {
            Ok(t) => Ok(t),
            Err(e) => {
                op(format!("source err = [{e:?}]"));
                Err(anyhow::Error::from(e))
            }
        }
    }
}