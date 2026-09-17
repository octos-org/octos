//! Integration tests that drive the `extern "C"` surface directly through the
//! rlib target (the same symbols a C host links against).
//!
//! Reading C strings the FFI hands back requires `unsafe`; this test crate is a
//! separate compilation unit and does not inherit lib.rs's crate-level allow.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::ptr;

use octos_ffi::{
    OctosRuntime, octos_embed, octos_embedding_model_ensure, octos_embedding_model_status,
    octos_last_error, octos_memory_load, octos_memory_search, octos_memory_stats,
    octos_memory_upsert, octos_run_task, octos_runtime_free, octos_runtime_new, octos_string_free,
    octos_version,
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
/// task actually runs; the default embedding model download is disabled, so
/// the runtime is keyword-only). The dummy key resolves through the reused
/// `Config::get_api_key` env_vars path.
fn valid_config_json() -> CString {
    CString::new(
        r#"{
            "provider": "openai",
            "model": "gpt-4o-mini",
            "api_key": "sk-ffi-test-dummy",
            "cwd": ".",
            "embedding_auto_download": false
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

/// Read an owned FFI string as JSON and free it.
fn take_json(p: *mut libc::c_char) -> serde_json::Value {
    assert!(!p.is_null(), "expected JSON, error={}", last_error_string());
    // SAFETY: non-null owned string from the FFI; read then freed exactly once.
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
    octos_string_free(p);
    serde_json::from_str(&s).unwrap()
}

#[test]
fn memory_entry_points_null_runtime_are_safe() {
    let req = CString::new(r#"{"records":[]}"#).unwrap();
    assert!(octos_memory_upsert(ptr::null_mut(), req.as_ptr()).is_null());
    assert!(last_error_string().contains("null"));
    assert!(octos_memory_search(ptr::null_mut(), req.as_ptr()).is_null());
    assert!(last_error_string().contains("null"));
    assert!(octos_memory_load(ptr::null_mut(), req.as_ptr()).is_null());
    assert!(last_error_string().contains("null"));
    assert!(octos_memory_stats(ptr::null_mut()).is_null());
    assert!(last_error_string().contains("null"));
}

#[test]
fn memory_entry_points_null_argument_are_safe() {
    let cfg = valid_config_json();
    let rt = octos_runtime_new(cfg.as_ptr());
    assert!(!rt.is_null(), "error={}", last_error_string());
    assert!(octos_memory_upsert(rt, ptr::null()).is_null());
    assert!(last_error_string().contains("null"));
    assert!(octos_memory_search(rt, ptr::null()).is_null());
    assert!(last_error_string().contains("null"));
    assert!(octos_memory_load(rt, ptr::null()).is_null());
    assert!(last_error_string().contains("null"));
    octos_runtime_free(rt);
}

#[test]
fn memory_upsert_search_load_round_trip_bm25_only() {
    // No embedder configured: the index is BM25-only and every call still
    // works (the ADR's "recall is never disabled for lack of an embedder").
    let cfg = valid_config_json();
    let rt = octos_runtime_new(cfg.as_ptr());
    assert!(!rt.is_null(), "error={}", last_error_string());

    let upsert = CString::new(
        serde_json::json!({
            "records": [
                {"id": "doc:mail:42", "kind": "document", "source": "mail",
                 "timestamp": "2026-09-01T10:00:00Z", "title": "Dentist appointment",
                 "abstract": "Sunrise Dental on the 24th", "fingerprint": "h42"},
                {"id": "doc:calendar:7", "kind": "document", "source": "calendar",
                 "timestamp": "2026-09-20T08:00:00Z", "title": "Weekend hike with Sam",
                 "abstract": "West Hill trail", "parent": "series:hikes"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let report = take_json(octos_memory_upsert(rt, upsert.as_ptr()));
    assert_eq!(report["inserted"], 2);
    assert_eq!(report["embedded"], 0);
    assert!(octos_last_error().is_null());

    let search = CString::new(r#"{"query":"dentist","since":"2026-09-01","limit":5}"#).unwrap();
    let hits = take_json(octos_memory_search(rt, search.as_ptr()));
    let hits = hits["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["id"], "doc:mail:42");
    assert_eq!(hits[0]["trust"], "untrusted");

    let id = CString::new("doc:calendar:7").unwrap();
    let loaded = take_json(octos_memory_load(rt, id.as_ptr()));
    assert_eq!(loaded["record"]["parent"], "series:hikes");
    assert_eq!(loaded["record"]["visits"], 1);

    let missing = CString::new("doc:calendar:none").unwrap();
    assert!(octos_memory_load(rt, missing.as_ptr()).is_null());
    assert_eq!(last_error_string(), "no such record");

    let stats = take_json(octos_memory_stats(rt));
    assert_eq!(stats["records"], 2);
    assert_eq!(stats["by_source"]["mail"], 1);

    let bank = CString::new(
        r#"{"records":[{"id":"bank:x","kind":"knowledge","source":"bank",
            "timestamp":"2026-09-01T00:00:00Z","title":"x","abstract":"y"}]}"#,
    )
    .unwrap();
    assert!(octos_memory_upsert(rt, bank.as_ptr()).is_null());
    assert!(last_error_string().contains("knowledge"));

    octos_runtime_free(rt);
}

#[test]
fn embedding_model_status_reports_absent_model_without_a_runtime() {
    // No runtime handle, no network: an empty data dir reports the default
    // model as absent, with the provenance the host needs to show a user.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
    let status = take_json(octos_embedding_model_status(data_dir.as_ptr()));
    assert_eq!(status["present"], false);
    assert_eq!(status["complete"], false);
    assert_eq!(status["bytes"], 0);
    let path = status["path"].as_str().unwrap();
    assert!(
        path.starts_with(dir.path().to_str().unwrap()) && path.ends_with(".gguf"),
        "got path {path}"
    );
    assert!(status["url"].as_str().unwrap().starts_with("https://"));
    assert!(
        status["license_url"]
            .as_str()
            .unwrap()
            .starts_with("https://")
    );
    assert_eq!(status["sha256"].as_str().unwrap().len(), 64);
    assert!(
        octos_last_error().is_null(),
        "success clears the last error"
    );

    // NULL data_dir is rejected, not dereferenced.
    assert!(octos_embedding_model_status(ptr::null()).is_null());
    assert!(last_error_string().contains("data_dir: null"));
}

#[test]
fn embedding_model_ensure_without_download_fails_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
    assert!(octos_embedding_model_ensure(data_dir.as_ptr(), false).is_null());
    let err = last_error_string();
    assert!(err.contains("download is disabled"), "got: {err}");
    assert!(
        !dir.path().join("models").exists(),
        "nothing is created when downloading is disabled"
    );
    assert!(octos_embedding_model_ensure(ptr::null(), false).is_null());
    assert!(last_error_string().contains("data_dir: null"));
}

#[test]
fn runtime_without_model_and_downloads_disabled_is_keyword_only() {
    // The default-features build compiles the embedder, but with the download
    // opted out and no model on disk the runtime still builds — keyword-only —
    // and `octos_embed` reports the classic "no embedder configured".
    let dir = tempfile::tempdir().unwrap();
    let cfg = CString::new(
        serde_json::json!({
            "provider": "openai", "model": "gpt-4o-mini", "api_key": "sk-ffi-test-dummy",
            "data_dir": dir.path(), "embedding_auto_download": false
        })
        .to_string(),
    )
    .unwrap();
    let rt = octos_runtime_new(cfg.as_ptr());
    assert!(!rt.is_null(), "error={}", last_error_string());
    let text = CString::new("hello").unwrap();
    assert!(octos_embed(rt, text.as_ptr()).is_null());
    assert_eq!(last_error_string(), "no embedder configured");
    let stats = take_json(octos_memory_stats(rt));
    assert_eq!(stats["embedder_id"], "");
    octos_runtime_free(rt);
    assert!(
        !dir.path().join("models").exists(),
        "no download was attempted"
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
        "max_iterations": 3,
        "embedding_auto_download": false
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
