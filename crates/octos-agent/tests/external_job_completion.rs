//! External services publish receipts independently of the agent's tool calls.
//! Watching one must notify the owning session without another model poll.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octos_agent::task_supervisor::TaskStatus;
use octos_agent::{Agent, AgentConfig, ToolRegistry};
use octos_core::{AgentId, Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, ToolSpec};
use octos_memory::EpisodeStore;
use serde_json::json;
use tempfile::TempDir;

const REQUEST_HASH: &str = "f4cc1eb28f9c95a491d8183c4eaea7cc07d43e9a65f26c9a4b63b76fb7ea296f";

struct SubmitReceipt;

#[async_trait]
impl octos_agent::tools::Tool for SubmitReceipt {
    fn name(&self) -> &str {
        "submit_fixture"
    }
    fn description(&self) -> &str {
        "Returns an independently submitted external job receipt."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    async fn execute(&self, _: &serde_json::Value) -> eyre::Result<octos_agent::tools::ToolResult> {
        Ok(octos_agent::tools::ToolResult {
            success: true,
            output: "Job submitted.".into(),
            structured_metadata: Some(json!({"octos_external_job":{
                "schema_version":1, "receipt_path":"job.json", "job_id":"render-1",
                "request_sha256":REQUEST_HASH, "status":"running"
            }})),
            ..Default::default()
        })
    }
}

#[derive(Clone, Copy)]
enum Submission {
    Adapter,
    Shell,
    Exec,
}

struct SubmitTwice(std::sync::atomic::AtomicUsize, Submission);
#[async_trait]
impl LlmProvider for SubmitTwice {
    async fn chat(
        &self,
        _: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ChatResponse {
            content: (n >= 2).then(|| "Continue independent work.".into()),
            reasoning_content: None,
            tool_calls: if n < 2 {
                vec![ToolCall {
                    id: format!("submit-{n}"),
                    name: if matches!(self.1, Submission::Exec) {
                        "exec_command"
                    } else {
                        "run_tool"
                    }
                    .into(),
                    arguments: {
                        let command = format!(
                            "printf '%s\\n' '{}'",
                            json!({"octos_external_job":{
                            "schema_version":1,"receipt_path":"job.json", "job_id":"render-1",
                            "request_sha256":REQUEST_HASH,"status":"running"}})
                        );
                        match self.1 {
                            Submission::Exec => json!({"cmd":command}),
                            Submission::Shell => {
                                json!({"name":"shell", "arguments":{"command":command}})
                            }
                            Submission::Adapter => {
                                json!({"name":"submit_fixture", "arguments":{"attempt":n}})
                            }
                        }
                    },
                    metadata: None,
                }]
            } else {
                vec![]
            },
            stop_reason: if n < 2 {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: Default::default(),
            provider_index: None,
        })
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "auto-job-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[tokio::test]
async fn tool_receipt_automatically_registers_one_watch_without_model_cooperation() {
    run_automatic_receipt(Submission::Adapter, Gate::None).await;
}

#[cfg(unix)]
#[tokio::test]
async fn real_shell_receipt_automatically_registers_one_watch() {
    run_automatic_receipt(Submission::Shell, Gate::None).await;
}

#[tokio::test]
async fn automatic_watch_respects_provider_denial() {
    run_automatic_receipt(Submission::Adapter, Gate::Provider).await;
}

#[cfg(unix)]
#[tokio::test]
async fn default_exec_command_receipt_automatically_registers_one_watch() {
    run_automatic_receipt(Submission::Exec, Gate::None).await;
}

#[derive(Clone, Copy)]
enum Gate {
    None,
    Provider,
    Host,
    Profile,
    Approval,
    GatewayApproval,
    RoundLimit,
    ToolLimit,
    GatewayLimit,
    Hook,
}

#[tokio::test]
async fn automatic_watch_respects_host_allowlist() {
    run_automatic_receipt(Submission::Adapter, Gate::Host).await;
}
#[tokio::test]
async fn automatic_watch_respects_profile_permissions() {
    run_automatic_receipt(Submission::Adapter, Gate::Profile).await;
}
#[tokio::test]
async fn automatic_watch_requires_explicit_human_approval_when_configured() {
    run_automatic_receipt(Submission::Adapter, Gate::Approval).await;
}
#[cfg(unix)]
#[tokio::test]
async fn automatic_watch_requires_gateway_approval_even_for_direct_exec_receipts() {
    run_automatic_receipt(Submission::Exec, Gate::GatewayApproval).await;
}

#[tokio::test]
async fn automatic_watch_respects_tool_round_budget() {
    run_automatic_receipt(Submission::Adapter, Gate::RoundLimit).await;
}

#[tokio::test]
async fn automatic_watch_counts_native_and_gateway_session_limits() {
    run_automatic_receipt(Submission::Adapter, Gate::ToolLimit).await;
    run_automatic_receipt(Submission::Adapter, Gate::GatewayLimit).await;
}
#[cfg(unix)]
#[tokio::test]
async fn automatic_watch_respects_before_tool_hook_denial() {
    run_automatic_receipt(Submission::Adapter, Gate::Hook).await;
}

async fn run_automatic_receipt(submission: Submission, gate: Gate) {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("job.json"),
        json!({
            "id":"render-1","request_sha256":REQUEST_HASH,"status":"running"
        })
        .to_string(),
    )
    .unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());
    registry.register(SubmitReceipt);
    if matches!(gate, Gate::Provider) {
        registry.set_provider_policy(octos_agent::tools::policy::ToolPolicy {
            deny: vec!["watch_external_job".into()],
            ..Default::default()
        });
    }
    let advertised = serde_json::to_value(registry.specs()).unwrap();
    if matches!(gate, Gate::Host) {
        registry.set_host_tool_allowlist(Some(&["run_tool".into(), "submit_fixture".into()]));
    }
    registry.set_session_key("auto-owner".into());
    let supervisor = registry.supervisor();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    registry.set_background_result_sender(Arc::new(move |payload| {
        let tx = tx.clone();
        Box::pin(async move { tx.send(payload).is_ok() })
    }));
    let provider = Arc::new(SubmitTwice(0.into(), submission));
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let mut config = AgentConfig {
        save_episodes: false,
        suppress_auto_send_files: true,
        ..Default::default()
    };
    if matches!(gate, Gate::Approval | Gate::GatewayApproval) {
        use octos_agent::approval::*;
        config.human_approval_rules = Some(HumanApprovalRules::new(vec![ApprovalRule {
            tools: vec![
                if matches!(gate, Gate::Approval) {
                    "watch_external_job"
                } else {
                    "run_tool"
                }
                .into(),
            ],
            risk_level: ApprovalRiskLevel::Normal,
            authorized_approvers: vec!["reviewer".into()],
            expires_in_secs: 300,
            on_timeout: ApprovalTimeoutBehavior::Notify,
        }]));
    }
    let mut agent = Agent::new(AgentId::new("auto-job"), provider.clone(), registry, memory)
        .with_config(config);
    if matches!(gate, Gate::ToolLimit | Gate::GatewayLimit) {
        let (name, limit) = if matches!(gate, Gate::ToolLimit) {
            ("watch_external_job", 0)
        } else {
            ("run_tool", 1)
        };
        agent = agent.with_session_limits(octos_agent::session::SessionLimits {
            per_tool_limits: [(name.into(), limit)].into_iter().collect(),
            ..Default::default()
        });
    }
    if matches!(gate, Gate::RoundLimit) {
        agent = agent.with_session_limits(octos_agent::session::SessionLimits {
            max_tool_rounds: Some(1),
            ..Default::default()
        });
    }
    if matches!(gate, Gate::Profile) {
        let profile = octos_agent::profile::ProfileDefinition::from_json_str(
            r#"{
            "name":"deny-watcher", "version":1, "tools":{"mode":"deny_list", "tools":["watch_external_job"]}
        }"#,
        )
        .unwrap();
        agent = agent.with_profile(Arc::new(profile));
    }
    if matches!(gate, Gate::Hook) {
        use octos_agent::hooks::{HookConfig, HookEvent, HookExecutor};
        agent = agent.with_hooks(Arc::new(HookExecutor::new(vec![HookConfig {
            event: HookEvent::BeforeToolCall,
            command: vec![
                "sh".into(),
                "-c".into(),
                "cat >/dev/null; printf 'observation denied'; exit 1".into(),
            ],
            timeout_ms: 5000,
            tool_filter: vec!["watch_external_job".into()],
            path_filter: vec![],
            requires_bin: None,
        }])));
    }
    let result = agent
        .process_message("Submit work.", &[], vec![])
        .await
        .unwrap();
    if !matches!(gate, Gate::Host) {
        assert_eq!(
            serde_json::to_value(agent.tool_registry().specs()).unwrap(),
            advertised
        );
    }
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    if !matches!(gate, Gate::None) {
        assert!(supervisor.get_tasks_for_session("auto-owner").is_empty());
        assert!(rx.try_recv().is_err());
        if matches!(gate, Gate::Approval | Gate::GatewayApproval) {
            assert!(
                result
                    .messages
                    .iter()
                    .any(|m| m.content.contains("requires approval"))
            );
        }
        return;
    }
    assert_eq!(supervisor.get_tasks_for_session("auto-owner").len(), 1);
    if matches!(submission, Submission::Shell | Submission::Exec) {
        assert!(
            result
                .messages
                .iter()
                .all(|m| !m.content.contains(REQUEST_HASH))
        );
    }
    assert!(
        result
            .messages
            .iter()
            .filter(|m| m.role == octos_core::MessageRole::Tool)
            .any(|m| m.content.contains("automatically"))
    );
    std::fs::write(
        dir.path().join("next.json"),
        json!({
            "id":"render-1","request_sha256":REQUEST_HASH,"status":"completed"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::rename(dir.path().join("next.json"), dir.path().join("job.json")).unwrap();
    let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload.terminal_status, Some(TaskStatus::Completed));
    assert!(rx.try_recv().is_err());
}

struct WatchThenFinish(std::sync::atomic::AtomicBool);

#[async_trait]
impl LlmProvider for WatchThenFinish {
    async fn chat(
        &self,
        _: &[Message],
        _: &[ToolSpec],
        _: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let first = !self.0.swap(true, std::sync::atomic::Ordering::SeqCst);
        Ok(ChatResponse {
            content: (!first).then(|| "Continuing independent work while the job runs.".into()),
            reasoning_content: None,
            tool_calls: if first {
                vec![ToolCall {
                    id: "watch-call".into(),
                    name: "run_tool".into(),
                    arguments: json!({"name":"watch_external_job", "arguments":{
                        "receipt_path":"job.json", "job_id":"render-1", "timeout_secs":30}}),
                    metadata: None,
                }]
            } else {
                vec![]
            },
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: Default::default(),
            provider_index: None,
        })
    }
    fn context_window(&self) -> u32 {
        128_000
    }
    fn model_id(&self) -> &str {
        "external-job-test"
    }
    fn provider_name(&self) -> &str {
        "mock"
    }
}

#[test]
fn watcher_is_builtin_background_tool_and_rebinds() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    let registry = ToolRegistry::with_builtins(first.path());
    assert!(registry.get("watch_external_job").is_some());
    assert!(registry.is_spawn_only("watch_external_job"));
    let rebound = registry.rebind_cwd(second.path(), Box::new(octos_agent::sandbox::NoSandbox));
    assert!(rebound.is_spawn_only("watch_external_job"));
}

async fn run_watch(terminal_status: &str, cancel: bool) {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("job.json"),
        r#"{"id":"render-1","status":"running"}"#,
    )
    .unwrap();
    let mut registry = ToolRegistry::with_builtins(dir.path());
    registry.set_session_key("owner-session".into());
    let supervisor = registry.supervisor();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    registry.set_background_result_sender(Arc::new(move |payload| {
        let tx = tx.clone();
        Box::pin(async move { tx.send(payload).is_ok() })
    }));
    let (terminal_tx, mut terminal_rx) = tokio::sync::mpsc::unbounded_channel();
    supervisor.set_on_terminal(move |event| {
        let _ = terminal_tx.send(event.clone());
    });
    let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
    let agent = Agent::new(
        AgentId::new("external-watch"),
        Arc::new(WatchThenFinish(false.into())),
        registry,
        memory,
    )
    .with_config(AgentConfig {
        save_episodes: false,
        suppress_auto_send_files: true,
        ..Default::default()
    });

