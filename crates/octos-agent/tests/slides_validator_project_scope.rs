//! Regression coverage for octos #997 — the slides-kind project-scope
//! `WorkspacePolicy` must wire the `mofa_slides` PPTX `MagicBytes` validator
//! into `validation.validators` so the contract-gate rejects HTML "success"
//! decks (the user-visible failure mode where `mofa_slides` writes an HTML
//! error page in place of the `.pptx`).
//!
//! Pre-fix, the `mofa_slides` validator was inserted ONLY into the
//! session-scope spawn_tasks table (`workspace_policy.rs:1127`) and the
//! slides-kind policy declared `spawn_tasks: BTreeMap::new()`. Because
//! `inspect_workspace_contract` reads `validation.validators` (not
//! `spawn_tasks`), the gate silently passed an HTML-as-PPTX deck.
//!
//! Run with `cargo test -p octos-agent --test slides_validator_project_scope`.

use octos_agent::workspace_git::WorkspaceProjectKind;
use octos_agent::workspace_policy::{
    MagicByteKind, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy,
};

#[test]
fn slides_kind_policy_wires_mofa_slides_pptx_magic_bytes_validator() {
    // Project-scope guarantee: a slides-kind workspace policy must declare
    // a hard-required `MagicBytes` validator for `**/*.pptx` so a downstream
    // `run_declared_validators` call rejects HTML-as-PPTX failure modes.
    let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);

    let pptx_validator = policy
        .validation
        .validators
        .iter()
        .find(|v| matches!(&v.spec, ValidatorSpec::MagicBytes { format, .. } if *format == MagicByteKind::Pptx))
        .expect(
            "slides-kind policy must declare a MagicBytes(Pptx) validator in \
             validation.validators (octos #997)",
        );

    // The validator must be hard-required so it actually demotes a failing
    // delivery — a soft validator would never block the gate.
    assert!(
        pptx_validator.required,
        "PPTX MagicBytes validator must be required = true so the gate blocks"
    );
    assert!(
        !pptx_validator.soft_fail,
        "PPTX MagicBytes validator must be a hard gate (soft_fail = false)"
    );
    assert_eq!(
        pptx_validator.phase,
        ValidatorPhaseKind::Completion,
        "PPTX MagicBytes validator must run at the Completion phase"
    );

    // Sanity: the glob must target `.pptx` files. The validator is glob-
    // based, not template-interpolated, so a recursive PPTX pattern is what
    // we want.
    let glob = match &pptx_validator.spec {
        ValidatorSpec::MagicBytes { glob, .. } => glob.clone(),
        _ => unreachable!("matched MagicBytes above"),
    };
    assert!(
        glob.ends_with(".pptx"),
        "MagicBytes glob should target .pptx files, got {glob:?}"
    );
}

#[tokio::test]
async fn html_pptx_fails_slides_kind_project_scope_validator_gate() {
    // End-to-end: a slides project with an HTML-content `.pptx` (the
    // mofa_slides skill failure mode) must trip the project-scope validator
    // gate via `run_declared_validators`. Pre-fix this passed silently
    // because the slides-kind policy declared no validators.
    use std::sync::Arc;

    use octos_agent::ToolRegistry;
    use octos_agent::validators::ValidatorPhase;
    use octos_agent::workspace_contract::run_declared_validators;

    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path();
    let output_dir = workspace_root.join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Failure mode: mofa_slides wrote an HTML error page in place of the
    // PPTX. The bytes-at-offset-0 are NOT the ZIP local-file-header signature
    // (`PK\x03\x04`) that a real .pptx carries, so MagicBytes(Pptx) must
    // reject this file.
    let html_error_page = b"<!DOCTYPE html><html><body>500 internal error</body></html>";
    std::fs::write(output_dir.join("deck.pptx"), html_error_page).unwrap();

    let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    let registry = Arc::new(ToolRegistry::new());

    let result = run_declared_validators(
        &registry,
        workspace_root,
        &policy.validation.validators,
        "slides/demo",
        ValidatorPhase::Completion,
        None,
    )
    .await;

    let err = result.expect_err(
        "HTML-as-PPTX deck must fail the slides project-scope validator gate (octos #997)",
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("magic_bytes") || rendered.contains("pptx"),
        "validator failure should call out magic_bytes/pptx, got: {rendered}"
    );
}

