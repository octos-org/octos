//! Over-cap outbound-frame preview (#2446): the executor-offload contract of
//! [`preview_oversized_frame`] — where the truncation runs, and that the
//! offload never changes what it produces.

use super::*;

// #2446: on a multi-threaded runtime the over-cap preview must go through
// `block_in_place` (so it never starves the connection task's worker) and
// stay byte-identical to the inline result — the offload changes WHERE the
// truncation runs, never what it produces.
#[test]
fn oversized_frame_preview_offload_is_byte_identical_on_multi_thread_runtime() {
    let messages: Vec<Value> = (0..1000)
        .map(|i| {
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "text": format!("msg-{i}-{}", "m".repeat(5 * 1024)),
            })
        })
        .collect();
    let value = json!({
        "jsonrpc": "2.0",
        "id": "hydrate-1",
        "result": {
            "session_id": "local:perf",
            "messages": messages,
        }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");
    assert!(
        frame.len() > MAX_TEXT_FRAME_BYTES,
        "fixture must be over cap"
    );

    // Inline baseline: no runtime on this thread, so the guard falls through.
    let inline = std::thread::spawn({
        let frame = frame.clone();
        move || preview_oversized_frame(frame)
    })
    .join()
    .expect("inline preview thread");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("multi-thread runtime");
    // Spawned (not handed to `block_on` directly, whose future runs on the
    // calling thread) so the preview executes on a runtime worker — the
    // connection task's shape, and the only context where `block_in_place`
    // actually parks a scheduler instead of running inline.
    let (offloaded, offloaded_thread) = runtime.block_on(async {
        runtime
            .spawn(async move {
                let thread = std::thread::current()
                    .name()
                    .map(str::to_owned)
                    .unwrap_or_default();
                (preview_oversized_frame(frame), thread)
            })
            .await
            .expect("offloaded preview task")
    });
    assert!(
        offloaded_thread.contains("tokio-rt-worker"),
        "the preview must run on a runtime worker, got {offloaded_thread:?}"
    );

    assert_eq!(inline, offloaded, "offload must not change the output");
    assert!(offloaded.len() < MAX_TEXT_FRAME_BYTES, "still under cap");
}

// #2446: the guard must bail (not `block_in_place`) on a current-threaded
// runtime — tokio panics on a blocking hop there, and the panic would take
// the connection task with it.
#[test]
fn oversized_frame_preview_survives_current_thread_runtime() {
    let value = json!({
        "jsonrpc": "2.0",
        "id": "hydrate-1",
        "result": { "text": "m".repeat(2 * 1024 * 1024) }
    });
    let frame = app_ui_codec::to_compact_json(&value).expect("serialize");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let out = runtime.block_on(async move { preview_oversized_frame(frame) });

    let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
    assert!(
        parsed["result"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("bytes truncated"))
    );
    assert!(out.len() < MAX_TEXT_FRAME_BYTES, "still under cap");
}