    tokio::time::timeout(
        Duration::from_secs(5),
        agent.process_message("Watch the submitted job", &[], vec![]),
    )
    .await
    .expect("watch must release the foreground before the job completes")
    .unwrap();
    let tasks = supervisor.get_tasks_for_session("owner-session");
    assert_eq!(tasks.len(), 1, "one native task, not a polling agent loop");
    assert!(supervisor.get_tasks_for_session("other-session").is_empty());
    assert!(rx.try_recv().is_err(), "running is not completed");
    if cancel {
        supervisor.cancel(&tasks[0].id).unwrap();
    }

    // The service atomically replaces its receipt; no further LLM/tool call.
    std::fs::write(
        dir.path().join("job.next"),
        json!({
            "id":"render-1", "status":terminal_status, "result":{"out":"exports/deck.pptx"},
        "error": if terminal_status == "failed" { Some("connection refused by renderer") } else { None },
        })
        .to_string(),
    )
    .unwrap();
    std::fs::rename(dir.path().join("job.next"), dir.path().join("job.json")).unwrap();

    if cancel {
        assert_eq!(
            supervisor.get_tasks_for_session("owner-session")[0].status,
            TaskStatus::Cancelled
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(750), rx.recv())
                .await
                .is_err(),
            "cancelled observer must not publish a late success"
        );
    } else {
        let event = tokio::time::timeout(Duration::from_secs(3), terminal_rx.recv())
            .await
            .expect("the supervisor must emit a terminal event promptly")
            .unwrap();
        assert_eq!(
            event.task.parent_session_key.as_deref(),
            Some("owner-session")
        );
        let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let expected = if terminal_status == "completed" {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        };
        assert_eq!(payload.terminal_status, Some(expected));
        assert!(payload.content.contains("render-1"));
        assert!(payload.content.contains(terminal_status));
        assert!(
            rx.try_recv().is_err(),
            "one terminal notification per watcher"
        );
    }
}

