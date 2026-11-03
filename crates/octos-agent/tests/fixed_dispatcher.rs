use octos_agent::ToolRegistry;
use serde_json::json;

#[tokio::test]
async fn discovery_and_execution_preserve_the_advertised_schema() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("input.txt"), "needle\n").unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());

    let before = serde_json::to_value(registry.specs()).unwrap();
    let names: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "discover_tools",
            "edit_file",
            "exec_command",
            "read_file",
            "run_tool",
            "write_file"
        ]
    );
    let spec = registry
        .execute("discover_tools", &json!({"name":"grep"}))
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&spec.output).unwrap();
    assert_eq!(value["name"], "grep");
    assert!(value["input_schema"]["properties"]["pattern"].is_object());
    let result = registry
        .execute(
            "run_tool",
            &json!({"name":"grep", "arguments":{"pattern":"needle"}}),
        )
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    assert!(result.output.contains("needle"));
    assert_eq!(serde_json::to_value(registry.specs()).unwrap(), before);
    assert_eq!(
        serde_json::to_value(registry.snapshot_excluding(&[]).specs()).unwrap(),
        before
    );
}

#[tokio::test]
async fn gateway_does_not_bypass_host_or_hidden_tool_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());

    registry.set_host_tool_allowlist(Some(&[
        "discover_tools".into(),
        "run_tool".into(),
        "read_file".into(),
    ]));
    assert!(
        registry
            .execute(
                "run_tool",
                &json!({"name":"write_file", "arguments":{"path":"forbidden", "content":"bad"}})
            )
            .await
            .is_err()
    );
    assert!(!dir.path().join("forbidden").exists());
    assert!(
        registry
            .execute("discover_tools", &json!({"name":"write_file"}))
            .await
            .is_err()
    );
    registry.mark_internal_hidden("read_file");
    assert!(
        registry
            .execute(
                "run_tool",
                &json!({"name":"read_file", "arguments":{"path":"a"}})
            )
            .await
            .is_err()
    );
    assert!(
        registry
            .execute("run_tool", &json!({"name":"run_tool", "arguments":{}}))
            .await
            .is_err()
    );
}

#[test]
fn denying_the_gateway_keeps_allowed_tools_directly_accessible() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());

    registry.set_host_tool_allowlist(Some(&["grep".into(), "read_file".into()]));
    assert!(registry.is_tool_visible("grep"));
    assert!(registry.specs().iter().any(|s| s.name == "grep"));
}

use async_trait::async_trait;
use octos_agent::tools::{Tool, ToolContext, ToolPermissions, ToolResult};

struct OptionalTool {
    large: bool,
}
#[async_trait]
impl Tool for OptionalTool {
    fn name(&self) -> &str {
        "optional_plugin"
    }
    fn description(&self) -> &str {
        "optional plugin search"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"huge":{"enum":["x".repeat(if self.large {30000} else {10})]}}})
    }
    async fn execute(&self, _: &serde_json::Value) -> eyre::Result<ToolResult> {
        Ok(ToolResult {
            output: "plugin ran".into(),
            success: true,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn late_plugin_registration_is_discoverable_without_changing_specs() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());

    let before = serde_json::to_value(registry.specs()).unwrap();
    registry.register(OptionalTool { large: false });
    registry.mark_as_plugin("optional_plugin");
    assert_eq!(serde_json::to_value(registry.specs()).unwrap(), before);
    let discovery = registry
        .execute("discover_tools", &json!({"query":"optional_plugin"}))
        .await
        .unwrap();
    assert!(discovery.output.contains("optional_plugin"));
    let result = registry
        .execute(
            "run_tool",
            &json!({"name":"optional_plugin","arguments":{}}),
        )
        .await
        .unwrap();
    assert_eq!(result.output, "plugin ran");
    assert_eq!(serde_json::to_value(registry.specs()).unwrap(), before);
}

#[tokio::test]
async fn oversized_schema_has_bounded_valid_json_and_can_be_inspected_in_fragments() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());

    registry.register(OptionalTool { large: true });
    let result = registry
        .execute("discover_tools", &json!({"name":"optional_plugin"}))
        .await
        .unwrap();
    assert!(result.output.len() < 24000);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.output).unwrap()["schema_omitted"],
        true
    );
    let result = registry
        .execute(
            "discover_tools",
            &json!({"name":"optional_plugin","schema_pointer":"/properties/path"}),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&result.output).unwrap()["schema"]["type"],
        "string"
    );
    for args in [
        json!({"limit":0}),
        json!({"limit":11}),
        json!({"name":"grep","query":"grep"}),
        json!({"schema_pointer":"/properties"}),
        json!({"typo":1}),
    ] {
        assert!(registry.execute("discover_tools", &args).await.is_err());
    }
    for args in [
        json!({"name":"grep","arguments":[]}),
        json!({"name":"grep"}),
        json!({"name":"grep","arguments":{},"typo":1}),
    ] {
        assert!(registry.execute("run_tool", &args).await.is_err());
    }
}

