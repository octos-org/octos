//! M7.2a — integration tests for the MCP server dispatch.
//!
//! These tests exercise the real `Agent` loop via the exposed
//! [`McpSessionDispatch`](octos_agent::mcp_server::McpSessionDispatch)
//! implementation. A stub LLM provider drives the loop so the tests
//! never need a real provider — the full path is: MCP request →
//! session dispatch → Agent::run_task → workspace contract enforcement
//! → MCP response.
//!
//! Acceptance invariants (issue #516):
//!
//! 1. `run_session` returns a populated artifact bundle when the Agent
//!    produces a real output file covered by the workspace contract.
//! 2. Every call mutates the supplied `SessionLifecycleObserver` in the
//!    order `Running → Verifying → Ready` (or `Failed`) so outer
//!    orchestrators see the real transitions.
//! 3. Workspace-contract enforcement runs identically to local dispatch:
//!    a missing artifact path must surface as `Failed` with a typed
//!    recovery hint, not a placeholder-zero success.
//! 4. Internal iteration messages never leak into the MCP response
//!    payload — only the final outcome fields are visible.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use octos_agent::mcp_server::{McpSessionDispatch, SessionLifecycleObserver};
use octos_agent::task_supervisor::TaskLifecycleState;
use octos_agent::validators::ValidatorStatus;
use octos_agent::{SandboxConfig, SandboxMode};
use octos_cli::commands::mcp_serve::{AgentLlmFactory, RealSessionDispatch, SessionDispatchConfig};
use octos_core::{Message, ToolCall};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
use serde_json::json;
use tempfile::TempDir;

/// Recording observer used to verify lifecycle transitions propagate
/// through the dispatch.
struct RecordingObserver {
    states: Mutex<Vec<TaskLifecycleState>>,
}

impl RecordingObserver {
    fn new() -> Self {
        Self {
            states: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> Vec<TaskLifecycleState> {
        self.states.lock().unwrap().clone()
    }
}

impl SessionLifecycleObserver for RecordingObserver {
    fn mark_state(&self, state: TaskLifecycleState) {
        self.states.lock().unwrap().push(state);
    }
}

/// Scripted LLM provider — returns responses in FIFO order until
/// exhausted, then panics. Every response carries an EndTurn or
/// ToolUse stop reason so the agent loop terminates deterministically.
struct ScriptedLlmProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

impl ScriptedLlmProvider {
    fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses),
        })
    }
}

#[async_trait]
impl LlmProvider for ScriptedLlmProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _config: &ChatConfig,
    ) -> eyre::Result<ChatResponse> {
        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            eyre::bail!("ScriptedLlmProvider: scripted responses exhausted");
        }
        Ok(responses.remove(0))
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn model_id(&self) -> &str {
        "scripted-test"
    }

    fn provider_name(&self) -> &str {
        "scripted"
    }
}

fn end_turn(text: &str) -> ChatResponse {
    ChatResponse {
        content: Some(text.to_string()),
        reasoning_content: None,
        tool_calls: vec![],
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 42,
            output_tokens: 17,
            ..Default::default()
        },
        provider_index: None,
    }
}

fn tool_use(call: ToolCall) -> ChatResponse {
    ChatResponse {
        content: None,
        reasoning_content: None,
        tool_calls: vec![call],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage {
            input_tokens: 30,
            output_tokens: 20,
            ..Default::default()
        },
        provider_index: None,
    }
}

/// Harness that pairs a real dispatch with the [`TempDir`] it runs against.
/// Holding the [`TempDir`] ensures the workspace outlives the [`Agent`] run;
/// relying on the caller to keep it alive avoids `std::mem::forget` leaks.
struct DispatchHarness {
    dispatch: RealSessionDispatch,
    _workspace: TempDir,
}

impl DispatchHarness {
    fn build(provider: Arc<dyn LlmProvider>, workspace: TempDir) -> Self {
        // Opt out of the sandbox for the scripted success/lifecycle paths so the
        // mcp-serve fail-closed no-backend check is host-independent (CI runners
        // have no bwrap/sandbox-exec). Confinement is exercised separately by
        // `should_block_shell_write_outside_workspace_via_sandbox`, which opts
        // into a real backend and self-skips when none is available.
        Self::build_with_sandbox(
            provider,
            workspace,
            SandboxConfig {
                mode: SandboxMode::None,
                ..SandboxConfig::default()
            },
        )
    }

