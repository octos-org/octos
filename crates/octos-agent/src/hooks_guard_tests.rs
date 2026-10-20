//! Guard-class circuit-breaker behavior from #2329. Sibling file (not
//! inline in `hooks.rs`) so the module stays under the 3000-line file
//! ratchet; wired via `#[path] mod guard_tests;` at the bottom of hooks.rs
//! (same pattern as validators.rs / validators_tests.rs).

use super::*;

fn before_tool_hook(command: Vec<&str>, timeout_ms: u64) -> HookConfig {
    HookConfig {
        event: HookEvent::BeforeToolCall,
        command: command.into_iter().map(String::from).collect(),
        timeout_ms,
        tool_filter: vec![],
        path_filter: vec![],
        requires_bin: None,
    }
}

#[tokio::test]
#[cfg(unix)]
async fn should_fail_closed_with_guard_unavailable_when_before_tool_call_hook_trips_breaker() {
    // #2329: the availability breaker must not silently strip a deny
    // gate. Three infra failures trip the breaker; the next call returns
    // GuardUnavailable (callers block the tool call fail-closed) instead
    // of Allow.
    let executor =
        HookExecutor::with_threshold(vec![before_tool_hook(vec!["sh", "-c", "exit 3"], 1000)], 3);
    let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
    for _ in 0..3 {
        let r = executor.run(HookEvent::BeforeToolCall, &payload).await;
        assert!(
            matches!(r, HookResult::Error(_)),
            "expected Error while counting failures, got {r:?}"
        );
    }
    let result = executor.run(HookEvent::BeforeToolCall, &payload).await;
    match result {
        HookResult::GuardUnavailable(msg) => {
            assert!(
                msg.contains("disabled after 3 consecutive infrastructure failures"),
                "message should name the trip and recovery: {msg}"
            );
        }
        other => panic!("expected GuardUnavailable, got {other:?}"),
    }
}

#[tokio::test]
#[cfg(unix)]
async fn should_keep_after_tool_call_breaker_fail_open() {
    // #2329 scoping: after-events are advisory checkers, not deny gates.
    // Their breaker keeps today's availability behavior — a tripped hook
    // is skipped silently and the event still comes back Allow (an
    // after-hook with empty output + nonzero exit is the infra-failure
    // shape that counts toward the breaker, #2129 review finding 1).
    let executor = HookExecutor::with_threshold(
        vec![HookConfig {
            event: HookEvent::AfterToolCall,
            command: vec!["sh".into(), "-c".into(), "exit 3".into()],
            timeout_ms: 1000,
            tool_filter: vec![],
            path_filter: vec![],
            requires_bin: None,
        }],
        2,
    );
    let payload = HookPayload::after_tool("shell", "tc1", String::new(), true, 5, None, None, None);
    for _ in 0..2 {
        executor.run(HookEvent::AfterToolCall, &payload).await;
    }
    let result = executor.run(HookEvent::AfterToolCall, &payload).await;
    assert_eq!(result, HookResult::Allow);
}

#[tokio::test]
#[cfg(unix)]
async fn should_re_arm_tripped_guard_breaker_explicitly() {
    // #2329: recovery from a tripped guard breaker is explicit. re_arm
    // clears the failure counters, so the hook runs (and errors) again
    // instead of staying disabled.
    let executor =
        HookExecutor::with_threshold(vec![before_tool_hook(vec!["sh", "-c", "exit 3"], 1000)], 3);
    let payload = HookPayload::before_tool("shell", serde_json::json!({}), "tc1", None);
    for _ in 0..4 {
        executor.run(HookEvent::BeforeToolCall, &payload).await;
    }
    executor.re_arm(None);
    let r = executor.run(HookEvent::BeforeToolCall, &payload).await;
    assert!(
        matches!(r, HookResult::Error(_)),
        "after re_arm the hook runs again, got {r:?}"
    );
}
