//! octos-wasm: a **browser/JS client-side** binding of octos's protocol +
//! utility types ([`octos_core`]).
//!
//! Use it from a web frontend to (de)serialize the `octos serve` WS/REST
//! protocol and to model messages / tasks / IDs — all client-side. The actual
//! **agent loop does NOT run in the browser**: `redb` (filesystem), `tokio`
//! threads, native TLS, and the llama.cpp embedder cannot target
//! `wasm32-unknown-unknown`. Run the agent via `octos serve` (native) and talk
//! to it over the network; this crate is the thin protocol/codec layer on the
//! client side. For native embedding use `octos-ffi` (C-ABI),
//! `octos-uniffi` (Python/Swift/Kotlin), or `octos-pyo3` (Python wheel).
//!
//! # Design
//!
//! Every export is split into a **pure logic layer** (plain `fn … -> String /
//! serde_json::Value / Result<_, RpcError>`, exercised by host `cargo test`)
//! and a thin `#[wasm_bindgen]` wrapper that marshals to/from [`JsValue`].
//! Structured values cross the boundary as ergonomic JS objects via
//! `serde-wasm-bindgen` (the `json_compatible()` serializer, so JS sees plain
//! objects — not ES `Map`s); wire frames are plain JSON **strings** (exactly
//! what you put on the WebSocket).
//!
//! ## Error convention
//!
//! Fallible exports reject with a structured **JSON-RPC error object**
//! (`{ code, message, data? }`, the same shape the decode path rejects with),
//! never a bare string. `code` is a JSON-RPC error code
//! (`invalid_params = -32602`, `frame_too_large = -32005`, …).
//!
//! # SAFETY
//!
//! The crate opts out of the workspace-wide `deny(unsafe_code)` with a
//! crate-level `#![allow(unsafe_code)]` because the `#[wasm_bindgen]` macro
//! expands to `unsafe` glue (exported shims, describe functions, and pointer
//! marshalling across the JS boundary). This is the same rationale `octos-ffi`
//! uses for its C-ABI boundary. The crate itself writes **no** hand-unsafe
//! code — all `unsafe` is macro-generated and upheld by wasm-bindgen.
#![allow(unsafe_code)]

use octos_core::app_ui_codec::{self, AppUiFrame};
use octos_core::ui_protocol::{RpcError, RpcNotification, RpcRequest};
use octos_core::{ClientMessageId, Message, SessionKey, TaskId, ThreadId, TurnId};
use serde::Serialize;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

// ===========================================================================
// Initialization
// ===========================================================================

/// Automatically installed on module load: routes Rust panics to
/// `console.error` with a readable message + stack instead of the opaque
/// `unreachable executed` trap. Safe to rely on; no need to call anything.
#[wasm_bindgen(start)]
pub fn start() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
}

// ===========================================================================
// serde <-> JsValue helpers
// ===========================================================================

/// Serialize a serde value into a plain JS object (not an ES `Map`).
fn to_js<T: Serialize + ?Sized>(value: &T) -> Result<JsValue, JsValue> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(|err| JsValue::from_str(&err.to_string()))
}

/// Serialize a decoded frame into a plain JS object, rendering integers that
/// exceed JS's safe-integer range (`> 2^53`) as `BigInt` so `u64` protocol
/// fields (cursor / event-sequence numbers) decode **losslessly**. Any
/// conversion failure surfaces as a structured [`RpcError`], never a bare
/// string.
fn frame_value_to_js(value: &Value) -> Result<JsValue, RpcError> {
    let serializer = serde_wasm_bindgen::Serializer::json_compatible()
        .serialize_large_number_types_as_bigints(true);
    value.serialize(&serializer).map_err(|err| {
        RpcError::internal_error(format!("failed to convert decoded frame to JS: {err}"))
    })
}

/// Render a JSON-RPC [`RpcError`] as a JS object (falling back to its message
/// string if it somehow fails to serialize).
fn rpc_error_to_js(err: RpcError) -> JsValue {
    to_js(&err).unwrap_or_else(|_| JsValue::from_str(&err.message))
}

