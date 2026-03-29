//! Regression tests for activate_tools across session actor lifecycles.
//!
//! These tests reproduce the OnceLock bug where `ActivateToolsTool` kept a stale
//! `Weak<ToolRegistry>` reference after session actors were dropped and recreated.
//! The fix (OnceLock → RwLock) allows `set_registry` to update the back-reference.

use std::path::PathBuf;
use std::sync::Arc;

use octos_agent::tools::activate_tools::ActivateToolsTool;
use octos_agent::tools::{Tool, ToolRegistry};

// ---------------------------------------------------------------------------
// Proof that the OLD OnceLock-based design fails this exact scenario.
// This test embeds a minimal OnceLock reproduction to prove the bug exists
// without the RwLock fix.
// ---------------------------------------------------------------------------
mod oncelock_proof {
    use std::sync::{OnceLock, Weak};

    /// Minimal reproduction of the OLD broken design.
    struct BrokenActivateTool {
        registry: OnceLock<Weak<Vec<String>>>, // stand-in for ToolRegistry
    }

    impl BrokenActivateTool {
        fn new() -> Self {
            Self {
                registry: OnceLock::new(),
            }
        }

        fn set_registry(&self, weak: Weak<Vec<String>>) {
            // OnceLock::set silently fails after the first call
            let _ = self.registry.set(weak);
        }

        fn can_upgrade(&self) -> bool {
            self.registry.get().and_then(|w| w.upgrade()).is_some()
        }
    }

    #[test]
    fn oncelock_fails_after_drop_and_rewire() {
        let tool = BrokenActivateTool::new();

        // Session 1: wire, verify
        let reg1 = std::sync::Arc::new(vec!["web_search".to_string()]);
        tool.set_registry(std::sync::Arc::downgrade(&reg1));
        assert!(tool.can_upgrade(), "session 1 should work");
        drop(reg1); // session 1 dropped

        // Session 2: re-wire — OnceLock::set SILENTLY FAILS
        let reg2 = std::sync::Arc::new(vec!["shell".to_string()]);
        tool.set_registry(std::sync::Arc::downgrade(&reg2));

        // THIS IS THE BUG: Weak still points to dead reg1, upgrade returns None
        assert!(
            !tool.can_upgrade(),
            "OnceLock BUG: second set_registry was silently ignored, Weak is stale"
        );
    }
}

/// Simulate the session actor lifecycle: create a ToolRegistry, wire activate_tools,
/// drop the registry, create a new one, re-wire. With the old OnceLock bug, the
/// second wire would silently fail and activate_tools would return
/// "tool registry not available".
#[tokio::test]
async fn activate_tools_survives_registry_drop_and_rewire() {
    let tool = ActivateToolsTool::new();

    // === Session 1: create registry, wire, verify it works ===
    {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        let registry = Arc::new(registry);

        tool.set_registry(Arc::downgrade(&registry));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(result.success, "session 1: list should succeed");
        assert!(
            result.output.contains("web_search") || result.output.contains("web_fetch"),
            "session 1: should list web tools, got: {}",
            result.output
        );
    }
    // registry Arc dropped here — Weak is now dead

    // === Session 2: create a NEW registry, re-wire the SAME ActivateToolsTool ===
    // With the OnceLock bug, set_registry() would silently fail here because
    // OnceLock::set() only succeeds on the first call.
    {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        let registry = Arc::new(registry);

        tool.set_registry(Arc::downgrade(&registry));

        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "session 2: should succeed after re-wire, got: {}",
            result.output
        );
        assert!(
            result.output.contains("web_search") || result.output.contains("web_fetch"),
            "session 2: should list web tools after re-wire, got: {}",
            result.output
        );
    }
}

/// After the first registry is dropped but before re-wiring, calling execute
/// should return a clear error (not panic).
#[tokio::test]
async fn activate_tools_returns_error_when_registry_dropped() {
    let tool = ActivateToolsTool::new();

    // Wire to a registry, then drop it
    {
        let registry = Arc::new(ToolRegistry::with_builtins(PathBuf::from("/tmp")));
        tool.set_registry(Arc::downgrade(&registry));
    }
    // Registry dropped — Weak::upgrade() returns None

    let result = tool.execute(&serde_json::json!({})).await;
    assert!(
        result.is_err(),
        "should return error when registry is dropped"
    );
    let err_msg = format!("{}", result.err().unwrap());
    assert!(
        err_msg.contains("not available"),
        "error should mention registry not available, got: {}",
        err_msg
    );
}

/// Simulate the SnapshotToolRegistryFactory pattern: a single ActivateToolsTool
/// instance is shared (via Arc<dyn Tool>) across multiple sequential sessions.
/// Each session creates a new ToolRegistry, wires the shared tool, uses it,
/// then drops.
#[tokio::test]
async fn shared_activate_tools_across_multiple_sessions() {
    let shared_tool = Arc::new(ActivateToolsTool::new());

    for session_num in 1..=5 {
        let mut registry = ToolRegistry::with_builtins(PathBuf::from("/tmp"));
        registry.defer_group("group:web");
        let registry = Arc::new(registry);

        // Wire the shared tool to this session's registry
        shared_tool.set_registry(Arc::downgrade(&registry));

        // Activate tools by name (the normal flow)
        let result = shared_tool
            .execute(&serde_json::json!({"tools": ["web_search"]}))
            .await
            .unwrap();

        assert!(
            result.success,
            "session {}: activate should succeed, got: {}",
            session_num, result.output
        );
        assert!(
            result.output.contains("web_search"),
            "session {}: should activate web_search, got: {}",
            session_num,
            result.output
        );

        // Registry will be dropped at end of loop iteration
    }
}
