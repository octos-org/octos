//! Synchronous DelegateTool.
//!
//! `DelegateTool` is the blocking sibling of [`SpawnTool`]. The parent agent
//! issues `delegate_task`, waits for the child to reach a terminal lifecycle
//! state, and returns the child's output through the contract-gated delivery
//! path. Children inherit the parent's workspace contract but run under the
//! `group:delegated` deny list (no re-delegation, no user messaging, no
//! memory writes).
//!
//! The [`DepthBudget`] is typed and serde-stable. Each level adds 1 to
//! `current`; when a child would exceed `max` (default [`MAX_DEPTH`] = 2),
//! `execute` returns a typed [`HarnessError::DelegateDepthExceeded`] in
//! synchronous failure mode so the parent surfaces the error immediately.
//!
//! Invariants locked in by the acceptance tests:
//! 1. MAX_DEPTH = 2 — grandchild returns typed error without spawning.
//! 2. Child `DepthBudget.current` = parent's + 1.
//! 3. `group:delegated` deny list applied at child dispatch.
//! 4. Child returns through the contract-gate (no bypass).
//! 5. Parent blocks until child reaches `TaskLifecycleState::Ready` or
//!    `TaskLifecycleState::Failed`.
//! 6. Child `TaskId` is in the same session but fresh.
//! 7. `DepthBudget` round-trips via serde.
//! 8. Zero new `unsafe` — nothing here touches raw pointers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use metrics::counter;
use octos_core::{AgentId, Task, TaskContext, TaskId, TaskKind};
use octos_llm::LlmProvider;
use octos_memory::EpisodeStore;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{Tool, ToolContext, ToolPolicy, ToolRegistry, ToolResult};
use crate::harness_errors::HarnessError;
use crate::harness_events::HARNESS_EVENT_SCHEMA_V1;
use crate::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use crate::{Agent, AgentConfig};

/// Group that deny-lists every child surface forbidden under delegation.
pub const DELEGATED_DENY_GROUP: &str = "group:delegated";

/// Hard ceiling on delegation depth. Level-0 (top) delegates produce
/// level-1 children; level-1 children may delegate one final level-2
/// child; any attempt past that must return `DelegateDepthExceeded`.
pub const MAX_DEPTH: u32 = 2;

/// Prometheus counter emitted on every terminal delegation outcome.
pub const DELEGATION_METRIC: &str = "octos_delegation_total";

/// Harness delegation event kind — surfaces through
/// `octos.harness.event.v1` with `{ kind: "delegation", ... }`.
pub const DELEGATION_EVENT_KIND: &str = "delegation";

/// Typed delegation depth budget.
///
/// `current` is the level the *owning* tool sits at. A parent at level 0
/// spawns children at level 1; those children run a `DelegateTool` whose
/// `current == 1` and whose own children would run at level 2. A child at
/// level `current` whose `current >= max` rejects delegation without
/// spawning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DepthBudget {
    pub current: u32,
    pub max: u32,
}

impl Default for DepthBudget {
    fn default() -> Self {
        Self::top_level()
    }
}

impl DepthBudget {
    /// Top-level delegation budget: depth 0, cap at [`MAX_DEPTH`].
    pub const fn top_level() -> Self {
        Self {
            current: 0,
            max: MAX_DEPTH,
        }
    }

    /// Budget the *owning* tool at the given level with the configured cap.
    pub const fn at_level(current: u32) -> Self {
        Self {
            current,
            max: MAX_DEPTH,
        }
    }

    /// True when the owning level has already saturated the budget and
    /// may not spawn any further children.
    pub const fn is_exhausted(&self) -> bool {
        self.current >= self.max
    }

    /// The depth at which the next child would run (current + 1).
    pub const fn child_depth(&self) -> u32 {
        self.current.saturating_add(1)
    }