#[tokio::test]
async fn valid_pptx_passes_slides_kind_project_scope_validator_gate() {
    // Positive case: a real .pptx (ZIP container with the local-file-header
    // signature at offset 0) must pass the project-scope gate so genuine
    // mofa_slides outputs are not blocked.
    use std::sync::Arc;

    use octos_agent::ToolRegistry;
    use octos_agent::validators::ValidatorPhase;
    use octos_agent::workspace_contract::run_declared_validators;

    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path();
    let output_dir = workspace_root.join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Minimal PPTX header: PK\x03\x04 (ZIP local-file-header magic). The
    // MagicBytes validator only inspects the leading bytes — a full valid
    // archive is not required for this check.
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(output_dir.join("deck.pptx"), pptx_bytes).unwrap();

    let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    let registry = Arc::new(ToolRegistry::new());

    let outcomes = run_declared_validators(
        &registry,
        workspace_root,
        &policy.validation.validators,
        "slides/demo",
        ValidatorPhase::Completion,
        None,
    )
    .await
    .expect("genuine PPTX must pass the slides project-scope validator gate");

    // Confirm the PPTX MagicBytes validator was actually exercised — not
    // skipped via an empty list.
    let pptx_outcome = outcomes
        .iter()
        .find(|o| o.kind == "magic_bytes")
        .expect("MagicBytes outcome must be recorded for a slides-kind gate run");
    assert_eq!(
        pptx_outcome.status,
        octos_agent::validators::ValidatorStatus::Pass
    );
}

