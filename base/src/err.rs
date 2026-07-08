use dashmap::DashMap;
use exception::{BizError, GlobalError};
use once_cell::sync::Lazy;
use std::borrow::Cow;

const FALLBACK_USER_MESSAGE: &str = "系统繁忙！请稍后重试。";

#[derive(Debug, Clone)]
pub struct ErrorOutput {
    pub code: u16,
    pub code_name: Cow<'static, str>,
    pub user_message: Cow<'static, str>,
    pub retryable: bool,
}

impl ErrorOutput {
    pub fn new(
        code: u16,
        code_name: impl Into<Cow<'static, str>>,
        user_message: impl Into<Cow<'static, str>>,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            code_name: code_name.into(),
            user_message: user_message.into(),
            retryable,
        }
    }
}

pub struct ErrorRegistration {
    pub register: fn(&DashMap<u16, ErrorOutput>),
}

inventory::collect!(ErrorRegistration);

//code: out_msg
static REGISTRY: Lazy<DashMap<u16, ErrorOutput>> = Lazy::new(|| {
    let map = DashMap::new();
    for reg in inventory::iter::<ErrorRegistration> {
        (reg.register)(&map);
    }
    map
});

pub trait CodeOutErr {
    fn out_err(&self) -> Cow<'static, str>;
}

pub fn error_output(code: u16) -> Option<ErrorOutput> {
    REGISTRY.get(&code).map(|item| item.clone())
}

pub fn global_error_output(error: &GlobalError) -> ErrorOutput {
    match error {
        GlobalError::BizErr(BizError { code, .. }) => error_output(*code).unwrap_or_else(|| {
            ErrorOutput::new(
                *code,
                Cow::Borrowed("Unregistered"),
                Cow::Borrowed(FALLBACK_USER_MESSAGE),
                false,
            )
        }),
        GlobalError::SysErr(_) => {
            error_output(BaseErrorCode::Internal.code()).unwrap_or_else(|| {
                ErrorOutput::new(
                    BaseErrorCode::Internal.code(),
                    Cow::Borrowed("Internal"),
                    Cow::Borrowed(FALLBACK_USER_MESSAGE),
                    false,
                )
            })
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
            $($name:ident => ($code:expr, $out:expr $(, $($meta:tt)+)?)),* $(,)?
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

            #[inline]
            pub fn code_name(self) -> &'static str {
                match self {
                    $(Self::$name => stringify!($name)),*
                }
            }

            #[inline]
            pub fn retryable(self) -> bool {
                match self {
                    $(Self::$name => $crate::__error_retryable!($($($meta)+)?)),*
                }
            }

            #[inline]
            pub fn output(self) -> $crate::err::ErrorOutput {
                $crate::err::ErrorOutput::new(
                    self.code(),
                    self.code_name(),
                    self.out_msg(),
                    self.retryable(),
                )
            }

            #[inline]
            pub fn from_code(code: u16) -> Option<Self> {
                match code {
                    $($code => Some(Self::$name),)*
                    _ => None,
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
                map: &$crate::dashmap::DashMap<u16, $crate::err::ErrorOutput>
            ) {
                $(
                    let output = $crate::err::ErrorOutput::new(
                        $code,
                        stringify!($name),
                        std::borrow::Cow::Borrowed($out),
                        $crate::__error_retryable!($($($meta)+)?),
                    );
                    if let Some(old) = map.insert($code, output) {
                        $crate::log::warn!(
                            "Err replaced; code={}, old={}, new={}",
                            $code, old.user_message, $out
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

#[doc(hidden)]
#[macro_export]
macro_rules! __error_retryable {
    () => {
        false
    };
    (retryable = $retryable:expr) => {
        $retryable
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
        Timeout => (1210, "设备响应超时或网络异常！请稍后重试。", retryable = true),
        Network => (1220, "网络异常！请稍后重试。", retryable = true),
        IoBusy => (1230, "系统繁忙！请稍后重试。", retryable = true),
        Internal => (1240, "系统繁忙！请稍后重试。"),
    }
}

impl CodeOutErr for GlobalError {
    fn out_err(&self) -> Cow<'static, str> {
        global_error_output(self).user_message
    }
}

#[test]
fn registered_biz_error_returns_registered_output() {
    let cow = GlobalError::BizErr(BizError {
        code: 1140,
        msg: "aaaa".to_string(),
    })
    .out_err();
    assert_eq!(cow, "请求的资源不存在或已被删除。");
}

#[test]
fn unregistered_biz_error_returns_fallback_without_logging() {
    let cow = GlobalError::BizErr(BizError {
        code: 11950,
        msg: "aaaa".to_string(),
    })
    .out_err();
    assert_eq!(cow, FALLBACK_USER_MESSAGE);
}

#[test]
fn error_output_includes_metadata() {
    let output = BaseErrorCode::Timeout.output();
    assert_eq!(output.code, 1210);
    assert_eq!(output.code_name, "Timeout");
    assert_eq!(output.user_message, "设备响应超时或网络异常！请稍后重试。");
    assert!(output.retryable);
}
