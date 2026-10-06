//! Offline tests for the default embedding model seam: the native
//! `embedding_model_status` / `embedding_model_ensure` helpers, their C entry
//! points, and a runtime that opts out of the download. Nothing here touches
//! the network: every test either disables downloading or only inspects disk.
use super::*;

fn runtime_without_download(data_dir: &Path) -> OctosRuntime {
    OctosRuntime::from_config(RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-model-not-a-real-key".into()),
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        embedding_auto_download: Some(false),
        ..RuntimeConfig::default()
    })
    .expect("runtime built")
}

fn parse(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("valid json")
}

#[test]
fn should_report_absent_model_for_an_empty_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let status = parse(&embedding_model_status(dir.path()).expect("status ok"));
    assert_eq!(status["present"], false);
    assert_eq!(status["complete"], false);
    assert_eq!(status["bytes"], 0);
    assert_eq!(
        status["path"],
        embed_model::default_model_path(dir.path())
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(status["url"], embed_model::DEFAULT_MODEL_URL);
    assert_eq!(
        status["license_url"],
        embed_model::DEFAULT_MODEL_LICENSE_URL
    );
    assert_eq!(status["sha256"], embed_model::DEFAULT_MODEL_SHA256);
    // Exactly the documented keys, so hosts can rely on the contract.
    let mut keys: Vec<&str> = status
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bytes",
            "complete",
            "license_url",
            "path",
            "present",
            "sha256",
            "url"
        ]
    );
}

#[test]
fn should_report_partial_file_as_present_but_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let path = embed_model::default_model_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not a whole gguf").unwrap();
    let status = parse(&embedding_model_status(dir.path()).unwrap());
    assert_eq!(status["present"], true);
    assert_eq!(status["complete"], false);
    assert_eq!(status["bytes"], 16);
}

#[test]
fn should_refuse_ensure_without_download_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let err = embedding_model_ensure(dir.path(), false).expect_err("absent + no download");
    assert!(matches!(err, CoreError::Embed(_)), "got {err:?}");
    assert!(
        err.to_string().contains("automatic download is disabled"),
        "got: {err}"
    );
    assert!(!dir.path().join("models").exists());
}

#[test]
fn should_reject_empty_data_dir() {
    let err = embedding_model_status(Path::new("")).expect_err("empty dir");
    assert!(err.to_string().contains("data_dir is empty"), "got: {err}");
    let err = embedding_model_ensure(Path::new(""), false).expect_err("empty dir");
    assert!(err.to_string().contains("data_dir is empty"), "got: {err}");
}

