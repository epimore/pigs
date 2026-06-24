use base::err::BaseErrorCode;
use base::exception::{BizError, GlobalError};
use thiserror::Error;
use tonic::{Code, Status};

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("invalid metadata value for {name}: {value}")]
    InvalidMetadata { name: &'static str, value: String },
    #[error("connection task failed: {0}")]
    Connection(String),
}

pub fn status_from_global_error(error: GlobalError) -> Status {
    match error {
        GlobalError::BizErr(error) => status_from_biz_error(error),
        GlobalError::SysErr(error) => Status::internal(error.to_string()),
    }
}

fn status_from_biz_error(error: BizError) -> Status {
    let code = match error.code {
        code if code == BaseErrorCode::NotFound.code() => Code::NotFound,
        code if code == BaseErrorCode::InvalidRequest.code() => Code::InvalidArgument,
        code if code == BaseErrorCode::AlreadyExists.code() => Code::AlreadyExists,
        code if code == BaseErrorCode::Unauthorized.code() => Code::Unauthenticated,
        code if code == BaseErrorCode::PermissionDenied.code() => Code::PermissionDenied,
        code if code == BaseErrorCode::InvalidState.code() => Code::FailedPrecondition,
        code if code == BaseErrorCode::Unsupported.code() => Code::Unimplemented,
        code if code == BaseErrorCode::Timeout.code() => Code::DeadlineExceeded,
        code if code == BaseErrorCode::Network.code() => Code::Unavailable,
        code if code == BaseErrorCode::IoBusy.code() => Code::ResourceExhausted,
        _ => Code::Internal,
    };
    let mut status = Status::new(code, error.msg);
    if let Ok(value) = error.code.to_string().parse() {
        status.metadata_mut().insert("x-error-code", value);
    }
    status
}

#[cfg(test)]
mod tests {
    use base::exception::GlobalError;

    use super::*;

    #[test]
    fn maps_known_business_errors() {
        let status = status_from_global_error(GlobalError::BizErr(BizError {
            code: BaseErrorCode::NotFound.code(),
            msg: "missing".to_string(),
        }));
        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.metadata().get("x-error-code").unwrap(), "1140");
    }
}