/// Reject any incoming JS value that is not representable as strict, finite
/// JSON: `undefined` anywhere, a non-finite number (`NaN` / `±Infinity`), or a
/// `BigInt` / function / symbol. Everything else (null, bool, finite number,
/// string, array, plain object) is accepted. This prevents the silent
/// `NaN`/`Infinity`/`undefined` -> `null` coercion that would otherwise change
/// RPC semantics on the wire.
fn ensure_finite_json(value: &JsValue) -> Result<(), RpcError> {
    if value.is_undefined() {
        return Err(RpcError::invalid_params(
            "params contain `undefined`, which is not valid JSON",
        ));
    }
    if value.is_null() {
        return Ok(());
    }
    if let Some(number) = value.as_f64() {
        if number.is_finite() {
            return Ok(());
        }
        return Err(RpcError::invalid_params(
            "params contain a non-finite number (NaN or Infinity), which is not valid JSON",
        ));
    }
    if value.as_bool().is_some() || value.is_string() {
        return Ok(());
    }
    if js_sys::Array::is_array(value) {
        let array: js_sys::Array = value.clone().unchecked_into();
        for item in array.iter() {
            ensure_finite_json(&item)?;
        }
        return Ok(());
    }
    if value.is_object() {
        let object: js_sys::Object = value.clone().unchecked_into();
        for entry in js_sys::Object::entries(&object).iter() {
            // Each `entry` is a `[key, value]` pair.
            let pair: js_sys::Array = entry.unchecked_into();
            ensure_finite_json(&pair.get(1))?;
        }
        return Ok(());
    }
    Err(RpcError::invalid_params(
        "params contain a value that is not valid JSON (BigInt, function, or symbol)",
    ))
}

/// Convert an already-validated JS value into a `serde_json::Value`.
fn js_value_to_json(value: JsValue) -> Result<Value, RpcError> {
    serde_wasm_bindgen::from_value(value)
        .map_err(|err| RpcError::invalid_params(format!("params are not valid JSON: {err}")))
}

// ===========================================================================
// String / ID utilities
// ===========================================================================

/// Validate a JS number as a byte length and narrow it to `usize`.
///
/// JS has one numeric type (`f64`), so a `usize` ABI would silently wrap large
/// / fractional / negative inputs on `wasm32` (where `usize` is 32-bit). We
/// require a finite, integral value in `0..=u32::MAX`.
fn checked_byte_len(max_bytes: f64) -> Result<usize, RpcError> {
    if !max_bytes.is_finite() {
        return Err(RpcError::invalid_params(
            "max_bytes must be a finite number",
        ));
    }
    if max_bytes.fract() != 0.0 {
        return Err(RpcError::invalid_params("max_bytes must be an integer"));
    }
    if !(0.0..=f64::from(u32::MAX)).contains(&max_bytes) {
        return Err(RpcError::invalid_params(format!(
            "max_bytes must be in 0..={}",
            u32::MAX
        )));
    }
    Ok(max_bytes as usize)
}

fn truncate_utf8_impl(s: &str, max_bytes: f64, suffix: &str) -> Result<String, RpcError> {
    let max = checked_byte_len(max_bytes)?;
    Ok(octos_core::truncated_utf8(s, max, suffix))
}

/// UTF-8 safe truncation: returns a copy of `s` clamped to `max_bytes` bytes at
/// a `char` boundary, with `suffix` appended when truncation occurs. Returns
/// `s` unchanged when it already fits.
///
/// `max_bytes` is a JS number that must be a finite integer in `0..=2^32-1`
/// (throws a structured `invalid_params` error otherwise — a `usize` ABI would
/// silently wrap on `wasm32`). Wraps [`octos_core::truncated_utf8`].
#[wasm_bindgen]
pub fn truncate_utf8(s: &str, max_bytes: f64, suffix: &str) -> Result<String, JsValue> {
    truncate_utf8_impl(s, max_bytes, suffix).map_err(rpc_error_to_js)
}