    /// Produce the budget the child should carry (incremented current).
    /// Returns `Err` if the parent's budget is already exhausted.
    pub fn increment(self) -> std::result::Result<Self, HarnessError> {
        if self.is_exhausted() {
            return Err(HarnessError::DelegateDepthExceeded {
                depth: self.current,
                limit: self.max,
                message: format!(
                    "delegation depth budget exhausted at depth {} (limit {})",
                    self.current, self.max
                ),
            });
        }
        Ok(Self {
            current: self.child_depth(),
            max: self.max,
        })
    }
}

/// Outcome labels used for the delegation metric and event payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationOutcome {
    Accepted,
    Completed,
    Failed,
    DepthExceeded,
}

impl DelegationOutcome {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::DepthExceeded => "depth_exceeded",
        }
    }
}

fn record_delegation(depth: u32, outcome: DelegationOutcome) {
    counter!(
        DELEGATION_METRIC,
        "depth" => depth.to_string(),
        "outcome" => outcome.label().to_string()
    )
    .increment(1);
}

/// Harness delegation event payload as surfaced to operators.
///
/// Consumers should read these off the `octos.harness.event.v1` stream.
/// The event's `kind` discriminator is always `"delegation"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationEvent {
    pub schema: String,
    pub kind: String,
    pub depth: u32,
    pub parent_task_id: String,
    pub child_task_id: String,
    pub outcome: String,
}

impl DelegationEvent {
    pub fn new(
        depth: u32,
        parent_task_id: impl Into<String>,
        child_task_id: impl Into<String>,
        outcome: DelegationOutcome,
    ) -> Self {
        Self {
            schema: HARNESS_EVENT_SCHEMA_V1.to_string(),
            kind: DELEGATION_EVENT_KIND.to_string(),
            depth,
            parent_task_id: parent_task_id.into(),
            child_task_id: child_task_id.into(),
            outcome: outcome.label().to_string(),
        }
    }
}

