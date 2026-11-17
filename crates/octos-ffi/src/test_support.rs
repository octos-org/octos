//! Shared fixtures for the octos-ffi test modules (second-round dedup,
//! T-0644). Every helper mirrors a chain that was previously duplicated
//! verbatim across `memory_tests`, `model_tests`, `incomplete_tests` and
//! `tests`; assertions stay in the calling tests.
use super::*;

/// Base offline runtime config shared by the native test runtimes: a fake key
/// (no auth-store or network touch) and the embedding download disabled so the
/// runtime is keyword-only.
pub(crate) fn offline_config(data_dir: Option<&Path>) -> RuntimeConfig {
    RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-test-not-a-real-key".into()),
        data_dir: data_dir.map(|d| d.to_string_lossy().into_owned()),
        embedding_auto_download: Some(false),
        ..RuntimeConfig::default()
    }
}

/// Parse a JSON string produced by a native or C entry point.
pub(crate) fn parse(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("valid json")
}

/// Read the thread-local last-error as an owned String (empty when unset).
pub(crate) fn last_error() -> String {
    let p = octos_last_error();
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null thread-local error string, copied immediately.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned()
}

/// Unique, caller-owned temp dir (tests using it clean up after themselves).
pub(crate) fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "octos-ffi-{prefix}-{}-{}",
        std::process::id(),
        MEM_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
