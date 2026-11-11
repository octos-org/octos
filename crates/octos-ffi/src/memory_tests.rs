//! Offline tests for the Recall memory seam: the native `memory_*` methods
//! (BM25 only — no embedder is configured, so nothing is embedded) and the
//! `octos_memory_*` C entry points' marshalling and NULL handling.
use super::*;

/// A runtime whose stores live in a scratch dir (no `data_dir`). The dummy
/// key builds the provider offline; no network is touched by memory calls
/// (the default embedding model download is disabled, so the runtime is
/// keyword-only).
fn scratch_runtime() -> OctosRuntime {
    OctosRuntime::from_config(RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-memory-not-a-real-key".into()),
        embedding_auto_download: Some(false),
        ..RuntimeConfig::default()
    })
    .expect("runtime built")
}

fn doc(id: &str, title: &str, abstract_: &str, source: &str, ts: &str) -> serde_json::Value {
    json!({
        "id": id,
        "kind": "document",
        "source": source,
        "timestamp": ts,
        "title": title,
        "abstract": abstract_,
        "fingerprint": format!("fp-{id}"),
    })
}

fn upsert_json(records: Vec<serde_json::Value>) -> String {
    json!({ "records": records }).to_string()
}

fn parse(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("valid json")
}

fn seed(rt: &OctosRuntime) -> serde_json::Value {
    parse(
        &rt.memory_upsert(&upsert_json(vec![
            doc(
                "doc:mail:1",
                "Dentist appointment",
                "Sunrise Dental on the 24th at 10:30",
                "mail",
                "2026-09-01T10:00:00Z",
            ),
            doc(
                "doc:calendar:hike",
                "Weekend hike with Sam",
                "West Hill trail, bring water",
                "calendar",
                "2026-09-20T08:00:00Z",
            ),
        ]))
        .expect("upsert ok"),
    )
}