/// Turn an arbitrary string into a filesystem-safe filename stem
/// (percent-encoded, clamped, collision-resistant). Wraps
/// [`octos_core::safe_filename`].
#[wasm_bindgen]
pub fn safe_filename(name: &str) -> String {
    octos_core::safe_filename(name)
}

/// Default per-tool output byte limit used by the server (client-side mirror
/// for UIs that show truncation hints). Wraps [`octos_core::tool_output_limit`].
#[wasm_bindgen]
pub fn tool_output_limit(tool_name: &str) -> usize {
    octos_core::tool_output_limit(tool_name)
}

// ===========================================================================
// ID generation
// ===========================================================================

/// Mint a fresh task id (UUID v7, temporally sortable). On wasm the timestamp
/// comes from JS `Date.now()` and the randomness from WebCrypto (uuid `js`
/// feature).
#[wasm_bindgen]
pub fn new_task_id() -> String {
    TaskId::new().to_string()
}

/// Mint a fresh turn id (UUID v7). Use as the `turn_id` on a `turn/start`
/// request (the client mints it so it can correlate the turn before the server
/// acknowledges).
#[wasm_bindgen]
pub fn new_turn_id() -> String {
    // `TurnId` has no `Display`; format its inner UUID (the wire form).
    TurnId::new().0.to_string()
}

/// Mint a fresh client-message-id (UUID v7). Use as the optimistic-UI /
/// idempotency token when submitting a user message.
#[wasm_bindgen]
pub fn new_client_message_id() -> String {
    ClientMessageId::generate().to_string()
}

fn new_thread_id_rooted_at_impl(cmid: &str) -> Result<String, RpcError> {
    // Reject empty ids at the core boundary (`try_new`), not silently produce
    // an empty thread id.
    let cmid =
        ClientMessageId::try_new(cmid).map_err(|err| RpcError::invalid_params(err.to_string()))?;
    let thread = ThreadId::rooted_at(&cmid);
    // The derived thread id must itself be a valid (non-empty) `ThreadId`.
    ThreadId::try_new(thread.as_str())
        .map(|valid| valid.as_str().to_string())
        .map_err(|err| RpcError::invalid_params(err.to_string()))
}

/// Derive the render-grouping `thread_id` a user message roots, from its
/// `client_message_id` (the canonical `thread_id == client_message_id` rule).
/// Rejects an empty `cmid` with a structured `invalid_params` error.
#[wasm_bindgen]
pub fn new_thread_id_rooted_at(cmid: &str) -> Result<String, JsValue> {
    new_thread_id_rooted_at_impl(cmid).map_err(rpc_error_to_js)
}

// ===========================================================================
// Message constructors (return ergonomic JS objects)
// ===========================================================================

/// Build a `user` [`Message`] as a JS object.
#[wasm_bindgen]
pub fn message_user(content: &str) -> Result<JsValue, JsValue> {
    to_js(&Message::user(content))
}

/// Build an `assistant` [`Message`] as a JS object.
#[wasm_bindgen]
pub fn message_assistant(content: &str) -> Result<JsValue, JsValue> {
    to_js(&Message::assistant(content))
}

/// Build a `system` [`Message`] as a JS object.
#[wasm_bindgen]
pub fn message_system(content: &str) -> Result<JsValue, JsValue> {
    to_js(&Message::system(content))
}

fn message_user_with_cmid_value(content: &str, cmid: &str) -> Result<Message, RpcError> {
    let cmid =
        ClientMessageId::try_new(cmid).map_err(|err| RpcError::invalid_params(err.to_string()))?;
    Ok(Message::user_with_cmid(content, cmid))
}

/// Build a `user` [`Message`] with an explicit `client_message_id` attached
/// (the production constructor — pins the optimistic-UI correlation token).
/// Rejects an empty `cmid` with a structured `invalid_params` error.
#[wasm_bindgen]
pub fn message_user_with_cmid(content: &str, cmid: &str) -> Result<JsValue, JsValue> {
    let message = message_user_with_cmid_value(content, cmid).map_err(rpc_error_to_js)?;
    to_js(&message)
}

