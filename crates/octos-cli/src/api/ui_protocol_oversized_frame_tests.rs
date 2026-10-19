//! Over-cap outbound-frame preview (#2446): the executor-offload contract of
//! [`preview_oversized_frame`] — where the truncation runs, and that the
//! offload never changes what it produces.

use super::*;

// #2446: on a multi-threaded runtime the over-cap preview must go through
// `block_in_place` (so it never starves the connection task's worker) and
// stay byte-identical to the inline result — the offload changes WHERE the
// truncation runs, never what it produces. Checked at both worker counts:
// 2 is the everyday shape, 1 is the measured/reproduced configuration whose
// replacement-worker spawn path inside `block_in_place` is the risky one.
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

    for worker_threads in [2, 1] {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .build()
            .expect("multi-thread runtime");
        // Spawned (not handed to `block_on` directly, whose future runs on
        // the calling thread) so the preview executes on a runtime worker —
        // the connection task's shape, and the only context where
        // `block_in_place` actually parks a scheduler instead of running
        // inline. The thread identity must differ from this block_on calling
        // thread; tokio's default worker names have changed across 1.x, so
        // the name is not asserted.
        let calling_thread = std::thread::current().id();
        let frame = frame.clone();
        let (offloaded, offloaded_thread) = runtime.block_on(async {
            runtime
                .spawn(async move { (preview_oversized_frame(frame), std::thread::current().id()) })
                .await
                .expect("offloaded preview task")
        });
        assert_ne!(
            offloaded_thread, calling_thread,
            "the preview must run on a runtime worker, not the block_on thread"
        );

        assert_eq!(inline, offloaded, "offload must not change the output");
        assert!(offloaded.len() < MAX_TEXT_FRAME_BYTES, "still under cap");
    }
}

// #2446: the starvation contract itself. On a single-worker multi-threaded
// runtime the inline preview freezes every co-scheduled task for the whole
// CPU stretch (measured 0 ticks for a 1 ms ticker across a ~1 s preview);
// with the offload the parked worker is replaced and the ticker keeps
// running. Without the guard the count is exactly 0, so the bound below is
// stable in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn oversized_frame_preview_does_not_starve_co_scheduled_tasks() {
    let messages: Vec<Value> = (0..2000)
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
        frame.len() > 4 * MAX_TEXT_FRAME_BYTES,
        "fixture must be far over cap for a measurable stretch"
    );

    let ticks = Arc::new(AtomicU64::new(0));
    let ticker = {
        let ticks = Arc::clone(&ticks);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                ticks.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    // The preview task runs ON the runtime worker — the connection task's
    // shape. The tick snapshot is taken inside the task, right after the
    // preview returns, with no yield in between: whatever the ticker managed
    // to run during the CPU stretch is exactly what this measures.
    let probe_ticks = Arc::clone(&ticks);
    let (during, under_cap) = tokio::spawn(async move {
        let before = probe_ticks.load(Ordering::Relaxed);
        let out = preview_oversized_frame(frame);
        let during = probe_ticks.load(Ordering::Relaxed) - before;
        (during, out.len() < MAX_TEXT_FRAME_BYTES)
    })
    .await
    .expect("preview task");
    ticker.abort();

    assert!(under_cap, "still under cap");
    assert!(
        during >= 5,
        "the offload must let co-scheduled tasks run during the \
         truncation stretch (got {during} ticks); an inline preview \
         measures 0"
    );
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