#[tokio::test]
async fn external_completion_notifies_parent_without_model_polling() {
    run_watch("completed", false).await;
}

#[tokio::test]
async fn external_failure_notifies_parent_without_false_success() {
    run_watch("failed", false).await;
}

#[tokio::test]
async fn cancelling_observer_does_not_publish_late_completion() {
    run_watch("completed", true).await;
}

#[tokio::test]
async fn terminal_receipts_and_identity_are_checked() {
    let dir = TempDir::new().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    for status in [
        "completed",
        "failed",
        "cancelled",
        "canceled",
        "interrupted",
        "surprise",
    ] {
        std::fs::write(
            dir.path().join("job.json"),
            json!({"id":"job", "status":status, "request_sha256":"abc"}).to_string(),
        )
        .unwrap();
        let result = registry.execute("watch_external_job", &json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":1, "request_sha256":"abc"})).await.unwrap();
        assert_eq!(
            result.success,
            status == "completed",
            "{status}: {}",
            result.output
        );
        assert!(result.output.contains(status));
    }
    std::fs::write(
        dir.path().join("job.json"),
        r#"{"id":"another-job","status":"completed","request_sha256":"other"}"#,
    )
    .unwrap();
    for args in [
        json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":1}),
        json!({"receipt_path":"job.json", "job_id":"another-job", "request_sha256":"abc", "timeout_secs":1}),
    ] {
        assert!(
            !registry
                .execute("watch_external_job", &args)
                .await
                .unwrap()
                .success
        );
    }
}

#[tokio::test]
async fn timeout_is_observation_timeout_not_job_failure_or_completion() {
    let dir = TempDir::new().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    std::fs::write(
        dir.path().join("job.json"),
        r#"{"id":"job","status":"running"}"#,
    )
    .unwrap();
    let result = registry
        .execute(
            "watch_external_job",
            &json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":1}),
        )
        .await
        .unwrap();
    assert!(!result.success);
    let report: serde_json::Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(report["watch_status"], "timed_out");
    assert_eq!(report["job_status"], "running");
    assert_eq!(report["job_terminal"], false);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(dir.path().join("job.json")).unwrap()
        )
        .unwrap()["status"],
        "running"
    );
}