    fn build_with_sandbox(
        provider: Arc<dyn LlmProvider>,
        workspace: TempDir,
        sandbox: SandboxConfig,
    ) -> Self {
        let factory = AgentLlmFactory::scripted(provider);
        let data_dir = workspace.path().join(".octos-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let config = SessionDispatchConfig {
            cwd: workspace.path().to_path_buf(),
            data_dir,
            max_iterations: 4,
            sandbox,
            tool_policy: None,
            tool_policy_by_provider: Default::default(),
            provider_name: String::new(),
        };
        Self {
            dispatch: RealSessionDispatch::new_for_test(config, factory),
            _workspace: workspace,
        }
    }
}

#[tokio::test]
async fn should_execute_real_agent_session_via_mcp_dispatch_and_return_artifact() {
    let workspace = TempDir::new().unwrap();
    let artifact_dir = workspace.path().join("output");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("deck.pptx");
    std::fs::write(&artifact_path, b"fake-pptx-bytes").unwrap();

    // Scripted provider: first turn writes no tool calls, just finishes
    // with a summary — this exercises the full loop (build_initial_messages
    // → call_llm → stop_reason::EndTurn → build_result). The dispatch is
    // responsible for pulling the artifact from the workspace and wrapping
    // it into a real outcome.
    let provider = ScriptedLlmProvider::new(vec![end_turn(
        "I wrote the slides and the deck is at output/deck.pptx",
    )]);
    let harness = DispatchHarness::build(provider, workspace);
    let observer = RecordingObserver::new();

    let outcome = harness
        .dispatch
        .run_session(
            "slides_delivery",
            &json!({
                "prompt": "generate a slide deck",
                "artifact_name": "primary",
                "expected_artifact": artifact_path.display().to_string(),
            }),
            &observer,
        )
        .await
        .expect("dispatch should succeed when artifact exists");

    assert_eq!(outcome.final_state, TaskLifecycleState::Ready);
    assert!(
        outcome.artifact_path.is_some(),
        "artifact_path must be populated on Ready outcome"
    );
    assert!(
        outcome
            .artifact_path
            .as_ref()
            .unwrap()
            .ends_with("deck.pptx"),
        "artifact path: {:?}",
        outcome.artifact_path
    );
    assert!(outcome.error.is_none());
    // cost must be a real token bundle, never a placeholder zero struct.
    assert_eq!(outcome.cost.input_tokens, 42);
    assert_eq!(outcome.cost.output_tokens, 17);
}

#[tokio::test]
async fn should_propagate_task_lifecycle_state_through_dispatch_observer() {
    let workspace = TempDir::new().unwrap();
    let artifact_dir = workspace.path().join("output");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("deck.pptx");
    std::fs::write(&artifact_path, b"payload").unwrap();

    let provider = ScriptedLlmProvider::new(vec![end_turn("done")]);
    let harness = DispatchHarness::build(provider, workspace);
    let observer = RecordingObserver::new();

    let _ = harness
        .dispatch
        .run_session(
            "slides_delivery",
            &json!({
                "prompt": "make slides",
                "expected_artifact": artifact_path.display().to_string(),
            }),
            &observer,
        )
        .await
        .expect("dispatch runs");

    let states = observer.snapshot();
    assert!(
        states.contains(&TaskLifecycleState::Running),
        "observer never saw Running transition: {states:?}"
    );
    assert!(
        states.contains(&TaskLifecycleState::Verifying),
        "observer never saw Verifying transition: {states:?}"
    );
    assert!(
        states.contains(&TaskLifecycleState::Ready) || states.contains(&TaskLifecycleState::Failed),
        "observer never saw a terminal transition: {states:?}"
    );

    // Ordering invariant: Running must come before Verifying, Verifying
    // before the terminal state.
    let idx = |target: TaskLifecycleState| states.iter().position(|s| *s == target);
    if let (Some(running), Some(verifying)) = (
        idx(TaskLifecycleState::Running),
        idx(TaskLifecycleState::Verifying),
    ) {
        assert!(
            running < verifying,
            "Running must precede Verifying: {states:?}"
        );
    }
}