// ===========================================================================
// SessionKey helpers (pure)
// ===========================================================================

/// `channel:chat_id` session key, e.g. `session_key_new("telegram", "42")`.
#[wasm_bindgen]
pub fn session_key_new(channel: &str, chat_id: &str) -> String {
    SessionKey::new(channel, chat_id).0
}

/// `channel:chat_id#topic` session key (empty topic == [`session_key_new`]).
#[wasm_bindgen]
pub fn session_key_with_topic(channel: &str, chat_id: &str, topic: &str) -> String {
    SessionKey::with_topic(channel, chat_id, topic).0
}

/// Base key without the topic suffix: `telegram:42#foo` -> `telegram:42`.
#[wasm_bindgen]
pub fn session_key_base(key: &str) -> String {
    SessionKey(key.to_string()).base_key().to_string()
}

/// Topic suffix if present: `telegram:42#foo` -> `foo` (else `undefined`).
#[wasm_bindgen]
pub fn session_key_topic(key: &str) -> Option<String> {
    SessionKey(key.to_string()).topic().map(str::to_string)
}

/// Profile id when the key is `{profile}:{channel}:{chat_id}` (else `undefined`).
#[wasm_bindgen]
pub fn session_key_profile_id(key: &str) -> Option<String> {
    SessionKey(key.to_string()).profile_id().map(str::to_string)
}

// ===========================================================================
// Protocol constants
// ===========================================================================

/// The UI protocol identifier string (`octos-ui/v1alpha1`).
#[wasm_bindgen]
pub fn ui_protocol_version() -> String {
    octos_core::ui_protocol::UI_PROTOCOL_V1.to_string()
}

/// Maximum accepted JSON-RPC text-frame size (bytes) for UI transports.
#[wasm_bindgen]
pub fn max_text_frame_bytes() -> usize {
    app_ui_codec::MAX_TEXT_FRAME_BYTES
}

/// The JSON-RPC version string (`2.0`) every AppUI wire frame must carry.
#[wasm_bindgen]
pub fn jsonrpc_version() -> String {
    octos_core::ui_protocol::JSON_RPC_VERSION.to_string()
}

// ===========================================================================
// Wire codec: decode (server -> client, and any AppUI JSON-RPC frame)
// ===========================================================================

/// Pure decode: validate + parse one AppUI JSON-RPC text frame into a tagged
/// `{ kind, frame }` value. `kind` is one of `request` / `response` / `error`
/// / `notification`; `frame` is the full JSON-RPC envelope.
fn decode_frame_impl(text: &str) -> Result<Value, RpcError> {
    Ok(frame_to_json(app_ui_codec::parse_text_frame(text)?))
}

/// As [`decode_frame_impl`] but tolerates a single trailing NDJSON line ending
/// (stdio transports emit one frame per `\n`-terminated line).
fn decode_ndjson_impl(text: &str) -> Result<Value, RpcError> {
    Ok(frame_to_json(app_ui_codec::parse_ndjson_frame(text)?))
}

fn frame_to_json(frame: AppUiFrame) -> Value {
    match frame {
        AppUiFrame::Request(req) => json!({ "kind": "request", "frame": req }),
        AppUiFrame::Response(res) => json!({ "kind": "response", "frame": res }),
        AppUiFrame::Error(err) => json!({ "kind": "error", "frame": err }),
        AppUiFrame::Notification(note) => json!({ "kind": "notification", "frame": note }),
    }
}

/// Decode + validate one AppUI JSON-RPC **text** frame received from the
/// server. Returns `{ kind, frame }` (see [`decode_frame_impl`]); rejects with
/// the JSON-RPC error object on malformed / oversized input. Integer fields
/// beyond JS's safe range decode as `BigInt` (lossless).
#[wasm_bindgen]
pub fn decode_ui_frame(text: &str) -> Result<JsValue, JsValue> {
    decode_frame_impl(text)
        .and_then(|value| frame_value_to_js(&value))
        .map_err(rpc_error_to_js)
}

