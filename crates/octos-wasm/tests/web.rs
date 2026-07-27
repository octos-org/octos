//! wasm-bindgen tests for the browser-facing surface.
//!
//! These exercise the real `#[wasm_bindgen]` exports (the ones that marshal
//! `JsValue`), which the host unit tests in `src/lib.rs` cannot reach. They
//! also prove the wasm feature-gating is runtime-safe: `new_task_id` /
//! `message_user` invoke `Uuid::now_v7()` + `Utc::now()`, which panic on
//! `wasm32-unknown-unknown` unless uuid `js` + chrono `wasmbind` are active.
//!
//! Run with: `wasm-pack test --node`
#![cfg(target_arch = "wasm32")]

use octos_wasm::{
    decode_ui_frame, encode_rpc_notification, encode_rpc_request, encode_rpc_request_ndjson,
    max_text_frame_bytes, message_assistant, message_system, message_user, new_client_message_id,
    new_task_id, new_thread_id_rooted_at, new_turn_id, safe_filename, session_key_new,
    truncate_utf8,
};
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

// --- helpers ---------------------------------------------------------------

/// A JS object round-trips back to a serde value for assertions.
fn as_value(js: &JsValue) -> serde_json::Value {
    serde_wasm_bindgen::from_value(js.clone()).expect("JsValue -> serde_json::Value")
}

/// Build a single-property JS object `{ key: value }`.
fn obj_with(key: &str, value: JsValue) -> JsValue {
    let obj = js_sys::Object::new();
    js_sys::Reflect::set(&obj, &JsValue::from_str(key), &value).expect("set property");
    obj.into()
}

/// Read the JSON-RPC `code` off a structured error object.
fn err_code(err: &JsValue) -> f64 {
    js_sys::Reflect::get(err, &JsValue::from_str("code"))
        .expect("error has a `code`")
        .as_f64()
        .expect("`code` is a number")
}

const INVALID_PARAMS: f64 = -32602.0;
const FRAME_TOO_LARGE: f64 = -32005.0;
const PARSE_ERROR: f64 = -32700.0;

// --- utilities -------------------------------------------------------------

#[wasm_bindgen_test]
fn truncate_utf8_multibyte_boundary_is_safe() {
    assert_eq!(truncate_utf8("hello", 10.0, "...").expect("fits"), "hello");
    let out = truncate_utf8("\u{4F60}\u{597D}\u{4E16}", 7.0, "...").expect("ok");
    assert_eq!(out, "\u{4F60}\u{597D}...");
}

#[wasm_bindgen_test]
fn truncate_utf8_rejects_bad_js_numbers() {
    // 2^32 would wrap to 0 under a `usize` ABI on wasm32.
    assert_eq!(
        err_code(&truncate_utf8("abcdef", 4_294_967_296.0, "…").unwrap_err()),
        INVALID_PARAMS
    );
    assert!(truncate_utf8("abcdef", -1.0, "…").is_err());
    assert!(truncate_utf8("abcdef", f64::NAN, "…").is_err());
    assert!(truncate_utf8("abcdef", f64::INFINITY, "…").is_err());
    assert!(truncate_utf8("abcdef", 2.5, "…").is_err());
    // A valid limit still truncates correctly.
    assert_eq!(truncate_utf8("abcdef", 3.0, "..").expect("ok"), "abc..");
}

#[wasm_bindgen_test]
fn safe_filename_encodes_specials() {
    assert_eq!(safe_filename("a b"), "a%20b");
}

// --- ids -------------------------------------------------------------------

#[wasm_bindgen_test]
fn new_task_id_uses_browser_randomness_and_clock() {
    // If uuid's `js` feature were missing this would panic (no randomness /
    // no time source on wasm32-unknown-unknown).
    let a = new_task_id();
    let b = new_task_id();
    assert_ne!(a, b);
    assert_eq!(a.len(), 36, "uuid string form");
    assert!(!new_client_message_id().is_empty());
}

#[wasm_bindgen_test]
fn new_turn_id_mints_unique_uuids() {
    let a = new_turn_id();
    assert_eq!(a.len(), 36);
    assert_ne!(a, new_turn_id());
}

#[wasm_bindgen_test]
fn thread_id_roots_at_cmid_and_rejects_empty() {
    assert_eq!(new_thread_id_rooted_at("cmid-9").expect("ok"), "cmid-9");
    let err = new_thread_id_rooted_at("").expect_err("empty rejected");
    assert_eq!(err_code(&err), INVALID_PARAMS);
}

// --- messages --------------------------------------------------------------

