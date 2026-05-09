//! Transport-neutral AppUI frame codec.
//!
//! `Framing::Json` is the current WebSocket behavior: one complete JSON
//! object per transport message. `Framing::Ndjson` is the stdio shape for M9:
//! newline-delimited JSON with buffering for partial lines.

use std::fmt;

use octos_core::ui_protocol::{RpcErrorResponse, RpcNotification, RpcRequest, RpcResponse};
use serde::Serialize;
use serde_json::Value;

pub(crate) const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
    Json,
    Ndjson,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AppUiFrame {
    Request(RpcRequest<Value>),
    Response(RpcResponse<Value>),
    Error(RpcErrorResponse),
    Notification(RpcNotification<Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodecError {
    FrameTooLarge { limit: usize, actual: usize },
    Parse(String),
    InvalidFrame(String),
    Serialize(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge { limit, actual } => {
                write!(f, "frame exceeds {limit} bytes: {actual} bytes")
            }
            Self::Parse(message) => write!(f, "{message}"),
            Self::InvalidFrame(message) => write!(f, "{message}"),
            Self::Serialize(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CodecError {}

pub(crate) struct AppUiCodec {
    framing: Framing,
    max_frame_bytes: usize,
    #[allow(dead_code)]
    buffer: Vec<u8>,
}

impl AppUiCodec {
    pub(crate) fn new(framing: Framing) -> Self {
        Self::with_max_frame_bytes(framing, DEFAULT_MAX_FRAME_BYTES)
    }

    pub(crate) fn with_max_frame_bytes(framing: Framing, max_frame_bytes: usize) -> Self {
        Self {
            framing,
            max_frame_bytes,
            buffer: Vec::new(),
        }
    }

    pub(crate) fn encode<T: Serialize>(&self, frame: &T) -> Result<Vec<u8>, CodecError> {
        let mut bytes =
            serde_json::to_vec(frame).map_err(|error| CodecError::Serialize(error.to_string()))?;
        if bytes.len() > self.max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                limit: self.max_frame_bytes,
                actual: bytes.len(),
            });
        }
        if self.framing == Framing::Ndjson {
            bytes.push(b'\n');
        }
        Ok(bytes)
    }

    pub(crate) fn decode(&self, bytes: &[u8]) -> Result<AppUiFrame, CodecError> {
        if bytes.len() > self.max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                limit: self.max_frame_bytes,
                actual: bytes.len(),
            });
        }
        decode_json_frame(trim_trailing_newline(bytes))
    }

    #[allow(dead_code)]
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<AppUiFrame>, CodecError> {
        match self.framing {
            Framing::Json => Ok(vec![self.decode(bytes)?]),
            Framing::Ndjson => self.push_ndjson_bytes(bytes),
        }
    }

    #[allow(dead_code)]
    fn push_ndjson_bytes(&mut self, bytes: &[u8]) -> Result<Vec<AppUiFrame>, CodecError> {
        let mut frames = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.buffer);
                let line = trim_trailing_carriage_return(&line);
                if !line.is_empty() {
                    frames.push(self.decode(line)?);
                }
                continue;
            }
            if self.buffer.len() >= self.max_frame_bytes {
                return Err(CodecError::FrameTooLarge {
                    limit: self.max_frame_bytes,
                    actual: self.buffer.len() + 1,
                });
            }
            self.buffer.push(*byte);
        }
        Ok(frames)
    }
}

fn trim_trailing_newline(bytes: &[u8]) -> &[u8] {
    trim_trailing_carriage_return(bytes.strip_suffix(b"\n").unwrap_or(bytes))
}