/// As [`decode_ui_frame`] but for an NDJSON line (one optional trailing `\n`).
#[wasm_bindgen]
pub fn decode_ui_ndjson_frame(text: &str) -> Result<JsValue, JsValue> {
    decode_ndjson_impl(text)
        .and_then(|value| frame_value_to_js(&value))
        .map_err(rpc_error_to_js)
}

// ===========================================================================
// Wire codec: encode (client -> server)
// ===========================================================================

/// Reject a built frame that exceeds the server's text-frame byte limit (it
/// would be refused and the connection closed). Checked on the frame BEFORE any
/// NDJSON newline is appended (the server strips that before its own check).
fn ensure_frame_fits(frame: String) -> Result<String, RpcError> {
    if frame.len() > app_ui_codec::MAX_TEXT_FRAME_BYTES {
        return Err(app_ui_codec::frame_too_large_error());
    }
    Ok(frame)
}

/// Pure encode of a JSON-RPC **request** frame (a compact single-line string),
/// rejecting an over-limit result.
fn encode_request_impl(id: &str, method: &str, params: Value) -> Result<String, RpcError> {
    let frame = app_ui_codec::to_compact_json(&RpcRequest::new(id, method, params))
        .expect("RpcRequest<Value> always serializes");
    ensure_frame_fits(frame)
}

/// Pure encode of a JSON-RPC **notification** frame (no id), rejecting an
/// over-limit result.
fn encode_notification_impl(method: &str, params: Value) -> Result<String, RpcError> {
    let frame = app_ui_codec::to_compact_json(&RpcNotification::new(method, params))
        .expect("RpcNotification<Value> always serializes");
    ensure_frame_fits(frame)
}

/// Validate incoming JS params, convert to JSON, and build a request frame.
fn encode_request_checked(id: &str, method: &str, params: JsValue) -> Result<String, RpcError> {
    ensure_finite_json(&params)?;
    encode_request_impl(id, method, js_value_to_json(params)?)
}

fn encode_notification_checked(method: &str, params: JsValue) -> Result<String, RpcError> {
    ensure_finite_json(&params)?;
    encode_notification_impl(method, js_value_to_json(params)?)
}

/// Encode a client->server JSON-RPC **request** as a compact text frame ready
/// to send on the WebSocket. `params` must be strict, finite JSON (object /
/// array / string / finite number / bool / null); `undefined`, `NaN`,
/// `Infinity`, and `BigInt` are rejected with a structured `invalid_params`
/// error, and an over-limit frame is rejected with `frame_too_large`.
#[wasm_bindgen]
pub fn encode_rpc_request(id: &str, method: &str, params: JsValue) -> Result<String, JsValue> {
    encode_request_checked(id, method, params).map_err(rpc_error_to_js)
}

/// Encode a client->server JSON-RPC **notification** as a compact text frame.
/// Same param validation and size limit as [`encode_rpc_request`].
#[wasm_bindgen]
pub fn encode_rpc_notification(method: &str, params: JsValue) -> Result<String, JsValue> {
    encode_notification_checked(method, params).map_err(rpc_error_to_js)
}

/// As [`encode_rpc_request`] but appends a trailing `\n` for NDJSON transports.
/// The size limit is checked on the frame BEFORE the newline is appended.
#[wasm_bindgen]
pub fn encode_rpc_request_ndjson(
    id: &str,
    method: &str,
    params: JsValue,
) -> Result<String, JsValue> {
    let frame = encode_request_checked(id, method, params).map_err(rpc_error_to_js)?;
    Ok(format!("{frame}\n"))
}

// ===========================================================================
// Host-only unit tests (pure logic layer). The `#[wasm_bindgen]` wrappers that
// touch `JsValue` are covered by the wasm tests in `tests/web.rs`.
// ===========================================================================
#[cfg(all(test, not(target_arch = "wasm32")))]
mod host_tests {
    use super::*;
    use octos_core::ui_protocol::{InputItem, TurnStartParams, rpc_error_codes};