#[test]
fn should_reject_bad_search_requests() {
    let rt = scratch_runtime();
    let err = |req: &str| rt.memory_search(req).unwrap_err().to_string();
    assert!(err(r#"{"query":"   "}"#).contains("query is empty"));
    assert!(err(r#"{"query":"x","kinds":["bogus"]}"#).contains("unknown kind 'bogus'"));
    assert!(err(r#"{"query":"x","since":"yesterday"}"#).contains("since:"));
    assert!(err(r#"{"query":"x","until":"2026-13-40"}"#).contains("until:"));
    assert!(err("not json").contains("invalid search json"));
    assert!(matches!(
        rt.memory_search("{}").unwrap_err(),
        CoreError::Memory(_)
    ));
}

#[test]
fn should_reject_knowledge_kind_empty_ids_and_oversized_batches() {
    let rt = scratch_runtime();
    let bank = json!({
        "id": "bank:sam-lee", "kind": "knowledge", "source": "bank",
        "timestamp": "2026-09-01T00:00:00Z", "title": "Sam Lee", "abstract": "friend"
    });
    let err = rt.memory_upsert(&upsert_json(vec![bank])).unwrap_err();
    assert!(err.to_string().contains("knowledge"), "got: {err}");
    assert!(matches!(err, CoreError::Memory(_)));

    let blank = doc("  ", "t", "a", "mail", "2026-09-01T00:00:00Z");
    let err = rt.memory_upsert(&upsert_json(vec![blank])).unwrap_err();
    assert!(err.to_string().contains("id is empty"), "got: {err}");

    let many: Vec<_> = (0..=MAX_UPSERT_RECORDS)
        .map(|i| {
            doc(
                &format!("doc:mail:{i}"),
                "t",
                "a",
                "mail",
                "2026-09-01T00:00:00Z",
            )
        })
        .collect();
    let err = rt.memory_upsert(&upsert_json(many)).unwrap_err();
    assert!(err.to_string().contains("too many records"), "got: {err}");

    let err = rt.memory_upsert("{\"records\": 3}").unwrap_err();
    assert!(
        err.to_string().contains("invalid upsert json"),
        "got: {err}"
    );
    // Nothing above reached the store.
    assert_eq!(parse(&rt.memory_stats().unwrap())["records"], 0);
    // An empty batch is a no-op, not an error.
    assert_eq!(
        parse(&rt.memory_upsert(r#"{"records":[]}"#).unwrap())["inserted"],
        0
    );
}

#[test]
fn should_force_untrusted_and_reset_counters_on_ingest() {
    let rt = scratch_runtime();
    let mut r = doc(
        "doc:mail:t",
        "Trusted?",
        "claims trust",
        "mail",
        "2026-09-01T00:00:00Z",
    );
    r["trust"] = json!("trusted");
    r["visits"] = json!(1000);
    r["promoted"] = json!(true);
    rt.memory_upsert(&upsert_json(vec![r])).unwrap();
    let loaded = parse(&rt.memory_load("doc:mail:t").unwrap());
    assert_eq!(loaded["record"]["trust"], "untrusted");
    assert_eq!(
        loaded["record"]["visits"], 1,
        "host-supplied visits ignored"
    );
    assert_eq!(loaded["record"]["promoted"], false);
}

#[test]
fn should_accept_caller_vectors_and_reject_length_mismatch() {
    let rt = scratch_runtime();
    let rec = doc(
        "doc:mail:v",
        "Vec",
        "with vector",
        "mail",
        "2026-09-01T00:00:00Z",
    );
    let err = rt
        .memory_upsert(&json!({"records": [rec.clone()], "vectors": []}).to_string())
        .unwrap_err();
    assert!(err.to_string().contains("vectors length 0"), "got: {err}");

    // A vector at least as wide as the recall dimension is stored (truncated).
    let vector: Vec<f32> = (0..DEFAULT_RECALL_DIMENSION + 8)
        .map(|i| i as f32)
        .collect();
    let report = parse(
        &rt.memory_upsert(&json!({"records": [rec], "vectors": [vector]}).to_string())
            .unwrap(),
    );
    assert_eq!(report["vectors_stored"], 1);
    assert_eq!(report["embedded"], 0, "caller vectors ⇒ nothing embedded");
    assert_eq!(parse(&rt.memory_stats().unwrap())["vectors_stored"], 1);
}

#[test]
fn should_report_no_such_record_on_load_miss() {
    let rt = scratch_runtime();
    let err = rt.memory_load("doc:mail:missing").unwrap_err();
    assert_eq!(err.to_string(), "no such record");
    assert!(
        rt.memory_load("  ")
            .unwrap_err()
            .to_string()
            .contains("id is empty")
    );
}

#[test]
fn should_persist_stores_under_data_dir_across_runtimes() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("octos-data");
    let cfg = || RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-memory-not-a-real-key".into()),
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        recall_dimension: Some(64),
        embedding_auto_download: Some(false),
        ..RuntimeConfig::default()
    };
    {
        let rt = OctosRuntime::from_config(cfg()).expect("runtime built");
        seed(&rt);
        assert_eq!(parse(&rt.memory_stats().unwrap())["dimension"], 64);
    }
    // Dropping a data_dir runtime must NOT remove the caller's directory.
    assert!(
        data_dir.join("episodes.redb").exists(),
        "episode store persisted"
    );
    assert!(
        data_dir.join("recall.redb").exists(),
        "recall store persisted"
    );
    let rt = OctosRuntime::from_config(cfg()).expect("runtime reopened");
    let hits = parse(&rt.memory_search(r#"{"query":"dentist"}"#).unwrap());
    assert_eq!(hits["hits"][0]["id"], "doc:mail:1");
    assert_eq!(parse(&rt.memory_stats().unwrap())["records"], 2);
    drop(rt);
    assert!(data_dir.exists());
}

#[test]
fn should_reject_zero_recall_dimension() {
    let err = OctosRuntime::from_config(RuntimeConfig {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key: Some("ffi-memory-not-a-real-key".into()),
        recall_dimension: Some(0),
        embedding_auto_download: Some(false),
        ..RuntimeConfig::default()
    })
    .err()
    .expect("zero dimension rejected");
    assert!(err.to_string().contains("recall_dimension"), "got: {err}");
}

#[test]
fn should_parse_time_bounds() {
    assert_eq!(
        parse_time_bound("2026-09-16", false).unwrap().to_rfc3339(),
        "2026-09-16T00:00:00+00:00"
    );
    assert_eq!(
        parse_time_bound("2026-09-16", true).unwrap().to_rfc3339(),
        "2026-09-16T23:59:59.999+00:00"
    );
    assert_eq!(
        parse_time_bound("2026-09-16T10:00:00+02:00", false)
            .unwrap()
            .to_rfc3339(),
        "2026-09-16T08:00:00+00:00"
    );
    assert!(parse_time_bound("last week", false).is_err());
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

#[test]
fn should_handle_null_arguments_in_c_memory_entry_points() {
    let req = CString::new(r#"{"records":[]}"#).unwrap();
    for (name, out) in [
        ("upsert", octos_memory_upsert(ptr::null_mut(), req.as_ptr())),
        ("search", octos_memory_search(ptr::null_mut(), req.as_ptr())),
        ("load", octos_memory_load(ptr::null_mut(), req.as_ptr())),
        ("stats", octos_memory_stats(ptr::null_mut())),
    ] {
        assert!(out.is_null(), "{name}: NULL runtime must fail");
        assert!(
            last_error().contains("null"),
            "{name}: got error {:?}",
            last_error()
        );
    }

    let mut rt = scratch_runtime();
    assert!(octos_memory_upsert(&mut rt, ptr::null()).is_null());
    assert!(last_error().contains("request_json: null pointer"));
    assert!(octos_memory_search(&mut rt, ptr::null()).is_null());
    assert!(last_error().contains("request_json: null pointer"));
    assert!(octos_memory_load(&mut rt, ptr::null()).is_null());
    assert!(last_error().contains("id: null pointer"));
    // Invalid UTF-8 is rejected without reading past it.
    let bad = CString::new(vec![0xffu8, 0xfe]).unwrap();
    assert!(octos_memory_search(&mut rt, bad.as_ptr()).is_null());
    assert!(last_error().contains("UTF-8"));
}
