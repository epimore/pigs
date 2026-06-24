use std::str::FromStr;

use tonic::metadata::{Ascii, MetadataMap, MetadataValue};
use tonic::service::Interceptor;
use tonic::{Request, Status};

use crate::error::RpcError;

const REQUEST_ID: &str = "x-request-id";
const TRACE_ID: &str = "x-trace-id";
const NODE_ID: &str = "x-node-id";
const INSTANCE_ID: &str = "x-instance-id";
const PROTOCOL_VERSION: &str = "x-protocol-version";
const AUTHORIZATION: &str = "authorization";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RpcMetadata {
    pub request_id: Option<String>,
    pub trace_id: Option<String>,
    pub node_id: Option<String>,
    pub instance_id: Option<String>,
    pub protocol_version: Option<String>,
    pub bearer_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClientMetadataInterceptor {
    metadata: RpcMetadata,
}

impl ClientMetadataInterceptor {
    pub fn new(metadata: RpcMetadata) -> Self {
        Self { metadata }
    }
}

impl Interceptor for ClientMetadataInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        insert_optional(
            request.metadata_mut(),
            REQUEST_ID,
            &self.metadata.request_id,
        )?;
        insert_optional(request.metadata_mut(), TRACE_ID, &self.metadata.trace_id)?;
        insert_optional(request.metadata_mut(), NODE_ID, &self.metadata.node_id)?;
        insert_optional(
            request.metadata_mut(),
            INSTANCE_ID,
            &self.metadata.instance_id,
        )?;
        insert_optional(
            request.metadata_mut(),
            PROTOCOL_VERSION,
            &self.metadata.protocol_version,
        )?;
        if let Some(token) = &self.metadata.bearer_token {
            insert_value(
                request.metadata_mut(),
                AUTHORIZATION,
                &format!("Bearer {token}"),
            )?;
        }
        Ok(request)
    }
}

pub fn extract_rpc_metadata<T>(request: &Request<T>) -> RpcMetadata {
    let metadata = request.metadata();
    RpcMetadata {
        request_id: get_value(metadata, REQUEST_ID),
        trace_id: get_value(metadata, TRACE_ID),
        node_id: get_value(metadata, NODE_ID),
        instance_id: get_value(metadata, INSTANCE_ID),
        protocol_version: get_value(metadata, PROTOCOL_VERSION),
        bearer_token: get_value(metadata, AUTHORIZATION)
            .and_then(|value| value.strip_prefix("Bearer ").map(str::to_string)),
    }
}

fn insert_optional(
    metadata: &mut MetadataMap,
    name: &'static str,
    value: &Option<String>,
) -> Result<(), Status> {
    if let Some(value) = value {
        insert_value(metadata, name, value)?;
    }
    Ok(())
}

fn insert_value(metadata: &mut MetadataMap, name: &'static str, value: &str) -> Result<(), Status> {
    let value = MetadataValue::<Ascii>::from_str(value).map_err(|_| {
        let error = RpcError::InvalidMetadata {
            name,
            value: value.to_string(),
        };
        Status::invalid_argument(error.to_string())
    })?;
    metadata.insert(name, value);
    Ok(())
}

fn get_value(metadata: &MetadataMap, name: &'static str) -> Option<String> {
    metadata
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_and_extracts_metadata() {
        let expected = RpcMetadata {
            request_id: Some("request-1".to_string()),
            trace_id: Some("trace-1".to_string()),
            node_id: Some("stream-1".to_string()),
            instance_id: Some("instance-1".to_string()),
            protocol_version: Some("v1".to_string()),
            bearer_token: Some("secret".to_string()),
        };
        let mut interceptor = ClientMetadataInterceptor::new(expected.clone());
        let request = interceptor.call(Request::new(())).unwrap();
        assert_eq!(extract_rpc_metadata(&request), expected);
    }
}