#[tokio::test]
async fn pending_missing_and_partial_receipt_can_be_replaced_atomically() {
    let dir = TempDir::new().unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    let receipt = dir.path().join("job.json");
    let producer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&receipt, "{").unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        std::fs::write(
            receipt.with_extension("next"),
            r#"{"id":"job","status":"completed"}"#,
        )
        .unwrap();
        std::fs::rename(receipt.with_extension("next"), &receipt).unwrap();
    });
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        registry.execute(
            "watch_external_job",
            &json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":30}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.success, "{}", result.output);
    producer.await.unwrap();
}

#[tokio::test]
async fn watcher_respects_rebound_workspace_and_tool_policy() {
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    std::fs::write(
        first.path().join("job.json"),
        r#"{"id":"wrong","status":"completed"}"#,
    )
    .unwrap();
    std::fs::write(
        second.path().join("job.json"),
        r#"{"id":"job","status":"completed"}"#,
    )
    .unwrap();
    let registry = ToolRegistry::with_builtins(first.path());
    let mut rebound = registry.rebind_cwd(second.path(), Box::new(octos_agent::sandbox::NoSandbox));
    let args = json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":1});
    assert!(
        rebound
            .execute("watch_external_job", &args)
            .await
            .unwrap()
            .success
    );
    let outside =
        json!({"receipt_path":first.path().join("job.json"), "job_id":"wrong", "timeout_secs":1});
    assert!(
        !rebound
            .execute("watch_external_job", &outside)
            .await
            .unwrap()
            .success
    );
    rebound.apply_policy(&octos_agent::tools::policy::ToolPolicy {
        allow: vec![],
        deny: vec!["group:fs".into()],
        ..Default::default()
    });
    assert!(rebound.get("watch_external_job").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn watcher_rejects_symlink_escape_and_non_regular_receipts() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::write(
        outside.path().join("job.json"),
        r#"{"id":"job","status":"completed"}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("job.json"),
        dir.path().join("leaf.json"),
    )
    .unwrap();
    let registry = ToolRegistry::with_builtins(dir.path());
    for path in ["escape/job.json", "leaf.json", "."] {
        let result = registry
            .execute(
                "watch_external_job",
                &json!({"receipt_path":path, "job_id":"job", "timeout_secs":1}),
            )
            .await
            .unwrap();
        assert!(!result.success, "{path}: {}", result.output);
    }
}

#[test]
fn profile_filter_cannot_retain_or_resurrect_an_excluded_observer() {
    use octos_agent::profile::ProfileTools;
    let first = TempDir::new().unwrap();
    let second = TempDir::new().unwrap();
    for filter in [
        ProfileTools::AllowList {
            tools: vec!["grep".into()],
        },
        ProfileTools::DenyList {
            tools: vec!["watch_external_job".into()],
        },
        ProfileTools::DenyList {
            tools: vec!["group:fs".into()],
        },
    ] {
        let mut registry = ToolRegistry::with_builtins(first.path());
        registry.filter_by_profile(&filter);
        assert!(registry.get("watch_external_job").is_none());
        assert!(!registry.is_tool_model_reachable("watch_external_job"));
        let rebound = registry.rebind_cwd(second.path(), Box::new(octos_agent::sandbox::NoSandbox));
        assert!(rebound.get("watch_external_job").is_none());
        assert!(!rebound.is_spawn_only("watch_external_job"));
    }
}