fn emit_delegation_event(sink_path: Option<&str>, event: &DelegationEvent) -> std::io::Result<()> {
    let Some(path) = sink_path else {
        return Ok(());
    };
    // The canonical `HarnessEvent` schema has `kind` as a tag with a fixed
    // enum of payload kinds. Delegation events are surfaced over the same
    // sink as structured NDJSON with the shared schema; we write our own
    // `{ schema, kind: "delegation", ... }` line rather than bolting a new
    // variant onto the payload enum, because delegation is observed at the
    // DelegateTool boundary and not dispatched through TaskSupervisor like
    // Progress/Phase events.
    let json = serde_json::to_string(event)
        .map_err(|error| std::io::Error::other(format!("serialize delegation event: {error}")))?;
    let path = if let Some(rest) = path.strip_prefix("file://") {
        PathBuf::from(rest.strip_prefix("localhost").unwrap_or(rest))
    } else {
        PathBuf::from(path)
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    use std::io::Write;
    writeln!(file, "{json}")
}

/// Build the tool policy applied to a delegated child registry.
///
/// The caller's `allow` list (if any) narrows the child toolset further;
/// `group:delegated` is always added to `deny`. Deny-wins semantics mean the
/// group is effective regardless of what the parent allows.
pub fn build_delegated_child_policy(allowed_tools: Vec<String>) -> ToolPolicy {
    ToolPolicy {
        allow: allowed_tools,
        deny: vec![DELEGATED_DENY_GROUP.to_string()],
        ..Default::default()
    }
}

/// Synchronous delegation tool.
///
/// The parent blocks until the child reaches a terminal lifecycle state.
/// Child workers inherit the parent's `working_dir` (and therefore the
/// workspace contract on disk), but run under [`build_delegated_child_policy`].
pub struct DelegateTool {
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    /// Base prompt injected into the child worker.
    worker_prompt: Option<String>,
    /// Depth budget owned by the *calling* (this) tool instance.
    depth_budget: DepthBudget,
    /// Monotonic child counter for display labels.
    child_count: AtomicU32,
    /// Optional provider-specific tool policy inherited from the parent.
    provider_policy: Option<ToolPolicy>,
    /// Optional supervisor so child tasks are tracked in the session's ledger.
    task_supervisor: Option<Arc<TaskSupervisor>>,
    session_key: Option<String>,
    /// Task id of the *parent* executing this tool — propagated into the
    /// delegation event payload for observability.
    parent_task_id: Option<String>,
    /// Optional harness event sink path for structured delegation events.
    harness_event_sink: Option<String>,
    /// Agent config inherited by child workers.
    worker_config: Option<AgentConfig>,
}

impl DelegateTool {
    /// Create a top-level delegate tool (depth 0).
    pub fn new(llm: Arc<dyn LlmProvider>, memory: Arc<EpisodeStore>, working_dir: PathBuf) -> Self {
        Self {
            llm,
            memory,
            working_dir,
            worker_prompt: None,
            depth_budget: DepthBudget::top_level(),
            child_count: AtomicU32::new(0),
            provider_policy: None,
            task_supervisor: None,
            session_key: None,
            parent_task_id: None,
            harness_event_sink: None,
            worker_config: None,
        }
    }

    /// Use a custom depth budget (primarily for constructing a child tool
    /// for a sub-level registry).
    pub fn with_depth_budget(mut self, budget: DepthBudget) -> Self {
        self.depth_budget = budget;
        self
    }

    pub fn with_worker_prompt(mut self, prompt: String) -> Self {
        self.worker_prompt = Some(prompt);
        self
    }

    pub fn with_provider_policy(mut self, policy: Option<ToolPolicy>) -> Self {
        self.provider_policy = policy;
        self
    }

    pub fn with_task_supervisor(
        mut self,
        supervisor: Arc<TaskSupervisor>,
        session_key: impl Into<String>,
    ) -> Self {
        self.task_supervisor = Some(supervisor);
        self.session_key = Some(session_key.into());
        self
    }

    pub fn with_parent_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.parent_task_id = Some(task_id.into());
        self
    }

    pub fn with_harness_event_sink(mut self, sink_path: impl Into<String>) -> Self {
        self.harness_event_sink = Some(sink_path.into());
        self
    }

    pub fn with_agent_config(mut self, config: AgentConfig) -> Self {
        self.worker_config = Some(config);
        self
    }

    /// Owned budget of this tool. Useful for tests that need to assert the
    /// child tool was constructed with the expected incremented level.
    pub fn depth_budget(&self) -> DepthBudget {
        self.depth_budget
    }

    /// Build a fresh child `DelegateTool` that sits one level deeper.
    /// Returns `Err` if this tool has already saturated its budget.
    pub fn child_tool(&self) -> std::result::Result<Self, HarnessError> {
        let child_budget = self.depth_budget.increment()?;
        Ok(Self {
            llm: self.llm.clone(),
            memory: self.memory.clone(),
            working_dir: self.working_dir.clone(),
            worker_prompt: self.worker_prompt.clone(),
            depth_budget: child_budget,
            child_count: AtomicU32::new(0),
            provider_policy: self.provider_policy.clone(),
            task_supervisor: self.task_supervisor.clone(),
            session_key: self.session_key.clone(),
            parent_task_id: self.parent_task_id.clone(),
            harness_event_sink: self.harness_event_sink.clone(),
            worker_config: self.worker_config.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    task: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    context: Option<String>,
}

fn compose_task_prompt(input: &Input) -> String {
    match &input.context {
        Some(extra) if !extra.is_empty() => format!("{extra}\n\n{}", input.task),
        _ => input.task.clone(),
    }
}

fn contract_failure_summary(working_dir: &Path) -> Option<String> {
    // Inspect every workspace contract beneath the working directory and
    // report the first unready one. A `None` return means "no contract
    // declared" — which is legal at the child boundary — or "all declared
    // contracts are ready".
    let Ok(statuses) = crate::inspect_workspace_contracts(working_dir) else {
        return Some(format!(
            "workspace contract inspection failed for {}",
            working_dir.display()
        ));
    };

    for status in statuses {
        if !status.policy_managed {
            continue;
        }
        if status.ready {
            continue;
        }
        let mut reasons = Vec::new();
        if let Some(error) = status.error.as_deref() {
            reasons.push(error.to_string());
        }
        reasons.extend(
            status
                .turn_end_checks
                .iter()
                .chain(status.completion_checks.iter())
                .filter(|check| !check.passed)
                .map(|check| match check.reason.as_deref() {
                    Some(reason) if !reason.is_empty() => {
                        format!("{}: {}", check.spec, reason)
                    }
                    _ => format!("{}: failed", check.spec),
                }),
        );
        reasons.extend(
            status
                .artifacts
                .iter()
                .filter(|a| !a.present)
                .map(|a| format!("missing artifact '{}' matching '{}'", a.name, a.pattern)),
        );
        let summary = if reasons.is_empty() {
            format!("workspace contract for {} is not ready", status.repo_label)
        } else {
            format!(
                "workspace contract for {} is not ready: {}",
                status.repo_label,
                reasons.join("; ")
            )
        };
        return Some(summary);
    }

    None
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn description(&self) -> &str {
        "Delegate a subtask synchronously to a restricted child agent. The caller blocks until the child's task reaches a terminal lifecycle state. Children inherit the parent's workspace but cannot re-delegate, spawn background workers, or message the user directly."
    }

    fn tags(&self) -> &[&str] {
        &["delegation"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Instruction for the delegated child agent."
                },
                "label": {
                    "type": "string",
                    "description": "Short display label for logs and events."
                },
                "allowed_tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional child allow-list (deny list still applies)."
                },
                "context": {
                    "type": "string",
                    "description": "Extra context prepended to the task prompt."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        // M8.1: legacy entry point. Re-enter via execute_with_context with
        // the zero-value context so out-of-band callers (tests, integrations
        // that have not been updated) still exercise the same code path.
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid delegate_task input")?;

        // M8.1: prefer the sink path threaded through the typed context. Fall
        // back to the tool's own configured sink for constructors that wired
        // it directly (and for the legacy zero-context entry path).
        let effective_sink: Option<&str> = ctx
            .harness_event_sink
            .as_deref()
            .or(self.harness_event_sink.as_deref());

        // Step 1: depth guard. A parent whose budget is already exhausted
        // must reject synchronously, without spawning.
        let child_budget = match self.depth_budget.increment() {
            Ok(child) => child,
            Err(error) => {
                record_delegation(
                    self.depth_budget.child_depth(),
                    DelegationOutcome::DepthExceeded,
                );
                let event = DelegationEvent::new(
                    self.depth_budget.child_depth(),
                    self.parent_task_id.clone().unwrap_or_default(),
                    String::new(),
                    DelegationOutcome::DepthExceeded,
                );
                let _ = emit_delegation_event(effective_sink, &event);
                warn!(
                    parent_depth = self.depth_budget.current,
                    max = self.depth_budget.max,
                    "delegate_task rejected: depth budget exhausted"
                );
                // Surface the typed variant through eyre so synchronous
                // callers can downcast. Report the human-facing message as
                // well so string-only callers stay readable.
                return Err(eyre::Report::new(error));
            }
        };

        // Step 2: register the child task so the operator dashboard and
        // persistence ledger see the new lineage. `register` mints a
        // fresh TaskId v7 inside the session — invariant #6.
        let child_num = self.child_count.fetch_add(1, Ordering::SeqCst);
        let label = input
            .label
            .clone()
            .unwrap_or_else(|| input.task.chars().take(60).collect());
        let tool_call_id = format!("delegate-{child_num}");
        let child_task_id = match self.task_supervisor.as_ref() {
            Some(supervisor) => {
                supervisor.register(&label, &tool_call_id, self.session_key.as_deref())
            }
            None => TaskId::new().to_string(),
        };

        record_delegation(child_budget.current, DelegationOutcome::Accepted);
        let _ = emit_delegation_event(
            effective_sink,
            &DelegationEvent::new(
                child_budget.current,
                self.parent_task_id.clone().unwrap_or_default(),
                &child_task_id,
                DelegationOutcome::Accepted,
            ),
        );
        info!(
            child_task_id = %child_task_id,
            depth = child_budget.current,
            task = %input.task,
            "delegate_task dispatching child"
        );

        // Step 3: mark supervisor "running" so the parent can observe the
        // transition from Queued -> Running -> Ready/Failed (invariant #5).
        if let Some(supervisor) = self.task_supervisor.as_ref() {
            supervisor.mark_running(&child_task_id);
        }

        // Step 4: build the restricted child toolset. The child re-registers
        // its own DelegateTool with the incremented budget so a leaf child
        // (depth == max) can still emit a typed DepthExceeded error via
        // evaluate/run, and so a depth-1 child can delegate one more level.
        let mut tools = ToolRegistry::with_builtins(&self.working_dir);
        tools.clear_spawn_only();

        let child_delegate = Self {
            llm: self.llm.clone(),
            memory: self.memory.clone(),
            working_dir: self.working_dir.clone(),
            worker_prompt: self.worker_prompt.clone(),
            depth_budget: child_budget,
            child_count: AtomicU32::new(0),
            provider_policy: self.provider_policy.clone(),
            task_supervisor: self.task_supervisor.clone(),
            session_key: self.session_key.clone(),
            parent_task_id: Some(child_task_id.clone()),
            // Child inherits the effective sink so the context-threaded path
            // still reaches grandchildren even if only the ToolContext set it.
            harness_event_sink: effective_sink.map(|s| s.to_string()),
            worker_config: self.worker_config.clone(),
        };
        tools.register_arc(Arc::new(child_delegate));

        let policy = build_delegated_child_policy(input.allowed_tools.clone());
        tools.apply_policy(&policy);
        if let Some(ref pp) = self.provider_policy {
            tools.set_provider_policy(pp.clone());
        }

        // Step 5: run the child synchronously.
        let worker_id = AgentId::new(format!("delegate-{child_num}"));
        let mut worker = Agent::new(worker_id, self.llm.clone(), tools, self.memory.clone());
        if let Some(ref config) = self.worker_config {
            worker = worker.with_config(config.clone());
        }
        if let Some(ref prompt) = self.worker_prompt {
            worker = worker.with_system_prompt(prompt.clone());
        }

        let subtask = Task::new(
            TaskKind::Code {
                instruction: compose_task_prompt(&input),
                files: vec![],
            },
            TaskContext {
                working_dir: self.working_dir.clone(),
                ..Default::default()
            },
        );

        let run_result = worker.run_task(&subtask).await;

        // Step 6: run the contract-gate. A ready workspace contract must
        // hold before we declare success. Absence of any contract is legal.
        let contract_failure = match run_result.as_ref() {
            Ok(task_result) if task_result.success => contract_failure_summary(&self.working_dir),
            _ => None,
        };

        // Step 7: terminal bookkeeping. Mark the supervisor Completed or
        // Failed so `lifecycle_state()` returns Ready or Failed (invariant
        // #5). The parent `await`s on this tool, which closes the gap.
        let (success, output) = match (&run_result, contract_failure.as_deref()) {
            (Ok(result), None) if result.success => (true, result.output.clone()),
            (Ok(_), Some(error)) => (false, error.to_string()),
            (Ok(result), None) => (false, result.output.clone()),
            (Err(error), _) => (false, error.to_string()),
        };

        if let Some(supervisor) = self.task_supervisor.as_ref() {
            if success {
                let files = match &run_result {
                    Ok(task_result) => task_result
                        .files_to_send
                        .iter()
                        .chain(task_result.files_modified.iter())
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                    _ => Vec::new(),
                };
                supervisor.mark_completed(&child_task_id, files);
            } else {
                supervisor.mark_failed(&child_task_id, output.clone());
            }
        }

        let terminal_outcome = if success {
            DelegationOutcome::Completed
        } else {
            DelegationOutcome::Failed
        };
        record_delegation(child_budget.current, terminal_outcome);
        let _ = emit_delegation_event(
            effective_sink,
            &DelegationEvent::new(
                child_budget.current,
                self.parent_task_id.clone().unwrap_or_default(),
                &child_task_id,
                terminal_outcome,
            ),
        );

        // Invariant #5 sanity check — if the supervisor is wired, the task
        // must have settled into a terminal lifecycle state by now.
        if let Some(supervisor) = self.task_supervisor.as_ref() {
            if let Some(task) = supervisor.get_task(&child_task_id) {
                let state = task.lifecycle_state();
                debug_assert!(
                    matches!(
                        state,
                        TaskLifecycleState::Ready | TaskLifecycleState::Failed
                    ),
                    "delegate child must reach Ready/Failed before returning, got {state:?}"
                );
            }
        }

        Ok(ToolResult {
            output,
            success,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_serde_round_trip_depth_budget() {
        let budget = DepthBudget { current: 1, max: 2 };
        let json = serde_json::to_string(&budget).unwrap();
        // Must be structurally stable: both fields as integers.
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(raw["current"], 1);
        assert_eq!(raw["max"], 2);
        let parsed: DepthBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, budget);
    }

    #[test]
    fn should_increment_depth_budget_per_level() {
        let top = DepthBudget::top_level();
        assert_eq!(top.current, 0);
        assert_eq!(top.max, MAX_DEPTH);

        let child = top.increment().expect("top-level must allow a child");
        assert_eq!(child.current, 1);
        assert_eq!(child.max, MAX_DEPTH);

        let grandchild = child
            .increment()
            .expect("depth-1 must allow one more child");
        assert_eq!(grandchild.current, 2);

        let refused = grandchild
            .increment()
            .expect_err("max depth reached must reject");
        match refused {
            HarnessError::DelegateDepthExceeded {
                depth,
                limit,
                message,
            } => {
                assert_eq!(depth, 2);
                assert_eq!(limit, MAX_DEPTH);
                assert!(message.contains("depth 2"));
            }
            other => panic!("expected DelegateDepthExceeded, got {other:?}"),
        }
    }

    #[test]
    fn should_flag_exhausted_budget() {
        assert!(!DepthBudget::top_level().is_exhausted());
        assert!(DepthBudget::at_level(MAX_DEPTH).is_exhausted());
    }

    #[test]
    fn should_apply_group_delegated_deny_list_to_child_policy() {
        let policy = build_delegated_child_policy(Vec::new());
        assert!(policy.deny.contains(&DELEGATED_DENY_GROUP.to_string()));
        // Deny wins over any allow list the child had permitted.
        let narrowed = build_delegated_child_policy(vec!["read_file".into(), "shell".into()]);
        assert!(narrowed.deny.contains(&DELEGATED_DENY_GROUP.to_string()));
        assert_eq!(narrowed.allow, vec!["read_file", "shell"]);
        assert!(!policy.is_allowed("delegate_task"));
        assert!(!policy.is_allowed("spawn"));
        assert!(!policy.is_allowed("send_message"));
        assert!(!policy.is_allowed("save_memory"));
        assert!(!policy.is_allowed("execute_code"));
    }

    #[test]
    fn should_emit_delegation_event_with_stable_schema() {
        let event = DelegationEvent::new(1, "parent-1", "child-2", DelegationOutcome::Accepted);
        let json = serde_json::to_string(&event).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(raw["schema"], HARNESS_EVENT_SCHEMA_V1);
        assert_eq!(raw["kind"], DELEGATION_EVENT_KIND);
        assert_eq!(raw["depth"], 1);
        assert_eq!(raw["parent_task_id"], "parent-1");
        assert_eq!(raw["child_task_id"], "child-2");
        assert_eq!(raw["outcome"], "accepted");
    }

    #[test]
    fn should_describe_delegation_outcome_labels() {
        assert_eq!(DelegationOutcome::Accepted.label(), "accepted");
        assert_eq!(DelegationOutcome::Completed.label(), "completed");
        assert_eq!(DelegationOutcome::Failed.label(), "failed");
        assert_eq!(DelegationOutcome::DepthExceeded.label(), "depth_exceeded");
    }
}