#[tokio::test]
async fn should_return_contract_artifact_on_session_ready_via_mcp() {
    let workspace = TempDir::new().unwrap();
    let artifact_dir = workspace.path().join("output");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("report.pdf");
    std::fs::write(&artifact_path, b"pdf-bytes").unwrap();

    let provider = ScriptedLlmProvider::new(vec![end_turn("report ready")]);
    let harness = DispatchHarness::build(provider, workspace);
    let observer = RecordingObserver::new();

    let outcome = harness
        .dispatch
        .run_session(
            "custom_report",
            &json!({
                "prompt": "produce the report",
                "expected_artifact": artifact_path.display().to_string(),
            }),
            &observer,
        )
        .await
        .expect("dispatch runs");

    assert_eq!(outcome.final_state, TaskLifecycleState::Ready);
    let returned_path = outcome
        .artifact_path
        .clone()
        .expect("Ready outcome must carry an artifact_path");
    assert!(
        returned_path.ends_with("report.pdf"),
        "artifact path should match the delivered artifact: {returned_path}",
    );
    // The dispatch must include contract enforcement — validator_results
    // is a Vec (possibly empty when no validators configured) but must
    // never be omitted from the MCP response structure.
    // Acceptance: the vec is populated or empty but reflects a real run.
    let _ = outcome.validator_results;
}

#[tokio::test]
async fn should_return_typed_failure_on_session_failed_via_mcp() {
    let workspace = TempDir::new().unwrap();
    // Intentionally do NOT create the expected artifact — dispatch must
    // surface a Failed outcome with a typed recovery hint, matching the
    // M6.1 HarnessError contract: no placeholder-zero success.
    let missing = workspace.path().join("output/never_written.pptx");

    let provider = ScriptedLlmProvider::new(vec![end_turn(
        "pretending to finish but nothing was produced",
    )]);
    let harness = DispatchHarness::build(provider, workspace);
    let observer = RecordingObserver::new();

    let outcome = harness
        .dispatch
        .run_session(
            "slides_delivery",
            &json!({
                "prompt": "attempt slides",
                "expected_artifact": missing.display().to_string(),
            }),
            &observer,
        )
        .await
        .expect("dispatch returns Ok with Failed outcome");

    assert_eq!(outcome.final_state, TaskLifecycleState::Failed);
    assert!(
        outcome.artifact_path.is_none(),
        "Failed outcome must not carry a spurious artifact_path: {:?}",
        outcome.artifact_path
    );
    let error = outcome
        .error
        .as_ref()
        .expect("Failed outcome must carry an error/recovery hint string");
    // Typed failure prefix (contract/recovery-hint-style). Keeps the
    // shape forward-compatible with M6.1 HarnessError — callers can
    // branch on the prefix without a full JSON schema migration.
    assert!(
        error.starts_with("contract_failed:")
            || error.starts_with("artifact_missing:")
            || error.starts_with("session_failed:"),
        "expected a typed failure prefix in error, got: {error}"
    );
}