#[tokio::test]
async fn project_root_validators_write_to_project_ledger_without_manual_seeding() {
    // octos #997 (round-2 fix): the load-bearing test for codex's review.
    //
    // Codex flagged that pre-round-2, the validator was DECLARED at the
    // slides-kind project policy but never RUN at the project root —
    // production decks that genuinely produce a valid PPTX would still
    // surface `ready = false` because `inspect_workspace_contract` reads
    // `<session>/slides/<slug>/.octos/validator_outcomes.jsonl` but the
    // production code path only wrote to `<session>/.octos/...`. The 9
    // fixture sites that manually `ledger.append(...)` a `Pass` were
    // masking the gap.
    //
    // This test exercises the production code path WITHOUT manually
    // seeding the ledger:
    //
    // 1. Build a slides workspace with a real PPTX and a project-scope
    //    `WorkspacePolicy::for_kind(Slides)` (which declares the
    //    hard-required `slides.mofa_slides.pptx_magic_bytes` validator).
    // 2. Invoke `run_project_root_validators` — the helper wired into the
    //    spawn completion path in this commit.
    // 3. Assert the project-root ledger file exists and contains a `Pass`
    //    for the slides-kind PPTX MagicBytes validator id.
    // 4. Assert `inspect_workspace_contract_at_root` reports `ready = true`
    //    against that project — proving the contract gate sees the run.
    //
    // PRE-FIX FAILURE QUOTE (verified by stubbing
    // `run_project_root_validators` to return an empty report — i.e.
    // mirroring the pre-round-2 state where nothing ran validators at the
    // project root):
    //
    //     assertion `left == right` failed: expected exactly one slides
    //     project to have run validators; got report =
    //     ProjectRootValidatorReport { projects_run: 0, failures: [] }
    //       left: 0
    //      right: 1
    //
    // i.e. the production code path never runs the declared validator at
    // the project root, so the ledger file never exists, and the
    // inspect-contract gate stays `ready = false` even with a genuine deck.
    use octos_agent::ToolRegistry;
    use octos_agent::inspect_workspace_contract_at_root;
    use octos_agent::validators::{ValidatorLedger, ValidatorStatus};
    use octos_agent::workspace_contract::run_project_root_validators;
    use octos_agent::workspace_policy::write_workspace_policy;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let session_root = dir.path();
    let project_root = session_root.join("slides").join("demo");
    let output_dir = project_root.join("output");
    let imgs_dir = output_dir.join("imgs");
    std::fs::create_dir_all(&imgs_dir).unwrap();

    // The slides-kind policy declares the hard-required PPTX MagicBytes
    // validator (octos #997). Persist it under the project root, as
    // `create_slides_project` would in production.
    write_workspace_policy(
        &project_root,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
    )
    .unwrap();

    // Required source files (turn-end checks) + a genuine PPTX (the
    // success case mofa_slides produces).
    std::fs::write(project_root.join("script.js"), "// slides").unwrap();
    std::fs::write(project_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(project_root.join("changelog.md"), "# changelog").unwrap();
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(output_dir.join("deck.pptx"), &pptx_bytes).unwrap();
    std::fs::write(imgs_dir.join("slide-01.png"), b"png").unwrap();

    // PRE-CONDITION: no validator outcome exists at the project root.
    // (Production code path has not been exercised yet.)
    let ledger_path = project_root.join(".octos").join("validator_outcomes.jsonl");
    assert!(
        !ledger_path.exists(),
        "ledger should not exist before the project-root validator run — \
         otherwise the test would not prove the production path writes it"
    );

    // ACT: invoke the production code path. This is what the spawn loop
    // calls after a successful `run_task` for slides workflows. No manual
    // ledger seeding.
    let registry = Arc::new(ToolRegistry::new());
    let report =
        run_project_root_validators(&registry, session_root, Some(WorkspaceProjectKind::Slides))
            .await;

    // The slides project should have been picked up + run.
    assert_eq!(
        report.projects_run, 1,
        "expected exactly one slides project to have run validators; got report = {report:?}"
    );
    assert!(
        report.failures.is_empty(),
        "genuine PPTX should not produce failures; got failures = {:?}",
        report.failures
    );

    // ASSERT 1: the project-root ledger file MUST exist after the
    // production code path runs (without manual seeding).
    assert!(
        ledger_path.exists(),
        "project ledger must exist at {} after the production code path \
         runs (no manual seeding) — this is the gap codex flagged",
        ledger_path.display()
    );

    // ASSERT 2: the ledger must contain a `Pass` for the slides-kind
    // PPTX MagicBytes validator id (the one declared by `for_kind(Slides)`).
    let ledger = ValidatorLedger::open(&ledger_path).expect("open project ledger");
    let entries = ledger.read_all().expect("read project ledger entries");
    let pptx_pass = entries
        .iter()
        .find(|o| {
            o.validator_id == "slides.mofa_slides.pptx_magic_bytes"
                && o.status == ValidatorStatus::Pass
        })
        .unwrap_or_else(|| {
            panic!(
                "project ledger must contain a Pass for \
                 slides.mofa_slides.pptx_magic_bytes; got entries = {entries:?}"
            )
        });
    assert_eq!(pptx_pass.kind, "magic_bytes");

    // ASSERT 3: the contract gate reads the ledger we just wrote and now
    // reports `ready = true`. This is the user-visible behaviour the gap
    // was suppressing.
    let status = inspect_workspace_contract_at_root(&project_root)
        .expect("inspect_workspace_contract_at_root must succeed");
    assert!(
        status.ready,
        "contract gate must report ready = true after project-root \
         validators run; status = {status:?}"
    );
}

// octos #997 (round-3 fix): codex flagged TWO more bypass paths still
// uncovered by the round-2 wiring. These tests drive the production
// code paths end-to-end so the spawn loop ACTUALLY writes the
// `<session>/slides/<slug>/.octos/validator_outcomes.jsonl` row that
// `inspect_workspace_contract` reads.
//
// Pre-round-3 these tests FAIL with messages like:
//     project ledger must exist at .../slides/demo/.octos/validator_outcomes.jsonl
//     after the spawn_only completion path runs
// because the spawn_only completion path at `agent/execution.rs:619`
// only calls `enforce_spawn_task_contract_with_args_and_output` (which
// runs validators at the SESSION root, not the project root) and the
// agent_mcp branch at `tools/spawn.rs:2087` does the same.

/// BLOCKING bypass #1 — spawn_only direct invocation.
///
/// A direct `mofa_slides(out=slides/demo/output/deck.pptx, ...)` runs
/// through the spawn_only intercept at `agent/execution.rs:307`. On
/// success the path calls ONLY
/// `enforce_spawn_task_contract_with_args_and_output` (which runs
/// validators at SESSION root and writes the session ledger). Pre-fix
/// the project ledger at
/// `<session>/slides/<slug>/.octos/validator_outcomes.jsonl` is NEVER
/// written.
#[tokio::test]
async fn spawn_only_mofa_slides_writes_project_ledger() {
    use async_trait::async_trait;
    use octos_agent::{
        Agent, AgentConfig, Tool, ToolRegistry, ToolResult, WorkspacePolicy, WorkspaceProjectKind,
        write_workspace_policy,
    };
    use octos_core::{AgentId, Message, MessageRole, ToolCall};
    use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
    use octos_memory::EpisodeStore;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    // ── Scripted LLM: invoke mofa_slides once, then EndTurn. ──────────────
    struct ScriptedLlm(std::sync::Mutex<Vec<ChatResponse>>);
    #[async_trait]
    impl LlmProvider for ScriptedLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            let mut r = self.0.lock().unwrap();
            if r.is_empty() {
                eyre::bail!("ScriptedLlm: out of responses");
            }
            Ok(r.remove(0))
        }
        fn context_window(&self) -> u32 {
            128_000
        }
        fn model_id(&self) -> &str {
            "spawn-only-slides-test"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    // ── Mock mofa_slides tool: reports the PPTX via `files_to_send`. ────
    struct FakeMofaSlides {
        pptx_abs_path: PathBuf,
    }
    #[async_trait]
    impl Tool for FakeMofaSlides {
        fn name(&self) -> &str {
            "mofa_slides"
        }
        fn description(&self) -> &str {
            "fake mofa_slides for #997 round-3 test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: "Generated PPTX: ok\n".into(),
                files_to_send: vec![self.pptx_abs_path.clone()],
                ..Default::default()
            })
        }
    }

    // ── Workspace setup (session root + slides/demo project). ──────────
    let dir = TempDir::new().unwrap();
    let session_root = dir.path();
    let project_root = session_root.join("slides").join("demo");
    let output_dir = project_root.join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Session-scope policy (carries `spawn_tasks.mofa_slides`).
    write_workspace_policy(session_root, &WorkspacePolicy::for_session()).unwrap();
    // Project-scope policy (carries the hard-required PPTX MagicBytes
    // validator — the round-3 fix is what gets this RUN).
    write_workspace_policy(
        &project_root,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
    )
    .unwrap();
    // Turn-end "required source file" checks (slides project policy).
    std::fs::write(project_root.join("script.js"), "// slides").unwrap();
    std::fs::write(project_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(project_root.join("changelog.md"), "# changelog").unwrap();
    // Real PPTX (ZIP local-file-header magic). MagicBytes only inspects
    // the leading 4 bytes.
    let pptx_path = output_dir.join("deck.pptx");
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(&pptx_path, &pptx_bytes).unwrap();

    // ── Tool registry + spawn_only mofa_slides. ───────────────────────
    let mut tools = ToolRegistry::with_builtins(session_root);
    tools.register(FakeMofaSlides {
        pptx_abs_path: pptx_path.clone(),
    });
    tools.mark_spawn_only("mofa_slides", None);
    let supervisor = tools.supervisor();

    // ── Agent. ─────────────────────────────────────────────────────────
    let memory = Arc::new(
        EpisodeStore::open(dir.path().join(".octos-mem"))
            .await
            .unwrap(),
    );
    let llm: Arc<dyn LlmProvider> = Arc::new(ScriptedLlm(std::sync::Mutex::new(vec![
        ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "call-slides-1".into(),
                name: "mofa_slides".into(),
                arguments: serde_json::json!({"task": "make a deck"}),
                metadata: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 1,
                ..Default::default()
            },
            provider_index: None,
        },
        ChatResponse {
            content: Some("done".into()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 5,
                ..Default::default()
            },
            provider_index: None,
        },
    ])));
    let agent = Agent::new(AgentId::new("slides-spawn-only"), llm, tools, memory).with_config(
        AgentConfig {
            save_episodes: false,
            suppress_auto_send_files: true,
            ..Default::default()
        },
    );

    let response = agent
        .process_message("make slides", &[], vec![])
        .await
        .expect("agent loop must succeed");
    // Sanity: the spawn_only intercept fires synchronously — the foreground
    // turn returns a Tool message for the spawn_only call.
    assert!(
        response
            .messages
            .iter()
            .any(|m| matches!(m.role, MessageRole::Tool)
                && m.tool_call_id.as_deref() == Some("call-slides-1")),
        "expected a synthetic Tool message for the spawn_only call; got: {:#?}",
        response.messages
    );

    // ── Wait for the background spawn_only task to reach terminal state.
    let ledger_path = project_root.join(".octos").join("validator_outcomes.jsonl");
    let mut completed = false;
    for _ in 0..100 {
        for task in supervisor.get_all_tasks() {
            if task.tool_name == "mofa_slides"
                && matches!(
                    task.status,
                    octos_agent::TaskStatus::Completed | octos_agent::TaskStatus::Failed
                )
            {
                completed = true;
                break;
            }
        }
        if completed && ledger_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        completed,
        "background spawn_only mofa_slides must reach terminal status"
    );

    // ── Load-bearing assertion: the project ledger must exist. ───────────
    //
    // PRE-FIX (round-2 HEAD 5457c184) FAILURE QUOTE:
    //   project ledger must exist at
    //   <session>/slides/demo/.octos/validator_outcomes.jsonl after the
    //   spawn_only completion path runs — the spawn loop only invokes
    //   enforce_spawn_task_contract_with_args_and_output (session scope)
    //   and never calls run_project_root_validators
    assert!(
        ledger_path.exists(),
        "project ledger must exist at {} after the spawn_only completion path runs \
         — the spawn loop only invokes enforce_spawn_task_contract_with_args_and_output \
         (session scope) and never calls run_project_root_validators",
        ledger_path.display()
    );

    // The ledger must contain the slides-kind PPTX MagicBytes Pass row.
    let ledger = octos_agent::ValidatorLedger::open(&ledger_path).expect("open project ledger");
    let entries = ledger.read_all().expect("read project ledger entries");
    let pass = entries
        .iter()
        .find(|o| {
            o.validator_id == "slides.mofa_slides.pptx_magic_bytes"
                && o.status == octos_agent::ValidatorStatus::Pass
        })
        .unwrap_or_else(|| {
            panic!(
                "project ledger must contain a Pass for slides.mofa_slides.pptx_magic_bytes; \
                 got entries = {entries:?}"
            )
        });
    assert_eq!(pass.kind, "magic_bytes");
}

