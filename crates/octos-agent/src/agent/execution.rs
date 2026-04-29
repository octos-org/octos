//! Tool execution: dispatching tool calls with hooks and timeout handling.
//!
//! # Batch admission (M8.8)
//!
//! Each turn of the agent loop receives a batch of tool calls from the LLM.
//! Before M8.8 every call in a batch fired in parallel, which races when a
//! mutating tool (shell, write_file, edit_file, diff_edit, save_memory) sits
//! next to a reader in the same batch. The executor now consults
//! [`crate::tools::Tool::concurrency_class`] for every call and picks one of
//! two admission strategies:
//!
//! - **All-Safe batch** — the classic path. Every call is [`ConcurrencyClass::Safe`]
//!   (read-only, side-effect-free). The executor spawns each call as a detached
//!   task and aggregates via `futures::join_all`, preserving call order.
//! - **Any-Exclusive batch** — new M8.8 path. At least one call reports
//!   [`ConcurrencyClass::Exclusive`]. The executor runs calls serially in LLM
//!   call order. On the first error (including hook denials and panics), the
//!   remaining peers are skipped and each receives a synthetic
//!   "cancelled due to sibling error" [`Message`] so the LLM still sees a
//!   result for every `tool_call_id`.
//!
//! The split is pessimistic — a batch containing one Exclusive + four Safe
//! tools still serializes the whole batch. An optimised "run Safe in parallel,
//! then Exclusive in order" pipeline is explicitly deferred (see the M8.8 spec).

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
use crate::task_supervisor::TaskRuntimeState;
use crate::tools::spawn::{BackgroundResultKind, BackgroundResultPayload};
use crate::tools::{ConcurrencyClass, TOOL_CTX, TURN_ATTACHMENT_CTX, ToolContext};
use crate::workspace_contract::{SpawnTaskContractResult, enforce_spawn_task_contract};

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
);

fn should_auto_send_tool_files(
    suppress_auto_send_files: bool,
    explicit_send_file_requested: bool,
    tool_name: &str,
) -> bool {
    !(suppress_auto_send_files || explicit_send_file_requested && tool_name != "send_file")
}

/// Produce the composite system-prompt text (worker prompt + realtime sensor
/// summary) used at the top of every agent turn. Centralizing this in
/// `execution.rs` keeps the message-building policy in a single location so
/// the conversation loop and task loop compose the same prompt.
///
/// Returns the prompt text the caller should paste into the first system
/// `Message`. When no realtime controller is attached this is byte-identical
/// to the stored system prompt.
pub(super) fn compose_system_prompt(agent: &Agent) -> String {
    let mut content = agent.system_prompt_snapshot();
    if let Some(summary) = agent.realtime_sensor_summary() {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
        content.push_str(&summary);
    }
    content
}

impl Agent {
    /// Spawn a single tool call as a detached `tokio::spawn` task.
    ///
    /// The returned [`JoinHandle`] yields the per-call [`ToolCallResult`]:
    /// tool-output [`Message`], modified file paths, files-to-send paths, and
    /// optional sub-agent [`TokenUsage`]. This is the worker used by both
    /// dispatch strategies in [`Agent::execute_tools`] — parallel (Safe) runs
    /// many in flight via `join_all`; serial (any-Exclusive) spawns one,
    /// awaits it, then spawns the next.
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