    #[test]
    fn truncate_utf8_passthrough_when_within_limit() {
        assert_eq!(truncate_utf8_impl("hello", 10.0, "...").unwrap(), "hello");
    }

    #[test]
    fn truncate_utf8_respects_multibyte_boundary() {
        // "你好世" = 9 bytes; a cut at 7 must back up to a char boundary (6).
        let out = truncate_utf8_impl("\u{4F60}\u{597D}\u{4E16}", 7.0, "...").unwrap();
        assert_eq!(out, "\u{4F60}\u{597D}...");
    }

    #[test]
    fn truncate_utf8_rejects_out_of_range_or_nonintegral_limits() {
        // 2^32 exceeds u32::MAX -> would wrap to 0 under a usize ABI.
        assert_eq!(
            truncate_utf8_impl("abcdef", 4_294_967_296.0, "…")
                .unwrap_err()
                .code,
            rpc_error_codes::INVALID_PARAMS
        );
        assert!(truncate_utf8_impl("abcdef", -1.0, "…").is_err());
        assert!(truncate_utf8_impl("abcdef", f64::NAN, "…").is_err());
        assert!(truncate_utf8_impl("abcdef", f64::INFINITY, "…").is_err());
        assert!(truncate_utf8_impl("abcdef", 2.5, "…").is_err());
        // u32::MAX itself is accepted.
        assert_eq!(
            truncate_utf8_impl("abcdef", f64::from(u32::MAX), "…").unwrap(),
            "abcdef"
        );
    }

    #[test]
    fn safe_filename_percent_encodes_specials() {
        assert_eq!(safe_filename("a b"), "a%20b");
        assert_eq!(safe_filename(""), "_");
    }

    #[test]
    fn tool_output_limit_matches_core() {
        assert_eq!(tool_output_limit("read_file"), 50_000);
        assert_eq!(tool_output_limit("unknown_tool"), 50_000);
    }

    #[test]
    fn task_ids_are_unique_valid_uuids() {
        let a = new_task_id();
        let b = new_task_id();
        assert_ne!(a, b, "two mints must differ");
        // Re-parse via octos-core's `TaskId: FromStr` (Uuid::parse_str under
        // the hood) so the host test needs no direct `uuid` dependency.
        assert!(
            a.parse::<octos_core::TaskId>().is_ok(),
            "must be a valid uuid: {a}"
        );
    }

