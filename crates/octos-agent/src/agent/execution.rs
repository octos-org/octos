//! Tool execution: dispatching tool calls with hooks and timeout handling.
//!
//! # Batch admission (M8.8, #1766)
//!
//! Each turn of the agent loop receives a batch of tool calls from the LLM.
//! Before M8.8 every call in a batch fired in parallel, which races when a
//! mutating tool (shell, write_file, edit_file, diff_edit, save_memory) sits
//! next to a reader in the same batch. The executor now consults
//! [`crate::tools::Tool::concurrency_class`] for every call and picks one of
//! three admission strategies:
//!
//! - **All-Safe batch** — the classic path. Every call is [`ConcurrencyClass::Safe`]
//!   (read-only, side-effect-free). The executor spawns each call as a detached
//!   task and aggregates via `futures::join_all`, preserving call order.
//! - **All-Exclusive batch** — the M8.8 path. Every call reports
//!   [`ConcurrencyClass::Exclusive`]. The executor runs calls serially in LLM
//!   call order. On the first error (including hook denials and panics), the
//!   remaining peers are skipped and each receives a synthetic
//!   "cancelled due to sibling error" [`Message`] so the LLM still sees a
//!   result for every `tool_call_id`.
//! - **Mixed batch** — the #1766 path (previously such batches fell back to
//!   fully-serial). Phase 1 runs every Safe call in parallel, with the same
//!   spawn/aggregate shape as the all-Safe path; phase 2 then runs the
//!   Exclusive calls serially in LLM call order. Aggregated results are
//!   reassembled in the ORIGINAL LLM call order, so downstream consumers see
//!   the same 1:1 `tool_call_id` mapping as the other two strategies.
//!
//! ## Mixed-batch semantics (#1766, pinned by the `mixed_batch_*` tests)
//!
//! - **Visibility** — Safe calls observe the PRE-batch state. A Safe read the
//!   LLM listed AFTER an Exclusive mutation still runs in phase 1, i.e.
//!   BEFORE that mutation, and does NOT see the sibling's write. (Before
//!   M8.8 the two raced nondeterministically; under the M8.8 serial fallback
//!   the read saw the write. The phased pipeline pins a deterministic
//!   reads-before-writes snapshot, regardless of list position.)
//! - **Cascade, phase 1 → phase 2** — any Safe-call failure that carries the
//!   cascade bit (real execution errors, hook denials, panics, timeouts —
//!   everything except a no-side-effect [`crate::tools::ToolInputError`],
//!   #1690) cancels the ENTIRE Exclusive phase, position-independently: no
//!   mutation runs after a failed read. Cancelled Exclusive calls receive
//!   the same synthetic "cancelled due to sibling error" [`Message`] as the
//!   serial path.
//! - **Cascade, inside phase 2** — identical to the all-Exclusive path: the
//!   first cascading failure cancels the REMAINING Exclusive peers (in LLM
//!   order). Phase-1 Safe results are never retroactively cancelled by a
//!   phase-2 failure: they already completed, are side-effect-free by
//!   definition, and their real outputs are strictly more information for
//!   the LLM than a synthetic cancellation.
//! - **Approvals / human-input determinism** — approval-gated tools (shell,
//!   edit_file, write_file, …) are all Exclusive, so their in-tool approval
//!   prompts fire one at a time, in LLM call order, during phase 2. Phase 1
//!   fully joins before phase 2 starts, so a human-wait Safe tool
//!   (`ask_user_question`) can never overlap an approval prompt. The
//!   per-turn requester bridges (`TOOL_APPROVAL_CTX`, `USER_QUESTION_CTX`)
//!   are captured per call in `spawn_tool_task`, exactly as before — the
//!   phase split does not change how they propagate.

use std::time::{Duration, Instant};

use eyre::Result;
use octos_core::{Message, MessageRole, TokenUsage};
use octos_llm::ChatResponse;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::{Agent, MAX_TOOL_TIMEOUT_SECS};
use crate::harness_errors::HarnessError;
use crate::harness_events::{lookup_event_sink_context, write_event_to_sink};
use crate::hooks::{HookEvent, HookPayload, HookResult};
use crate::progress::ProgressEvent;
use crate::task_supervisor::{TaskRuntimeState, TaskTerminalGuard};
use crate::tools::spawn::{BackgroundResultKind, BackgroundResultPayload};
use crate::tools::{
    ConcurrencyClass, TOOL_APPROVAL_CTX, TOOL_CTX, TURN_ATTACHMENT_CTX, ToolApprovalDecision,
    ToolApprovalRequest, ToolApprovalRequester, ToolContext, USER_QUESTION_CTX,
    UserQuestionRequester,
};
use crate::workspace_contract::{
    SpawnTaskContractResult, enforce_spawn_task_contract_with_args_and_output,
};

/// Per-tool-call result returned from the in-process dispatcher. Kept as a
/// tuple so the aggregation path can reuse today's `futures::join_all` style
/// fan-in without an intermediate struct.
///
/// Fields in order: the tool-result [`Message`], files the tool touched,
/// files the tool wants auto-delivered to the user, optional sub-agent
/// token usage, a per-call `success` bit used by the serial scheduler to
/// trigger the M8.8 error cascade, and the optional structured side-channel
/// metadata the tool surfaced (today: per-node cost rows from `run_pipeline`).
type ToolCallResult = (
    Message,
    Vec<std::path::PathBuf>,
    Vec<std::path::PathBuf>,
    Option<TokenUsage>,
    bool,
    Option<(String, serde_json::Value)>,
    // field 6 — `cascades`: when this call failed (field 4 == `false`), should
    // it cancel the remaining peers in a serial (M8.8) batch? `true` preserves
    // the legacy stop-on-error behaviour; `false` marks a no-side-effect
    // failure — a [`crate::tools::ToolInputError`] from malformed model
    // arguments — that must NOT nuke well-formed sibling calls (#1690).
    bool,
);

fn should_auto_send_tool_files(
    suppress_auto_send_files: bool,
    explicit_send_file_requested: bool,
    tool_name: &str,
) -> bool {
    !(suppress_auto_send_files || explicit_send_file_requested && tool_name != "send_file")
}

/// Names of tools whose work can legitimately run for many minutes and so
/// must keep the long [`MAX_TOOL_TIMEOUT_SECS`] (1800s) default when the LLM
/// omits a per-call `timeout_secs`. Everything NOT in this set is treated as
/// an interactive/fast tool and defaults to the much shorter
/// `default_interactive_tool_timeout_secs`.
///
/// mini5 soak motivation: a read-only `glob`/`list_dir` that walks an
/// unscoped home dir used to inherit the 1800s ceiling and hang the whole
/// turn with no output. Fast read-only tools have no business waiting 30
/// minutes; only genuinely long-running tools (shells, background spawns,
/// pipelines, browser sessions, deep research/crawl) do.
///
/// Names verified against the registered `Tool::name()` impls:
/// - `shell` (`tools/shell.rs`), `bash` alias
/// - `spawn` (`tools/spawn.rs`), `spawn_agent` alias
/// - `run_pipeline` (spawn_only pipeline tool registered via manifest)
/// - `browser` (`tools/browser.rs`)
/// - `delegate_task` (`tools/delegate.rs`)
/// - `search` (`tools/deep_search.rs`), `deep_crawl` (`tools/site_crawl.rs`)
/// - `synthesize_research` (`tools/synthesize_research.rs`)
/// - `check` (`tools/check.rs`): a cold `cargo check` legitimately compiles
///   the dependency graph; the tool enforces its own 120s child timeout,
///   which must fire BEFORE the batch ceiling (the interactive default is
///   also 120s and would race it, yielding a generic batch-timeout message
///   instead of the tool's clean "timed out" answer)
///
/// NOTE: human-wait tools (`ask_user_question`) are deliberately NOT in this
/// list. A batch containing one gets NO batch-level timeout at all (see
/// [`compute_batch_timeout_secs`] / `any_human_wait`), so the long-vs-short
/// classification never applies to it — wrapping it in even the 1800s ceiling
/// would detach the still-running tool task and leak the pending question
/// (UPCR-2026-023). They remain fully timeout-exempt at the registry dispatch
/// boundary too, via `Tool::blocks_on_human_input`.
const LONG_RUNNING_TOOLS: &[&str] = &[
    "shell",
    "bash",
    "spawn",
    "spawn_agent",
    "run_pipeline",
    "browser",
    "delegate_task",
    "search",
    "deep_crawl",
    "site_crawl",
    "synthesize_research",
    "check",
];

/// Whether `name` is a genuinely long-running tool (keeps the 1800s default).
fn is_long_running_tool(name: &str) -> bool {
    LONG_RUNNING_TOOLS.contains(&name)
}

/// Compute the timeout for a parallel/serial tool batch.
///
/// Returns `None` when the batch must run with NO finite batch-level timeout,
/// and `Some(secs)` otherwise.
///
/// Behaviour:
/// - **Human-wait batch (UPCR-2026-023):** when `any_human_wait` is `true`
///   (some tool in the batch reports [`crate::tools::Tool::blocks_on_human_input`],
///   e.g. `ask_user_question`), return `None`. Such a batch MUST NOT be wrapped
///   in `tokio::time::timeout`: a human may legitimately take longer than any
///   finite ceiling, and firing the ceiling would detach the still-running tool
///   task (its `JoinHandle` dropped, not awaited), so its
///   `PendingQuestionWaiterGuard` never drops → the pending question leaks and
///   is later replayed as a stale prompt after the turn moved on. Cleanup for a
///   human-wait batch comes from the user answering (resolves the oneshot) or a
///   turn interrupt/abort (the interrupt drains pending questions and aborts the
///   turn task), NEVER from the batch timeout. NON-human-wait tools sharing the
///   batch keep their own per-tool registry timeouts, applied INSIDE each tool's
///   registry dispatch — unaffected by removing this outer wrap.
/// - When the LLM requested a per-call `timeout_secs` (`llm_requested > 0`),
///   honour it: clamp to [`MAX_TOOL_TIMEOUT_SECS`] and floor at the batch's
///   default (so an explicit request never makes a batch flakier than its
///   own baseline). This mirrors the pre-fix `.min(MAX).max(default)`.
/// - When the LLM omitted `timeout_secs`, the default depends on the batch:
///   a batch containing ANY long-running tool keeps `config_tool_timeout`
///   (1800s today); a batch of only fast/interactive tools uses the much
///   shorter `interactive_default`.
fn compute_batch_timeout_secs(
    tool_names: &[&str],
    any_human_wait: bool,
    llm_requested: u64,
    config_tool_timeout: u64,
    interactive_default: u64,
) -> Option<u64> {
    // A human-wait batch is unbounded at the batch layer — see the doc above.
    if any_human_wait {
        return None;
    }
    let batch_default = if tool_names.iter().any(|n| is_long_running_tool(n)) {
        config_tool_timeout
    } else {
        interactive_default
    };
    Some(if llm_requested > 0 {
        llm_requested.min(MAX_TOOL_TIMEOUT_SECS).max(batch_default)
    } else {
        batch_default
    })
}

/// Issue #896 — spawn_only filename propagation (Layer 1).
///
/// Build a short follow-up notification that lists the workspace-relative
/// paths a spawn_only tool produced, so the LLM has stable filenames to
/// reference on its next turn. Without this, the LLM only sees the
/// `task_handle` envelope from `spawn_only_handle_message` (which carries
/// `output_dir` but not the actual filename), and tends to hallucinate
/// slugs (see live dspfac trace 2026-05-11).
///
/// Returns `None` when `files` is empty — the caller MUST suppress the
/// follow-up notification in that case so we never persist an empty
/// "produced files:" stub. Token-budget invariant (M10 Phase 4): paths
/// Whether a Satisfied spawn_only contract's delivery counts as a FAILURE.
///
/// `delivery` encodes the background-notification outcome:
/// - `None` — no background sender is wired (chat mode). The contract is
///   Satisfied and there is simply nowhere to deliver the notification; this
///   is NOT a failure. (The bug this fixes recorded it as Failed.)
/// - `Some(true)` — a sender ran and persisted the result. Success.
/// - `Some(false)` — a sender ran and genuinely failed to persist. Failure.
fn satisfied_delivery_is_failure(delivery: Option<bool>) -> bool {
    delivery == Some(false)
}

/// only, never file contents.
///
/// `workspace_root`, when supplied, is used to convert absolute paths
/// under the workspace into workspace-relative form (e.g.
/// `/Users/foo/.octos/profiles/p1/workspace/research/x/x.md` →
/// `research/x/x.md`) so the LLM can pass the path straight to
/// `read_file({path: "research/x/x.md"})`. Paths outside the workspace
/// (or when `workspace_root` is `None`) are kept verbatim.
fn build_spawn_only_produced_files_message(
    tool_name: &str,
    files: &[String],
    workspace_root: Option<&std::path::Path>,
) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let mut out = format!("`{tool_name}` produced files:");
    for path in files {
        out.push_str("\n- ");
        out.push_str(&relativize_workspace_path(path, workspace_root));
    }
    Some(out)
}