#[tokio::test]
async fn should_not_leak_internal_iteration_messages_via_dispatch() {
    let workspace = TempDir::new().unwrap();
    let artifact_dir = workspace.path().join("output");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("deck.pptx");
    std::fs::write(&artifact_path, b"data").unwrap();

    // Drive the agent through a multi-turn loop so it generates internal
    // messages. The MCP outcome must only expose the terminal summary —
    // tool arguments, iteration text, and per-turn reasoning must never
    // leak into the MCP response.
    let tool_call = ToolCall {
        id: "call-1".to_string(),
        name: "read_file".to_string(),
        arguments: json!({"path": artifact_path.display().to_string()}),
        metadata: None,
    };
    let provider = ScriptedLlmProvider::new(vec![
        tool_use(tool_call),
        end_turn("Verified deck exists. FINAL_SUMMARY_TEXT_ONLY"),
    ]);
    let harness = DispatchHarness::build(provider, workspace);
    let observer = RecordingObserver::new();

    let outcome = harness
        .dispatch
        .run_session(
            "slides_delivery",
            &json!({
                "prompt": "check deck",
                "expected_artifact": artifact_path.display().to_string(),
            }),
            &observer,
        )
        .await
        .expect("dispatch should complete");

    // Serialize the full outcome and make sure none of the iteration
    // internals bleed through into the MCP payload.
    let rendered = serde_json::to_string(&serde_json::json!({
        "final_state": match outcome.final_state {
            TaskLifecycleState::Queued => "queued",
            TaskLifecycleState::Running => "running",
            TaskLifecycleState::Verifying => "verifying",
            TaskLifecycleState::Ready => "ready",
            TaskLifecycleState::Failed => "failed",
            TaskLifecycleState::Cancelled => "cancelled",
        },
        "artifact_path": outcome.artifact_path,
        "artifact_content": outcome.artifact_content,
        "validator_results": outcome.validator_results,
        "cost": outcome.cost,
        "error": outcome.error,
    }))
    .unwrap();

    // No tool arguments (e.g. `"read_file"`) and no iteration-level
    // reasoning text should be visible.
    assert!(
        !rendered.contains("read_file"),
        "MCP outcome leaked tool call internals: {rendered}"
    );
    assert!(
        !rendered.contains("\"call-1\""),
        "MCP outcome leaked tool call ID: {rendered}"
    );
    assert!(
        !rendered.contains("iteration"),
        "MCP outcome leaked internal iteration text: {rendered}"
    );
}

#[tokio::test]
async fn should_populate_validator_results_when_workspace_policy_declares_validators() {
    use octos_agent::workspace_policy::{
        Validator, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy, WorkspacePolicyKind,
        WorkspaceSnapshotTrigger, WorkspaceTrackingPolicy, WorkspaceVersionControlPolicy,
        WorkspaceVersionControlProvider, write_workspace_policy,
    };
    use octos_agent::{
        ValidationPolicy, WorkspaceArtifactsPolicy, workspace_policy::WorkspacePolicyWorkspace,
    };

    let workspace = TempDir::new().unwrap();
    let artifact_dir = workspace.path().join("output");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join("deck.pptx");
    std::fs::write(&artifact_path, b"data").unwrap();

    // Write a workspace policy with a typed file-existence validator so the
    // dispatch has something concrete to run at completion phase.
    let policy = WorkspacePolicy {
        schema_version: octos_agent::WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Slides,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: false,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: false,
        },
        tracking: WorkspaceTrackingPolicy { ignore: Vec::new() },
        validation: ValidationPolicy {
            on_turn_end: Vec::new(),
            on_source_change: Vec::new(),
            on_completion: Vec::new(),
            validators: vec![Validator {
                id: "deck-exists".into(),
                required: true,
                soft_fail: false,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::FileExists {
                    path: "output/deck.pptx".into(),
                    min_bytes: None,
                },
            }],
        },
        artifacts: WorkspaceArtifactsPolicy::default(),
        spawn_tasks: std::collections::BTreeMap::new(),
        compaction: None,
    };
    write_workspace_policy(workspace.path(), &policy).unwrap();

    let provider = ScriptedLlmProvider::new(vec![end_turn("deck created")]);
    let harness = DispatchHarness::build(provider, workspace);
    let observer = RecordingObserver::new();

    let outcome = harness
        .dispatch
        .run_session(
            "slides_delivery",
            &json!({
                "prompt": "make slides",
                "expected_artifact": artifact_path.display().to_string(),
            }),
            &observer,
        )
        .await
        .expect("dispatch should succeed");

    assert_eq!(outcome.final_state, TaskLifecycleState::Ready);
    assert!(
        !outcome.validator_results.is_empty(),
        "validator_results must reflect the declared validator, got {:?}",
        outcome.validator_results,
    );
    let entry = &outcome.validator_results[0];
    assert_eq!(entry.validator_id, "deck-exists");
    assert_eq!(entry.status, ValidatorStatus::Pass);
}

