//! Shared AppUI JSON-RPC text-frame codec.
//!
//! This module owns transport-neutral framing rules used by WebSocket text
//! frames and stdio NDJSON lines. Transport layers should split/read frames,
//! then delegate envelope validation and compact serialization here.

pub use crate::ui_protocol::MAX_TEXT_FRAME_BYTES;
use crate::ui_protocol::{
    JSON_RPC_VERSION, RpcError, RpcErrorResponse, RpcNotification, RpcRequest, RpcResponse,
};
use serde::Serialize;
use serde_json::{Value, json};

/// JSON-RPC server-error slot used when a text frame exceeds the AppUI limit.
pub const FRAME_TOO_LARGE: i64 = -32005;

/// A validated AppUI JSON-RPC envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum AppUiFrame {
    Request(RpcRequest<Value>),
    Response(RpcResponse<Value>),
    Error(RpcErrorResponse),
    Notification(RpcNotification<Value>),
}

/// Serialize an AppUI envelope as compact JSON.
pub fn to_compact_json<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize,
{
    serde_json::to_string(value)
}

/// Serialize an AppUI envelope as one NDJSON frame.
pub fn to_ndjson_frame<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize,
{
    let mut frame = to_compact_json(value)?;
    frame.push('\n');
    Ok(frame)
}