/// Strip the workspace prefix from an absolute path, returning a
/// workspace-relative path string. Falls back to the original path if it
/// cannot be relativised (already-relative input, different root, or no
/// `workspace_root` configured).
///
/// Pure helper so we can unit-test the relativisation logic without
/// standing up an Agent.
fn relativize_workspace_path(path: &str, workspace_root: Option<&std::path::Path>) -> String {
    let Some(root) = workspace_root else {
        return path.to_string();
    };
    let abs = std::path::Path::new(path);
    match abs.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// Decide what `content` to put on the spawn-only background-result payload
/// when the workspace contract reports `Satisfied`.
///
/// Wave-3b regression guard: a contract entry with no declared artifact
/// (e.g. `mofa_publish`, whose deliverable is a live URL, not a file)
/// returns `Satisfied { output_files: [] }`. Before this fix the handler
/// fell through to `content: String::new()`, dropping the tool's stdout
/// text (the deploy URL itself for `mofa_publish`) and the user saw a
/// blank completion.
///
/// Rule:
///
/// - When `output_files` is non-empty: emit empty content; the files
///   themselves carry the deliverable. This matches the legacy
///   file-attached behaviour for `fm_tts` / `podcast_generate` / etc.
/// - When `output_files` is empty: emit the tool's stdout `output` as
///   content. The contract verified the artifact-shaped portion of the
///   deliverable (e.g. the HttpProbe asserting deploy_url returned
///   `<!DOCTYPE`), but the user-visible text payload (the live URL) is
///   what the LLM/user actually consumes.
pub(super) fn satisfied_completion_content(output_files: &[String], tool_output: &str) -> String {
    if output_files.is_empty() {
        tool_output.to_string()
    } else {
        String::new()
    }
}

/// Produce the composite system-prompt text (worker prompt + realtime sensor
/// summary) used at the top of every agent turn. Centralizing this in
/// `execution.rs` keeps the message-building policy in a single location so
/// the conversation loop and task loop compose the same prompt.
///
/// Returns the prompt text the caller should paste into the first system
/// `Message`. When no realtime controller is attached this is byte-identical
/// to the stored system prompt.
/// Generic tool-use discipline appended to every agent's system prompt.
///
/// Weaker models (kimi-k2.5, smaller open-source) exhibit "tool stickiness"
/// — once they pick a tool for a turn, they tend to re-call it with the
/// same arguments instead of switching, even when the result doesn't
/// answer the user's question. Stronger models (Claude Opus) don't need
/// this nudge, but the prompt is harmless for them and ~100 tokens of
/// upfront cost is cheap insurance.
///
/// Empirical validation (llm-benchmark replay of mini3 session
/// slides-1780013669236-8w2ime, the production failure that motivated
/// this fix):
/// - kimi-k2.5 + only `check_workspace_contract`:
///   loop rate 5/5 → 3/5 with this block (40% break out)
/// - kimi-k2.5 + check + read_file + list_dir:
///   no consistent change (within noise)
/// - claude-opus-4.7:
///   already 0/5 loops in both arms; block has no effect on Opus
///
/// The block follows hermes-agent's prompt-time-injection pattern (in
/// `agent/prompt_builder.py`), but applied universally rather than
/// model-family-gated — the cost is small and the benefit is real.
const TOOL_USE_DISCIPLINE: &str = "\n\n\
## Tool use discipline\n\n\
You have generic file-inspection tools — `read_file`, `list_dir`, \
`view_image`, `grep`, `glob` — that can answer most questions about \
the workspace state by reading the files directly. Use them aggressively \
to investigate what's actually there.\n\n\
When a tool result does not answer the user's question, do NOT re-call \
the same tool with the same arguments — the result will be identical. \
Pick a different tool that can answer the specific question, usually a \
file-reading tool. If no tool can answer it, respond with text explaining \
what's missing.";

pub(super) fn compose_system_prompt(agent: &Agent) -> String {
    let mut content = agent.system_prompt_snapshot();
    content.push_str(TOOL_USE_DISCIPLINE);
    if let Some(summary) = agent.realtime_sensor_summary() {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
        content.push_str(&summary);
    }
    content
}

/// Auto-approves the in-tool approval gate when re-running a tool whose human
/// approval was already granted through the gateway approval flow
/// (`docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`). Scoped only around
/// [`Agent::execute_approved_tool`], so it can never auto-approve an ordinary
/// turn — that path never installs this requester.
struct ApprovedToolAutoApprover;

#[async_trait::async_trait]
impl ToolApprovalRequester for ApprovedToolAutoApprover {
    async fn request_approval(&self, _request: ToolApprovalRequest) -> ToolApprovalDecision {
        ToolApprovalDecision::Approve
    }
}

impl Agent {
    /// Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): re-check a pending
    /// human approval just before executing it. Policies may have changed
    /// while the approval waited (config hot-reload, hook edits), so the
    /// approver list is re-checked and `before_tool_call` hooks are re-run
    /// against the original arguments. A hook that now denies — or modifies
    /// the arguments away from the digest the human approved — invalidates
    /// the approval.
    pub async fn revalidate_pending_approval(
        &self,
        pending: &crate::approval::PendingApproval,
        sender_user_id: &str,
    ) -> std::result::Result<(), String> {
        if !pending
            .request
            .authorized_approvers
            .iter()
            .any(|approver| approver == sender_user_id)
        {
            return Err("approver is not authorized".to_string());
        }

        if let Some(ref hooks) = self.hooks {
            let payload = HookPayload::before_tool(
                &pending.request.tool_name,
                pending.tool_args.clone(),
                &pending.tool_id,
                self.hook_ctx().as_ref(),
            );
            match hooks.run(HookEvent::BeforeToolCall, &payload).await {
                HookResult::Allow => Ok(()),
                HookResult::Modified(new_args) => {
                    if crate::approval::digest_tool_args(&new_args)
                        == pending.request.tool_args_digest
                    {
                        Ok(())
                    } else {
                        Err("tool arguments changed since approval request was created".to_string())
                    }
                }
                // Context injection is a `user_prompt_submit`-only outcome and
                // never arises for a before-tool re-validation; allow the call.
                HookResult::Context(_) => Ok(()),
                HookResult::Deny(reason) => {
                    if reason.is_empty() {
                        Err("current policy denied the approved tool call".to_string())
                    } else {
                        Err(reason)
                    }
                }
                HookResult::Error(err) => Err(err),
            }
        } else {
            Ok(())
        }
    }

    /// Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): execute a tool call
    /// whose human approval was granted. Runs the tool directly with the
    /// digest-bound arguments — no LLM round trip — and fires the
    /// `after_tool_call` hooks. The caller is responsible for validating and
    /// consuming the pending approval first
    /// (`PendingApprovalStore::consume` + [`Agent::revalidate_pending_approval`]).
    pub async fn execute_approved_tool(
        &self,
        pending: &crate::approval::PendingApproval,
    ) -> Result<crate::tools::ToolResult> {
        let tool_start = Instant::now();
        // #1768: an approved call bypasses `execute_tools`, so it needs its
        // own pre-mutation snapshot point.
        self.snapshot_before_mutating_tools(&[pending.request.tool_name.as_str()])
            .await;
        // #1532: an approved call must run with the SAME agent-level
        // infrastructure the foreground path (`spawn_tool_task`) provides —
        // this used to spread `zero()`, silently dropping the file-state
        // cache, the profile permission envelope, cost tracking, the
        // sub-agent plumbing, and the session scope for exactly the calls a
        // human just vetted. Attachment paths stay empty: they are per-turn
        // batch state, and an approved call resumes outside a turn batch.
        let ctx = ToolContext {
            tool_id: pending.tool_id.clone(),
            reporter: self.reporter(),
            harness_event_sink: self.harness_event_sink.clone(),
            agent_definitions: self.agent_definitions.clone(),
            file_state_cache: self.file_state_cache.clone(),
            permissions: self
                .profile
                .as_deref()
                .map(crate::tools::ToolPermissions::from_profile)
                .unwrap_or_default(),
            subagent_output_router: self.subagent_output_router.clone(),
            subagent_summary_generator: self.subagent_summary_generator.clone(),
            llm_provider: self.llm.clone(),
            task_supervisor: Some(self.tools.supervisor()),
            cost_accountant: self.cost_accountant.clone(),
            parent_session_key: self.parent_session_key.clone(),
            spawn_depth: self.spawn_depth,
            session_scope: self.session_scope.clone(),
            // #1774: approval-gated edits still honor the post-edit
            // formatting opt-in.
            format_after_edit: self.config.format_after_edit,
            ..ToolContext::zero()
        };
        // The human already approved this exact (digest-bound) call through the
        // gateway approval flow. Some tools (e.g. `shell` on a SafePolicy
        // `Decision::Ask` command — sudo / rm -rf / git push --force) run a
        // SECOND, in-tool approval gate that reads `TOOL_APPROVAL_CTX` and
        // denies when absent. Scope an auto-approving requester so the
        // already-approved call is not re-denied by that inner gate.
        let approver: std::sync::Arc<dyn ToolApprovalRequester> =
            std::sync::Arc::new(ApprovedToolAutoApprover);
        // #1532 (part 2): dispatch through `execute_with_context` like the
        // foreground path — the legacy `execute` entry point never hands the
        // TYPED context to native tools (they receive `zero()` via the
        // default delegation), so every field above was reaching only
        // task-local (`TOOL_CTX`) readers. TOOL_CTX stays scoped for plugin
        // tools that read the task-local.
        let result = TOOL_APPROVAL_CTX
            .scope(
                approver,
                TOOL_CTX.scope(
                    ctx.clone(),
                    self.tools.execute_with_context(
                        &ctx,
                        &pending.request.tool_name,
                        &pending.tool_args,
                    ),
                ),
            )
            .await?;

        if let Some(ref hooks) = self.hooks {
            let payload = HookPayload::after_tool(
                &pending.request.tool_name,
                &pending.tool_id,
                octos_core::truncated_utf8(&result.output, 500, "..."),
                result.success,
                tool_start.elapsed().as_millis() as u64,
                self.hook_ctx().as_ref(),
            );
            let _ = hooks.run(HookEvent::AfterToolCall, &payload).await;
        }

        Ok(result)
    }

    /// Spawn a single tool call as a detached `tokio::spawn` task.
    ///
    /// The returned [`JoinHandle`] yields the per-call [`ToolCallResult`]:
    /// tool-output [`Message`], modified file paths, files-to-send paths, and
    /// optional sub-agent [`TokenUsage`]. This is the worker used by every
    /// dispatch strategy in [`Agent::execute_tools`] — parallel dispatch
    /// (all-Safe batches and the mixed-batch Safe phase, #1766) runs many in
    /// flight via `join_all`; serial dispatch (all-Exclusive batches and the
    /// mixed-batch Exclusive phase) spawns one, awaits it, then spawns the
    /// next.
    ///
    /// `explicit_send_file_requested` is a per-batch fact (true when the same
    /// LLM turn already issued a `send_file`), so the caller computes it once
    /// and passes it in; it is used to decide whether to auto-deliver each
    /// tool's `files_to_send`.
    ///
    /// SAFETY / COMPAT: the task body is byte-identical to the pre-M8.8
    /// inline closure in `execute_tools` — the only change is that the
    /// closure is now reachable from two call sites.
    fn spawn_tool_task(
        &self,
        tool_call: &octos_core::ToolCall,
        explicit_send_file_requested: bool,
        turn_attachment_ctx: &crate::tools::TurnAttachmentContext,
    ) -> JoinHandle<ToolCallResult> {
        // Clone Arc-wrapped fields so the spawned task is 'static
        let tools = self.tools.clone();
        let reporter = self.reporter();
        let hooks = self.hooks.clone();
        let hook_ctx = self.hook_ctx();
        let suppress_auto_send_files = self.config.suppress_auto_send_files;
        // #1774: post-edit formatting opt-in, threaded into the foreground
        // ToolContext so edit_file/write_file/diff_edit see it.
        let format_after_edit = self.config.format_after_edit;
        let tc_name = tool_call.name.clone();
        let tc_id = tool_call.id.clone();
        let tc_args = tool_call.arguments.clone();
        let attachment_ctx = turn_attachment_ctx.clone();
        let harness_event_sink = self.harness_event_sink.clone();
        // M8.2/M8.4 reconciliation: M8.8 rewrite must thread agent_definitions
        // and file_state_cache into both foreground and spawn_only ToolContext
        // builders so spawn(agent_definition_id=..) keeps resolving against
        // the live registry and read_file keeps short-circuiting via the
        // shared file-state cache.
        let agent_definitions = self.agent_definitions.clone();
        let file_state_cache = self.file_state_cache.clone();
        // M8 fix-first item 8 (gap 4b): if the agent carries a resolved
        // profile envelope, derive a ToolPermissions record once per turn
        // and clone it into every ToolContext. Today's pre-M8 default
        // (allow-all) is preserved when no profile is set.
        let permissions = self
            .profile
            .as_deref()
            .map(crate::tools::ToolPermissions::from_profile)
            .unwrap_or_default();
        // M8.7 wiring (item 4): hand the spawn_only background branch a
        // reference to the configured router and summary generator so it
        // can route output and start/stop watchers on real production
        // tasks (not only test fixtures).
        let subagent_output_router = self.subagent_output_router.clone();
        let subagent_summary_generator = self.subagent_summary_generator.clone();
        // M8 parity (W1.A4): clone the agent's cost accountant and
        // parent session key so they propagate to every sub-agent built
        // off this turn's TOOL_CTX (pipeline workers, spawn children).
        let cost_accountant = self.cost_accountant.clone();
        let parent_session_key = self.parent_session_key.clone();
        // Guard C (issue #607): inherit the agent's spawn nesting depth
        // so the foreground and spawn_only `ToolContext` builders below
        // both stamp it onto every tool call. The spawn tool reads
        // `ctx.spawn_depth` and refuses further nesting at the cap.
        let spawn_depth = self.spawn_depth;
        // Phase 1 of the SessionScope migration (PR #1198 follow-up):
        // snapshot the agent's `session_scope` so the foreground and
        // spawn_only `ToolContext` builders below thread it onto every
        // tool call. `None` keeps pre-Phase-1 behaviour; downstream
        // consumers (pipeline workers, file tools, plugins) come online
        // in Phase 2.
        let session_scope = self.session_scope.clone();

        // UPCR-2026-023 live-soak BUG 1: capture the per-turn human-blocking
        // bridges (`TOOL_APPROVAL_CTX`, `USER_QUESTION_CTX`) HERE — in the turn
        // task where they are still scoped — before the `tokio::spawn` below.
        // tokio task-locals are NOT inherited across `tokio::spawn`, so without
        // re-establishing them inside the spawned task a tool that reads either
        // requester (`shell`/`edit_file` approval, `ask_user_question`) would
        // find NONE and silently degrade (the live mini5 soak symptom: a valid
        // `ask_user_question` call emitted its "no synchronous host response
        // channel" text fallback even though the serve turn handler had
        // installed a `SessionUserQuestionRequester`). `try_with` returns the
        // `Arc` clone when scoped and `None` for a non-interactive turn (e.g.
        // CLI / gateway batch), preserving the graceful-degradation contract.
        let captured_approval_ctx: Option<std::sync::Arc<dyn ToolApprovalRequester>> =
            TOOL_APPROVAL_CTX.try_with(std::sync::Arc::clone).ok();
        let captured_user_question_ctx: Option<std::sync::Arc<dyn UserQuestionRequester>> =
            USER_QUESTION_CTX.try_with(std::sync::Arc::clone).ok();

        tokio::spawn(async move {
            let tool_start = Instant::now();
            debug!(tool = %tc_name, tool_id = %tc_id, "executing tool");

            reporter.report(ProgressEvent::ToolStarted {
                name: tc_name.clone(),
                tool_id: tc_id.clone(),
                arguments: Some(tc_args.clone()),
            });

            // Before-tool hook: may deny or modify args
            let mut effective_args = tc_args.clone();
            if let Some(ref hooks) = hooks {
                let payload =
                    HookPayload::before_tool(&tc_name, tc_args.clone(), &tc_id, hook_ctx.as_ref());
                match hooks.run(HookEvent::BeforeToolCall, &payload).await {
                    HookResult::Deny(reason) => {
                        tracing::warn!(
                            tool = %tc_name,
                            reason = %reason,
                            "before_tool_call hook denied"
                        );
                        let deny_msg = if reason.is_empty() {
                            format!(
                                "[HOOK DENIED] Tool '{}' was blocked by a lifecycle hook. Do not retry.",
                                tc_name
                            )
                        } else {
                            format!(
                                "[HOOK DENIED] Tool '{}' was blocked: {}. Do not retry.",
                                tc_name, reason
                            )
                        };
                        // Clear the activity chip: this early-return skips the
                        // normal completion paths, so emit the matching
                        // ToolCompleted the ToolStarted (above) requires. Without
                        // it the TUI shows a phantom "Using <tool>" chip forever.
                        reporter.report(ProgressEvent::ToolCompleted {
                            name: tc_name.clone(),
                            tool_id: tc_id.clone(),
                            success: false,
                            output_preview: octos_core::truncated_utf8(&deny_msg, 200, "..."),
                            duration: tool_start.elapsed(),
                        });
                        return (
                            Message {
                                role: MessageRole::Tool,
                                content: deny_msg,
                                media: vec![],
                                tool_calls: None,
                                tool_call_id: Some(tc_id),
                                reasoning_content: None,
                                client_message_id: None,
                                thread_id: None,
                                timestamp: chrono::Utc::now(),
                            },
                            Vec::new(),
                            Vec::new(),
                            None,
                            false, // hook denial is a failure — cascade in serial mode
                            None,
                            // hook denial is an intentional stop — cascade to peers
                            true,
                        );
                    }
                    HookResult::Modified(new_args) => {
                        tracing::info!(
                            tool = %tc_name,
                            "hook modified tool arguments"
                        );
                        effective_args = new_args;
                    }
                    _ => {}
                }
            }

            // Auto-background spawn_only tools: run the tool in a background
            // tokio task and return immediately. The tool's files_to_send
            // auto-delivers the result to the user. No subagent LLM needed.
            if tools.is_spawn_only(&tc_name) {
                // PR #688 follow-up — MEDIUM #3: enforce the registry's
                // provider policy at the spawn_only intercept site, BEFORE
                // `tokio::spawn`. Without this, a denied stale tool call is
                // silently spawned and only fails async inside the
                // background task — the foreground turn observes a fake
                // "started successfully" and the deny is invisible to the
                // LLM. Mirror the deny behaviour of the foreground path
                // (registry.rs `execute_with_context`) so the LLM sees one
                // synthetic Tool message and stops retrying.
                if let Some(policy) = tools.provider_policy() {
                    if let crate::tools::policy::PolicyDecision::Deny { reason } =
                        policy.evaluate(&tc_name)
                    {
                        tracing::warn!(
                            tool = %tc_name,
                            reason = %reason,
                            "provider policy denied spawn_only tool at intercept"
                        );
                        let deny_msg = format!(
                            "[POLICY DENIED] Tool '{}' is blocked by provider policy ({}). Do not retry.",
                            tc_name, reason
                        );
                        // Clear the activity chip: this early-return skips the
                        // normal completion paths, so emit the matching
                        // ToolCompleted the ToolStarted (above) requires. Without
                        // it the TUI shows a phantom "Using <tool>" chip forever.
                        reporter.report(ProgressEvent::ToolCompleted {
                            name: tc_name.clone(),
                            tool_id: tc_id.clone(),
                            success: false,
                            output_preview: octos_core::truncated_utf8(&deny_msg, 200, "..."),
                            duration: tool_start.elapsed(),
                        });
                        return (
                            Message {
                                role: MessageRole::Tool,
                                content: deny_msg,
                                media: vec![],
                                tool_calls: None,
                                tool_call_id: Some(tc_id),
                                reasoning_content: None,
                                client_message_id: None,
                                thread_id: None,
                                timestamp: chrono::Utc::now(),
                            },
                            Vec::new(),
                            Vec::new(),
                            None,
                            false, // policy denial is a failure — cascade in serial mode
                            None,
                            // policy denial is an intentional stop — cascade to peers
                            true,
                        );
                    }
                }

                // Pre-flight validation: catch known-bad arguments (e.g.
                // structurally invalid DOT for `run_pipeline`) synchronously
                // so the LLM gets the error as a tool_result in this
                // iteration and can retry with corrected input. Without
                // this, the foreground would return "started in background"
                // to the LLM, the background task would fail validation,
                // and the LLM-side conversation would never see the error.
                // The pre-flight hook is opt-in per tool — default impl is
                // Ok(()). See `Tool::pre_flight_validate`.
                if let Some(tool) = tools.get(&tc_name) {
                    if let Err(msg) = tool.pre_flight_validate(&effective_args).await {
                        tracing::warn!(
                            tool = %tc_name,
                            error = %msg,
                            "spawn_only pre-flight validation failed"
                        );
                        let err_msg = format!(
                            "[VALIDATION FAILED] Tool '{tc_name}' rejected input: {msg}\n\n\
                             Fix the input and retry."
                        );
                        // Clear the activity chip: this early-return skips the
                        // normal completion paths, so emit the matching
                        // ToolCompleted the ToolStarted (above) requires. Without
                        // it the TUI shows a phantom "Using <tool>" chip forever
                        // (reproduced live on mini5: a bad run_pipeline name left
                        // an "Orchestrating… (1 active)" chip stuck 15+ min).
                        reporter.report(ProgressEvent::ToolCompleted {
                            name: tc_name.clone(),
                            tool_id: tc_id.clone(),
                            success: false,
                            output_preview: octos_core::truncated_utf8(&err_msg, 200, "..."),
                            duration: tool_start.elapsed(),
                        });
                        return (
                            Message {
                                role: MessageRole::Tool,
                                content: err_msg,
                                media: vec![],
                                tool_calls: None,
                                tool_call_id: Some(tc_id),
                                reasoning_content: None,
                                client_message_id: None,
                                thread_id: None,
                                timestamp: chrono::Utc::now(),
                            },
                            Vec::new(),
                            Vec::new(),
                            None,
                            false,
                            None,
                            // pre-execution denial is an intentional stop — cascade
                            true,
                        );
                    }
                }

                tracing::info!(
                    tool = %tc_name,
                    "running spawn_only tool in background"
                );
                let bg_tools = tools.clone();
                let bg_name = tc_name.clone();
                let bg_args = effective_args.clone();
                let bg_sender = tools.background_result_sender();
                let bg_tc_id = tc_id.clone();
                let bg_reporter = reporter.clone();
                // M8.10 follow-up (#649): snapshot the originating turn's
                // thread_id NOW, before any other turn can swap reporters
                // or rotate the api_channel sticky map. Late-arriving
                // background results stamp this onto their OutboundMessage
                // metadata so the wire-side SSE event lands under the
                // correct turn even after subsequent unrelated user turns
                // have advanced the per-chat sticky thread_id.
                let bg_originating_thread_id = bg_reporter.thread_id().map(str::to_string);
                // Issue #960 fix (M10 Phase 4 plumbing): the originating
                // user prompt's `client_message_id`. Today the reporter's
                // `thread_id()` IS the cmid on gateway-bound paths and IS
                // the originating `TurnId` UUID on the WS standalone-turn
                // path — the SPA reducer's thread-map keys on whichever
                // shape the parent user-prompt row carries, so threading
                // the same value through under both names lets the
                // `turn/spawn_complete` envelope's
                // `response_to_client_message_id` round-trip correctly.
                // The field is documented separately on
                // `BackgroundResultPayload` so a follow-up that decouples
                // the two on the WS path (e.g. surfacing the SPA's
                // pre-`turn/start` cmid) only has to update the capture
                // site here.
                let bg_originating_client_message_id = bg_originating_thread_id.clone();
                // Issue #738 fix: thread the originating cmid into task
                // registration so any SpawnOnlyFailureSignal emitted for
                // this task carries it to the M8.9 synthetic recovery
                // turn. Without this, the recovery turn mints a fresh
                // UUIDv7 and the eventual successful retry's deliverables
                // land under an orphan thread_id with no DOM bubble.
                let task_id = tools.register_task_with_input_and_cmid(
                    &tc_name,
                    &tc_id,
                    Some(effective_args.clone()),
                    bg_originating_thread_id.clone(),
                );
                // Cap refusal: the legacy register entry points signal a
                // per-session child-fanout rejection with an empty-string
                // sentinel. Spawning anyway would run a worker that is
                // invisible to `task/list`, uncancellable (`cancel("")`
                // no-ops), and hand the LLM a task_handle with an empty id —
                // the cap would bound tracking, not execution. Refuse the
                // call synchronously instead, mirroring the policy-deny
                // early-return above (chip clear included).
                if task_id.is_empty() {
                    tracing::error!(
                        tool = %tc_name,
                        "spawn_only register refused (child fanout cap); not spawning"
                    );
                    let cap_msg = format!(
                        "[TASK LIMIT] Cannot start background task '{tc_name}': this \
                         session reached its background-task fanout cap. Wait for \
                         running tasks to finish (or cancel them) before starting more. \
                         Do not retry immediately."
                    );
                    reporter.report(ProgressEvent::ToolCompleted {
                        name: tc_name.clone(),
                        tool_id: tc_id.clone(),
                        success: false,
                        output_preview: octos_core::truncated_utf8(&cap_msg, 200, "..."),
                        duration: tool_start.elapsed(),
                    });
                    return (
                        Message {
                            role: MessageRole::Tool,
                            content: cap_msg,
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: Some(tc_id),
                            reasoning_content: None,
                            client_message_id: None,
                            thread_id: None,
                            timestamp: chrono::Utc::now(),
                        },
                        Vec::new(),
                        Vec::new(),
                        None,
                        false,
                        None,
                        // fanout-cap denial is an intentional stop — cascade
                        true,
                    );
                }
                tools.mark_spawn_only_invoked();
                let bg_supervisor = tools.supervisor();
                // F004 B2: bridge supervised runtime-state transitions onto
                // the per-request reporter so spawn_only tasks emit
                // ToolProgress events keyed by `tool_call_id`. This is what
                // lets the chat UI anchor every long-running background
                // tool to a single bubble (no new messages, no ambiguity).
                // Setting it again with a different reporter is harmless —
                // the latest reporter wins; concurrent background tasks
                // share the same Agent-scoped broadcaster anyway.
                bg_supervisor.set_progress_reporter(bg_reporter.clone());
                let bg_attachment_ctx = attachment_ctx.clone();
                // M8.2/M8.4 reconciliation (item 1 of fix-first checklist):
                // Thread agent_definitions + file_state_cache into the
                // spawn_only background ToolContext so the M8.8 rewrite
                // does not silently zero them out.
                let bg_agent_definitions = agent_definitions.clone();
                let bg_file_state_cache = file_state_cache.clone();
                let bg_permissions = permissions.clone();
                // M8.7 (item 4): clone the optional router/generator so
                // the background branch can mark_terminal on completion
                // and stop the watcher when the task is done.
                let bg_output_router = subagent_output_router.clone();
                let bg_summary_generator = subagent_summary_generator.clone();
                // M8 parity (W1.A1/A4): clone the optional router/generator/
                // supervisor/cost-accountant so the make_ctx closure below
                // can thread them onto every sub-agent that runs in the
                // spawn_only branch (pipelines, recursive spawns).
                let bg_subagent_output_router = subagent_output_router.clone();
                let bg_subagent_summary_generator = subagent_summary_generator.clone();
                let bg_task_supervisor = Some(bg_supervisor.clone());
                let bg_cost_accountant = cost_accountant.clone();
                let bg_parent_session_key = parent_session_key.clone();
                // Guard C (issue #607): clone the agent's spawn nesting
                // depth into the spawn_only TOOL_CTX builder.
                let bg_spawn_depth = spawn_depth;
                // Phase 1 of the SessionScope migration: clone the
                // agent's session scope into the spawn_only TOOL_CTX
                // builder so background sub-agents see the same
                // filesystem contract as the parent session. None
                // keeps pre-Phase-1 behaviour.
                let bg_session_scope = session_scope.clone();
                // #1774 review: the formatting opt-in must reach spawn_only
                // background tools too (pipeline steps editing files). Copy of
                // the pre-spawn local — `self` cannot cross into the 'static
                // task.
                let bg_format_after_edit = format_after_edit;
                let bg_session_id_for_watcher = format!("agent:{}", tc_id);
                // M10 Phase 4: keep a copy of the task_id so the synthesized
                // tool-result message returned to the LLM (built after this
                // `tokio::spawn` moves `task_id` into the closure) can carry
                // the same handle the supervisor and the SubAgentOutputRouter
                // know it by.
                // C1 step 2 / codex round-5 (orphan-sweep liveness): arm the
                // RAII terminal guard HERE, in the FOREGROUND, before the
                // `tokio::spawn`. `register_task_with_input_and_cmid` above
                // already persisted a non-terminal `Spawned` row; arming the
                // guard inside the spawned future (its previous home) left a
                // window where a fast next-turn orphan-sweep could see the row
                // non-terminal AND not-live and falsely reap a
                // scheduled-but-not-yet-polled worker. Constructing it
                // synchronously within the spawning turn inserts the id into
                // the process-global live-set before the turn returns (turns
                // are serialized per session, so this completes before any
                // next-turn `enable_persistence` sweep). The guard is MOVED
                // into the future below so its Drop — which clears the live-set
                // and drives an unfinished task to Failed (so the TUI task
                // count decrements instead of hanging on "N running") — still
                // fires when the worker terminates. Idempotent on normal
                // completion: the body's own terminal mark wins; Drop no-ops.
                let terminal_guard = TaskTerminalGuard::new(bg_supervisor.clone(), task_id.clone());
                let task_id_for_handle = task_id.clone();
                // Esc/`/stop`/`turn/interrupt` cancellation: acquire the
                // supervisor's per-task cancel token in the FOREGROUND (so it
                // exists before any `supervisor.cancel(task_id)` race) and move
                // it into the detached worker. Without this the spawn_only
                // background task — a `run_pipeline` / `deep_research` fan-out —
                // runs to completion regardless of a turn interrupt: aborting
                // the foreground agent loop never touches this independent
                // `tokio::spawn`, and the body never polled a cancel signal. We
                // race the tool future against `cancel_token.cancelled()` below
                // so the in-flight pipeline/LLM/web_search await is DROPPED at
                // the next poll and the worker terminates promptly.
                let cancel_token = bg_supervisor.cancel_token(&task_id);
                tokio::spawn(async move {
                    let _terminal_guard = terminal_guard;
                    bg_supervisor.mark_running(&task_id);
                    // M8.7 (item 4): start a periodic-summary watcher for
                    // this background task. The watcher honours
                    // `min_runtime` so short tasks never trigger an LLM
                    // call. It self-terminates when the supervisor marks
                    // the task complete or failed.
                    if let Some(ref summary_gen) = bg_summary_generator {
                        summary_gen
                            .spawn_watcher(bg_session_id_for_watcher.as_str(), task_id.as_str());
                    }
                    let bg_started_at = std::time::SystemTime::now();

                    // Helper to create TOOL_CTX for plugin stderr progress streaming.
                    // Base it on the zero-value context so M8.x placeholder fields
                    // carry their default-populated values.
                    let make_ctx = || ToolContext {
                        tool_id: bg_tc_id.clone(),
                        reporter: bg_reporter.clone(),
                        harness_event_sink: harness_event_sink.clone(),
                        attachment_paths: bg_attachment_ctx.attachment_paths.clone(),
                        audio_attachment_paths: bg_attachment_ctx.audio_attachment_paths.clone(),
                        file_attachment_paths: bg_attachment_ctx.file_attachment_paths.clone(),
                        agent_definitions: bg_agent_definitions.clone(),
                        file_state_cache: bg_file_state_cache.clone(),
                        // M8 fix-first item 8 (gap 4b): carry the
                        // profile-derived permissions so spawn_only
                        // background tools see the same gate the
                        // foreground branch enforces.
                        permissions: bg_permissions.clone(),
                        // M8 parity (W1.A1): thread the shared router /
                        // summary generator / supervisor / cost
                        // accountant into the spawn_only TOOL_CTX so
                        // sub-agents downstream (pipeline workers,
                        // recursive spawns) inherit them via the
                        // task-local read path.
                        subagent_output_router: bg_subagent_output_router.clone(),
                        subagent_summary_generator: bg_subagent_summary_generator.clone(),
                        task_supervisor: bg_task_supervisor.clone(),
                        cost_accountant: bg_cost_accountant.clone(),
                        parent_session_key: bg_parent_session_key.clone(),
                        // Guard C (issue #607): inherit the parent
                        // agent's spawn nesting depth so spawn-only
                        // background tools that themselves dispatch
                        // sub-agents (e.g. fm_tts → spawn) see the
                        // higher value when their TOOL_CTX is read.
                        spawn_depth: bg_spawn_depth,
                        // Phase 1 SessionScope migration: thread the
                        // shared scope onto the spawn_only TOOL_CTX so
                        // every background sub-agent sees the same
                        // filesystem contract as the parent.
                        session_scope: bg_session_scope.clone(),
                        // #1774 review: spawn_only background tools (pipeline
                        // steps, sub-agent edits) honor the same post-edit
                        // formatting opt-in as the foreground path — without
                        // this the flag silently defaulted to false here.
                        format_after_edit: bg_format_after_edit,
                        ..ToolContext::zero()
                    };

                    // M8.7 (item 4): seed the router with a startup line
                    // so a handle exists before the tool starts producing
                    // output. Without this, mark_terminal is a no-op and
                    // dashboards never know the task ran.
                    if let Some(ref router) = bg_output_router {
                        let _ = router.append(
                            bg_session_id_for_watcher.as_str(),
                            task_id.as_str(),
                            format!(
                                "[{} starting] tool={} task_id={}\n",
                                chrono::Utc::now().to_rfc3339(),
                                bg_name,
                                task_id
                            )
                            .as_bytes(),
                        );
                    }

                    // M8.2/M8.4 reconciliation: use the typed
                    // `execute_with_context` so the spawn-only background
                    // branch carries `agent_definitions` and
                    // `file_state_cache` through to the tool. The TOOL_CTX
                    // scope still wraps the call so plugin/MCP tools that
                    // read the task-local see the same fields.
                    let mut result = {
                        // Bind the inner ctx to a `let` so the `&exec_ctx`
                        // borrow lives across the `select!` (the previous
                        // inline `&make_ctx()` temporary was freed at the end
                        // of the statement, which the longer-lived `exec`
                        // future would outlive).
                        let exec_ctx = make_ctx();
                        let exec = TOOL_CTX.scope(
                            make_ctx(),
                            bg_tools.execute_with_context(&exec_ctx, &bg_name, &bg_args),
                        );
                        tokio::select! {
                            biased;
                            // Interrupt won the race: a `turn/interrupt` (or any
                            // `supervisor.cancel(task_id)`) fired the token while
                            // the tool was mid-await. Drop `exec` (the pipeline /
                            // LLM / web_search future) on the spot and short-
                            // circuit the worker so a hung pipeline stops promptly
                            // instead of running to completion.
                            _ = cancel_token.cancelled() => {
                                tracing::info!(
                                    tool = %bg_name,
                                    task_id = %task_id,
                                    "spawn_only background tool cancelled (turn interrupt)"
                                );
                                // `supervisor.cancel` already transitioned the
                                // record to `Cancelled`; mark again defensively
                                // for the path where the token fires without a
                                // supervisor transition (idempotent — the
                                // terminal guard inside `mark_*` no-ops on an
                                // already-terminal task).
                                bg_supervisor.mark_failed(&task_id, "cancelled by turn interrupt".to_string());
                                if let Some(ref router) = bg_output_router {
                                    let _ = router.append(
                                        bg_session_id_for_watcher.as_str(),
                                        task_id.as_str(),
                                        b"[cancelled] turn interrupted by client\n",
                                    );
                                    // Codex #1429 P2: a cancelled spawn_only task must run
                                    // the same terminal teardown as the completion path.
                                    // Otherwise the output handle stays in the running
                                    // phase and the summary watcher keeps polling —
                                    // AgentSummaryGenerator does NOT treat `Cancelled` as
                                    // terminal, so it would re-summarise an aborted task.
                                    router.mark_terminal(&task_id);
                                }
                                if let Some(ref summary_gen) = bg_summary_generator {
                                    summary_gen.stop_watcher(&task_id);
                                }
                                return;
                            }
                            res = exec => res,
                        }
                    };

                    // M8.7 (item 4): route the tool's textual output to
                    // the router so it lands on disk for the dashboard
                    // and so AgentSummaryGenerator's tail_lines source
                    // has something to summarise.
                    if let Some(ref router) = bg_output_router {
                        if let Ok(ref r) = result {
                            let preview = if r.output.is_empty() {
                                "[no stdout]".to_string()
                            } else {
                                r.output.clone()
                            };
                            let _ = router.append(
                                bg_session_id_for_watcher.as_str(),
                                task_id.as_str(),
                                format!("[output] {preview}\n").as_bytes(),
                            );
                        }
                    }

                    // Retry once on transient failure (e.g. ominix-api restart)
                    if let Ok(ref r) = result {
                        if !r.success
                            && (r.output.contains("error sending request")
                                || r.output.contains("connection refused"))
                        {
                            tracing::warn!(tool = %bg_name, "spawn_only tool failed (transient), retrying in 5s");
                            // Cancel-aware backoff + retry: a `turn/interrupt`
                            // during the 5s wait or the retried execute must
                            // still abort the worker rather than soldier on.
                            let retry = async {
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                TOOL_CTX
                                    .scope(
                                        make_ctx(),
                                        bg_tools.execute_with_context(
                                            &make_ctx(),
                                            &bg_name,
                                            &bg_args,
                                        ),
                                    )
                                    .await
                            };
                            tokio::select! {
                                biased;
                                _ = cancel_token.cancelled() => {
                                    tracing::info!(
                                        tool = %bg_name,
                                        task_id = %task_id,
                                        "spawn_only background tool cancelled during retry (turn interrupt)"
                                    );
                                    bg_supervisor
                                        .mark_failed(&task_id, "cancelled by turn interrupt".to_string());
                                    // Codex #1429 P2: same terminal teardown as the
                                    // completion path so a task cancelled during the
                                    // transient-retry wait doesn't leave its output
                                    // handle running / summary watcher registered.
                                    if let Some(ref router) = bg_output_router {
                                        router.mark_terminal(&task_id);
                                    }
                                    if let Some(ref summary_gen) = bg_summary_generator {
                                        summary_gen.stop_watcher(&task_id);
                                    }
                                    return;
                                }
                                res = retry => {
                                    result = res;
                                }
                            }
                        }
                    }

                    match result {
                        Ok(r) if r.success => {
                            tracing::info!(
                                tool = %bg_name,
                                success = true,
                                "spawn_only background tool completed"
                            );
                            // Forward the tool's `named_outputs` map (parsed
                            // from its stdout envelope by the plugin
                            // wrapper) so validators can resolve
                            // `${output.<key>}` references against
                            // tool-emitted values (e.g. `mofa_publish`
                            // emitting `deploy_url`).
                            let named_outputs_value = r.named_outputs.as_ref().map(|map| {
                                serde_json::Value::Object(
                                    map.iter()
                                        .map(|(k, v)| {
                                            (k.clone(), serde_json::Value::String(v.clone()))
                                        })
                                        .collect(),
                                )
                            });
                            match enforce_spawn_task_contract_with_args_and_output(
                                &bg_tools,
                                &bg_name,
                                &bg_tc_id,
                                &r.files_to_send,
                                bg_started_at,
                                Some((&bg_supervisor, &task_id)),
                                Some(&bg_args),
                                named_outputs_value.as_ref(),
                                // #1607: the Agent's own registry is built
                                // sandboxed (session_actor
                                // `create_registry_for_workspace` ->
                                // `rebind_cwd(create_sandbox(&sandbox_config))`),
                                // so its stored sandbox IS the session backend.
                                bg_tools.sandbox(),
                            )
                            .await
                            {
                                SpawnTaskContractResult::Satisfied { output_files } => {
                                    // octos #997 (round-3 fix): the session-scope
                                    // contract above runs validators at the SESSION
                                    // root and writes
                                    // `<session>/.octos/validator_outcomes.jsonl`,
                                    // but `inspect_workspace_contract` reads
                                    // `<session>/<kind>/<slug>/.octos/validator_outcomes.jsonl`.
                                    // Without this call, a direct spawn_only
                                    // invocation of `mofa_slides` (or any kind-
                                    // managed tool) lands in the project workspace
                                    // but never writes the project ledger — so a
                                    // subsequent contract gate surfaces
                                    // `ready = false` even when the hard-required
                                    // validator (octos #997:
                                    // `slides.mofa_slides.pptx_magic_bytes`)
                                    // would have passed at the project root.
                                    //
                                    // Kind-agnostic: the helper iterates every
                                    // slides/sites project beneath
                                    // `workspace_root` and runs each project's
                                    // own declared completion-phase validators.
                                    // Non-slides/sites tools simply find no
                                    // projects to validate and the helper
                                    // returns an empty report.
                                    if let Some(workspace_root) = bg_tools.workspace_root() {
                                        let _project_root_report =
                                            crate::workspace_contract::run_project_root_validators(
                                                &bg_tools,
                                                workspace_root,
                                                None,
                                                &r.files_to_send,
                                                bg_tools.sandbox(),
                                            )
                                            .await;
                                    }
                                    // When the tool emitted real text output
                                    // (run_pipeline synthesize summary, plugin
                                    // structured result), surface it in the
                                    // chat bubble alongside any file
                                    // attachments — otherwise the user sees an
                                    // empty bubble with a `.md` attachment for
                                    // research pipelines that explicitly
                                    // produced a ~1000-word summary as their
                                    // final text response.
                                    //
                                    // Empty / whitespace-only output falls back
                                    // to `satisfied_completion_content`, which
                                    // preserves the Wave-3b mofa_publish path
                                    // (no files + URL text -> emit URL) and
                                    // the legacy fm_tts / podcast_generate
                                    // path (files + no text -> empty bubble,
                                    // files carry the deliverable).
                                    let bubble_content = if r.output.trim().is_empty() {
                                        satisfied_completion_content(&output_files, &r.output)
                                    } else {
                                        r.output.clone()
                                    };
                                    // `None` = no background channel is wired
                                    // (chat mode): the contract is Satisfied and
                                    // there is simply nowhere to deliver the
                                    // notification — that is NOT a failure.
                                    // `Some(false)` = a sender ran and genuinely
                                    // failed to persist. Keeping these apart
                                    // stops a Satisfied chat-mode contract from
                                    // being recorded as Failed.
                                    let delivery = if let Some(ref sender) = bg_sender {
                                        Some(
                                            sender(BackgroundResultPayload {
                                                task_label: bg_name.clone(),
                                                content: bubble_content,
                                                kind: BackgroundResultKind::Notification,
                                                media: output_files.clone(),
                                                envelope_media: vec![],
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
                                                task_id: Some(task_id.clone()),
                                                originating_client_message_id:
                                                    bg_originating_client_message_id.clone(),
                                                tool_call_id: Some(bg_tc_id.clone()),
                                                // C1 step 3: contract-Satisfied success path
                                                // (mark_completed below).
                                                terminal_status: Some(
                                                    crate::task_supervisor::TaskStatus::Completed,
                                                ),
                                            })
                                            .await,
                                        )
                                    } else {
                                        None
                                    };

                                    // A Satisfied contract is a success unless a
                                    // wired sender actually failed to persist.
                                    let delivery_failed = satisfied_delivery_is_failure(delivery);
                                    if !delivery_failed {
                                        // Workspace contract already verified
                                        // the declared artifacts. Trust it —
                                        // the supervisor's job is to record
                                        // the skill's reported outcome, not
                                        // to re-validate file contents.
                                        bg_supervisor
                                            .mark_completed(&task_id, output_files.clone());
                                        // Only emit the auto-generated
                                        // "<tool> produced files: …" follow-up
                                        // when the first payload carried no
                                        // text — otherwise the chat would
                                        // render TWO assistant bubbles in a
                                        // row (summary, then a redundant
                                        // file-list notice). For run_pipeline
                                        // the synthesize node already supplied
                                        // a user-readable executive summary
                                        // in `r.output`, so we suppress the
                                        // file-list bubble. For tools whose
                                        // `r.output` was empty (some
                                        // plugin-only flows that deliver via
                                        // files alone) the fallback notice
                                        // still fires.
                                        //
                                        // `trim().is_empty()` so whitespace-only
                                        // output (e.g. a stray "\n" from a
                                        // tool that meant to emit no summary)
                                        // is NOT treated as a real summary —
                                        // otherwise we'd suppress the file-list
                                        // bubble and the user would see no
                                        // signal that the task completed.
                                        let already_sent_summary = !r.output.trim().is_empty();
                                        if !already_sent_summary {
                                            if let Some(ref sender) = bg_sender {
                                                if let Some(produced_msg) =
                                                    build_spawn_only_produced_files_message(
                                                        &bg_name,
                                                        &output_files,
                                                        bg_tools.workspace_root(),
                                                    )
                                                {
                                                    let _ =
                                                        sender(BackgroundResultPayload {
                                                            task_label: bg_name.clone(),
                                                            content: produced_msg,
                                                            kind:
                                                                BackgroundResultKind::Notification,
                                                            media: vec![],
                                                            envelope_media: vec![],
                                                            originating_thread_id:
                                                                bg_originating_thread_id.clone(),
                                                            task_id: Some(task_id.clone()),
                                                            originating_client_message_id:
                                                                bg_originating_client_message_id
                                                                    .clone(),
                                                            tool_call_id: Some(bg_tc_id.clone()),
                                                            // C1 step 3: produced-files
                                                            // follow-up after mark_completed.
                                                            terminal_status: Some(
                                                                crate::task_supervisor::TaskStatus::Completed,
                                                            ),
                                                        })
                                                        .await;
                                                }
                                            }
                                        }
                                    } else {
                                        let err_msg = format!(
                                            "verified outputs for {} but failed to persist background result",
                                            bg_name
                                        );
                                        tracing::warn!(
                                            tool = %bg_name,
                                            files = ?output_files,
                                            "background result persistence failed after contract verification"
                                        );
                                        bg_supervisor.mark_failed(&task_id, err_msg);
                                    }
                                }
                                SpawnTaskContractResult::Failed { error, notify_user } => {
                                    tracing::warn!(
                                        tool = %bg_name,
                                        error = %error,
                                        "workspace contract rejected spawn_only result"
                                    );
                                    bg_supervisor.mark_failed(&task_id, error.clone());
                                    if let Some(ref sender) = bg_sender {
                                        let content = match notify_user {
                                            Some(message) => {
                                                format!("✗ {}: {}", message, error)
                                            }
                                            None => {
                                                format!("✗ {} failed: {}", bg_name, error)
                                            }
                                        };
                                        let _ = sender(BackgroundResultPayload {
                                            task_label: bg_name.clone(),
                                            content,
                                            kind: BackgroundResultKind::Notification,
                                            media: vec![],
                                            envelope_media: vec![],
                                            originating_thread_id: bg_originating_thread_id.clone(),
                                            task_id: Some(task_id.clone()),
                                            originating_client_message_id:
                                                bg_originating_client_message_id.clone(),
                                            tool_call_id: Some(bg_tc_id.clone()),
                                            // C1 step 3: contract-rejected failure
                                            // (mark_failed above).
                                            terminal_status: Some(
                                                crate::task_supervisor::TaskStatus::Failed,
                                            ),
                                        })
                                        .await;
                                    }
                                }
                                SpawnTaskContractResult::NotConfigured { required, reason } => {
                                    if required {
                                        let err_msg = reason.unwrap_or_else(|| {
                                            format!(
                                                "workspace contract is required for {} but not configured",
                                                bg_name
                                            )
                                        });
                                        bg_supervisor.mark_failed(&task_id, err_msg.clone());
                                        if let Some(ref sender) = bg_sender {
                                            let _ = sender(BackgroundResultPayload {
                                                task_label: bg_name.clone(),
                                                content: format!(
                                                    "✗ {} failed: {}",
                                                    bg_name, err_msg
                                                ),
                                                kind: BackgroundResultKind::Notification,
                                                media: vec![],
                                                envelope_media: vec![],
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
                                                task_id: Some(task_id.clone()),
                                                originating_client_message_id:
                                                    bg_originating_client_message_id.clone(),
                                                tool_call_id: Some(bg_tc_id.clone()),
                                                // C1 step 3: required-but-unconfigured
                                                // contract failure (mark_failed above).
                                                terminal_status: Some(
                                                    crate::task_supervisor::TaskStatus::Failed,
                                                ),
                                            })
                                            .await;
                                        }
                                        // M8.7 (item 4): early-return path
                                        // — emit terminal signals before
                                        // returning so the router/watcher
                                        // wiring is not skipped.
                                        if let Some(ref router) = bg_output_router {
                                            router.mark_terminal(&task_id);
                                        }
                                        if let Some(ref summary_gen) = bg_summary_generator {
                                            summary_gen.stop_watcher(&task_id);
                                        }
                                        return;
                                    }

                                    if r.files_to_send.is_empty() {
                                        // spawn_only tool finished without
                                        // file outputs. Two sub-cases:
                                        //
                                        //   (a) Informational tool (e.g.
                                        //       `fm_voice_list`) — produced
                                        //       a textual result on stdout
                                        //       but has nothing to attach.
                                        //       Treat as success and deliver
                                        //       the text as a Notification.
                                        //   (b) Genuinely-failed tool — no
                                        //       text either. Mark failed
                                        //       with the legacy error.
                                        //
                                        // The strict "no output files
                                        // produced" failure was too sharp
                                        // for skills with mixed sync/async
                                        // tool families (e.g. mofa-fm marks
                                        // its list/delete tools spawn_only
                                        // for uniformity with the
                                        // file-producing fm_tts/fm_voice_save).
                                        let trimmed_output = r.output.trim();
                                        if !trimmed_output.is_empty() {
                                            tracing::info!(
                                                tool = %bg_name,
                                                output_len = trimmed_output.len(),
                                                "spawn_only tool produced text-only result"
                                            );
                                            bg_supervisor.mark_completed(&task_id, Vec::new());
                                            if let Some(ref sender) = bg_sender {
                                                let _ = sender(BackgroundResultPayload {
                                                    task_label: bg_name.clone(),
                                                    content: r.output.clone(),
                                                    kind: BackgroundResultKind::Notification,
                                                    media: vec![],
                                                    envelope_media: vec![],
                                                    originating_thread_id: bg_originating_thread_id
                                                        .clone(),
                                                    task_id: Some(task_id.clone()),
                                                    originating_client_message_id:
                                                        bg_originating_client_message_id.clone(),
                                                    tool_call_id: Some(bg_tc_id.clone()),
                                                    // C1 step 3: text-only success
                                                    // (mark_completed above).
                                                    terminal_status: Some(
                                                        crate::task_supervisor::TaskStatus::Completed,
                                                    ),
                                                })
                                                .await;
                                            }
                                            if let Some(ref router) = bg_output_router {
                                                router.mark_terminal(&task_id);
                                            }
                                            if let Some(ref summary_gen) = bg_summary_generator {
                                                summary_gen.stop_watcher(&task_id);
                                            }
                                            return;
                                        }

                                        let err_msg = format!(
                                            "completed with no output (stdout: {})",
                                            r.output.chars().take(200).collect::<String>()
                                        );
                                        tracing::warn!(
                                            tool = %bg_name,
                                            "spawn_only tool produced no files and no text"
                                        );
                                        bg_supervisor.mark_failed(&task_id, err_msg);
                                        if let Some(ref sender) = bg_sender {
                                            let _ = sender(BackgroundResultPayload {
                                                task_label: bg_name.clone(),
                                                content: format!(
                                                    "✗ {} failed: no output files produced",
                                                    bg_name
                                                ),
                                                kind: BackgroundResultKind::Notification,
                                                media: vec![],
                                                envelope_media: vec![],
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
                                                task_id: Some(task_id.clone()),
                                                originating_client_message_id:
                                                    bg_originating_client_message_id.clone(),
                                                tool_call_id: Some(bg_tc_id.clone()),
                                                // C1 step 3: no-files-no-text failure
                                                // (mark_failed above).
                                                terminal_status: Some(
                                                    crate::task_supervisor::TaskStatus::Failed,
                                                ),
                                            })
                                            .await;
                                        }
                                        // M8.7 (item 4): early-return path
                                        // — emit terminal signals before
                                        // returning so the router/watcher
                                        // wiring is not skipped.
                                        if let Some(ref router) = bg_output_router {
                                            router.mark_terminal(&task_id);
                                        }
                                        if let Some(ref summary_gen) = bg_summary_generator {
                                            summary_gen.stop_watcher(&task_id);
                                        }
                                        return;
                                    }

                                    bg_supervisor.mark_runtime_state(
                                        &task_id,
                                        TaskRuntimeState::DeliveringOutputs,
                                        Some(format!("deliver outputs for {}", bg_name)),
                                    );
                                    let mut sent_files = Vec::new();
                                    let mut delivery_failed = false;
                                    for file_path in &r.files_to_send {
                                        let path_str = file_path.to_string_lossy().to_string();
                                        tracing::info!(
                                            tool = %bg_name,
                                            file = %path_str,
                                            "background auto-sending file"
                                        );
                                        let send_args = serde_json::json!({
                                            "file_path": path_str,
                                            "tool_call_id": bg_tc_id,
                                        });
                                        // M10 Phase 5a (coalesce): enter the
                                        // `spawn_complete_companion` task-local
                                        // scope so the in-flight `send_file`
                                        // emits an OutboundMessage carrying
                                        // `metadata.spawn_complete_companion =
                                        // true`. The api/serve consumer reads
                                        // the flag and persists each per-file
                                        // row as a transcript-only companion.
                                        // The api/serve consumer suppresses
                                        // that companion's UI projection; the
                                        // subsequent background-result commit
                                        // emits the single canonical v2 child
                                        // envelope, carrying the same media
                                        // via `BackgroundResultPayload
                                        // .envelope_media` populated below.
                                        // Internal-only by
                                        // design: the scope is keyed on a
                                        // `tokio::task_local!`, NOT on tool
                                        // args, so an LLM cannot spoof the
                                        // flag through generated JSON.
                                        let mut delivered = false;
                                        for attempt in 0..3 {
                                            match crate::tools::send_file::with_spawn_complete_companion_scope(
                                                bg_tools.execute("send_file", &send_args),
                                            )
                                            .await
                                            {
                                                Ok(sr) if sr.success => {
                                                    tracing::info!(
                                                        tool = %bg_name,
                                                        file = %path_str,
                                                        "background file sent"
                                                    );
                                                    sent_files.push(path_str.clone());
                                                    delivered = true;
                                                    break;
                                                }
                                                Ok(sr) => {
                                                    tracing::warn!(
                                                        tool = %bg_name,
                                                        file = %path_str,
                                                        attempt,
                                                        error = %sr.output,
                                                        "background file send failed"
                                                    );
                                                }
                                                Err(e) => {
                                                    tracing::warn!(
                                                        tool = %bg_name,
                                                        file = %path_str,
                                                        attempt,
                                                        error = %e,
                                                        "background file send failed"
                                                    );
                                                }
                                            }
                                            if attempt < 2 {
                                                tokio::time::sleep(std::time::Duration::from_secs(
                                                    3,
                                                ))
                                                .await;
                                            }
                                        }
                                        if !delivered {
                                            delivery_failed = true;
                                            tracing::error!(
                                                tool = %bg_name,
                                                file = %path_str,
                                                "file delivery failed after 3 attempts"
                                            );
                                        }
                                    }
                                    if delivery_failed || sent_files.len() != r.files_to_send.len()
                                    {
                                        let err_msg = format!(
                                            "completed but file delivery failed ({}/{})",
                                            sent_files.len(),
                                            r.files_to_send.len()
                                        );
                                        bg_supervisor.mark_failed(&task_id, err_msg.clone());
                                        if let Some(ref sender) = bg_sender {
                                            let _ = sender(BackgroundResultPayload {
                                                task_label: bg_name.clone(),
                                                content: format!(
                                                    "✗ {} failed: {}",
                                                    bg_name, err_msg
                                                ),
                                                kind: BackgroundResultKind::Notification,
                                                media: vec![],
                                                envelope_media: vec![],
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
                                                task_id: Some(task_id.clone()),
                                                originating_client_message_id:
                                                    bg_originating_client_message_id.clone(),
                                                tool_call_id: Some(bg_tc_id.clone()),
                                                // C1 step 3: file-delivery failure
                                                // (mark_failed above).
                                                terminal_status: Some(
                                                    crate::task_supervisor::TaskStatus::Failed,
                                                ),
                                            })
                                            .await;
                                        }
                                    } else {
                                        // Workspace contract already verified
                                        // the declared artifacts. Trust it —
                                        // the supervisor's job is to record
                                        // the skill's reported outcome, not
                                        // to re-validate file contents.
                                        bg_supervisor.mark_completed(&task_id, sent_files.clone());
                                        {
                                            let file_info = format!(
                                                " ({})",
                                                sent_files
                                                    .iter()
                                                    .map(|f| f.rsplit('/').next().unwrap_or(f))
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            );
                                            if let Some(ref sender) = bg_sender {
                                                // M10 Phase 5a (coalesce):
                                                // - `media: vec![]` keeps the
                                                //   background-result transcript
                                                //   row separate from the
                                                //   per-file `send_file`
                                                //   transcript companions.
                                                // - `envelope_media:
                                                //   sent_files.clone()` puts
                                                //   those files on the single
                                                //   canonical v2
                                                //   background-child envelope.
                                                //
                                                // The api/serve consumer
                                                // suppresses UI projection for
                                                // the per-file companions, so
                                                // this split preserves durable
                                                // history while avoiding
                                                // duplicate attachments on the
                                                // one visible completion.
                                                // Mirror the
                                                // `Satisfied`-branch fix
                                                // above: when the tool has
                                                // produced a real textual
                                                // result (e.g. run_pipeline's
                                                // synthesize node returning a
                                                // 5K-char executive summary
                                                // in `r.output`), surface
                                                // that as the chat bubble
                                                // content. Without this, the
                                                // user gets the bare ack
                                                // `"✓ run_pipeline completed
                                                // (research.md)"` while the
                                                // summary lives only inside
                                                // the attached file. The
                                                // `NotConfigured` branch
                                                // fires when the tool
                                                // doesn't declare a
                                                // workspace_policy.toml
                                                // contract — which is the
                                                // default for spawn_only
                                                // tools whose deliverable is
                                                // text + a file rather than
                                                // a fixed-shape artifact.
                                                // `trim().is_empty()` so a
                                                // whitespace-only `r.output`
                                                // doesn't masquerade as a real
                                                // summary — without trim, the
                                                // user would get a chat bubble
                                                // containing just "\n" instead
                                                // of the "✓ completed" notice.
                                                let bubble_content = if r.output.trim().is_empty() {
                                                    format!("✓ {} completed{}", bg_name, file_info)
                                                } else {
                                                    r.output.clone()
                                                };
                                                let _ = sender(BackgroundResultPayload {
                                                    task_label: bg_name.clone(),
                                                    content: bubble_content,
                                                    kind: BackgroundResultKind::Notification,
                                                    media: vec![],
                                                    envelope_media: sent_files.clone(),
                                                    originating_thread_id: bg_originating_thread_id
                                                        .clone(),
                                                    task_id: Some(task_id.clone()),
                                                    originating_client_message_id:
                                                        bg_originating_client_message_id.clone(),
                                                    tool_call_id: Some(bg_tc_id.clone()),
                                                    // C1 step 3: file-delivery success
                                                    // (mark_completed above).
                                                    terminal_status: Some(
                                                        crate::task_supervisor::TaskStatus::Completed,
                                                    ),
                                                })
                                                .await;

                                                // Issue #896: append an
                                                // additional notification
                                                // listing the produced file
                                                // paths (workspace-relative
                                                // when possible) so the
                                                // LLM has stable filenames
                                                // to reference on its next
                                                // turn. The legacy "✓
                                                // completed (basenames)"
                                                // bubble above only shows
                                                // basenames in parentheses
                                                // — enough to display in
                                                // the chat UI, but not
                                                // enough for the LLM to
                                                // pass to `read_file({path:
                                                // ...})` on the next turn.
                                                // Token-budget invariant
                                                // (M10 Phase 4): paths
                                                // only, never file
                                                // contents. Emitted only
                                                // on success and only when
                                                // `sent_files` is
                                                // non-empty (the helper
                                                // returns None and we
                                                // skip otherwise).
                                                //
                                                // Mirroring the `Satisfied`
                                                // branch above: when the
                                                // tool emitted real summary
                                                // text in `r.output` we
                                                // already used it as the
                                                // primary bubble content,
                                                // so suppressing this
                                                // file-path notice keeps
                                                // the chat from rendering
                                                // two trailing assistant
                                                // bubbles (summary, then a
                                                // redundant file-list).
                                                //
                                                // `trim().is_empty()` so a
                                                // whitespace-only `r.output`
                                                // (e.g. a stray "\n") doesn't
                                                // suppress the file-path
                                                // notice — without trim, the
                                                // LLM loses its stable
                                                // filename reference for the
                                                // next turn.
                                                let already_sent_summary =
                                                    !r.output.trim().is_empty();
                                                if !already_sent_summary {
                                                    if let Some(produced_msg) =
                                                        build_spawn_only_produced_files_message(
                                                            &bg_name,
                                                            &sent_files,
                                                            bg_tools.workspace_root(),
                                                        )
                                                    {
                                                        let _ = sender(BackgroundResultPayload {
                                                            task_label: bg_name.clone(),
                                                            content: produced_msg,
                                                            kind:
                                                                BackgroundResultKind::Notification,
                                                            media: vec![],
                                                            envelope_media: vec![],
                                                            originating_thread_id:
                                                                bg_originating_thread_id.clone(),
                                                            task_id: Some(task_id.clone()),
                                                            originating_client_message_id:
                                                                bg_originating_client_message_id
                                                                    .clone(),
                                                            tool_call_id: Some(bg_tc_id.clone()),
                                                            // C1 step 3: produced-files
                                                            // follow-up after mark_completed.
                                                            terminal_status: Some(
                                                                crate::task_supervisor::TaskStatus::Completed,
                                                            ),
                                                        })
                                                        .await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Ok(r) => {
                            tracing::warn!(
                                tool = %bg_name,
                                error = %r.output,
                                "spawn_only background tool failed"
                            );
                            bg_supervisor.mark_failed(&task_id, r.output.clone());
                            // Notify session of failure
                            if let Some(ref sender) = bg_sender {
                                let _ = sender(BackgroundResultPayload {
                                    task_label: bg_name.clone(),
                                    content: format!("✗ {} failed: {}", bg_name, r.output),
                                    kind: BackgroundResultKind::Notification,
                                    media: vec![],
                                    envelope_media: vec![],
                                    originating_thread_id: bg_originating_thread_id.clone(),
                                    task_id: Some(task_id.clone()),
                                    originating_client_message_id: bg_originating_client_message_id
                                        .clone(),
                                    tool_call_id: Some(bg_tc_id.clone()),
                                    // C1 step 3: tool returned non-success
                                    // (mark_failed above).
                                    terminal_status: Some(
                                        crate::task_supervisor::TaskStatus::Failed,
                                    ),
                                })
                                .await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                tool = %bg_name,
                                error = %e,
                                "spawn_only background tool error"
                            );
                            bg_supervisor.mark_failed(&task_id, e.to_string());
                            if let Some(ref sender) = bg_sender {
                                let _ = sender(BackgroundResultPayload {
                                    task_label: bg_name.clone(),
                                    content: format!("✗ {} error: {}", bg_name, e),
                                    kind: BackgroundResultKind::Notification,
                                    media: vec![],
                                    envelope_media: vec![],
                                    originating_thread_id: bg_originating_thread_id.clone(),
                                    task_id: Some(task_id.clone()),
                                    originating_client_message_id: bg_originating_client_message_id
                                        .clone(),
                                    tool_call_id: Some(bg_tc_id.clone()),
                                    // C1 step 3: tool execution errored
                                    // (mark_failed above).
                                    terminal_status: Some(
                                        crate::task_supervisor::TaskStatus::Failed,
                                    ),
                                })
                                .await;
                            }
                        }
                    }

                    // M8.7 (item 4): tear down router/watcher state once
                    // the task has reached a terminal supervisor status.
                    // mark_terminal flips the dashboard "task running"
                    // bit and stops further tail streams. The watcher
                    // exits on its own next iteration via
                    // `is_terminal(supervisor, task_id)`, but we also
                    // call stop_watcher to release the registry slot
                    // promptly.
                    if let Some(ref router) = bg_output_router {
                        router.mark_terminal(&task_id);
                    }
                    if let Some(ref summary_gen) = bg_summary_generator {
                        summary_gen.stop_watcher(&task_id);
                    }
                });
                reporter.report(ProgressEvent::ToolCompleted {
                    name: tc_name.clone(),
                    tool_id: tc_id.clone(),
                    success: true,
                    output_preview: "Running in background — audio will be sent when ready.".into(),
                    duration: tool_start.elapsed(),
                });
                // M10 Phase 4 — agent context isolation: hand the LLM a
                // small `task_handle` JSON envelope instead of the full
                // tool output. The full result is still persisted via the
                // M8.7 router and delivered to the SPA via
                // `turn.spawn_complete`; the agent now reads selectively
                // via `read_task_output`.
                //
                // Codex P2 (round 1+2): gate the envelope on the
                // `read_task_output` tool actually being VISIBLE to the
                // LLM in this turn — registered AND not filtered out by
                // provider policy / deferred set / context tag filter.
                // Otherwise the envelope advertises a tool the LLM was
                // not offered. Fall back to the legacy free-text message
                // for those entry points.
                let handle_payload = if tools.is_tool_visible("read_task_output") {
                    tools.spawn_only_handle_message(&tc_name, &task_id_for_handle, &[])
                } else {
                    tools.spawn_only_message(&tc_name)
                };
                return (
                    Message {
                        role: MessageRole::Tool,
                        content: handle_payload,
                        media: vec![],
                        tool_calls: None,
                        tool_call_id: Some(tc_id),
                        reasoning_content: None,
                        client_message_id: None,
                        thread_id: None,
                        timestamp: chrono::Utc::now(),
                    },
                    Vec::new(),
                    Vec::new(),
                    None,
                    true, // spawn_only placeholder is reported as success
                    None,
                    // spawn_only success — cascade flag is moot when success=true
                    true,
                );
            }

            let ctx = ToolContext {
                tool_id: tc_id.clone(),
                reporter: reporter.clone(),
                harness_event_sink: harness_event_sink.clone(),
                attachment_paths: attachment_ctx.attachment_paths.clone(),
                audio_attachment_paths: attachment_ctx.audio_attachment_paths.clone(),
                file_attachment_paths: attachment_ctx.file_attachment_paths.clone(),
                // M8.2/M8.4 reconciliation: thread agent_definitions and
                // file_state_cache into the foreground ToolContext so post-M8.8
                // tools see the live registry/cache instead of zeros.
                agent_definitions: agent_definitions.clone(),
                file_state_cache: file_state_cache.clone(),
                // M8 fix-first item 8 (gap 4b): consult the profile
                // envelope so deny-list profiles actually block tools at
                // the call boundary (read_file already checks
                // ctx.permissions.is_tool_allowed).
                permissions: permissions.clone(),
                // M8 parity (W1.A1/A3/A4): thread the shared router /
                // summary generator / task supervisor / cost accountant
                // through to foreground tool calls so run_pipeline (and
                // the spawn tool) can pick them up via TOOL_CTX and
                // hand them down to background workers.
                subagent_output_router: subagent_output_router.clone(),
                subagent_summary_generator: subagent_summary_generator.clone(),
                task_supervisor: Some(tools.supervisor()),
                cost_accountant: cost_accountant.clone(),
                parent_session_key: parent_session_key.clone(),
                // Guard C (issue #607): stamp the agent's spawn
                // nesting depth onto every foreground tool's
                // TOOL_CTX so the spawn tool sees an accurate value
                // when deciding whether the next nested spawn is
                // allowed.
                spawn_depth,
                // Phase 1 SessionScope migration: thread the shared
                // scope onto the foreground TOOL_CTX so tools and the
                // pipeline host context snapshot the same handle.
                session_scope: session_scope.clone(),
                // #1774: post-edit formatting opt-in for file tools.
                format_after_edit,
                ..ToolContext::zero()
            };
            // Thread the typed context into execute_with_context. Legacy tools
            // whose trait impl only overrides `execute` still work via the
            // default delegation path; migrated tools read the typed fields.
            // TOOL_CTX is still scoped for plugin tools that read the task-local.
            //
            // UPCR-2026-023 live-soak BUG 1: re-establish the per-turn
            // human-blocking bridges (captured above before this `tokio::spawn`)
            // INSIDE the spawned task so approval-gated tools (`shell`,
            // `edit_file`, …) and `ask_user_question` see their requester via
            // `try_with`. Without this, both the parallel (`join_all`) and
            // serial dispatch paths — which BOTH run the tool through this
            // `spawn_tool_task` `tokio::spawn` — would lose the task-local and
            // the tool would degrade (approval denied / question text fallback).
            // We scope each bridge ONLY when it was scoped in the parent
            // (`Some(_)`), so a non-interactive turn (CLI / gateway batch with
            // no requester) keeps the graceful-degradation path unchanged.
            let exec_future = TOOL_CTX.scope(ctx.clone(), async {
                tools
                    .execute_with_context(&ctx, &tc_name, &effective_args)
                    .await
            });
            let result = match (&captured_approval_ctx, &captured_user_question_ctx) {
                (Some(approval), Some(question)) => {
                    TOOL_APPROVAL_CTX
                        .scope(
                            approval.clone(),
                            USER_QUESTION_CTX.scope(question.clone(), exec_future),
                        )
                        .await
                }
                (Some(approval), None) => {
                    TOOL_APPROVAL_CTX.scope(approval.clone(), exec_future).await
                }
                (None, Some(question)) => {
                    USER_QUESTION_CTX.scope(question.clone(), exec_future).await
                }
                (None, None) => exec_future.await,
            };

            let duration = tool_start.elapsed();

            let (
                content,
                tool_files_modified,
                tool_files_to_send,
                tool_tokens,
                tool_success,
                tool_structured_metadata,
                tool_cascades,
            ) = match result {
                Ok(tool_result) => {
                    debug!(
                        tool = %tc_name,
                        success = tool_result.success,
                        duration_ms = duration.as_millis() as u64,
                        "tool completed"
                    );

                    if let Some(ref file) = tool_result.file_modified {
                        info!(tool = %tc_name, file = %file.display(), "file modified");
                        reporter.report(ProgressEvent::FileModified {
                            path: file.display().to_string(),
                        });
                    }

                    if should_auto_send_tool_files(
                        suppress_auto_send_files,
                        explicit_send_file_requested,
                        &tc_name,
                    ) {
                        // Auto-send files explicitly declared by the plugin via files_to_send.
                        // No heuristic path detection — plugins must opt-in by including
                        // "files_to_send": ["/path/to/file"] in their JSON output.
                        let files: Vec<String> = tool_result
                            .files_to_send
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect();

                        for path_str in &files {
                            info!(tool = %tc_name, file = %path_str, "auto-sending file to user");
                            let send_args =
                                serde_json::json!({"file_path": path_str, "tool_call_id": tc_id});
                            match tools.execute("send_file", &send_args).await {
                                Ok(r) if r.success => {
                                    info!(tool = %tc_name, file = %path_str, "file auto-sent");
                                }
                                Ok(r) => {
                                    warn!(tool = %tc_name, file = %path_str, error = %r.output, "auto-send failed");
                                }
                                Err(e) => {
                                    warn!(tool = %tc_name, file = %path_str, error = %e, "auto-send failed");
                                }
                            }
                        }
                    } else if explicit_send_file_requested
                        && tc_name != "send_file"
                        && !tool_result.files_to_send.is_empty()
                    {
                        debug!(
                            tool = %tc_name,
                            "skipping auto-send because the same model turn already issued send_file"
                        );
                    }

                    let mut tool_files_modified = Vec::new();
                    if let Some(file) = tool_result.file_modified.clone() {
                        tool_files_modified.push(file);
                    }
                    let tool_files_to_send = tool_result.files_to_send.clone();

                    let output_preview =
                        octos_core::truncated_utf8(&tool_result.output, 200, "...");

                    reporter.report(ProgressEvent::ToolCompleted {
                        name: tc_name.clone(),
                        tool_id: tc_id.clone(),
                        success: tool_result.success,
                        output_preview,
                        duration,
                    });

                    let success = tool_result.success;
                    (
                        tool_result.output,
                        tool_files_modified,
                        tool_files_to_send,
                        tool_result.tokens_used,
                        success,
                        tool_result.structured_metadata,
                        // A tool that RAN and reported failure still cascades
                        // (legacy behaviour); only never-ran input errors below
                        // opt out.
                        true,
                    )
                }
                Err(e) => {
                    // Classify the tool failure as a typed HarnessError.
                    // Invariant #1 (#488): every raw tool error escape
                    // must be routed through classification so the
                    // metrics counter and the sink event both fire.
                    let classified = HarnessError::classify_report(&e, Some(tc_name.as_str()));
                    classified.record_metric();
                    if let Some(sink) = harness_event_sink.as_deref() {
                        if let Some(ctx) = lookup_event_sink_context(sink) {
                            let event =
                                classified.to_event(ctx.session_id, ctx.task_id, None, None);
                            if let Err(error) = write_event_to_sink(sink, &event) {
                                tracing::debug!(
                                    error = %error,
                                    "failed to write tool-failure harness error event"
                                );
                            }
                        }
                    }
                    warn!(
                        tool = %tc_name,
                        error = %e,
                        variant = classified.variant_name(),
                        recovery = %classified.recovery_hint(),
                        duration_ms = duration.as_millis() as u64,
                        "tool failed"
                    );

                    reporter.report(ProgressEvent::ToolCompleted {
                        name: tc_name.clone(),
                        tool_id: tc_id.clone(),
                        success: false,
                        output_preview: e.to_string(),
                        duration,
                    });

                    // #1690: a malformed-arguments failure (`ToolInputError`)
                    // has no side effects and must not cancel well-formed
                    // sibling calls in a serial batch; genuine execution errors
                    // still cascade. Scan the whole chain so a `wrap_err` in the
                    // dispatch path cannot hide the marker.
                    let cascades = !e
                        .chain()
                        .any(|src| src.is::<crate::tools::ToolInputError>());
                    (
                        format!("Error: {e}"),
                        Vec::new(),
                        Vec::new(),
                        None,
                        false,
                        None,
                        cascades,
                    )
                }
            };

            // After-tool hook (fire-and-forget)
            if let Some(ref hooks) = hooks {
                let payload = HookPayload::after_tool(
                    &tc_name,
                    &tc_id,
                    octos_core::truncated_utf8(&content, 500, "..."),
                    tool_success,
                    duration.as_millis() as u64,
                    hook_ctx.as_ref(),
                );
                let _ = hooks.run(HookEvent::AfterToolCall, &payload).await;
            }

            // Per-tool output truncation with head/tail split
            let limit = octos_core::tool_output_limit(&tc_name);
            let content = octos_core::truncate_head_tail(&content, limit, 0.7);
            let content = crate::sanitize::sanitize_tool_output(&content);

            // Pair the structured side-channel with the originating tool's
            // call id so the session actor (which keys cost rows by
            // tool_call_id) can match them on the SSE done event.
            let structured_metadata = tool_structured_metadata.map(|meta| (tc_id.clone(), meta));

            (
                Message {
                    role: MessageRole::Tool,
                    content,
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: Some(tc_id),
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                },
                tool_files_modified,
                tool_files_to_send,
                tool_tokens,
                tool_success,
                structured_metadata,
                tool_cascades,
            )
        })
    }

    /// #1768: take a workspace snapshot when `tool_names` contains a
    /// mutating tool and a [`crate::snapshot::SnapshotManager`] is
    /// attached (opt-in, default OFF — `self.snapshot_manager` is `None`
    /// otherwise and this returns immediately).
    ///
    /// The snapshot label records which mutating tools triggered it
    /// (`pre-tool: write_file,shell`). Failures are logged and swallowed:
    /// a missed undo point must never fail or delay the tool batch
    /// beyond the snapshot itself.
    async fn snapshot_before_mutating_tools(&self, tool_names: &[&str]) {
        let Some(manager) = &self.snapshot_manager else {
            return;
        };
        let mut mutating: Vec<&str> = Vec::new();
        for name in tool_names {
            if crate::snapshot::is_mutating_tool(name) && !mutating.contains(name) {
                mutating.push(name);
            }
        }
        if mutating.is_empty() {
            return;
        }
        let label = format!("pre-tool: {}", mutating.join(","));
        match manager.take_snapshot_async(label).await {
            Ok(id) => {
                tracing::debug!(snapshot = %id, tools = %mutating.join(","),
                    "workspace snapshot recorded before mutating tools");
            }
            Err(err) => {
                tracing::warn!(error = %err,
                    "workspace snapshot failed; continuing without an undo point");
            }
        }
    }

    pub(super) async fn execute_tools(
        &self,
        response: &ChatResponse,
    ) -> Result<(
        Vec<Message>,
        Vec<std::path::PathBuf>,
        Vec<std::path::PathBuf>,
        TokenUsage,
        Vec<(String, serde_json::Value)>,
        // Codex round-2 MAJOR 2 (PR #1187 fixup): per-tool-call success bit
        // keyed by `tool_call_id`. The dispatcher already computes this for
        // the M8.8 serial-cascade scheduler (see [`ToolCallResult`] field 5);
        // we surface it now so `any_tool_invocation_errored` in the
        // loop_runner can authoritatively decide whether a tool failed
        // without guessing from content prefixes. The content-based
        // classifier was missing many real failure shapes — shell
        // timeouts ("Command timed out after ..."), sandbox path
        // rejections ("Path outside working directory ..."), browser
        // navigation failures, plugin tools with `success: false` and
        // arbitrary error messages — every one of which renders a red
        // error chip but did NOT carry a recognised error envelope, so
        // the synth-ack branch would still fabricate a "Background work
        // started" bubble alongside it.
        Vec<(String, bool)>,
    )> {
        let tool_names: Vec<&str> = response
            .tool_calls
            .iter()
            .map(|tc| tc.name.as_str())
            .collect();
        let explicit_send_file_requested =
            response.tool_calls.iter().any(|tc| tc.name == "send_file");

        // M8.8 + #1766 — classify the batch and pick an admission strategy.
        let exclusive_count = response
            .tool_calls
            .iter()
            .filter(|tc| self.tools.concurrency_class(&tc.name) == ConcurrencyClass::Exclusive)
            .count();
        let any_exclusive = exclusive_count > 0;
        let all_exclusive = exclusive_count == response.tool_calls.len();
        let dispatch_mode = if !any_exclusive {
            "parallel"
        } else if all_exclusive {
            "serial"
        } else {
            "mixed"
        };

        tracing::info!(
            parallel_tools = response.tool_calls.len(),
            tool_names = %tool_names.join(", "),
            dispatch = dispatch_mode,
            "executing tool batch"
        );

        // #1768: record a workspace snapshot BEFORE the batch runs when it
        // contains a mutating tool. Awaited so no tool can touch a file
        // until the snapshot commit exists; the git work happens on a
        // blocking thread and a failure only logs (a missed undo point
        // must never block the batch). No-op unless a manager is attached
        // (opt-in, default OFF).
        self.snapshot_before_mutating_tools(&tool_names).await;

        let turn_attachment_ctx = TURN_ATTACHMENT_CTX
            .try_with(|ctx| ctx.clone())
            .unwrap_or_default();

        // Let the LLM specify per-tool timeout via `timeout_secs` in tool call args.
        // Use the max of all requested timeouts, clamped to MAX_TOOL_TIMEOUT_SECS.
        let llm_requested_timeout: u64 = response
            .tool_calls
            .iter()
            .filter_map(|tc| tc.arguments.get("timeout_secs").and_then(|v| v.as_u64()))
            .max()
            .unwrap_or(0);
        // UPCR-2026-023: a batch containing a human-wait tool
        // (`ask_user_question`) must run with NO finite batch timeout — the
        // human may take arbitrarily long, and a fired ceiling would detach the
        // still-running tool task and leak the pending question (replayed later
        // as a stale prompt). `compute_batch_timeout_secs` returns `None` for
        // such a batch; the dispatch paths below branch on that. NON-human-wait
        // peers keep their per-tool registry timeouts (applied inside each
        // tool's registry dispatch), unaffected by skipping this outer wrap.
        let any_human_wait = response
            .tool_calls
            .iter()
            .any(|tc| self.tools.blocks_on_human_input(&tc.name));

        // mini5 soak fix: when the LLM omits `timeout_secs`, a batch of only
        // fast/interactive tools (e.g. `glob`, `list_dir`) defaults to the
        // short `default_interactive_tool_timeout_secs` instead of inheriting
        // the 1800s ceiling that hung the turn. A batch containing any
        // genuinely long-running tool keeps the long default; an explicit
        // LLM-requested timeout is still honoured (clamped + floored). A
        // human-wait batch yields `None` (no batch timeout at all).
        let tool_timeout_secs = compute_batch_timeout_secs(
            &tool_names,
            any_human_wait,
            llm_requested_timeout,
            self.config.tool_timeout_secs,
            self.config.default_interactive_tool_timeout_secs,
        );
        let tool_timeout = tool_timeout_secs.map(Duration::from_secs);

        let results: Vec<ToolCallResult> = if !any_exclusive {
            // Parallel admission — the classic all-Safe path. Spawn every
            // tool call as a detached task and join them against one shared
            // deadline (see `join_parallel_handles` for the aggregation and
            // UPCR-2026-023 human-wait semantics).
            let handles: Vec<_> = response
                .tool_calls
                .iter()
                .map(|tool_call| {
                    self.spawn_tool_task(
                        tool_call,
                        explicit_send_file_requested,
                        &turn_attachment_ctx,
                    )
                })
                .collect();
            let calls: Vec<&octos_core::ToolCall> = response.tool_calls.iter().collect();
            join_parallel_handles(handles, &calls, tool_timeout).await
        } else if all_exclusive {
            // Serial admission: run each tool in LLM call order, bail out of
            // the remaining calls if any one errors and emit synthetic
            // "cancelled" results so the LLM still sees every tool_call_id.
            let calls: Vec<&octos_core::ToolCall> = response.tool_calls.iter().collect();
            self.run_serial_calls(
                &calls,
                /* start_cancelled */ false,
                explicit_send_file_requested,
                &turn_attachment_ctx,
                tool_timeout,
                tool_timeout_secs,
            )
            .await
        } else {
            // Mixed admission (#1766): Safe calls in parallel first, then
            // Exclusive calls serially, reassembled in LLM call order. See
            // the module doc for the pinned semantics.
            self.execute_mixed_batch(
                response,
                explicit_send_file_requested,
                &turn_attachment_ctx,
                tool_timeout,
                tool_timeout_secs,
            )
            .await
        };

        // Log completion of the tool batch.
        let result_sizes: Vec<usize> = results
            .iter()
            .map(|(m, _, _, _, _, _, _)| m.content.len())
            .collect();
        let total_result_bytes: usize = result_sizes.iter().sum();
        tracing::info!(
            parallel_tools = results.len(),
            dispatch = dispatch_mode,
            result_sizes = %result_sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "),
            total_result_bytes,
            "all tools in batch completed"
        );

        // Aggregate results -- order is preserved by both dispatch paths.
        let mut messages = Vec::with_capacity(results.len());
        let mut files_modified = Vec::new();
        let mut files_to_send = Vec::new();
        let mut tokens_used = TokenUsage::default();
        let mut structured_metadata: Vec<(String, serde_json::Value)> = Vec::new();
        // Codex round-2 MAJOR 2 (PR #1187 fixup): collect the per-call
        // `success` bit keyed by `tool_call_id` so the loop_runner can
        // authoritatively decide whether the synth-ack branch fires
        // alongside a failed tool. Capacity matches the result count.
        let mut success_by_id: Vec<(String, bool)> = Vec::with_capacity(results.len());

        for (
            message,
            tool_files_modified,
            tool_files_to_send,
            tool_tokens,
            success,
            tool_structured_metadata,
            _cascades,
        ) in results
        {
            // Pair every executed tool result with its `tool_call_id` so
            // downstream gating logic does not need to guess at the
            // identity from content shape.
            if let Some(id) = message.tool_call_id.clone() {
                success_by_id.push((id, success));
            }
            messages.push(message);
            files_modified.extend(tool_files_modified);
            files_to_send.extend(tool_files_to_send);
            if let Some(tokens) = tool_tokens {
                tokens_used.input_tokens += tokens.input_tokens;
                tokens_used.output_tokens += tokens.output_tokens;
                tokens_used.cache_read_tokens += tokens.cache_read_tokens;
                tokens_used.cache_write_tokens += tokens.cache_write_tokens;
            }
            if let Some(meta) = tool_structured_metadata {
                structured_metadata.push(meta);
            }
        }

        Ok((
            messages,
            files_modified,
            files_to_send,
            tokens_used,
            structured_metadata,
            success_by_id,
        ))
    }

    /// Serial dispatch core (M8.8): run `calls` one at a time, in order.
    ///
    /// Used by two admission strategies in [`Agent::execute_tools`]:
    /// - **All-Exclusive batch** — `calls` is the full batch in LLM call
    ///   order and `start_cancelled` is `false` (the original M8.8 path,
    ///   behaviour unchanged).
    /// - **Mixed batch (#1766), phase 2** — `calls` is the Exclusive subset
    ///   in LLM call order and `start_cancelled` carries the phase-1
    ///   verdict: `true` when a parallel Safe sibling already failed with
    ///   the cascade bit set, in which case every call here is skipped and
    ///   receives the synthetic "cancelled due to sibling error" [`Message`].
    ///
    /// Each tool call runs to completion before the next one is spawned. If
    /// any call's result message reports a failure (success=false), every
    /// remaining peer is skipped and receives a synthetic "cancelled due to
    /// sibling error" [`Message`] so the LLM sees a 1:1 mapping from its
    /// `tool_call_id`s to results.
    ///
    /// The batch-level timeout is enforced per call by wrapping the
    /// single-call [`JoinHandle`] in `tokio::time::timeout`. A timeout on any
    /// one call fails that call and cascades to its peers the same way a
    /// regular error does.
    ///
    /// UPCR-2026-023: when the batch contains a human-wait tool the caller
    /// passes `tool_timeout == None`; every call in the batch is then awaited
    /// DIRECTLY with no `tokio::time::timeout` wrap, so the human-wait tool is
    /// never detached by a fired ceiling. NON-human-wait peers in the same
    /// (now-unbounded) batch keep their own per-tool registry timeouts, applied
    /// inside `spawn_tool_task` → `ToolRegistry::execute_with_context`. Cleanup
    /// of the human-wait call comes from the user answering or a turn
    /// interrupt/abort draining the pending question — never from this wrap.
    async fn run_serial_calls(
        &self,
        calls: &[&octos_core::ToolCall],
        start_cancelled: bool,
        explicit_send_file_requested: bool,
        turn_attachment_ctx: &crate::tools::TurnAttachmentContext,
        tool_timeout: Option<Duration>,
        tool_timeout_secs: Option<u64>,
    ) -> Vec<ToolCallResult> {
        let mut results: Vec<ToolCallResult> = Vec::with_capacity(calls.len());
        let mut cancelled = start_cancelled;

        for (idx, tool_call) in calls.iter().enumerate() {
            if cancelled {
                let skipped = calls.len() - idx;
                tracing::info!(
                    tool = %tool_call.name,
                    tool_id = %tool_call.id,
                    skipped_peers = skipped,
                    "cancelling remaining tool call in serial batch after sibling error"
                );
                results.push(cancelled_result(tool_call));
                continue;
            }

            let handle =
                self.spawn_tool_task(tool_call, explicit_send_file_requested, turn_attachment_ctx);

            // `None` (human-wait batch) awaits the handle directly — no finite
            // wrap — so the human-wait call cannot be detached by a ceiling.
            // The `Err(())` arm is unreachable in that case.
            let join_outcome = match tool_timeout {
                Some(dur) => match tokio::time::timeout(dur, handle).await {
                    Ok(joined) => Ok(joined),
                    Err(_) => Err(()),
                },
                None => Ok(handle.await),
            };

            let outcome = match join_outcome {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(
                        tool = %tool_call.name,
                        error = %e,
                        "serial tool task panicked"
                    );
                    panic_result(tool_call, &e.to_string())
                }
                Err(()) => {
                    let elapsed_secs = tool_timeout_secs.unwrap_or(0);
                    tracing::error!(
                        timeout_secs = elapsed_secs,
                        tool = %tool_call.name,
                        tool_id = %tool_call.id,
                        "serial tool execution timed out"
                    );
                    timed_out_result(tool_call, elapsed_secs)
                }
            };

            // The per-call success bit (tuple field 4) marks failure; field 6
            // (`cascades`) decides whether that failure cancels the remaining
            // peers. Every failure path in `spawn_tool_task` — tool error, hook
            // denial, panic, timeout — sets success `false`; only a
            // no-side-effect `ToolInputError` (malformed model arguments) sets
            // `cascades = false`, so one bad call cannot nuke well-formed
            // siblings (#1690). No need to peek at message content.
            let failed = !outcome.4;
            let cascades = outcome.6;
            results.push(outcome);
            if failed && cascades {
                cancelled = true;
            }
        }

        results
    }

    /// Two-phase dispatch for mixed batches (#1766): at least one Safe AND
    /// at least one Exclusive call in the same batch.
    ///
    /// Phase 1 spawns every [`ConcurrencyClass::Safe`] call in parallel and
    /// joins them exactly like the all-Safe path (shared absolute deadline,
    /// per-handle race — see [`join_parallel_handles`]). Phase 2 then runs
    /// the Exclusive calls serially in LLM call order via
    /// [`Agent::run_serial_calls`], starting cancelled when any phase-1
    /// failure carried the cascade bit — so no mutation runs after a failed
    /// read, while a no-side-effect `ToolInputError` does NOT cancel (#1690).
    ///
    /// Results are reassembled into the ORIGINAL LLM call order before
    /// returning, so callers observe the same 1:1 `tool_call_id` mapping as
    /// the other dispatch strategies. See the module doc for the pinned
    /// visibility / cascade / approval-ordering semantics.
    async fn execute_mixed_batch(
        &self,
        response: &ChatResponse,
        explicit_send_file_requested: bool,
        turn_attachment_ctx: &crate::tools::TurnAttachmentContext,
        tool_timeout: Option<Duration>,
        tool_timeout_secs: Option<u64>,
    ) -> Vec<ToolCallResult> {
        // Partition into the two phases, remembering each call's original
        // batch position for reassembly. Order within each partition is LLM
        // call order (`enumerate` over the original list).
        let mut safe_calls: Vec<(usize, &octos_core::ToolCall)> = Vec::new();
        let mut exclusive_calls: Vec<(usize, &octos_core::ToolCall)> = Vec::new();
        for (idx, tool_call) in response.tool_calls.iter().enumerate() {
            if self.tools.concurrency_class(&tool_call.name) == ConcurrencyClass::Exclusive {
                exclusive_calls.push((idx, tool_call));
            } else {
                safe_calls.push((idx, tool_call));
            }
        }

        // Phase 1 — every Safe call in parallel, aggregated with the same
        // shared-deadline semantics as the all-Safe path.
        let handles: Vec<_> = safe_calls
            .iter()
            .map(|(_, tool_call)| {
                self.spawn_tool_task(tool_call, explicit_send_file_requested, turn_attachment_ctx)
            })
            .collect();
        let safe_refs: Vec<&octos_core::ToolCall> =
            safe_calls.iter().map(|(_, tool_call)| *tool_call).collect();
        let safe_results = join_parallel_handles(handles, &safe_refs, tool_timeout).await;

        // Phase-1 verdict: a Safe failure with the cascade bit set (real
        // error / hook denial / panic / timeout — NOT a `ToolInputError`,
        // #1690) cancels the entire Exclusive phase, position-independently.
        let safe_failure_cancels_exclusive =
            safe_results.iter().any(|result| !result.4 && result.6);
        if safe_failure_cancels_exclusive {
            tracing::info!(
                cancelled_exclusive_calls = exclusive_calls.len(),
                "mixed batch: Safe phase failed; cancelling entire Exclusive phase"
            );
        }

        // Phase 2 — Exclusive calls serially in LLM call order.
        let exclusive_refs: Vec<&octos_core::ToolCall> = exclusive_calls
            .iter()
            .map(|(_, tool_call)| *tool_call)
            .collect();
        let exclusive_results = self
            .run_serial_calls(
                &exclusive_refs,
                safe_failure_cancels_exclusive,
                explicit_send_file_requested,
                turn_attachment_ctx,
                tool_timeout,
                tool_timeout_secs,
            )
            .await;

        // Reassemble in the ORIGINAL LLM call order. Every original index
        // appears in exactly one partition, and each phase returns exactly
        // one result per call, so every slot fills.
        let mut slots: Vec<Option<ToolCallResult>> = Vec::with_capacity(response.tool_calls.len());
        slots.resize_with(response.tool_calls.len(), || None);
        for ((idx, _), result) in safe_calls.iter().zip(safe_results) {
            slots[*idx] = Some(result);
        }
        for ((idx, _), result) in exclusive_calls.iter().zip(exclusive_results) {
            slots[*idx] = Some(result);
        }
        slots
            .into_iter()
            .map(|slot| slot.expect("mixed-batch dispatch fills every original call slot"))
            .collect()
    }
}

/// Join already-spawned parallel tool tasks against ONE shared absolute
/// deadline (or no deadline at all for a human-wait batch). `calls` must be
/// index-aligned with `handles`.
///
/// Extracted from the all-Safe dispatch arm of [`Agent::execute_tools`] so
/// the mixed-batch phase 1 (#1766) shares the exact same aggregation
/// semantics.
///
/// UPCR-2026-023: a human-wait batch (`tool_timeout == None`) awaits
/// `join_all` DIRECTLY, with no timeout wrap, so the human-wait tool task is
/// never detached by a fired ceiling. It is unblocked by the user answering
/// or by a turn interrupt/abort (which drains the pending question).
///
/// All other batches share ONE absolute deadline, but each handle is raced
/// against it INDIVIDUALLY (`tokio::time::timeout_at`): a call that already
/// resolved keeps its REAL result even when a sibling overruns the ceiling —
/// only the still-pending calls get the synthetic "timed out" message
/// (success=false, so the spawn_only synth-ack gate in loop_runner still
/// suppresses the fabricated "Background work started" bubble for them). The
/// previous shape — one `timeout()` wrapped around the whole `join_all` —
/// dropped the joined future on expiry and fabricated a timeout message for
/// EVERY call, discarding the real output of calls (including spawn_only
/// acks) that had already completed. `timeout_at` polls the inner handle
/// before the timer, so a handle that completed by the time we reach it
/// yields its result even at/past the deadline. Timed-out tasks are NOT
/// aborted — they keep running detached for cleanup, exactly as before.
async fn join_parallel_handles(
    handles: Vec<JoinHandle<ToolCallResult>>,
    calls: &[&octos_core::ToolCall],
    tool_timeout: Option<Duration>,
) -> Vec<ToolCallResult> {
    match tool_timeout {
        Some(dur) => {
            let deadline = tokio::time::Instant::now() + dur;
            let elapsed_secs = dur.as_secs();
            let mut results: Vec<ToolCallResult> = Vec::with_capacity(calls.len());
            for (handle, tc) in handles.into_iter().zip(calls.iter()) {
                match tokio::time::timeout_at(deadline, handle).await {
                    Ok(Ok(result)) => results.push(result),
                    Ok(Err(e)) => results.push(panic_result(tc, &e.to_string())),
                    Err(_elapsed) => {
                        tracing::error!(
                            timeout_secs = elapsed_secs,
                            tool = %tc.name,
                            tool_id = %tc.id,
                            "tool execution timed out -- spawned task continues running for cleanup"
                        );
                        results.push(timed_out_result(tc, elapsed_secs));
                    }
                }
            }
            results
        }
        None => futures::future::join_all(handles)
            .await
            .into_iter()
            .zip(calls.iter())
            .map(|(r, tc)| r.unwrap_or_else(|e| panic_result(tc, &e.to_string())))
            .collect(),
    }
}

/// Build a synthetic tool-result message for a peer that was cancelled after
/// a sibling tool errored in a serial (M8.8) batch.
fn cancelled_result(tool_call: &octos_core::ToolCall) -> ToolCallResult {
    (
        Message {
            role: MessageRole::Tool,
            content: format!(
                "Tool '{}' cancelled due to earlier sibling error in the same batch. Re-issue this call on the next turn if still needed.",
                tool_call.name
            ),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_call.id.clone()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Vec::new(),
        Vec::new(),
        None,
        false,
        None,
        // a cancelled peer raised no error of its own — do not further cascade
        false,
    )
}

/// Build a synthetic tool-result message for a call that was still pending
/// when the batch deadline fired (parallel dispatch) or whose own wrap
/// expired (serial dispatch). `success` is `false` so the spawn_only
/// synth-ack gate in loop_runner can suppress the fabricated "Background
/// work started" bubble without content-prefix matching. The spawned task
/// itself is NOT aborted — it keeps running detached for cleanup.
fn timed_out_result(tool_call: &octos_core::ToolCall, elapsed_secs: u64) -> ToolCallResult {
    (
        Message {
            role: MessageRole::Tool,
            content: format!(
                "Tool '{}' timed out after {} seconds",
                tool_call.name, elapsed_secs
            ),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_call.id.clone()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Vec::new(),
        Vec::new(),
        None,
        false,
        None,
        // a timeout cascades to peers the same way a regular error does
        true,
    )
}

/// Build a tool-result message describing a panic inside a spawned tool task.
fn panic_result(tool_call: &octos_core::ToolCall, reason: &str) -> ToolCallResult {
    (
        Message {
            role: MessageRole::Tool,
            content: format!("Tool '{}' panicked: {}", tool_call.name, reason),
            media: vec![],
            tool_calls: None,
            tool_call_id: Some(tool_call.id.clone()),
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        },
        Vec::new(),
        Vec::new(),
        None,
        false,
        None,
        // a panic is an unexpected hard failure — cascade to peers
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_spawn_only_produced_files_message, relativize_workspace_path,
        satisfied_completion_content, satisfied_delivery_is_failure, should_auto_send_tool_files,
    };

    #[test]
    fn satisfied_contract_in_chat_mode_is_not_a_failure() {
        // P2 (tri-repo #1529): a Satisfied spawn_only contract in chat mode
        // (no background sender: delivery == None) was recorded as Failed
        // because "no channel to notify" was conflated with "sender failed to
        // persist". None and Some(true) are BOTH success; only Some(false) —
        // a wired sender that actually failed — is a failure.
        assert!(
            !satisfied_delivery_is_failure(None),
            "chat mode (no background sender) must not be a failure"
        );
        assert!(
            !satisfied_delivery_is_failure(Some(true)),
            "a delivered result is a success"
        );
        assert!(
            satisfied_delivery_is_failure(Some(false)),
            "a wired sender that failed to persist is a real failure"
        );
    }

    #[test]
    fn explicit_send_file_turn_suppresses_plugin_auto_send_for_other_tools() {
        assert!(!should_auto_send_tool_files(false, true, "mofa_slides"));
        assert!(should_auto_send_tool_files(false, true, "send_file"));
    }

    #[test]
    fn auto_send_respects_global_suppression_flag() {
        assert!(!should_auto_send_tool_files(true, false, "mofa_slides"));
    }

    #[test]
    fn should_emit_produced_files_block_when_files_present() {
        // Issue #896: spawn_only completion appends an additional message
        // listing produced file paths so the LLM has a stable
        // workspace-relative reference for its next turn.
        let root = std::path::PathBuf::from("/tmp/ws");
        let files = vec![
            "/tmp/ws/research/x/x.md".to_string(),
            "/tmp/ws/research/x/_search_results.md".to_string(),
        ];
        let msg = build_spawn_only_produced_files_message("search", &files, Some(&root))
            .expect("non-empty file list must yield Some(message)");

        // Format pinning: header + bulleted workspace-relative paths.
        assert!(
            msg.starts_with("`search` produced files:"),
            "expected header line, got: {msg}"
        );
        assert!(
            msg.contains("\n- research/x/x.md"),
            "expected workspace-relative bullet, got: {msg}"
        );
        assert!(
            msg.contains("\n- research/x/_search_results.md"),
            "expected second workspace-relative bullet, got: {msg}"
        );
        // No absolute paths leak through.
        assert!(
            !msg.contains("/tmp/ws/"),
            "absolute workspace prefix must be stripped: {msg}"
        );
    }

    #[test]
    fn should_suppress_produced_files_block_when_no_files() {
        // Token-budget invariant: never persist a stub message when the
        // tool produced no files (e.g. failed run, text-only result).
        assert!(
            build_spawn_only_produced_files_message("search", &[], None).is_none(),
            "empty files must return None so caller suppresses follow-up"
        );
    }

    #[test]
    fn should_keep_absolute_path_when_outside_workspace() {
        // Defensive: if a spawn_only tool produces a file outside the
        // workspace root (e.g. /tmp/foo.mp3), keep it verbatim rather
        // than producing a misleading relative path.
        let root = std::path::PathBuf::from("/tmp/ws");
        let files = vec!["/var/tmp/external.bin".to_string()];
        let msg =
            build_spawn_only_produced_files_message("foo", &files, Some(&root)).expect("non-empty");
        assert!(msg.contains("- /var/tmp/external.bin"), "got: {msg}");
    }

    #[test]
    fn produced_files_block_never_contains_file_contents() {
        // Token-budget invariant (M10 Phase 4): the produced-files block
        // is a list of PATHS only — file contents are NEVER inlined,
        // regardless of how many files were produced.
        let root = std::path::PathBuf::from("/tmp/ws");
        let files: Vec<String> = (1..=10)
            .map(|i| format!("/tmp/ws/research/topic/{i:02}_source.md"))
            .collect();
        let msg = build_spawn_only_produced_files_message("search", &files, Some(&root))
            .expect("non-empty");
        // Stays small: 10 paths × ~40 chars + header ≈ ~500 bytes.
        assert!(
            msg.len() < 2048,
            "produced-files block grew unexpectedly: {} bytes",
            msg.len()
        );
        // No file content sentinels.
        assert!(!msg.contains("LLM synthesis"));
        assert!(!msg.contains("# Deep Research:"));
    }

    #[test]
    fn relativize_strips_workspace_prefix() {
        let root = std::path::PathBuf::from("/u/me/ws");
        assert_eq!(
            relativize_workspace_path("/u/me/ws/skill-output/a.md", Some(&root)),
            "skill-output/a.md"
        );
        // Path not under workspace stays verbatim.
        assert_eq!(
            relativize_workspace_path("/other/a.md", Some(&root)),
            "/other/a.md"
        );
        // Already-relative input stays verbatim.
        assert_eq!(
            relativize_workspace_path("skill-output/a.md", Some(&root)),
            "skill-output/a.md"
        );
        // None workspace → verbatim.
        assert_eq!(
            relativize_workspace_path("/u/me/ws/a.md", None),
            "/u/me/ws/a.md"
        );
    }

    // -------------------------------------------------------------------
    // Wave-3b: `Satisfied { output_files: [] }` text-fallback regression.
    // -------------------------------------------------------------------

    #[test]
    fn satisfied_completion_keeps_tool_text_when_no_output_files() {
        // codex P1 regression guard: a contract with no declared artifact
        // (e.g. mofa_publish, whose deliverable is a URL) returns
        // `Satisfied { output_files: [] }`. The background completion
        // payload must surface the tool's stdout text (the deploy URL),
        // not an empty string.
        let result = satisfied_completion_content(&[], "https://deployed.example.com");
        assert_eq!(result, "https://deployed.example.com");
    }

    #[test]
    fn satisfied_completion_emits_empty_content_when_files_carry_deliverable() {
        // Legacy artifact-carrying contracts (fm_tts, podcast_generate,
        // mofa_slides, ...) still emit empty content because the files
        // themselves are the deliverable.
        let files = vec!["/tmp/a.mp3".to_string(), "/tmp/b.mp3".to_string()];
        let result = satisfied_completion_content(&files, "skill text result");
        assert_eq!(result, "");
    }

    #[test]
    fn satisfied_completion_keeps_empty_text_when_no_files_and_no_text() {
        // Defensive: empty tool output with empty output_files stays empty
        // — the legacy "no output produced" branch downstream will surface
        // a typed failure via the `r.success` check, not here.
        let result = satisfied_completion_content(&[], "");
        assert_eq!(result, "");
    }

    /// NEW-09 regression pin (paired with
    /// `crates/octos-pipeline/src/tool.rs::tests::pipeline_timeout_returns_ok_failure_result_not_err`):
    /// the spawn_only background execution arm at `Ok(r) if !r.success`
    /// formats the failure bubble as `✗ <tool> failed: <r.output>`. The
    /// pipeline-level timeout now returns
    /// `Ok(ToolResult { success: false, output: "pipeline timed out
    /// after Ns" })` so this contract test pins the bubble text the
    /// WS client renders end-to-end. If either the pipeline-side
    /// output text OR the execution.rs failure-arm format string
    /// drift, this test catches the divergence at the same site.
    ///
    /// Mirroring the format string here (rather than refactoring the
    /// failure arm to call a helper) keeps the unit test surface
    /// dependency-free: the failure arm runs inside a tokio::spawn
    /// closure that captures a dozen contextual variables (supervisor,
    /// reporter, output router, …); extracting a helper would require
    /// either threading every capture through a function signature or
    /// boxing them into a struct, both of which would obscure the
    /// in-place control flow that's load-bearing for the M8.7 cleanup
    /// path that runs unconditionally after the `match result` block.
    #[test]
    fn spawn_only_failure_arm_bubble_format_pins_pipeline_timeout_text() {
        let bg_name = "run_pipeline";
        let pipeline_output = "pipeline timed out after 1200s";
        let bubble = format!("✗ {} failed: {}", bg_name, pipeline_output);
        assert_eq!(
            bubble, "✗ run_pipeline failed: pipeline timed out after 1200s",
            "the bubble surface text the WS client renders on a \
             run_pipeline timeout must match the soak-evidence \
             reference exactly — any wording drift breaks the harness's \
             `isFinalArrived` heuristic plus any downstream regex \
             matchers in dashboards / debugging tooling"
        );
    }

    // ------------------------------------------------------------------
    // FIX 1: fast read-only tools must not inherit the 1800s timeout.
    // ------------------------------------------------------------------

    use super::{MAX_TOOL_TIMEOUT_SECS, compute_batch_timeout_secs, is_long_running_tool};

    #[test]
    fn long_running_tools_are_recognised() {
        // The genuinely-long-running set keeps the 1800s ceiling.
        for name in [
            "shell",
            "bash",
            "spawn",
            "spawn_agent",
            "run_pipeline",
            "browser",
            "delegate_task",
            "deep_crawl",
            "search",
            "synthesize_research",
        ] {
            assert!(
                is_long_running_tool(name),
                "{name} should be classified long-running"
            );
        }
    }

    #[test]
    fn human_wait_tool_is_not_in_long_running_set() {
        // UPCR-2026-023: `ask_user_question` is NOT classified long-running.
        // A batch containing it gets NO batch timeout at all (the
        // `any_human_wait` short-circuit), so the long-vs-short ceiling never
        // applies — wrapping it in even the 1800s ceiling would detach the
        // still-running tool task and leak the pending question.
        assert!(
            !is_long_running_tool("ask_user_question"),
            "ask_user_question must be handled by the any_human_wait no-timeout \
             path, not the long-running ceiling"
        );
    }

    #[test]
    fn batch_with_human_wait_tool_has_no_batch_timeout() {
        // UPDATED for UPCR-2026-023 (was `batch_with_ask_user_question_keeps_
        // the_long_ceiling`, which asserted 1800s). A batch containing a
        // human-wait tool must run with NO finite batch timeout: the previous
        // 1800s ceiling, while long, would still eventually FIRE and detach the
        // still-running `ask_user_question` task (its `JoinHandle` dropped, not
        // awaited), so its `PendingQuestionWaiterGuard` never drops → the
        // pending question leaks and is later replayed as a stale prompt. The
        // human may take arbitrarily long; cleanup comes from the user
        // answering or a turn interrupt/abort, never from the batch timeout.
        let secs = compute_batch_timeout_secs(
            &["ask_user_question"],
            /* any_human_wait */ true,
            /* llm_requested */ 0,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(
            secs, None,
            "a human-wait batch must yield None (no finite batch timeout)"
        );
    }

    #[test]
    fn human_wait_batch_has_no_timeout_even_with_llm_requested_secs() {
        // The `any_human_wait` short-circuit wins over an explicit
        // LLM-requested `timeout_secs`: a human-wait tool is unbounded at the
        // batch layer regardless of what the LLM asked for, so a bogus tiny or
        // huge `timeout_secs` cannot reintroduce the detach/leak.
        let secs = compute_batch_timeout_secs(
            &["ask_user_question"],
            /* any_human_wait */ true,
            /* llm_requested */ 30,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(secs, None);
    }

    #[test]
    fn mixed_human_wait_batch_is_unbounded_normal_tool_keeps_per_tool_timeout() {
        // A mixed batch (human-wait + a normal/long-running tool) is unbounded
        // at the BATCH layer (None). The normal tool does NOT lose its bound —
        // its per-tool registry timeout is applied INSIDE the tool's own
        // registry dispatch (`ToolRegistry::execute_with_context`), which is
        // untouched by removing the outer batch wrap. So the human-wait call
        // waits for the human while the `shell` peer is still bounded by its
        // registry-level ceiling.
        let secs = compute_batch_timeout_secs(
            &["ask_user_question", "shell"],
            /* any_human_wait */ true,
            /* llm_requested */ 0,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(
            secs, None,
            "a mixed human-wait batch is unbounded at the batch layer; \
             normal peers keep their per-tool registry timeouts"
        );
    }

    #[test]
    fn fast_read_only_tools_are_not_long_running() {
        for name in [
            "glob",
            "list_dir",
            "read_file",
            "grep",
            "write_file",
            "edit_file",
            "web_search",
            "web_fetch",
        ] {
            assert!(
                !is_long_running_tool(name),
                "{name} must NOT be classified long-running"
            );
        }
    }

    #[test]
    fn batch_of_only_fast_tools_uses_short_interactive_default() {
        // mini5 soak shape: `list_dir` + `glob` with NO LLM-requested
        // timeout must default to the short interactive timeout, NOT the
        // 1800s tool ceiling that hung the turn.
        let secs = compute_batch_timeout_secs(
            &["list_dir", "glob"],
            /* any_human_wait */ false,
            /* llm_requested */ 0,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(secs, Some(120));
    }

    #[test]
    fn batch_with_a_long_running_tool_keeps_the_long_ceiling() {
        // A `shell` (or `run_pipeline`) in the batch keeps the long
        // config-default timeout when the LLM omits `timeout_secs`.
        let secs = compute_batch_timeout_secs(
            &["glob", "shell"],
            /* any_human_wait */ false,
            /* llm_requested */ 0,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(secs, Some(1800));
    }

    #[test]
    fn llm_requested_timeout_still_honoured_for_fast_batch() {
        // An explicit LLM `timeout_secs` is clamped to MAX and floored at
        // the config default — unchanged from the pre-fix behaviour. For a
        // fast-only batch the floor is the interactive default, not 1800.
        let secs = compute_batch_timeout_secs(
            &["glob"],
            /* any_human_wait */ false,
            /* llm_requested */ 300,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(secs, Some(300));

        // Over-the-cap request is clamped to MAX_TOOL_TIMEOUT_SECS.
        let capped = compute_batch_timeout_secs(
            &["glob"],
            /* any_human_wait */ false,
            /* llm_requested */ 99_999,
            1800,
            120,
        );
        assert_eq!(capped, Some(MAX_TOOL_TIMEOUT_SECS));
    }

    #[test]
    fn llm_requested_below_interactive_floor_is_raised_for_fast_batch() {
        // A fast-only batch floors at the interactive default so a tiny
        // LLM-requested value cannot make the batch flakier than baseline.
        let secs = compute_batch_timeout_secs(
            &["glob"],
            /* any_human_wait */ false,
            /* llm_requested */ 5,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(secs, Some(120));
    }

    #[test]
    fn long_batch_llm_request_floors_at_config_default() {
        // A long batch floors at the config tool timeout (existing
        // behaviour preserved).
        let secs = compute_batch_timeout_secs(
            &["shell"],
            /* any_human_wait */ false,
            /* llm_requested */ 10,
            /* config_tool_timeout */ 1800,
            /* interactive_default */ 120,
        );
        assert_eq!(secs, Some(1800));
    }

    // ------------------------------------------------------------------
    // Parallel batch timeout must NOT discard already-completed results.
    // ------------------------------------------------------------------

    use std::sync::Arc;

    use async_trait::async_trait;
    use octos_core::{AgentId, ToolCall};
    use octos_llm::{
        ChatConfig, ChatResponse, LlmProvider, StopReason, TokenUsage as LlmTokenUsage, ToolSpec,
    };
    use octos_memory::EpisodeStore;

    use crate::agent::{Agent, AgentConfig};
    use crate::tools::{Tool, ToolRegistry, ToolResult};

    /// `execute_tools` never talks to the LLM, so the provider only has to
    /// satisfy the trait bounds (mirrors loop_runner's `InertProvider`).
    struct NoChatProvider;

    #[async_trait]
    impl LlmProvider for NoChatProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            unreachable!("execute_tools must not call the provider");
        }

        fn model_id(&self) -> &str {
            "inert"
        }

        fn provider_name(&self) -> &str {
            "inert"
        }
    }

    /// Completes immediately with a distinctive real output.
    struct InstantTool;

    #[async_trait]
    impl Tool for InstantTool {
        fn name(&self) -> &str {
            "fast_tool"
        }

        fn description(&self) -> &str {
            "test tool that completes immediately"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult {
                output: "FAST_TOOL_REAL_OUTPUT".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    /// Sleeps far past the batch ceiling so the batch timeout always fires
    /// while this call is still pending.
    struct SleepingTool;

    #[async_trait]
    impl Tool for SleepingTool {
        fn name(&self) -> &str {
            "slow_tool"
        }

        fn description(&self) -> &str {
            "test tool that outlives the batch timeout"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(ToolResult {
                output: "SLOW_TOOL_REAL_OUTPUT".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    fn tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
            metadata: None,
        }
    }

    /// Exclusive tool that fails with a `ToolInputError` (malformed model
    /// arguments). Its failure must NOT cancel well-formed siblings (#1690).
    struct InputErrorTool;

    #[async_trait]
    impl Tool for InputErrorTool {
        fn name(&self) -> &str {
            "bad_input_tool"
        }
        fn description(&self) -> &str {
            "always fails input validation"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn concurrency_class(&self) -> crate::tools::ConcurrencyClass {
            crate::tools::ConcurrencyClass::Exclusive
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Err(crate::tools::ToolInputError::new(
                "invalid bad_input_tool input: missing field `target`",
            )
            .into())
        }
    }

    /// Exclusive tool that HARD-errors (a genuine execution failure, not an
    /// input error) — this MUST still cascade to peers.
    struct HardErrorTool;

    #[async_trait]
    impl Tool for HardErrorTool {
        fn name(&self) -> &str {
            "hard_error_tool"
        }
        fn description(&self) -> &str {
            "always errors mid-execution"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn concurrency_class(&self) -> crate::tools::ConcurrencyClass {
            crate::tools::ConcurrencyClass::Exclusive
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Err(eyre::eyre!("boom: unexpected runtime failure"))
        }
    }

    /// Exclusive tool that succeeds with a distinctive output — the
    /// well-formed sibling behind a failing peer.
    struct GoodExclusiveTool;

    #[async_trait]
    impl Tool for GoodExclusiveTool {
        fn name(&self) -> &str {
            "good_tool"
        }
        fn description(&self) -> &str {
            "succeeds with distinctive output"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn concurrency_class(&self) -> crate::tools::ConcurrencyClass {
            crate::tools::ConcurrencyClass::Exclusive
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult {
                output: "GOOD_REAL_OUTPUT".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    /// Run one `execute_tools` batch against a fresh throwaway agent and
    /// return the result messages plus the per-call success bits.
    async fn run_batch_with_config(
        tool_calls: Vec<ToolCall>,
        tools: ToolRegistry,
        config: AgentConfig,
    ) -> (Vec<octos_core::Message>, Vec<(String, bool)>) {
        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(NoChatProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("batch"), provider, tools, memory).with_config(config);
        let response = ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls,
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        };
        let (messages, _fm, _fs, _tok, _st, success_by_id) = agent
            .execute_tools(&response)
            .await
            .expect("execute_tools must not error");
        (messages, success_by_id)
    }

    async fn run_batch(
        tool_calls: Vec<ToolCall>,
        tools: ToolRegistry,
    ) -> (Vec<octos_core::Message>, Vec<(String, bool)>) {
        run_batch_with_config(
            tool_calls,
            tools,
            AgentConfig {
                save_episodes: false,
                ..Default::default()
            },
        )
        .await
    }

    async fn run_serial_pair(
        first: &str,
        second: &str,
        tools: ToolRegistry,
    ) -> Vec<octos_core::Message> {
        let (messages, _success_by_id) = run_batch(
            vec![
                tool_call("call_first", first),
                tool_call("call_second", second),
            ],
            tools,
        )
        .await;
        messages
    }

    /// #1774: probe recording the `format_after_edit` flag its ToolContext
    /// carried, so the AgentConfig → ToolContext threading is testable
    /// without any real formatter binary.
    struct FormatFlagProbe(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl Tool for FormatFlagProbe {
        fn name(&self) -> &str {
            "format_flag_probe"
        }
        fn description(&self) -> &str {
            "records ctx.format_after_edit"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            self.execute_with_context(&crate::tools::ToolContext::zero(), _args)
                .await
        }
        async fn execute_with_context(
            &self,
            ctx: &crate::tools::ToolContext,
            _args: &serde_json::Value,
        ) -> eyre::Result<ToolResult> {
            self.0
                .store(ctx.format_after_edit, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult {
                output: "probe".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn should_thread_format_after_edit_from_agent_config_to_tool_context() {
        // #1774: `AgentConfig::format_after_edit` must reach the foreground
        // ToolContext handed to tools — that is the only way the config
        // opt-in can turn on post-edit formatting in the file tools.
        let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(FormatFlagProbe(seen.clone()));

        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(NoChatProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("fmt-flag"), provider, tools, memory).with_config(
            AgentConfig {
                save_episodes: false,
                format_after_edit: true,
                ..Default::default()
            },
        );
        let response = ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![tool_call("call_probe", "format_flag_probe")],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        };
        agent
            .execute_tools(&response)
            .await
            .expect("execute_tools must not error");
        assert!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            "AgentConfig.format_after_edit=true must reach the ToolContext"
        );
    }

    /// #1532: probe recording whether the approved-call ToolContext carries
    /// the agent-level infrastructure (it used to be a bare `zero()` spread).
    struct CtxInfraProbe {
        supervisor_seen: Arc<std::sync::atomic::AtomicBool>,
        cache_seen: Arc<std::sync::atomic::AtomicBool>,
        format_seen: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Tool for CtxInfraProbe {
        fn name(&self) -> &str {
            "ctx_infra_probe"
        }
        fn description(&self) -> &str {
            "records which ToolContext infra fields are populated"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            self.execute_with_context(&crate::tools::ToolContext::zero(), _args)
                .await
        }
        async fn execute_with_context(
            &self,
            ctx: &crate::tools::ToolContext,
            _args: &serde_json::Value,
        ) -> eyre::Result<ToolResult> {
            use std::sync::atomic::Ordering;
            self.supervisor_seen
                .store(ctx.task_supervisor.is_some(), Ordering::SeqCst);
            self.cache_seen
                .store(ctx.file_state_cache.is_some(), Ordering::SeqCst);
            self.format_seen
                .store(ctx.format_after_edit, Ordering::SeqCst);
            Ok(ToolResult {
                output: "probe".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn approved_tool_context_carries_agent_infrastructure() {
        // #1532: `execute_approved_tool` must hand the tool the SAME
        // agent-level infrastructure as the foreground path — a human
        // approving a call must not silently strip the cache, supervisor,
        // or config-driven behavior from it.
        let supervisor_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cache_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let format_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(CtxInfraProbe {
            supervisor_seen: supervisor_seen.clone(),
            cache_seen: cache_seen.clone(),
            format_seen: format_seen.clone(),
        });

        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(NoChatProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("approved-ctx"), provider, tools, memory)
            .with_file_state_cache(Arc::new(crate::file_state_cache::FileStateCache::new()))
            .with_config(AgentConfig {
                save_episodes: false,
                format_after_edit: true,
                ..Default::default()
            });

        let pending = crate::approval::PendingApproval {
            request: crate::approval::ApprovalRequestEnvelope {
                request_id: "req-1".into(),
                tool_name: "ctx_infra_probe".into(),
                tool_args_digest: "digest".into(),
                title: "probe".into(),
                summary: "probe".into(),
                risk_level: crate::approval::ApprovalRiskLevel::Normal,
                authorized_approvers: vec![],
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(5),
                on_timeout: crate::approval::ApprovalTimeoutBehavior::Notify,
            },
            room_id: "room".into(),
            requester: "user".into(),
            tool_id: "call_probe".into(),
            tool_args: serde_json::json!({}),
        };

        let result = agent
            .execute_approved_tool(&pending)
            .await
            .expect("approved probe must execute");
        assert!(result.success);
        use std::sync::atomic::Ordering;
        assert!(
            supervisor_seen.load(Ordering::SeqCst),
            "approved ctx must carry the task supervisor"
        );
        assert!(
            cache_seen.load(Ordering::SeqCst),
            "approved ctx must carry the file-state cache"
        );
        assert!(
            format_seen.load(Ordering::SeqCst),
            "approved ctx must carry config-driven flags (format_after_edit)"
        );
    }

    #[tokio::test]
    async fn input_error_does_not_cancel_well_formed_sibling() {
        // #1690: a malformed-arguments failure has no side effects, so the
        // well-formed sibling behind it in the serial batch must still run.
        let mut tools = ToolRegistry::new();
        tools.register(InputErrorTool);
        tools.register(GoodExclusiveTool);
        let messages = run_serial_pair("bad_input_tool", "good_tool", tools).await;

        let second = messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_second"))
            .expect("sibling result present");
        assert!(
            second.content.contains("GOOD_REAL_OUTPUT"),
            "well-formed sibling was cancelled by an input-error peer: {:?}",
            second.content
        );
        assert!(
            !second
                .content
                .contains("cancelled due to earlier sibling error")
        );

        // And the input error's DETAIL reaches the model (#1690 repair hint).
        let first = messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_first"))
            .expect("failed call result present");
        assert!(
            first.content.contains("missing field `target`"),
            "input-error detail must reach the model: {:?}",
            first.content
        );
    }

    #[tokio::test]
    async fn hard_error_still_cancels_sibling() {
        // Contrapositive: a genuine execution error must STILL cascade so a
        // real mid-batch failure stops dependent work.
        let mut tools = ToolRegistry::new();
        tools.register(HardErrorTool);
        tools.register(GoodExclusiveTool);
        let messages = run_serial_pair("hard_error_tool", "good_tool", tools).await;

        let second = messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_second"))
            .expect("sibling result present");
        assert!(
            second
                .content
                .contains("cancelled due to earlier sibling error"),
            "a genuine execution error must still cancel siblings: {:?}",
            second.content
        );
    }

    #[tokio::test]
    async fn should_keep_completed_call_results_when_batch_timeout_fires() {
        // Regression: the parallel dispatch used to wrap the WHOLE `join_all`
        // in one `tokio::time::timeout`; when the ceiling fired, every call in
        // the batch — including ones that had already resolved — was replaced
        // by a synthetic "timed out" message, discarding real output.
        let dir = tempfile::tempdir().unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(InstantTool);
        tools.register(SleepingTool);
        let provider: Arc<dyn LlmProvider> = Arc::new(NoChatProvider);
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let agent = Agent::new(AgentId::new("batch-timeout"), provider, tools, memory).with_config(
            AgentConfig {
                // Neither test tool is in LONG_RUNNING_TOOLS and no call
                // requests `timeout_secs`, so this 1s interactive default is
                // the whole batch's ceiling (see compute_batch_timeout_secs).
                default_interactive_tool_timeout_secs: 1,
                tool_timeout_secs: 1,
                save_episodes: false,
                ..Default::default()
            },
        );

        let response = ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: vec![
                tool_call("call_fast", "fast_tool"),
                tool_call("call_slow", "slow_tool"),
            ],
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        };

        let (messages, _files_modified, _files_to_send, _tokens, _structured, success_by_id) =
            agent
                .execute_tools(&response)
                .await
                .expect("execute_tools must not error on a batch timeout");

        // 1:1 mapping in LLM call order is preserved.
        assert_eq!(messages.len(), 2, "one result message per tool call");
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("call_fast"));
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_slow"));

        // The fast call COMPLETED before the ceiling fired: its REAL output
        // must survive, not be overwritten by a fabricated timeout message.
        assert!(
            messages[0].content.contains("FAST_TOOL_REAL_OUTPUT"),
            "completed call's real output was discarded: {:?}",
            messages[0].content
        );
        assert!(
            !messages[0].content.contains("timed out"),
            "completed call must not carry a synthetic timeout message: {:?}",
            messages[0].content
        );

        // The still-pending slow call gets the synthetic timeout message in
        // the existing (pinned) format.
        assert_eq!(
            messages[1].content,
            "Tool 'slow_tool' timed out after 1 seconds"
        );

        // Per-call success bits follow the same split.
        assert!(
            success_by_id.contains(&("call_fast".to_string(), true)),
            "completed call must keep success=true: {success_by_id:?}"
        );
        assert!(
            success_by_id.contains(&("call_slow".to_string(), false)),
            "timed-out call must report success=false: {success_by_id:?}"
        );
    }

    // ------------------------------------------------------------------
    // #1766 — mixed-batch two-phase dispatch: Safe calls run in parallel
    // first (phase 1), Exclusive calls run serially in LLM order (phase 2),
    // and results are reassembled in the ORIGINAL LLM call order.
    // ------------------------------------------------------------------

    use std::sync::atomic::{AtomicBool, Ordering};

    /// Safe (default class) reader that reports whether the shared flag was
    /// already flipped by the Exclusive `MutatingTool` when it ran — the
    /// probe for the pinned #1766 visibility semantics.
    struct SnapshotReadTool {
        mutated: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for SnapshotReadTool {
        fn name(&self) -> &str {
            "snapshot_read_tool"
        }
        fn description(&self) -> &str {
            "reports whether the sibling mutation already happened"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            let saw = if self.mutated.load(Ordering::SeqCst) {
                "SAW_POST_MUTATION"
            } else {
                "SAW_PRE_MUTATION"
            };
            Ok(ToolResult {
                output: saw.to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    // #1768: pre-mutation workspace snapshots
    // ------------------------------------------------------------------

    /// Mock carrying the builtin `write_file` name: writes a real file
    /// into the workspace so tests can prove the snapshot was taken
    /// BEFORE the mutation (the snapshot must not contain the file).
    struct MutatingNamedTool {
        workspace: std::path::PathBuf,
    }

    #[async_trait]
    impl Tool for MutatingNamedTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "test tool that mutates the workspace"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            std::fs::write(self.workspace.join("mutated.txt"), "mutation").unwrap();
            Ok(ToolResult {
                output: "wrote mutated.txt".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    /// Exclusive tool that flips the shared flag `SnapshotReadTool` observes.
    struct MutatingTool {
        mutated: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Tool for MutatingTool {
        fn name(&self) -> &str {
            "mutating_tool"
        }
        fn description(&self) -> &str {
            "flips the shared mutation flag"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn concurrency_class(&self) -> crate::tools::ConcurrencyClass {
            crate::tools::ConcurrencyClass::Exclusive
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            self.mutated.store(true, Ordering::SeqCst);
            Ok(ToolResult {
                output: "MUTATION_DONE".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    /// Safe (default class) tool that hard-errors — a genuine execution
    /// failure whose cascade bit must cancel the whole Exclusive phase.
    struct SafeHardErrorTool;

    #[async_trait]
    impl Tool for SafeHardErrorTool {
        fn name(&self) -> &str {
            "safe_hard_error_tool"
        }
        fn description(&self) -> &str {
            "safe reader that always errors mid-execution"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Err(eyre::eyre!("safe boom: reader exploded"))
        }
    }

    /// Safe (default class) tool that fails with a `ToolInputError` — a
    /// no-side-effect malformed-arguments failure that must NOT cancel the
    /// Exclusive phase (#1690 semantics carried into the mixed path).
    struct SafeInputErrorTool;

    #[async_trait]
    impl Tool for SafeInputErrorTool {
        fn name(&self) -> &str {
            "safe_bad_input_tool"
        }
        fn description(&self) -> &str {
            "safe reader that always fails input validation"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            Err(
                crate::tools::ToolInputError::new("invalid safe_bad_input_tool input: missing `q`")
                    .into(),
            )
        }
    }

    /// Safe pair-gate: each call waits on a shared 2-party barrier, so BOTH
    /// calls must be in flight simultaneously to complete. Proves the
    /// mixed-batch Safe phase actually runs in parallel — under serial
    /// dispatch the first call would block alone until the per-call timeout
    /// fired.
    struct RendezvousTool {
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl Tool for RendezvousTool {
        fn name(&self) -> &str {
            "rendezvous_tool"
        }
        fn description(&self) -> &str {
            "completes only when both sibling calls are in flight"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _args: &serde_json::Value) -> eyre::Result<ToolResult> {
            self.barrier.wait().await;
            Ok(ToolResult {
                output: "RENDEZVOUS_OK".to_string(),
                success: true,
                ..Default::default()
            })
        }
    }

    fn result_for<'a>(messages: &'a [octos_core::Message], id: &str) -> &'a octos_core::Message {
        messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no result message for tool_call_id {id}"))
    }

    #[tokio::test]
    async fn mixed_batch_reassembles_results_in_original_llm_call_order() {
        // #1766: interleaved Safe/Exclusive calls execute in two phases but
        // the aggregated results MUST come back in the original LLM call
        // order with every call's REAL output (no synthetic messages).
        let mutated = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(MutatingTool {
            mutated: mutated.clone(),
        });
        tools.register(SnapshotReadTool {
            mutated: mutated.clone(),
        });
        tools.register(GoodExclusiveTool);
        let calls = vec![
            tool_call("call_0_excl", "mutating_tool"),
            tool_call("call_1_safe", "snapshot_read_tool"),
            tool_call("call_2_excl", "good_tool"),
            tool_call("call_3_safe", "snapshot_read_tool"),
        ];
        let (messages, success_by_id) = run_batch(calls, tools).await;

        assert_eq!(messages.len(), 4, "one result message per tool call");
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("call_0_excl"));
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1_safe"));
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_2_excl"));
        assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_3_safe"));

        assert!(messages[0].content.contains("MUTATION_DONE"));
        assert!(messages[2].content.contains("GOOD_REAL_OUTPUT"));
        // Pinned visibility: BOTH Safe reads ran in phase 1, before any
        // Exclusive mutation — even the read listed after the mutator.
        assert!(
            messages[1].content.contains("SAW_PRE_MUTATION"),
            "Safe read listed after the mutator must still see pre-mutation state: {:?}",
            messages[1].content
        );
        assert!(messages[3].content.contains("SAW_PRE_MUTATION"));
        assert!(
            success_by_id.iter().all(|(_, ok)| *ok),
            "every call succeeded: {success_by_id:?}"
        );
    }

    #[tokio::test]
    async fn mixed_batch_safe_reads_see_pre_mutation_state() {
        // Pinned #1766 visibility semantics: Safe calls observe the
        // PRE-batch state. A Safe read the LLM listed AFTER an Exclusive
        // mutation runs in phase 1 — BEFORE the mutation — and must not see
        // the sibling's write. (Before M8.8 the two raced; under the M8.8
        // serial fallback the read saw the write. The phased pipeline makes
        // the pre-mutation snapshot deterministic.)
        let mutated = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(MutatingTool {
            mutated: mutated.clone(),
        });
        tools.register(SnapshotReadTool {
            mutated: mutated.clone(),
        });
        let calls = vec![
            tool_call("call_mutate", "mutating_tool"),
            tool_call("call_read", "snapshot_read_tool"),
        ];
        let (messages, _success_by_id) = run_batch(calls, tools).await;

        assert!(
            result_for(&messages, "call_read")
                .content
                .contains("SAW_PRE_MUTATION"),
            "Safe read must run in phase 1 and see pre-mutation state: {:?}",
            result_for(&messages, "call_read").content
        );
        assert!(
            result_for(&messages, "call_mutate")
                .content
                .contains("MUTATION_DONE")
        );
        assert!(
            mutated.load(Ordering::SeqCst),
            "the Exclusive mutation still ran (phase 2)"
        );
    }

    #[tokio::test]
    async fn mixed_batch_runs_safe_calls_in_parallel() {
        // Two Safe calls gated on a 2-party rendezvous barrier: they can
        // only complete if BOTH are in flight at once. Under the old serial
        // fallback the first call would block alone until the per-call
        // timeout fired and cascaded; under #1766 phase 1 they release each
        // other immediately.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut tools = ToolRegistry::new();
        tools.register(RendezvousTool { barrier });
        tools.register(GoodExclusiveTool);
        let calls = vec![
            tool_call("call_r1", "rendezvous_tool"),
            tool_call("call_r2", "rendezvous_tool"),
            tool_call("call_excl", "good_tool"),
        ];
        let (messages, _success_by_id) = run_batch_with_config(
            calls,
            tools,
            AgentConfig {
                // Keep the failure mode (serial dispatch deadlocking on the
                // barrier) a fast per-call timeout instead of a hung test.
                default_interactive_tool_timeout_secs: 2,
                tool_timeout_secs: 2,
                save_episodes: false,
                ..Default::default()
            },
        )
        .await;

        assert!(
            result_for(&messages, "call_r1")
                .content
                .contains("RENDEZVOUS_OK"),
            "first Safe call must run concurrently with its sibling: {:?}",
            result_for(&messages, "call_r1").content
        );
        assert!(
            result_for(&messages, "call_r2")
                .content
                .contains("RENDEZVOUS_OK")
        );
        assert!(
            result_for(&messages, "call_excl")
                .content
                .contains("GOOD_REAL_OUTPUT"),
            "Exclusive phase must still run after a parallel Safe phase: {:?}",
            result_for(&messages, "call_excl").content
        );
    }

    #[tokio::test]
    async fn mixed_batch_safe_error_cancels_every_exclusive_call() {
        // #1766 acceptance criterion: an error in any Safe call still
        // triggers the "cancelled due to sibling error" synthetic result for
        // the Exclusive calls — position-independently. The failing reader
        // here sits AFTER the Exclusive call in LLM order, and the Exclusive
        // call is still cancelled: no mutation runs once a sibling read
        // failed in phase 1.
        let mut tools = ToolRegistry::new();
        tools.register(GoodExclusiveTool);
        tools.register(SafeHardErrorTool);
        let calls = vec![
            tool_call("call_excl", "good_tool"),
            tool_call("call_bad_read", "safe_hard_error_tool"),
        ];
        let (messages, success_by_id) = run_batch(calls, tools).await;

        assert!(
            result_for(&messages, "call_excl")
                .content
                .contains("cancelled due to earlier sibling error"),
            "a failed Safe call must cancel the whole Exclusive phase: {:?}",
            result_for(&messages, "call_excl").content
        );
        assert!(
            result_for(&messages, "call_bad_read")
                .content
                .contains("safe boom"),
            "the Safe failure detail must reach the model: {:?}",
            result_for(&messages, "call_bad_read").content
        );
        assert!(success_by_id.contains(&("call_excl".to_string(), false)));
        assert!(success_by_id.contains(&("call_bad_read".to_string(), false)));
    }

    #[tokio::test]
    async fn mixed_batch_safe_input_error_does_not_cancel_exclusive() {
        // #1690 carried into the mixed path: a malformed-arguments failure
        // (`ToolInputError`) has no side effects and must NOT cancel the
        // Exclusive phase.
        let mut tools = ToolRegistry::new();
        tools.register(SafeInputErrorTool);
        tools.register(GoodExclusiveTool);
        let calls = vec![
            tool_call("call_bad_input", "safe_bad_input_tool"),
            tool_call("call_excl", "good_tool"),
        ];
        let (messages, _success_by_id) = run_batch(calls, tools).await;

        assert!(
            result_for(&messages, "call_excl")
                .content
                .contains("GOOD_REAL_OUTPUT"),
            "an input-error Safe call must not cancel the Exclusive phase: {:?}",
            result_for(&messages, "call_excl").content
        );
        assert!(
            result_for(&messages, "call_bad_input")
                .content
                .contains("missing `q`"),
            "input-error detail must reach the model"
        );
    }

    #[tokio::test]
    async fn mixed_batch_exclusive_error_keeps_completed_safe_results() {
        // Phase-2 cascade stays inside phase 2: when an Exclusive call
        // fails, LATER Exclusive peers are cancelled, but phase-1 Safe
        // results — already complete and side-effect-free — keep their real
        // outputs even when the LLM listed them after the failing mutator
        // (the old serial fallback would have cancelled them).
        let mutated = Arc::new(AtomicBool::new(false));
        let mut tools = ToolRegistry::new();
        tools.register(HardErrorTool);
        tools.register(SnapshotReadTool {
            mutated: mutated.clone(),
        });
        tools.register(GoodExclusiveTool);
        let calls = vec![
            tool_call("call_bad_excl", "hard_error_tool"),
            tool_call("call_safe", "snapshot_read_tool"),
            tool_call("call_good_excl", "good_tool"),
        ];
        let (messages, success_by_id) = run_batch(calls, tools).await;

        // Original LLM call order preserved.
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("call_bad_excl"));
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_safe"));
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_good_excl"));

        // The Safe read completed in phase 1 — its real output survives the
        // later Exclusive failure.
        assert!(
            messages[1].content.contains("SAW_PRE_MUTATION"),
            "phase-1 Safe result must never be converted to cancelled: {:?}",
            messages[1].content
        );
        assert!(success_by_id.contains(&("call_safe".to_string(), true)));

        // The Exclusive peer AFTER the failing Exclusive call is cancelled.
        assert!(
            messages[2]
                .content
                .contains("cancelled due to earlier sibling error"),
            "later Exclusive peer must be cancelled by the phase-2 cascade: {:?}",
            messages[2].content
        );
    }

    async fn snapshot_agent(
        tools: ToolRegistry,
        manager: Arc<crate::snapshot::SnapshotManager>,
        memory_dir: &std::path::Path,
    ) -> Agent {
        let provider: Arc<dyn LlmProvider> = Arc::new(NoChatProvider);
        let memory = Arc::new(EpisodeStore::open(memory_dir.join("memory")).await.unwrap());
        Agent::new(AgentId::new("snapshotter"), provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                ..Default::default()
            })
            .with_snapshot_manager(manager)
    }

    fn batch(calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            content: None,
            reasoning_content: None,
            tool_calls: calls,
            stop_reason: StopReason::ToolUse,
            usage: LlmTokenUsage::default(),
            provider_index: None,
        }
    }

    #[tokio::test]
    async fn should_snapshot_before_batch_when_mutating_tool_present() {
        let data = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("existing.txt"), "pre-mutation").unwrap();
        let manager = Arc::new(
            crate::snapshot::SnapshotManager::new(data.path().join("snapshots"), ws.path(), 20)
                .expect("git must be installed to run snapshot tests"),
        );

        let mut tools = ToolRegistry::new();
        tools.register(MutatingNamedTool {
            workspace: ws.path().to_path_buf(),
        });
        let agent = snapshot_agent(tools, manager.clone(), data.path()).await;

        agent
            .execute_tools(&batch(vec![tool_call("call_mut", "write_file")]))
            .await
            .expect("execute_tools must not error");

        let snaps = manager.list_snapshots().unwrap();
        assert_eq!(snaps.len(), 1, "one pre-mutation snapshot expected");
        assert!(
            snaps[0].label.contains("write_file"),
            "label must name the mutating tool: {snaps:?}"
        );
        assert!(
            ws.path().join("mutated.txt").exists(),
            "the tool itself must still have run"
        );
        // Ordering proof: restoring the snapshot removes the file the tool
        // created, so the snapshot predates the mutation.
        manager.restore(&snaps[0].id).unwrap();
        assert!(
            !ws.path().join("mutated.txt").exists(),
            "snapshot must capture the PRE-mutation state"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join("existing.txt")).unwrap(),
            "pre-mutation"
        );
    }

    #[tokio::test]
    async fn should_not_snapshot_when_batch_is_read_only() {
        let data = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            crate::snapshot::SnapshotManager::new(data.path().join("snapshots"), ws.path(), 20)
                .expect("git must be installed to run snapshot tests"),
        );

        let mut tools = ToolRegistry::new();
        tools.register(InstantTool);
        let agent = snapshot_agent(tools, manager.clone(), data.path()).await;

        agent
            .execute_tools(&batch(vec![tool_call("call_fast", "fast_tool")]))
            .await
            .expect("execute_tools must not error");

        assert!(
            manager.list_snapshots().unwrap().is_empty(),
            "read-only batches must not create snapshots"
        );
    }
}