/// Security regression for the M7.2 session-dispatch RCE: the per-session tool
/// registry must confine `shell` to the workspace `cwd`. Before the fix the
/// dispatch built tools with `with_builtins` (`NoSandbox`), so an outer MCP
/// caller could drive `shell` to write anywhere the octos process could.
///
/// Self-gates on backend availability: where `SandboxMode::Auto` resolves to
/// `NoSandbox` (no `sandbox-exec` on macOS / no `bwrap` on Linux), OS-level
/// confinement cannot be exercised, so the test skips instead of failing.
#[tokio::test]
async fn should_block_shell_write_outside_workspace_via_sandbox() {
    // Probe the resolved backend: NoSandbox wraps with `sh`/`cmd`, an
    // enforcing backend wraps with `sandbox-exec`/`bwrap`/`docker`.
    let sandbox = octos_agent::create_sandbox(&SandboxConfig::default());
    let program = sandbox
        .wrap_command("true", std::path::Path::new("."))
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    if program == "sh" || program == "cmd" {
        eprintln!(
            "skipping should_block_shell_write_outside_workspace_via_sandbox: \
             no enforcing sandbox backend on this host (wrap program = {program:?})"
        );
        return;
    }
    // Docker wraps the command into a container whose filesystem does not map
    // the host `cwd`/sibling temp dirs the assertions below rely on, so the
    // write-path checks don't apply.
    if program.ends_with("docker") {
        eprintln!(
            "skipping should_block_shell_write_outside_workspace_via_sandbox: \
             docker backend needs container-relative paths"
        );
        return;
    }
    let workspace = TempDir::new().unwrap();
    let ws_path = workspace.path().to_path_buf();

    // The backend binary exists, but on some hosts it is present yet unusable
    // (`sandbox-exec` denied, `bwrap` without user namespaces, Docker with no
    // daemon). Trusting the wrapper name alone would let the test proceed and
    // then fail its own positive control. Actually run a harmless command
    // through the wrapper — using the SAME absolute workspace the real run
    // uses: bwrap binds the cwd (`--bind <cwd> <cwd> --chdir <cwd>`), and a
    // relative "." would bind at the sandbox root and hide `/bin`, failing the
    // probe (and silently skipping this regression) on an otherwise-working
    // Linux backend. If it can't execute, OS confinement can't be exercised
    // here, so skip rather than fail.
    let probe_ok = sandbox
        .wrap_command("true", &ws_path)
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false);
    if !probe_ok {
        eprintln!(
            "skipping should_block_shell_write_outside_workspace_via_sandbox: \
             sandbox backend {program:?} is present but not runnable on this host"
        );
        return;
    }
    // A sibling temp dir that is NOT under the workspace cwd. The sandbox
    // allows writes only under cwd, so a write here must be denied.
    let escape_dir = TempDir::new().unwrap();
    let escape_file = escape_dir.path().join("escape.txt");
    // Positive control inside cwd — proves the shell actually executed under
    // the sandbox, so an absent escape file is a real denial rather than the
    // shell failing to start.
    let inside_file = ws_path.join("inside.txt");
    let command = format!(
        "echo ok > {}; echo pwned > {}",
        inside_file.display(),
        escape_file.display()
    );

    let provider = ScriptedLlmProvider::new(vec![
        tool_use(ToolCall {
            id: "escape-1".to_string(),
            name: "shell".to_string(),
            arguments: json!({ "command": command }),
            metadata: None,
        }),
        end_turn("attempted the writes"),
    ]);
    // Opt into a real backend (Auto) — this test asserts OS-level confinement,
    // and self-skipped above when Auto resolves to NoSandbox on this host.
    let harness =
        DispatchHarness::build_with_sandbox(provider, workspace, SandboxConfig::default());
    let observer = RecordingObserver::new();

    // Outcome may be Ready or Failed depending on artifact resolution; the
    // regression is about the filesystem effect, not the returned state.
    let _ = harness
        .dispatch
        .run_session("coding", &json!({ "prompt": "run the command" }), &observer)
        .await;

    assert!(
        inside_file.exists(),
        "positive control failed: shell did not write inside the workspace cwd \
         at {} — the sandbox test cannot distinguish a real denial from a shell \
         that never ran",
        inside_file.display()
    );
    assert!(
        !escape_file.exists(),
        "sandbox must block shell writes outside the workspace cwd; \
         escape file was created at {}",
        escape_file.display()
    );
}