/// Strip a single NDJSON line ending from a byte buffer in place.
pub fn strip_ndjson_line_ending_bytes(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

/// Parse one NDJSON frame after removing one optional trailing line ending.
pub fn parse_ndjson_frame(text: &str) -> Result<AppUiFrame, RpcError> {
    parse_text_frame(strip_ndjson_line_ending(text))
}

/// Parse and validate one AppUI JSON-RPC text frame.
pub fn parse_text_frame(text: &str) -> Result<AppUiFrame, RpcError> {
    validate_text_frame_boundary(text)?;
    let value: Value =
        serde_json::from_str(text).map_err(|err| RpcError::parse_error(err.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| RpcError::parse_error("envelope must be an object"))?;

    validate_jsonrpc_version(object.get("jsonrpc"))?;

    let has_method = object.contains_key("method");
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");

    match (has_method, has_result, has_error) {
        (true, false, false) => parse_request_or_notification(value),
        (false, true, false) => parse_response(value),
        (false, false, true) => parse_error_response(value),
        (false, false, false) => Err(RpcError::parse_error(
            "envelope missing method, result, or error",
        )),
        _ => Err(RpcError::parse_error(
            "envelope has conflicting JSON-RPC fields",
        )),
    }
}

/// Return the canonical AppUI frame-too-large error.
pub fn frame_too_large_error() -> RpcError {
    RpcError::new(
        FRAME_TOO_LARGE,
        format!("AppUI text frame exceeds {MAX_TEXT_FRAME_BYTES} bytes"),
    )
    .with_data(json!({ "limit_bytes": MAX_TEXT_FRAME_BYTES }))
}

fn strip_ndjson_line_ending(text: &str) -> &str {
    text.strip_suffix('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .unwrap_or(text)
}

fn validate_text_frame_boundary(text: &str) -> Result<(), RpcError> {
    if text.len() > MAX_TEXT_FRAME_BYTES {
        return Err(frame_too_large_error());
    }
    if text.trim().is_empty() {
        return Err(RpcError::parse_error("frame is empty"));
    }
    if text.contains('\n') || text.contains('\r') {
        return Err(RpcError::parse_error(
            "frame must be a single-line JSON object",
        ));
    }
    Ok(())
}

fn validate_jsonrpc_version(jsonrpc: Option<&Value>) -> Result<(), RpcError> {
    match jsonrpc.and_then(Value::as_str) {
        Some(JSON_RPC_VERSION) => Ok(()),
        Some(version) => Err(RpcError::invalid_request(format!(
            "unsupported JSON-RPC version: {version}"
        ))),
        None => Err(RpcError::invalid_request(
            "rpc envelope `jsonrpc` must be \"2.0\"",
        )),
    }
}

fn parse_request_or_notification(value: Value) -> Result<AppUiFrame, RpcError> {
    match value.get("id") {
        None => parse_notification(value),
        Some(Value::String(_)) => {
            let request = serde_json::from_value::<RpcRequest<Value>>(value)
                .map_err(|err| RpcError::parse_error(err.to_string()))?;
            Ok(AppUiFrame::Request(request))
        }
        Some(Value::Null) => Err(RpcError::parse_error(
            "rpc envelope `id` must not be null; omit the field for notifications",
        )),
        Some(Value::Number(_)) => Err(RpcError::parse_error(
            "rpc envelope `id` must be a string; numeric ids are not supported",
        )),
        Some(_) => Err(RpcError::parse_error(
            "rpc envelope `id` must be a string when present",
        )),
    }
}

fn parse_notification(value: Value) -> Result<AppUiFrame, RpcError> {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::parse_error("notification missing method"))?
        .to_owned();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Ok(AppUiFrame::Notification(RpcNotification {
        jsonrpc: JSON_RPC_VERSION.to_owned(),
        method,
        params,
    }))
}

fn parse_response(value: Value) -> Result<AppUiFrame, RpcError> {
    validate_string_id(value.get("id"), "rpc response")?;
    let response = serde_json::from_value::<RpcResponse<Value>>(value)
        .map_err(|err| RpcError::parse_error(err.to_string()))?;
    Ok(AppUiFrame::Response(response))
}

fn parse_error_response(value: Value) -> Result<AppUiFrame, RpcError> {
    validate_error_response_id(value.get("id"))?;
    let response = serde_json::from_value::<RpcErrorResponse>(value)
        .map_err(|err| RpcError::parse_error(err.to_string()))?;
    Ok(AppUiFrame::Error(response))
}

fn validate_string_id(id: Option<&Value>, envelope: &str) -> Result<(), RpcError> {
    match id {
        Some(Value::String(_)) => Ok(()),
        Some(Value::Null) => Err(RpcError::parse_error(format!(
            "{envelope} `id` must be a string"
        ))),
        Some(Value::Number(_)) => Err(RpcError::parse_error(format!(
            "{envelope} `id` must be a string; numeric ids are not supported"
        ))),
        Some(_) => Err(RpcError::parse_error(format!(
            "{envelope} `id` must be a string"
        ))),
        None => Err(RpcError::parse_error(format!("{envelope} missing id"))),
    }
}

fn validate_error_response_id(id: Option<&Value>) -> Result<(), RpcError> {
    match id {
        Some(Value::String(_)) | Some(Value::Null) => Ok(()),
        Some(Value::Number(_)) => Err(RpcError::parse_error(
            "rpc error response `id` must be a string or null; numeric ids are not supported",
        )),
        Some(_) => Err(RpcError::parse_error(
            "rpc error response `id` must be a string or null",
        )),
        None => Err(RpcError::parse_error("rpc error response missing id")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_protocol::{methods, rpc_error_codes};

    #[test]
    fn request_golden_roundtrip() {
        let request = RpcRequest::new(
            "req-1",
            methods::SESSION_OPEN,
            json!({ "session_id": "local:demo" }),
        );
        let golden = r#"{"jsonrpc":"2.0","id":"req-1","method":"session/open","params":{"session_id":"local:demo"}}"#;

        assert_eq!(to_compact_json(&request).expect("serialize"), golden);
        assert_eq!(
            to_ndjson_frame(&request).expect("serialize ndjson"),
            format!("{golden}\n")
        );
        assert_eq!(
            parse_text_frame(golden).expect("parse request"),
            AppUiFrame::Request(request.clone())
        );
        assert_eq!(
            parse_ndjson_frame(&format!("{golden}\n")).expect("parse ndjson request"),
            AppUiFrame::Request(request)
        );
    }

    #[test]
    fn response_golden_roundtrip() {
        let response = RpcResponse::success("req-1", json!({ "ok": true }));
        let golden = r#"{"jsonrpc":"2.0","id":"req-1","result":{"ok":true}}"#;

        assert_eq!(to_compact_json(&response).expect("serialize"), golden);
        assert_eq!(
            parse_text_frame(golden).expect("parse response"),
            AppUiFrame::Response(response)
        );
    }

    #[test]
    fn error_golden_roundtrip() {
        let response = RpcErrorResponse::new(None, RpcError::parse_error("invalid json"));
        let golden =
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"invalid json"}}"#;

        assert_eq!(to_compact_json(&response).expect("serialize"), golden);
        assert_eq!(
            parse_text_frame(golden).expect("parse error"),
            AppUiFrame::Error(response)
        );
    }

    #[test]
    fn notification_golden_roundtrip() {
        let notification = RpcNotification::new("server/heartbeat", json!({}));
        let golden = r#"{"jsonrpc":"2.0","method":"server/heartbeat","params":{}}"#;

        assert_eq!(to_compact_json(&notification).expect("serialize"), golden);
        assert_eq!(
            parse_text_frame(golden).expect("parse notification"),
            AppUiFrame::Notification(notification)
        );
    }

    #[test]
    fn malformed_frames_are_rejected_deterministically() {
        let cases = [
            ("", rpc_error_codes::PARSE_ERROR, "empty"),
            ("   ", rpc_error_codes::PARSE_ERROR, "empty"),
            ("{", rpc_error_codes::PARSE_ERROR, "EOF"),
            ("[]", rpc_error_codes::PARSE_ERROR, "object"),
            (
                r#"{"jsonrpc":"2.0",
"method":"ping","params":{}}"#,
                rpc_error_codes::PARSE_ERROR,
                "single-line",
            ),
            (
                r#"{"jsonrpc":"1.0","id":"req-1","method":"ping","params":{}}"#,
                rpc_error_codes::INVALID_REQUEST,
                "unsupported JSON-RPC version",
            ),
            (
                r#"{"id":"req-1","method":"ping","params":{}}"#,
                rpc_error_codes::INVALID_REQUEST,
                "jsonrpc",
            ),
            (
                r#"{"jsonrpc":"2.0","id":null,"method":"ping","params":{}}"#,
                rpc_error_codes::PARSE_ERROR,
                "null",
            ),
            (
                r#"{"jsonrpc":"2.0","id":42,"method":"ping","params":{}}"#,
                rpc_error_codes::PARSE_ERROR,
                "numeric ids",
            ),
            (
                r#"{"jsonrpc":"2.0","id":true,"method":"ping","params":{}}"#,
                rpc_error_codes::PARSE_ERROR,
                "id",
            ),
            (
                r#"{"jsonrpc":"2.0","id":42,"result":{}}"#,
                rpc_error_codes::PARSE_ERROR,
                "numeric ids",
            ),
            (
                r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"bad"}}"#,
                rpc_error_codes::PARSE_ERROR,
                "missing id",
            ),
        ];

        for (frame, code, message) in cases {
            let err = parse_text_frame(frame).expect_err(frame);
            assert_eq!(err.code, code, "{frame}");
            assert!(
                err.message.contains(message),
                "expected {message:?} in {:?} for {frame}",
                err.message
            );
        }
    }

    #[test]
    fn oversized_frame_is_rejected_before_deserialization() {
        let frame = "x".repeat(MAX_TEXT_FRAME_BYTES + 1);

        let err = parse_text_frame(&frame).expect_err("too large");

        assert_eq!(err.code, FRAME_TOO_LARGE);
        assert_eq!(
            err.data.as_ref().and_then(|data| data.get("limit_bytes")),
            Some(&json!(MAX_TEXT_FRAME_BYTES))
        );
    }
}
