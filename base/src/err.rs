use std::borrow::Cow;
use dashmap::DashMap;
use log::error;
use once_cell::sync::Lazy;
use exception::{BizError, GlobalError};

pub struct ErrorRegistration {
    pub register: fn(&DashMap<u16, Cow<'static, str>>),
}

inventory::collect!(ErrorRegistration);

//code: out_msg
static REGISTRY: Lazy<DashMap<u16, Cow<'static, str>>> = Lazy::new(|| {
    let map = DashMap::new();
    for reg in inventory::iter::<ErrorRegistration> {
        (reg.register)(&map);
    }
    map
});

pub trait CodeOutErr {
    fn out_err(&self) -> Cow<'static, str>;
}
impl CodeOutErr for GlobalError{
    fn out_err(&self) -> Cow<'static, str> {
        match self {
            GlobalError::BizErr(BizError{ code,msg }) => {
                match REGISTRY
                    .get(code) {
                    None => {
                        error!("Out error [BIZ]: {}",msg);
                        Cow::Borrowed("系统繁忙！请稍后重试。")
                    }
                    Some(e) => {
                        e.clone()
                    }
                }
            }
            GlobalError::SysErr(e) => {
                error!("Out error [SYSTEM]: {}",e);
                Cow::Borrowed("系统繁忙！请稍后重试。")
            }
        }
    }
}

/// 业务错误结构体
/// A保留：0..999;B对外暴露:1000..9999；C系统自用:10000..65535；
/// 1000..1099网络异常
/// 1100..1199数据异常
#[macro_export]
macro_rules! define_errors {
    (
        $enum_name:ident {
            $($name:ident => ($code:expr, $out:expr)),* $(,)?
        }
    ) => {
        #[derive(Debug, Clone, Copy)]
        #[repr(u16)]
        pub enum $enum_name {
            $($name = $code),*
        }

        impl $enum_name {
            #[inline]
            pub fn code(self) -> u16 {
                self as u16
            }

            #[inline]
            pub fn out_msg(self) -> &'static str {
                match self {
                    $(Self::$name => $out),*
                }
            }
        }

        // compile-time 冲突检测:同 crate 强校验
        $crate::paste::paste! {
            $(
                #[allow(non_upper_case_globals)]
                const [<__ERR_CODE_ $code>]: () = ();
            )*
        }

        // 唯一注册函数 + inventory
        $crate::paste::paste! {
            #[allow(non_snake_case)]
            fn [<__register_ $enum_name>](
                map: &dashmap::DashMap<u16, std::borrow::Cow<'static, str>>
            ) {
                $(
                    if let Some(old) = map.insert($code, std::borrow::Cow::Borrowed($out)) {
                        log::warn!(
                            "Err replaced; code={}, old={}, new={}",
                            $code, old, $out
                        );
                    }
                )*
            }

            $crate::inventory::submit! {
                $crate::err::ErrorRegistration {
                    register: [<__register_ $enum_name>]
                }
            }
        }
    };
}

define_errors! {
    BaseErrorCode {
        NotFound => (1140, "请求的资源不存在或已被删除。"),
        InvalidRequest => (1150, "无效的请求。"),
        AlreadyExists => (1160, "已存在，请勿重复操作。"),
        Unauthorized => (1170, "未登录用户，无法操作。"),
        PermissionDenied => (1180, "用户操作权限不足。"),
        InvalidState => (1190, "数据校验失败！请检查输入。"),
        Unsupported => (1200, "系统暂不支持！敬请期待。"),
        Timeout => (1210, "设备响应超时或网络异常！请稍后重试。"),
        Network => (1220, "网络异常！请稍后重试。"),
        IoBusy => (1230, "系统繁忙！请稍后重试。"),
        Internal => (1240, "系统繁忙！请稍后重试。"),
    }
}

#[test]
fn test_err_out(){
    let cow = GlobalError::BizErr(BizError { code: 1140, msg: "aaaa".to_string() }).out_err();
    println!("{}",cow);
    let cow = GlobalError::BizErr(BizError { code: 11950, msg: "aaaa".to_string() }).out_err();
    println!("{}",cow);
}