/// BLOCKING bypass #2 — `agent_mcp` branch in `tools/spawn.rs`.
///
/// The `agent_mcp` branch returns after the SESSION-scope validator
/// check (`tools/spawn.rs:2087-2113`). Pre-round-3 it never invokes
/// `run_project_root_validators`, so a slides workflow that dispatches
/// through MCP completes without writing the project ledger.
///
/// octos #997 (round-4 fix): omitting `terminal_output` masks the
/// real production gap because `workflow_uses_contract_terminal_delivery`
/// returns `false` when `required_artifact_kind` is absent, so the
/// `resolve_contract_terminal_files` block is skipped entirely. The
/// production `slides_delivery` workflow ALWAYS sets
/// `terminal_output.required_artifact_kind = "presentation"`, which makes
/// `resolve_contract_terminal_files` run BEFORE the project-root
/// validator (round-3 ordering) and early-return on
/// `inspect_workspace_contract_at_root` failure — leaving the ledger
/// empty. Re-ordering ensures the validator runs FIRST.
#[tokio::test]
async fn agent_mcp_slides_writes_project_ledger() {
    use async_trait::async_trait;
    use octos_agent::tools::SpawnTool;
    use octos_agent::{
        DispatchOutcome, DispatchRequest, DispatchResponse, McpAgentBackend, SharedBackend, Tool,
        WorkspacePolicy, WorkspaceProjectKind, write_workspace_policy,
    };
    use octos_core::Message;
    use octos_llm::{ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage, ToolSpec};
    use octos_memory::EpisodeStore;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    // ── Fake MCP backend that returns a contract-shaped Success. ─────
    struct FakeBackend {
        pptx_abs_path: PathBuf,
    }
    #[async_trait]
    impl McpAgentBackend for FakeBackend {
        fn backend_label(&self) -> &'static str {
            "local"
        }
        fn endpoint_label(&self) -> String {
            "fake".into()
        }
        async fn dispatch(&self, _request: DispatchRequest) -> DispatchResponse {
            DispatchResponse {
                outcome: DispatchOutcome::Success,
                output: "Generated PPTX: ok".into(),
                files_to_send: vec![self.pptx_abs_path.clone()],
                error: None,
            }
        }
    }

    // ── Workspace setup. ─────────────────────────────────────────────
    let dir = TempDir::new().unwrap();
    let session_root = dir.path();
    let project_root = session_root.join("slides").join("demo");
    let output_dir = project_root.join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    write_workspace_policy(session_root, &WorkspacePolicy::for_session()).unwrap();
    write_workspace_policy(
        &project_root,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
    )
    .unwrap();
    std::fs::write(project_root.join("script.js"), "// slides").unwrap();
    std::fs::write(project_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(project_root.join("changelog.md"), "# changelog").unwrap();
    let pptx_path = output_dir.join("deck.pptx");
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(&pptx_path, &pptx_bytes).unwrap();

    // ── SpawnTool wired with the fake MCP backend. ──────────────────
    struct UnusedLlm;
    #[async_trait]
    impl LlmProvider for UnusedLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("unused".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }
        fn context_window(&self) -> u32 {
            128_000
        }
        fn model_id(&self) -> &str {
            "unused"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
    }

    let memory = Arc::new(
        EpisodeStore::open(dir.path().join(".octos-mem"))
            .await
            .unwrap(),
    );
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(16);
    let backend: SharedBackend = Arc::new(FakeBackend {
        pptx_abs_path: pptx_path.clone(),
    });
    let spawn_tool = SpawnTool::new(
        Arc::new(UnusedLlm),
        memory,
        session_root.to_path_buf(),
        in_tx,
    )
    .with_mcp_agent_backend(backend, Some("run_task".into()));

    // ── Dispatch through the agent_mcp branch. ─────────────────────
    //
    // octos #997 (round-4 fix): set `terminal_output.required_artifact_kind`
    // to match production `slides_delivery` shape. This makes
    // `workflow_uses_contract_terminal_delivery` return `true` so the
    // `resolve_contract_terminal_files` block at `spawn.rs:2059` runs.
    // Pre-fix that block ran BEFORE the project-root validator, calling
    // `inspect_workspace_contract_at_root(self.working_dir = session_root)`
    // which errors with
    // "unsupported workspace project root for git snapshot: <session_root>"
    // (the session root's parent is not `slides/sites`) — the early-return
    // at `spawn.rs:2068-2073` returns `Status: FAILED` BEFORE the
    // project-root validator at `:2128` ever executes. Result: the project
    // ledger is empty.
    //
    // Post-fix: the project-root validator runs FIRST and writes the
    // ledger; the orthogonal terminal-files error still demotes the
    // dispatch result, but the ledger persists.
    let result = spawn_tool
        .execute(&serde_json::json!({
            "task": "make a deck",
            "label": "slides-mcp",
            "mode": "sync",
            "backend": "agent_mcp",
            "allowed_tools": [],
            "workflow": {
                "workflow_kind": "slides",
                "current_phase": "design",
                "terminal_output": {
                    "deliver_final_artifact_only": true,
                    "forbid_intermediate_files": true,
                    "required_artifact_kind": "presentation"
                }
            }
        }))
        .await
        .expect("agent_mcp dispatch must not error");

    // ── Load-bearing assertion: project ledger must exist. ───────────
    //
    // PRE-FIX (round-3 HEAD dbc74780) FAILURE QUOTE:
    //   project ledger must exist at
    //   <session>/slides/demo/.octos/validator_outcomes.jsonl after
    //   the agent_mcp branch completes — pre-round-4 the
    //   `resolve_contract_terminal_files` block ran BEFORE the project-root
    //   validator and early-returned on the orthogonal terminal-files
    //   error, leaving the project ledger empty
    //
    // The dispatch result itself may fail because
    // `inspect_workspace_contract_at_root(session_root)` errors when the
    // session root's parent is not `slides/sites` — that's a separate,
    // pre-existing edge in the agent_mcp wiring orthogonal to #997's
    // project-ledger bypass. The load-bearing invariant is that the
    // project-root validator runs FIRST and the ledger is populated
    // regardless of the downstream gate outcome.
    let ledger_path = project_root.join(".octos").join("validator_outcomes.jsonl");
    assert!(
        ledger_path.exists(),
        "project ledger must exist at {} after the agent_mcp branch completes \
         — pre-round-4 the `resolve_contract_terminal_files` block ran BEFORE \
         the project-root validator and early-returned on the orthogonal \
         terminal-files error, leaving the project ledger empty. \
         result.success={}, result.output={}",
        ledger_path.display(),
        result.success,
        result.output
    );

    let ledger = octos_agent::ValidatorLedger::open(&ledger_path).expect("open project ledger");
    let entries = ledger.read_all().expect("read project ledger entries");
    let pass = entries
        .iter()
        .find(|o| {
            o.validator_id == "slides.mofa_slides.pptx_magic_bytes"
                && o.status == octos_agent::ValidatorStatus::Pass
        })
        .unwrap_or_else(|| {
            panic!(
                "project ledger must contain a Pass for slides.mofa_slides.pptx_magic_bytes; \
                 got entries = {entries:?}"
            )
        });
    assert_eq!(pass.kind, "magic_bytes");
}
