//! Integration tests that drive the `extern "C"` surface directly through the
//! rlib target (the same symbols a C host links against).
//!
//! Reading C strings the FFI hands back requires `unsafe`; this test crate is a
//! separate compilation unit and does not inherit lib.rs's crate-level allow.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::ptr;

use octos_ffi::{
    OctosRuntime, octos_last_error, octos_run_task, octos_runtime_free, octos_runtime_new,
    octos_string_free, octos_version,
};

/// Helper: read the thread-local last-error as an owned String (or empty).
fn last_error_string() -> String {
    let p = octos_last_error();
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null pointer into the thread-local CString; copied immediately.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// A config that constructs a provider offline (no network is touched until a
/// task actually runs). The dummy key resolves through the reused
/// `Config::get_api_key` env_vars path.
fn valid_config_json() -> CString {
    CString::new(
        r#"{
            "provider": "openai",
            "model": "gpt-4o-mini",
            "api_key": "sk-ffi-test-dummy",
            "cwd": "."
        }"#,
    )
    .unwrap()
}

#[test]
fn valid_config_returns_nonnull_handle_and_frees() {
    let cfg = valid_config_json();
    let rt = octos_runtime_new(cfg.as_ptr());
    assert!(
        !rt.is_null(),
        "expected a handle, error={}",
        last_error_string()
    );
    // Round-trip the handle through free (must not crash / UB).
    octos_runtime_free(rt);
}

#[test]
fn empty_config_returns_null_and_sets_error() {
    let empty = CString::new("").unwrap();
    let rt = octos_runtime_new(empty.as_ptr());
    assert!(rt.is_null());
    assert!(
        !last_error_string().is_empty(),
        "last_error should be populated on failure"
    );
}

#[test]
fn invalid_config_json_returns_null_and_sets_error() {
    let bad = CString::new("{ this is not json").unwrap();
    let rt = octos_runtime_new(bad.as_ptr());
    assert!(rt.is_null());
    assert!(last_error_string().contains("invalid config_json"));
}

#[test]
fn missing_required_field_returns_null() {
    // No `provider` / `model`.
    let cfg = CString::new(r#"{"api_key":"x"}"#).unwrap();
    let rt = octos_runtime_new(cfg.as_ptr());
    assert!(rt.is_null());
    assert!(!last_error_string().is_empty());
}

#[test]
fn runtime_new_null_arg_is_safe() {
    let rt = octos_runtime_new(ptr::null());
    assert!(rt.is_null());
    assert!(!last_error_string().is_empty());
}

#[test]
fn run_task_null_runtime_is_safe() {
    let brief = CString::new(r#"{"prompt":"hi"}"#).unwrap();
    let out = octos_run_task(ptr::null_mut(), brief.as_ptr());
    assert!(out.is_null());
    assert!(last_error_string().contains("null"));
}

#[test]
fn run_task_null_brief_is_safe() {
    let cfg = valid_config_json();
    let rt = octos_runtime_new(cfg.as_ptr());
    assert!(!rt.is_null(), "error={}", last_error_string());
    let out = octos_run_task(rt, ptr::null());
    assert!(out.is_null());
    assert!(!last_error_string().is_empty());
    octos_runtime_free(rt);
}

#[test]
fn runtime_free_null_is_safe() {
    // Must not panic / UB.
    octos_runtime_free(ptr::null_mut());
}

#[test]
fn string_free_null_is_safe() {
    octos_string_free(ptr::null_mut());
}

#[test]
fn version_is_nonnull_and_readable() {
    let v = octos_version();
    assert!(!v.is_null());
    // SAFETY: static NUL-terminated string.
    let s = unsafe { CStr::from_ptr(v) }.to_str().unwrap();
    assert!(!s.is_empty());
}

#[test]
fn last_error_reflects_most_recent_failure() {
    // First failure: invalid JSON.
    let bad = CString::new("{ nope").unwrap();
    let _ = octos_runtime_new(bad.as_ptr());
    let first = last_error_string();
    assert!(first.contains("invalid config_json"), "got: {first}");

    // Second, different failure: null pointer. The stored error must update.
    let out = octos_run_task(ptr::null_mut(), ptr::null());
    assert!(out.is_null());
    let second = last_error_string();
    assert!(second.contains("null"), "got: {second}");
    assert_ne!(
        first, second,
        "last_error did not update to the newest failure"
    );
}

/// Real end-to-end run. Ignored: needs a live provider + network. Configure via
/// env: `OCTOS_FFI_TEST_PROVIDER`, `OCTOS_FFI_TEST_MODEL`, and the provider's
/// key env var (e.g. `OPENAI_API_KEY`). Run with:
///   cargo test -p octos-ffi --test ffi -- --ignored e2e_run_task
#[test]
#[ignore = "needs a real API key + network"]
fn e2e_run_task_returns_output_containing_ok() {
    let provider = std::env::var("OCTOS_FFI_TEST_PROVIDER").unwrap_or_else(|_| "openai".into());
    let model = std::env::var("OCTOS_FFI_TEST_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let key_env = std::env::var("OCTOS_FFI_TEST_KEY_ENV")
        .unwrap_or_else(|_| format!("{}_API_KEY", provider.to_uppercase()));

    let cfg_json = serde_json::json!({
        "provider": provider,
        "model": model,
        "api_key_env": key_env,
        "cwd": ".",
        "max_iterations": 3
    })
    .to_string();
    let cfg = CString::new(cfg_json).unwrap();
    let rt: *mut OctosRuntime = octos_runtime_new(cfg.as_ptr());
    assert!(!rt.is_null(), "runtime_new failed: {}", last_error_string());

    let brief = CString::new(r#"{"prompt":"Reply with exactly OK","max_iterations":3}"#).unwrap();
    let out = octos_run_task(rt, brief.as_ptr());
    assert!(!out.is_null(), "run_task failed: {}", last_error_string());

    // SAFETY: non-null owned string from octos_run_task.
    let json_str = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_owned();
    octos_string_free(out);
    octos_runtime_free(rt);

    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    let output = parsed["output"].as_str().unwrap_or_default();
    assert!(output.contains("OK"), "output did not contain OK: {output}");
}