fn trim_trailing_carriage_return(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

fn decode_json_frame(bytes: &[u8]) -> Result<AppUiFrame, CodecError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| CodecError::Parse(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| CodecError::InvalidFrame("JSON-RPC frame must be an object".into()))?;

    if object.contains_key("method") && object.contains_key("id") {
        return serde_json::from_value(value)
            .map(AppUiFrame::Request)
            .map_err(|error| CodecError::InvalidFrame(error.to_string()));
    }
    if object.contains_key("method") {
        return serde_json::from_value(value)
            .map(AppUiFrame::Notification)
            .map_err(|error| CodecError::InvalidFrame(error.to_string()));
    }
    if object.contains_key("result") {
        return serde_json::from_value(value)
            .map(AppUiFrame::Response)
            .map_err(|error| CodecError::InvalidFrame(error.to_string()));
    }
    if object.contains_key("error") {
        return serde_json::from_value(value)
            .map(AppUiFrame::Error)
            .map_err(|error| CodecError::InvalidFrame(error.to_string()));
    }

    Err(CodecError::InvalidFrame(
        "JSON-RPC frame must contain method, result, or error".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{RpcError, methods};
    use serde_json::json;

    #[test]
    fn json_framing_round_trips_a_request_without_delimiter() {
        let codec = AppUiCodec::new(Framing::Json);
        let request = RpcRequest::new("1", methods::SESSION_OPEN, json!({ "session_id": "s" }));

        let encoded = codec.encode(&request).expect("encode");
        let decoded = codec.decode(&encoded).expect("decode");

        assert!(!encoded.ends_with(b"\n"));
        assert!(matches!(
            decoded,
            AppUiFrame::Request(request) if request.id == "1"
                && request.method == methods::SESSION_OPEN
        ));
    }

    #[test]
    fn ndjson_framing_adds_delimiter_and_decodes_partial_lines() {
        let mut codec = AppUiCodec::new(Framing::Ndjson);
        let request = RpcRequest::new("2", methods::TURN_START, json!({ "input": [] }));
        let encoded = codec.encode(&request).expect("encode");

        assert!(encoded.ends_with(b"\n"));
        let split = encoded.len() - 2;
        assert!(
            codec
                .push_bytes(&encoded[..split])
                .expect("partial")
                .is_empty()
        );
        let decoded = codec.push_bytes(&encoded[split..]).expect("complete");

        assert_eq!(decoded.len(), 1);
        assert!(matches!(
            &decoded[0],
            AppUiFrame::Request(request) if request.id == "2"
                && request.method == methods::TURN_START
        ));
    }

    #[test]
    fn ndjson_decodes_multiple_frames_from_one_chunk() {
        let mut codec = AppUiCodec::new(Framing::Ndjson);
        let notification = RpcNotification::new(methods::MESSAGE_DELTA, json!({ "text": "hi" }));
        let response = RpcResponse::success("3", json!({ "accepted": true }));
        let mut bytes = codec.encode(&notification).expect("notification");
        bytes.extend(codec.encode(&response).expect("response"));

        let decoded = codec.push_bytes(&bytes).expect("decode");

        assert_eq!(decoded.len(), 2);
        assert!(
            matches!(&decoded[0], AppUiFrame::Notification(frame) if frame.method == methods::MESSAGE_DELTA)
        );
        assert!(matches!(&decoded[1], AppUiFrame::Response(frame) if frame.id == "3"));
    }

    #[test]
    fn decodes_json_rpc_error_frames() {
        let codec = AppUiCodec::new(Framing::Json);
        let error = RpcErrorResponse::new(Some("4".into()), RpcError::invalid_request("bad"));
        let encoded = codec.encode(&error).expect("encode");

        let decoded = codec.decode(&encoded).expect("decode");

        assert!(matches!(
            decoded,
            AppUiFrame::Error(frame) if frame.id.as_deref() == Some("4")
        ));
    }

    #[test]
    fn rejects_oversized_frames_before_json_parse() {
        let codec = AppUiCodec::with_max_frame_bytes(Framing::Json, 4);

        let error = codec.decode(b"{not-json}").expect_err("too large");

        assert_eq!(
            error,
            CodecError::FrameTooLarge {
                limit: 4,
                actual: 10
            }
        );
    }

    #[test]
    fn ndjson_rejects_oversized_partial_line() {
        let mut codec = AppUiCodec::with_max_frame_bytes(Framing::Ndjson, 4);

        let error = codec.push_bytes(b"12345").expect_err("too large");

        assert_eq!(
            error,
            CodecError::FrameTooLarge {
                limit: 4,
                actual: 5
            }
        );
    }
}