#[wasm_bindgen_test]
fn message_user_serializes_with_chrono_wasmbind() {
    // Message::user calls Utc::now(); chrono `wasmbind` must supply the clock.
    let value = as_value(&message_user("hi there").expect("build user message"));
    assert_eq!(value["role"], serde_json::json!("user"));
    assert_eq!(value["content"], serde_json::json!("hi there"));
    assert!(
        value.get("timestamp").is_some(),
        "timestamp stamped via chrono wasmbind"
    );

    let sys = as_value(&message_system("sys").expect("system"));
    assert_eq!(sys["role"], serde_json::json!("system"));
    let asst = as_value(&message_assistant("ok").expect("assistant"));
    assert_eq!(asst["role"], serde_json::json!("assistant"));
}

// --- decode ----------------------------------------------------------------

#[wasm_bindgen_test]
fn decode_ui_frame_returns_tagged_object() {
    let frame = r#"{"jsonrpc":"2.0","id":"req-1","method":"session/open","params":{"session_id":"local:demo"}}"#;
    let value = as_value(&decode_ui_frame(frame).expect("decode valid frame"));
    assert_eq!(value["kind"], serde_json::json!("request"));
    assert_eq!(value["frame"]["method"], serde_json::json!("session/open"));
}

#[wasm_bindgen_test]
fn decode_ui_frame_rejects_bad_input_with_rpc_error() {
    let err = decode_ui_frame("not json").expect_err("must reject");
    assert_eq!(err_code(&err), PARSE_ERROR);
}

#[wasm_bindgen_test]
fn decode_preserves_large_u64_as_bigint() {
    // 2^53 + 1: unrepresentable as an f64 JS number; must decode as a BigInt
    // (not throw, and not lose precision).
    let frame = r#"{"jsonrpc":"2.0","method":"m","params":{"seq":9007199254740993}}"#;
    let decoded = decode_ui_frame(frame).expect("decode ok");
    let frame_v = js_sys::Reflect::get(&decoded, &JsValue::from_str("frame")).unwrap();
    let params_v = js_sys::Reflect::get(&frame_v, &JsValue::from_str("params")).unwrap();
    let seq = js_sys::Reflect::get(&params_v, &JsValue::from_str("seq")).unwrap();
    assert_eq!(
        seq.js_typeof().as_string().as_deref(),
        Some("bigint"),
        "large u64 must decode as BigInt"
    );
}

// --- encode ----------------------------------------------------------------

#[wasm_bindgen_test]
fn encode_request_builds_a_valid_wire_frame() {
    let params = serde_wasm_bindgen::to_value(&serde_json::json!({"session_id": "local:demo"}))
        .expect("params to JsValue");
    let wire = encode_rpc_request("req-1", "session/open", params).expect("encode");
    assert_eq!(
        wire,
        r#"{"jsonrpc":"2.0","id":"req-1","method":"session/open","params":{"session_id":"local:demo"}}"#
    );
    // Our own encoded frame decodes cleanly.
    let decoded = as_value(&decode_ui_frame(&wire).expect("re-decode"));
    assert_eq!(decoded["kind"], serde_json::json!("request"));
}

#[wasm_bindgen_test]
fn encode_notification_omits_id() {
    let params = serde_wasm_bindgen::to_value(&serde_json::json!({})).expect("params");
    let wire = encode_rpc_notification("server/ping", params).expect("encode");
    assert_eq!(
        wire,
        r#"{"jsonrpc":"2.0","method":"server/ping","params":{}}"#
    );
}

#[wasm_bindgen_test]
fn all_encoders_reject_non_finite_and_undefined_params() {
    let bad = || {
        vec![
            ("NaN", JsValue::from_f64(f64::NAN)),
            ("Infinity", JsValue::from_f64(f64::INFINITY)),
            ("undefined", JsValue::undefined()),
        ]
    };

    for (label, value) in bad() {
        let err = encode_rpc_request("req-1", "m", obj_with("a", value)).expect_err(label);
        assert_eq!(err_code(&err), INVALID_PARAMS, "request/{label}");
    }
    for (label, value) in bad() {
        let err = encode_rpc_notification("m", obj_with("a", value)).expect_err(label);
        assert_eq!(err_code(&err), INVALID_PARAMS, "notification/{label}");
    }
    for (label, value) in bad() {
        let err = encode_rpc_request_ndjson("req-1", "m", obj_with("a", value)).expect_err(label);
        assert_eq!(err_code(&err), INVALID_PARAMS, "ndjson/{label}");
    }
}

#[wasm_bindgen_test]
fn encode_rejects_oversized_frame_instead_of_emitting() {
    let blob = "x".repeat(max_text_frame_bytes() + 100);
    let params = obj_with("blob", JsValue::from_str(&blob));
    let err = encode_rpc_request("req-1", "m", params).expect_err("over the limit");
    assert_eq!(err_code(&err), FRAME_TOO_LARGE);
}

#[wasm_bindgen_test]
fn session_key_new_builds_channel_chat() {
    assert_eq!(session_key_new("telegram", "42"), "telegram:42");
}