        tokio::spawn(async move {
            let tool_start = Instant::now();
            debug!(tool = %tc_name, tool_id = %tc_id, "executing tool");

            reporter.report(ProgressEvent::ToolStarted {
                name: tc_name.clone(),
                tool_id: tc_id.clone(),
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
                tracing::info!(
                    tool = %tc_name,
                    "running spawn_only tool in background"
                );
                let bg_tools = tools.clone();
                let bg_name = tc_name.clone();
                let bg_args = effective_args.clone();
                let bg_sender = tools.background_result_sender();
                let bg_tc_id = tc_id.clone();
                let task_id =
                    tools.register_task_with_input(&tc_name, &tc_id, Some(effective_args.clone()));
                tools.mark_spawn_only_invoked();
                let bg_supervisor = tools.supervisor();
                let bg_reporter = reporter.clone();
                // M8.10 follow-up (#649): snapshot the originating turn's
                // thread_id NOW, before any other turn can swap reporters
                // or rotate the api_channel sticky map. Late-arriving
                // background results stamp this onto their OutboundMessage
                // metadata so the wire-side SSE event lands under the
                // correct turn even after subsequent unrelated user turns
                // have advanced the per-chat sticky thread_id.
                let bg_originating_thread_id = bg_reporter.thread_id().map(str::to_string);
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
                let bg_session_id_for_watcher = format!("agent:{}", tc_id);
                tokio::spawn(async move {
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
                    let mut result = TOOL_CTX
                        .scope(
                            make_ctx(),
                            bg_tools.execute_with_context(&make_ctx(), &bg_name, &bg_args),
                        )
                        .await;

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
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            result = TOOL_CTX
                                .scope(
                                    make_ctx(),
                                    bg_tools.execute_with_context(&make_ctx(), &bg_name, &bg_args),
                                )
                                .await;
                        }
                    }

                    match result {
                        Ok(r) if r.success => {
                            tracing::info!(
                                tool = %bg_name,
                                success = true,
                                "spawn_only background tool completed"
                            );
                            match enforce_spawn_task_contract(
                                &bg_tools,
                                &bg_name,
                                &bg_tc_id,
                                &r.files_to_send,
                                bg_started_at,
                                Some((&bg_supervisor, &task_id)),
                            )
                            .await
                            {
                                SpawnTaskContractResult::Satisfied { output_files } => {
                                    let result_persisted = if let Some(ref sender) = bg_sender {
                                        sender(BackgroundResultPayload {
                                            task_label: bg_name.clone(),
                                            content: String::new(),
                                            kind: BackgroundResultKind::Notification,
                                            media: output_files.clone(),
                                            originating_thread_id: bg_originating_thread_id.clone(),
                                        })
                                        .await
                                    } else {
                                        false
                                    };

                                    if result_persisted {
                                        if let Err(validation_error) = bg_supervisor
                                            .mark_completed_with_validation(
                                                &task_id,
                                                output_files.clone(),
                                            )
                                        {
                                            tracing::warn!(
                                                tool = %bg_name,
                                                files = ?output_files,
                                                error = %validation_error,
                                                "workspace contract satisfied but supervisor artifact validation rejected outputs"
                                            );
                                            if let Some(ref sender) = bg_sender {
                                                let _ = sender(BackgroundResultPayload {
                                                    task_label: bg_name.clone(),
                                                    content: format!(
                                                        "✗ {} failed: {}",
                                                        bg_name, validation_error
                                                    ),
                                                    kind: BackgroundResultKind::Notification,
                                                    media: vec![],
                                                    originating_thread_id: bg_originating_thread_id
                                                        .clone(),
                                                })
                                                .await;
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
                                            originating_thread_id: bg_originating_thread_id.clone(),
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
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
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
                                        let err_msg = format!(
                                            "completed with no output (stdout: {})",
                                            r.output.chars().take(200).collect::<String>()
                                        );
                                        tracing::warn!(
                                            tool = %bg_name,
                                            "spawn_only tool produced no files"
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
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
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
                                            "tool_call_id": bg_tc_id
                                        });
                                        let mut delivered = false;
                                        for attempt in 0..3 {
                                            match bg_tools.execute("send_file", &send_args).await {
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
                                                originating_thread_id: bg_originating_thread_id
                                                    .clone(),
                                            })
                                            .await;
                                        }
                                    } else {
                                        match bg_supervisor.mark_completed_with_validation(
                                            &task_id,
                                            sent_files.clone(),
                                        ) {
                                            Ok(()) => {
                                                let file_info = format!(
                                                    " ({})",
                                                    sent_files
                                                        .iter()
                                                        .map(|f| f.rsplit('/').next().unwrap_or(f))
                                                        .collect::<Vec<_>>()
                                                        .join(", ")
                                                );
                                                if let Some(ref sender) = bg_sender {
                                                    let _ = sender(BackgroundResultPayload {
                                                        task_label: bg_name.clone(),
                                                        content: format!(
                                                            "✓ {} completed{}",
                                                            bg_name, file_info
                                                        ),
                                                        kind: BackgroundResultKind::Notification,
                                                        media: vec![],
                                                        originating_thread_id:
                                                            bg_originating_thread_id.clone(),
                                                    })
                                                    .await;
                                                }
                                            }
                                            Err(validation_error) => {
                                                tracing::warn!(
                                                    tool = %bg_name,
                                                    files = ?sent_files,
                                                    error = %validation_error,
                                                    "delivered outputs but supervisor artifact validation rejected them"
                                                );
                                                if let Some(ref sender) = bg_sender {
                                                    let _ = sender(BackgroundResultPayload {
                                                        task_label: bg_name.clone(),
                                                        content: format!(
                                                            "✗ {} failed: {}",
                                                            bg_name, validation_error
                                                        ),
                                                        kind: BackgroundResultKind::Notification,
                                                        media: vec![],
                                                        originating_thread_id:
                                                            bg_originating_thread_id.clone(),
                                                    })
                                                    .await;
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
                                    originating_thread_id: bg_originating_thread_id.clone(),
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
                                    originating_thread_id: bg_originating_thread_id.clone(),
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
                return (
                    Message {
                        role: MessageRole::Tool,
                        content: tools.spawn_only_message(&tc_name),
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
                ..ToolContext::zero()
            };
            // Thread the typed context into execute_with_context. Legacy tools
            // whose trait impl only overrides `execute` still work via the
            // default delegation path; migrated tools read the typed fields.
            // TOOL_CTX is still scoped for plugin tools that read the task-local.
            let result = TOOL_CTX
                .scope(
                    ctx.clone(),
                    tools.execute_with_context(&ctx, &tc_name, &effective_args),
                )
                .await;

            let duration = tool_start.elapsed();

            let (
                content,
                tool_files_modified,
                tool_files_to_send,
                tool_tokens,
                tool_success,
                tool_structured_metadata,
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

                    (
                        format!("Error: {e}"),
                        Vec::new(),
                        Vec::new(),
                        None,
                        false,
                        None,
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
            )
        })
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
    )> {
        let tool_names: Vec<&str> = response
            .tool_calls
            .iter()
            .map(|tc| tc.name.as_str())
            .collect();
        let explicit_send_file_requested =
            response.tool_calls.iter().any(|tc| tc.name == "send_file");

        // M8.8 — classify the batch and pick an admission strategy.
        let any_exclusive = response
            .tool_calls
            .iter()
            .any(|tc| self.tools.concurrency_class(&tc.name) == ConcurrencyClass::Exclusive);

        tracing::info!(
            parallel_tools = response.tool_calls.len(),
            tool_names = %tool_names.join(", "),
            dispatch = if any_exclusive { "serial" } else { "parallel" },
            "executing tool batch"
        );

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
        let tool_timeout_secs = if llm_requested_timeout > 0 {
            llm_requested_timeout
                .min(MAX_TOOL_TIMEOUT_SECS)
                .max(self.config.tool_timeout_secs)
        } else {
            self.config.tool_timeout_secs
        };
        let tool_timeout = Duration::from_secs(tool_timeout_secs);

        let results: Vec<ToolCallResult> = if any_exclusive {
            // Serial admission: run each tool in LLM call order, bail out of
            // the remaining calls if any one errors and emit synthetic
            // "cancelled" results so the LLM still sees every tool_call_id.
            self.execute_serial_batch(
                response,
                explicit_send_file_requested,
                &turn_attachment_ctx,
                tool_timeout,
                tool_timeout_secs,
            )
            .await
        } else {
            // Parallel admission — today's behaviour. Spawn every tool call
            // as a detached task and join them.
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

            match tokio::time::timeout(tool_timeout, futures::future::join_all(handles)).await {
                Ok(results) => results
                    .into_iter()
                    .zip(response.tool_calls.iter())
                    .map(|(r, tc)| r.unwrap_or_else(|e| panic_result(tc, &e.to_string())))
                    .collect(),
                Err(_) => {
                    tracing::error!(
                        timeout_secs = tool_timeout_secs,
                        tool_count = response.tool_calls.len(),
                        tools = %tool_names.join(", "),
                        "tool execution timed out -- spawned tasks continue running for cleanup"
                    );
                    let messages = response
                        .tool_calls
                        .iter()
                        .map(|tc| Message {
                            role: MessageRole::Tool,
                            content: format!(
                                "Tool '{}' timed out after {} seconds",
                                tc.name, tool_timeout_secs
                            ),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: Some(tc.id.clone()),
                            reasoning_content: None,
                            client_message_id: None,
                            thread_id: None,
                            timestamp: chrono::Utc::now(),
                        })
                        .collect();
                    return Ok((messages, vec![], vec![], TokenUsage::default(), Vec::new()));
                }
            }
        };

        // Log completion of the tool batch.
        let result_sizes: Vec<usize> = results
            .iter()
            .map(|(m, _, _, _, _, _)| m.content.len())
            .collect();
        let total_result_bytes: usize = result_sizes.iter().sum();
        tracing::info!(
            parallel_tools = results.len(),
            dispatch = if any_exclusive { "serial" } else { "parallel" },
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

        for (
            message,
            tool_files_modified,
            tool_files_to_send,
            tool_tokens,
            _success,
            tool_structured_metadata,
        ) in results
        {
            messages.push(message);
            files_modified.extend(tool_files_modified);
            files_to_send.extend(tool_files_to_send);
            if let Some(tokens) = tool_tokens {
                tokens_used.input_tokens += tokens.input_tokens;
                tokens_used.output_tokens += tokens.output_tokens;
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
        ))
    }

    /// Serial dispatch for batches that contain at least one Exclusive tool (M8.8).
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
    async fn execute_serial_batch(
        &self,
        response: &ChatResponse,
        explicit_send_file_requested: bool,
        turn_attachment_ctx: &crate::tools::TurnAttachmentContext,
        tool_timeout: Duration,
        tool_timeout_secs: u64,
    ) -> Vec<ToolCallResult> {
        let mut results: Vec<ToolCallResult> = Vec::with_capacity(response.tool_calls.len());
        let mut cancelled = false;

        for (idx, tool_call) in response.tool_calls.iter().enumerate() {
            if cancelled {
                let skipped = response.tool_calls.len() - idx;
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

            let outcome = match tokio::time::timeout(tool_timeout, handle).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::warn!(
                        tool = %tool_call.name,
                        error = %e,
                        "serial tool task panicked"
                    );
                    panic_result(tool_call, &e.to_string())
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = tool_timeout_secs,
                        tool = %tool_call.name,
                        tool_id = %tool_call.id,
                        "serial tool execution timed out"
                    );
                    (
                        Message {
                            role: MessageRole::Tool,
                            content: format!(
                                "Tool '{}' timed out after {} seconds",
                                tool_call.name, tool_timeout_secs
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
                    )
                }
            };

            // The per-call success bit (the 5-th tuple element) drives the
            // cascade. Every failure path in `spawn_tool_task` — tool error,
            // hook denial, panic, timeout — sets it to `false`, so we do not
            // need to peek at the message content.
            let failed = !outcome.4;
            results.push(outcome);
            if failed {
                cancelled = true;
            }
        }

        results
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
    )
}

#[cfg(test)]
mod tests {
    use super::should_auto_send_tool_files;

    #[test]
    fn explicit_send_file_turn_suppresses_plugin_auto_send_for_other_tools() {
        assert!(!should_auto_send_tool_files(false, true, "mofa_slides"));
        assert!(should_auto_send_tool_files(false, true, "send_file"));
    }

    #[test]
    fn auto_send_respects_global_suppression_flag() {
        assert!(!should_auto_send_tool_files(true, false, "mofa_slides"));
    }
}