#[tokio::test]
async fn runtime_profile_denials_apply_to_target_and_gateway_and_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());

    for denied in ["write_file", "run_tool"] {
        let profile = octos_agent::profile::ProfileDefinition {
            tools: octos_agent::profile::ProfileTools::DenyList {
                tools: vec![denied.into()],
            },
            ..Default::default()
        };
        let ctx = ToolContext {
            permissions: ToolPermissions::from_profile(&profile),
            ..ToolContext::zero()
        };
        assert!(
            registry
                .execute_with_context(
                    &ctx,
                    "run_tool",
                    &json!({"name":"write_file","arguments":{"path":"x","content":"bad"}})
                )
                .await
                .is_err()
        );
        if denied == "write_file" {
            assert!(
                registry
                    .execute_with_context(&ctx, "discover_tools", &json!({"name":"write_file"}))
                    .await
                    .is_err()
            );
        }
    }
    assert!(!dir.path().join("x").exists());
}

#[test]
fn profile_preserves_exact_allowlist() {
    use octos_agent::profile::{ProfileDefinition, ProfileTools};
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());
    ProfileDefinition {
        tools: ProfileTools::AllowList {
            tools: vec!["grep".into()],
        },
        ..Default::default()
    }
    .apply_to_registry(&mut registry);
    assert_eq!(
        registry
            .specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["grep"]
    );
}

struct PendingTool(std::sync::Arc<std::sync::atomic::AtomicBool>);
#[async_trait]
impl Tool for PendingTool {
    fn name(&self) -> &str {
        "pending_tool"
    }
    fn description(&self) -> &str {
        "Wait until cancelled"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: &serde_json::Value) -> eyre::Result<ToolResult> {
        struct Dropped(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _guard = Dropped(self.0.clone());
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancelling_dispatch_drops_the_native_future_without_detaching_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    registry.register(PendingTool(dropped.clone()));

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        registry.execute("run_tool", &json!({"name":"pending_tool","arguments":{}})),
    )
    .await;
    assert!(result.is_err());
    assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn provider_policy_blocks_target_even_when_the_gateway_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());

    registry.set_provider_policy(octos_agent::tools::policy::ToolPolicy {
        deny: vec!["write_file".into()],
        ..Default::default()
    });
    assert!(registry.is_tool_visible("run_tool"));
    assert!(
        registry
            .execute(
                "run_tool",
                &json!({"name":"write_file","arguments":{"path":"x","content":"bad"}})
            )
            .await
            .is_err()
    );
    assert!(
        registry
            .execute("discover_tools", &json!({"name":"write_file"}))
            .await
            .is_err()
    );
}

#[test]
fn dispatcher_schema_footprint_stays_small_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    assert_eq!(registry.specs().len(), 6);
    let bytes = serde_json::to_vec(&registry.specs()).unwrap().len();
    assert!(bytes < 6000, "dispatcher schema grew to {bytes} bytes");
    println!(
        "default dispatcher: {} tools / {bytes} schema bytes",
        registry.specs().len()
    );
}

#[tokio::test]
async fn ordinary_coding_session_uses_dispatcher_without_an_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());
    octos_agent::profile::ProfileDefinition::builtin("coding")
        .unwrap()
        .apply_to_registry(&mut registry);
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(
        names,
        [
            "discover_tools",
            "edit_file",
            "exec_command",
            "read_file",
            "run_tool",
            "write_file"
        ]
    );
    assert!(
        registry
            .execute("discover_tools", &json!({"name":"web_search"}))
            .await
            .unwrap()
            .success
    );
    assert!(registry.is_tool_model_reachable("apply_patch"));
}

#[test]
fn obsolete_tool_surface_profiles_are_removed() {
    for name in ["coding-full", "coding-compact", "coding-dispatch"] {
        assert!(
            octos_agent::profile::ProfileDefinition::builtin(name).is_none(),
            "obsolete profile {name} still available"
        );
    }
}

#[test]
fn swarm_profile_uses_standard_dispatcher_and_preserves_command_polling() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());
    octos_agent::profile::ProfileDefinition::builtin("swarm")
        .unwrap()
        .apply_to_registry(&mut registry);
    assert_eq!(
        registry
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        [
            "discover_tools",
            "edit_file",
            "exec_command",
            "read_file",
            "run_tool",
            "write_file"
        ]
    );
    assert!(registry.is_tool_model_reachable("write_stdin"));
}