#[test]
fn should_build_keyword_only_runtime_when_download_is_opted_out() {
    // No model on disk + `embedding_auto_download: false`: the runtime still
    // builds, reports no embedder, and the Recall index works BM25-only.
    let dir = tempfile::tempdir().unwrap();
    let rt = runtime_without_download(dir.path());
    assert!(!rt.embedding_configured, "no embedder must be configured");
    assert!(!rt.has_embedder());
    assert!(matches!(rt.embed("hello"), Err(CoreError::NoEmbedder)));
    assert!(
        !dir.path().join("models").exists(),
        "no download must have been attempted"
    );

    let report = parse(
        &rt.memory_upsert(
            &json!({"records": [{
                "id": "doc:mail:1", "kind": "document", "source": "mail",
                "timestamp": "2026-09-01T10:00:00Z", "title": "Dentist appointment",
                "abstract": "Sunrise Dental on the 24th"
            }]})
            .to_string(),
        )
        .expect("upsert ok"),
    );
    assert_eq!(report["inserted"], 1);
    assert_eq!(report["embedded"], 0);
    let hits = parse(
        &rt.memory_search(r#"{"query":"dentist"}"#)
            .expect("search ok"),
    );
    assert_eq!(hits["hits"][0]["id"], "doc:mail:1");
    let stats = parse(&rt.memory_stats().unwrap());
    assert_eq!(stats["embedder_id"], "", "keyword-only ⇒ no embedder id");
    assert_eq!(stats["vectors_stored"], 0);
}

#[test]
fn should_keep_explicit_model_path_semantics_when_download_is_opted_out() {
    // An explicit path is loaded as-is; a bogus one is a hard error (never a
    // silent keyword-only fallback), independent of the download switch.
    // (The file exists but is not a GGUF: llama-cpp-2 `debug_assert!`s on a
    // MISSING path, so a garbage file is what exercises its load error.)
    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("nope.gguf");
    std::fs::write(&bogus, b"definitely not a gguf model").unwrap();
    let result = OctosRuntime::from_config(RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-model-not-a-real-key".into()),
        embedding_model_path: Some(bogus.to_string_lossy().into_owned()),
        embedding_auto_download: Some(false),
        ..RuntimeConfig::default()
    });
    #[cfg(feature = "embed-llama")]
    {
        let err = result.err().expect("missing explicit model must fail");
        assert!(matches!(err, CoreError::Embed(_)), "got {err:?}");
        assert!(err.to_string().contains("nope.gguf"), "got: {err}");
    }
    #[cfg(not(feature = "embed-llama"))]
    {
        // Feature off: the path is recorded (so `octos_embed` can say "not
        // compiled in") but never loaded.
        let rt = result.expect("feature-off build ignores the path");
        assert!(rt.embedding_configured);
        assert!(!rt.has_embedder());
    }
}

// ---------------------------------------------------------------------------
// C entry points
// ---------------------------------------------------------------------------

fn last_error() -> String {
    let p = octos_last_error();
    if p.is_null() {
        return String::new();
    }
    // SAFETY: non-null thread-local error string, copied immediately.
    unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned()
}

fn take_json(p: *mut c_char) -> serde_json::Value {
    assert!(!p.is_null(), "expected JSON, error={}", last_error());
    // SAFETY: fresh caller-owned allocation from this crate; read then freed once.
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
    octos_string_free(p);
    parse(&s)
}

#[test]
fn should_serve_model_status_and_ensure_through_c_without_a_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();

    let status = take_json(octos_embedding_model_status(data_dir.as_ptr()));
    assert_eq!(status["present"], false);
    assert_eq!(status["complete"], false);
    assert_eq!(status["sha256"], embed_model::DEFAULT_MODEL_SHA256);
    assert!(
        octos_last_error().is_null(),
        "success clears the last error"
    );

    assert!(octos_embedding_model_ensure(data_dir.as_ptr(), false).is_null());
    assert!(
        last_error().contains("automatic download is disabled"),
        "got: {}",
        last_error()
    );
}

#[test]
fn should_reject_null_and_invalid_data_dir_in_c_model_entry_points() {
    assert!(octos_embedding_model_status(ptr::null()).is_null());
    assert!(last_error().contains("data_dir: null pointer"));
    assert!(octos_embedding_model_ensure(ptr::null(), true).is_null());
    assert!(last_error().contains("data_dir: null pointer"));
    let bad = CString::new(vec![0xffu8, 0xfe]).unwrap();
    assert!(octos_embedding_model_status(bad.as_ptr()).is_null());
    assert!(last_error().contains("UTF-8"));
    let empty = CString::new("").unwrap();
    assert!(octos_embedding_model_status(empty.as_ptr()).is_null());
    assert!(last_error().contains("data_dir is empty"));
    // Like every other fallible entry point, a stale partial result is
    // cleared at entry.
    LAST_PARTIAL_RESULT.with(|slot| {
        *slot.borrow_mut() = Some(TaskResult {
            output: "stale".into(),
            iterations: 1,
            tokens: TokenUsage::default(),
        })
    });
    assert!(octos_embedding_model_ensure(ptr::null(), false).is_null());
    assert!(octos_take_last_partial_result().is_null());
}