    #[test]
    fn turn_ids_are_unique_valid_uuids() {
        let a = new_turn_id();
        let b = new_turn_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36, "hyphenated uuid form");
    }

    #[test]
    fn thread_id_roots_at_client_message_id() {
        assert_eq!(
            new_thread_id_rooted_at_impl("cmid-123").unwrap(),
            "cmid-123"
        );
    }

    #[test]
    fn thread_id_and_cmid_helpers_reject_empty() {
        assert_eq!(
            new_thread_id_rooted_at_impl("").unwrap_err().code,
            rpc_error_codes::INVALID_PARAMS
        );
        assert_eq!(
            message_user_with_cmid_value("hi", "").unwrap_err().code,
            rpc_error_codes::INVALID_PARAMS
        );
        // Non-empty still works.
        assert!(message_user_with_cmid_value("hi", "cmid-1").is_ok());
    }

    #[test]
    fn session_key_helpers_parse_all_dimensions() {
        assert_eq!(session_key_new("telegram", "42"), "telegram:42");
        assert_eq!(
            session_key_with_topic("telegram", "42", "research"),
            "telegram:42#research"
        );
        assert_eq!(session_key_base("telegram:42#research"), "telegram:42");
        assert_eq!(
            session_key_topic("telegram:42#research"),
            Some("research".to_string())
        );
        assert_eq!(session_key_topic("telegram:42"), None);
        assert_eq!(
            session_key_profile_id("_main:telegram:42"),
            Some("_main".to_string())
        );
    }

    #[test]
    fn decode_request_frame_is_tagged() {
        let frame = r#"{"jsonrpc":"2.0","id":"req-1","method":"session/open","params":{"session_id":"local:demo"}}"#;
        let decoded = decode_frame_impl(frame).expect("valid request frame");
        assert_eq!(decoded["kind"], json!("request"));
        assert_eq!(decoded["frame"]["method"], json!("session/open"));
        assert_eq!(decoded["frame"]["id"], json!("req-1"));
    }

    #[test]
    fn decode_notification_and_error_frames_are_tagged() {
        let note = r#"{"jsonrpc":"2.0","method":"server/heartbeat","params":{}}"#;
        assert_eq!(
            decode_frame_impl(note).expect("valid notification")["kind"],
            json!("notification")
        );

        let err = r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"invalid json"}}"#;
        let decoded = decode_frame_impl(err).expect("valid error frame");
        assert_eq!(decoded["kind"], json!("error"));
        assert_eq!(decoded["frame"]["error"]["code"], json!(-32700));
    }

    #[test]
    fn decode_preserves_u64_beyond_safe_integer_range() {
        // 2^53 + 1: not representable as an f64, must survive as an exact u64.
        let frame = r#"{"jsonrpc":"2.0","method":"m","params":{"seq":9007199254740993}}"#;
        let decoded = decode_frame_impl(frame).expect("valid frame");
        let seq = &decoded["frame"]["params"]["seq"];
        assert_eq!(seq.as_u64(), Some(9_007_199_254_740_993));
    }

    #[test]
    fn decode_rejects_malformed_frame() {
        // Multi-line frame is rejected by the single-line invariant.
        let err = decode_frame_impl("{\n}").expect_err("must reject");
        assert_eq!(err.code, rpc_error_codes::PARSE_ERROR);
    }

    #[test]
    fn decode_ndjson_strips_trailing_newline() {
        let frame = "{\"jsonrpc\":\"2.0\",\"method\":\"server/heartbeat\",\"params\":{}}\n";
        assert_eq!(
            decode_ndjson_impl(frame).expect("valid ndjson")["kind"],
            json!("notification")
        );
    }

    #[test]
    fn encode_request_roundtrips_through_decode() {
        let wire =
            encode_request_impl("req-1", "session/open", json!({"session_id": "local:demo"}))
                .expect("within size limit");
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","id":"req-1","method":"session/open","params":{"session_id":"local:demo"}}"#
        );
        let decoded = decode_frame_impl(&wire).expect("re-decode our own frame");
        assert_eq!(decoded["kind"], json!("request"));
    }

    #[test]
    fn encode_notification_omits_id() {
        let wire = encode_notification_impl("server/ping", json!({})).expect("within size limit");
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","method":"server/ping","params":{}}"#
        );
    }

    #[test]
    fn encode_rejects_oversized_frame() {
        let blob = "x".repeat(app_ui_codec::MAX_TEXT_FRAME_BYTES + 100);
        let err = encode_request_impl("req-1", "m", json!({ "blob": blob }))
            .expect_err("over the frame limit");
        assert_eq!(err.code, app_ui_codec::FRAME_TOO_LARGE);
    }

    /// Verifies the corrected `turn/start` example shape deserializes into
    /// octos-core's authoritative `TurnStartParams` (guards the README /
    /// example against protocol drift).
    #[test]
    fn turn_start_example_shape_matches_core() {
        let params = json!({
            "session_id": "web:demo",
            "turn_id": "00000000-0000-0000-0000-000000000000",
            "input": [{ "kind": "text", "text": "summarize the repo" }],
        });
        let parsed: TurnStartParams =
            serde_json::from_value(params).expect("matches TurnStartParams");
        assert_eq!(parsed.input.len(), 1);
        assert!(matches!(parsed.input[0], InputItem::Text { .. }));
        assert_eq!(parsed.session_id.0, "web:demo");
    }
}
