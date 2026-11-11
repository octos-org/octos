//! Pure model records for the agent orchestrator (A-03).
//! DTO/state records only; no orchestration, persistence, or lock logic.

use super::*;

/// #1935 codex round 3 (TOCTOU) — the goal identity + the exact objective a
/// completion verifier judged, captured under ONE state-lock acquisition by
/// [`InProcessAgentOrchestrator::goal_verification_snapshot`]. The pair is
/// mutually consistent by construction; `maybe_complete_goal_from_model`
/// re-checks BOTH fields against the live record after the verifier await,
/// so neither a clear/recreate (new id — even with the same objective text)
/// nor a same-id objective edit can complete on a stale Done verdict.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GoalVerificationSnapshot {
    pub(crate) goal_id: String,
    pub(crate) objective: String,
    /// #1935 codex round 4 (ABA) — the record's user-intent mutation counter
    /// at snapshot time. `set_goal`'s user-path replace preserves `goal_id`,
    /// and the user can restore the identical objective text, so (id,
    /// objective, status) can ALL match again at verdict time; the revision
    /// is the incarnation that still differs. Checked by
    /// `maybe_complete_goal_from_model` / `model_transition_goal_guarded`.
    pub(crate) revision: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentArtifactRecord {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) path: Option<String>,
    pub(crate) content: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentUpsert {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) path: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) backend_kind: String,
    pub(crate) status: String,
    pub(crate) last_task: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) profile_id: String,
}

pub(crate) type NativeSpecialistEventSender = mpsc::UnboundedSender<NativeSpecialistAppUiEvent>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeSpecialistAppUiEvent {
    pub(crate) method: &'static str,
    pub(crate) params: Value,
}

pub(crate) struct NativeSpecialistLaunchRequest {
    pub(crate) agent_id: Option<String>,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) task: String,
    pub(crate) cwd: PathBuf,
    pub(crate) llm: Arc<dyn LlmProvider>,
    pub(crate) memory: Arc<EpisodeStore>,
    pub(crate) tools: Arc<ToolRegistry>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) agent_config: Option<AgentConfig>,
    pub(crate) task_ledger_path: Option<PathBuf>,
    pub(crate) event_tx: Option<NativeSpecialistEventSender>,
    pub(crate) dispatch_policy: Option<Arc<octos_agent::DispatchPolicy>>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NativeSpecialistRunResult {
    pub(crate) agent_id: String,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) status: String,
    pub(crate) output_len: usize,
    pub(crate) artifacts: Vec<AgentArtifactRecord>,
}

/// #1707 round 3 (codex Blockers 3+4) — durable mark for one delivered
/// `ChildCompleted`: the terminal status the master was last told about and
/// the monotonically increasing REVISION of that delivery. The revision maps
/// onto the persisted record's `attempt` field so a status CORRECTION
/// (`failed` → `completed`, same identity key) persists with `attempt =
/// revision + 1` — a distinct `continuation_queued` event id AND an
/// attempt-eligible upsert that can replace an older `Completed` tombstone —
/// while an equal-or-lower attempt can never downgrade the record.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DeliveredChildMark {
    pub(crate) status: String,
    pub(crate) revision: u64,
    /// #7 round 2 (outer-loop rejection fix) — TRUE only when the mark was
    /// seeded from the persistent store at boot ([`seed_delivered_terminal_
    /// marks`]): that is the durable proof a PREVIOUS runtime already
    /// delivered this verdict, which is the only legitimate signal for the
    /// stale-reforward idempotency gate. A mark created IN THIS PROCESS
    /// (first delivery, status correction, /stop purge) must NOT gate a
    /// later upsert — e.g. the mirror test's idempotent re-upsert, or a
    /// same-lifetime status refresh — because the timestamp alone cannot
    /// distinguish "pre-boot replay" from "just delivered a moment ago"
    /// (the lazy singleton boot makes `now < boot` routinely true for the
    /// first fixture in a test process).
    pub(crate) seeded_from_store: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AutonomyAgentRecord {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) session_id: SessionKey,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) path: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) backend_kind: String,
    pub(crate) status: String,
    pub(crate) last_task: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) workspace_scope: Option<WorkspaceScope>,
    pub(crate) profile_id: String,
    pub(crate) output: String,
    pub(crate) artifacts: Vec<AgentArtifactRecord>,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    /// #1021 / M17-C — most-recent dispatch context contract for this child agent. Populated by specialist runners (CLI / native / MCP) when they emit a dispatch and surfaced through `agent/updated` so AppUI clients can tell `managed_payload` from `external_context_unmanaged` per child.
    pub(crate) context_contract: Option<DispatchContextContract>,
    /// True when this record was rebuilt from the supervisor store at boot —
    /// an agent from a PREVIOUS server lifetime (necessarily terminal; the
    /// replay flips still-running children to "interrupted"). Restored records
    /// stay individually queryable (`agent/status`, `agent/artifact/list`) but
    /// are EXCLUDED from `agent/list`: a fresh lifetime's roster must not
    /// resurface dead history as chips in the client strip. Cleared if a live
    /// upsert reuses the id (the agent is active in THIS lifetime again).
    pub(crate) restored: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct AutonomyGoalRecord {
    pub(crate) profile_id: String,
    pub(crate) goal_id: String,
    pub(crate) objective: String,
    pub(crate) status: String,
    pub(crate) token_budget: u64,
    pub(crate) tokens_used: u64,
    pub(crate) time_used_seconds: u64,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    /// #979 / M15-C2 — number of goal continuation turns this goal has
    /// driven since `set_goal` was first called (or since the goal was
    /// last reset to `active`). Used together with
    /// `last_continued_at_ms` and `rate_window_*` to enforce the
    /// min-delay + max-per-hour fire policy.
    pub(crate) continuations_used: u32,
    /// Wall-clock ms of the last successful goal-continuation fire.
    /// Zero means no continuation has fired yet. Drives the min-delay
    /// gate on subsequent fires.
    pub(crate) last_continued_at_ms: i64,
    /// Start of the current sliding rate-limit window (one hour).
    pub(crate) rate_window_start_ms: i64,
    /// Number of continuations counted within `rate_window_start_ms`.
    pub(crate) rate_window_count: u32,
    /// `true` once the orchestrator has enqueued the budget-exhaustion
    /// wrap-up turn so a `record_goal_turn` call after `budget_limited`
    /// does not re-emit duplicate wrap-ups on every subsequent
    /// continuation attempt.
    pub(crate) wrap_up_emitted: bool,
    /// #1693 — consecutive autonomous continuation turns that consumed
    /// ZERO tokens (turn error / interrupt before the model ran). A
    /// failing goal charges nothing, so its budget never exhausts and
    /// the scheduler would retry 12×/hour forever; at
    /// [`GOAL_MAX_CONSECUTIVE_FAILED_TURNS`] the goal flips to
    /// `blocked` (user-resumable). Reset by any token-consuming turn
    /// and by user re-activation.
    pub(crate) consecutive_failed_turns: u32,
    /// #26b — consecutive autonomous continuation turns that made NO
    /// observable progress (goal revision unchanged AND zero tokens charged
    /// AND no new ledger finding). Distinct from `consecutive_failed_turns`
    /// (which only tracks zero-token turns): a turn can SPEND tokens and
    /// still be inert (the model re-reading without acting). At
    /// [`GOAL_NO_PROGRESS_TURN_LIMIT`] the turn-end hook stops re-arming
    /// itself and parks the goal `blocked` (user-resumable), so a spinning
    /// session cannot burn the budget forever. Any revision bump (a real
    /// goal_update / status change) or any token spend resets it.
    pub(crate) no_progress_turns: u32,
    /// #1857 PR 5a — the durable fleet this goal drives, once `goal_plan`
    /// has created it (`<goal_id>`). `None` before the keeper decomposes the
    /// objective onto a fleet; set once and treated as idempotent (a second
    /// `goal_plan` returns "already planned" rather than recreating). Rides
    /// the `SupervisedGroupRecord.metadata` open bag — no schema bump.
    pub(crate) fleet_id: Option<String>,
    /// #1857 PR 5a — the controller workspace root captured at goal-turn start
    /// from one `session_workspaces().snapshot(wire)` (the LOAD-BEARING seam:
    /// fleet create stamps it and the paired provenance onto `FleetRecord` so a
    /// `ChildDone` wake can rehydrate a headless keeper across restart). `None`
    /// until a live-client goal turn stashes it; `goal_plan` refuses to create a
    /// fleet without it. Persisted in the metadata bag alongside `fleet_id`.
    pub(crate) controller_workspace_root: Option<String>,
    /// Provenance paired with `controller_workspace_root`: `Some(true)` for an
    /// explicit cwd, `Some(false)` for a derived Tier-3 root, `None` for legacy
    /// records whose provenance is unknown (and therefore unsafe as a hint).
    pub(crate) controller_workspace_has_runtime_hint: Option<bool>,
    /// #1935 codex round 4 (ABA) — monotonic counter of USER/MODEL-INTENT
    /// mutations. BUMP SET (documented contract): `set_goal` on an existing
    /// record (create starts at 0; replace / objective edit / pause /
    /// reactivate / budget change — bumped even when the submitted values
    /// equal the current ones, so an edit-away-and-back X→Y→X leaves
    /// evidence), `model_transition_goal*` (complete/blocked), and the
    /// verified sentinel completion flip. Pure ACCOUNTING deliberately does
    /// NOT bump: token/time charges, continuation counters, rate window,
    /// `wrap_up_emitted` bookkeeping, and the AUTOMATIC `budget_limited`
    /// exhaustion flip — an in-flight verified completion must survive its
    /// own turn/verifier charge crossing the budget. Restored snapshots
    /// without the field default to 0 (metadata `unwrap_or(0)`, the
    /// supervisor-bag equivalent of `#[serde(default)]`).
    pub(crate) revision: u64,
    /// #1935 codex round 4 — wrap-up generation, bumped whenever
    /// `wrap_up_emitted` is CLEARED (user reactivation). Rides the wrap-up
    /// continuation dedupe key (`…/wrap_up/<profile>/<goal_id>/<gen>`) so a
    /// durably tombstoned gen-N wrap-up (refused while paused — see
    /// `stale_drop_should_tombstone`) can never mask the gen-N+1 remint on
    /// supervisor replay (`Completed` outranks `Queued` per key). Restored
    /// snapshots without the field default to 0.
    pub(crate) wrap_up_generation: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct AutonomyLoopRecord {
    pub(crate) loop_id: String,
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) prompt: String,
    pub(crate) mode: String,
    pub(crate) interval_seconds: Option<u64>,
    pub(crate) status: String,
    pub(crate) next_run_at_ms: Option<i64>,
    pub(crate) last_run_at_ms: Option<i64>,
    pub(crate) expires_at_ms: i64,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    /// #1130 — number of fires this loop has consumed against
    /// `LOOP_DEFAULT_MAX_FIRES`. Persisted to the supervisor store so the
    /// runtime budget gate is enforced across daemon restarts and across
    /// repeated `fire_now` invocations (`loop_runtime_view` was previously
    /// rebuilding the runtime with a zeroed `fires_used` counter, so the
    /// max-fires safety cap never tripped). Defaults to 0 for legacy
    /// snapshots that pre-date this field.
    pub(crate) fires_used: u32,
}

/// #1977 — one monitor's durable record: the spec plus wake accounting.
/// Persisted to the supervisor store (loop-record idiom) so the SPEC
/// survives a restart and re-arms at boot; the child PROCESS never does.
#[derive(Debug, Clone)]
pub(crate) struct AutonomyMonitorRecord {
    pub(crate) monitor_id: String,
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) name: String,
    pub(crate) argv: Vec<String>,
    pub(crate) filter_regex: Option<String>,
    /// `"poll"` | `"stream"` (string like `AutonomyLoopRecord.mode`).
    pub(crate) mode: String,
    pub(crate) interval_seconds: Option<u64>,
    pub(crate) batch_ms: u32,
    pub(crate) max_events_per_hour: u32,
    pub(crate) persistent: bool,
    /// `active` | `paused` | `expired` | `deleted`.
    pub(crate) status: String,
    /// Why a paused monitor paused: `user` | `flooded` | `process_exited`.
    pub(crate) pause_reason: Option<String>,
    pub(crate) goal_id: Option<String>,
    /// Working directory for the probe process (session workspace root
    /// when known at create, else the profile data dir).
    pub(crate) cwd: Option<String>,
    /// Profile data dir rooting the monitor-notes sidecar.
    pub(crate) data_dir: Option<String>,
    /// Poll-mode change baseline: the batch hash of the LAST observation
    /// that fired (or established the baseline). PERSISTED so a restart
    /// emits at most ONE catch-up wake — the first observation after
    /// re-arm fires only when it differs from this hash (the loop
    /// restart rule). `None` = never observed (first observation
    /// baselines silently: creating a monitor must not burn a wake).
    pub(crate) last_emit_hash: Option<String>,
    /// Sliding one-hour flood accountant (mirrors the goal rate window).
    pub(crate) rate_window_start_ms: i64,
    pub(crate) rate_window_count: u32,
    pub(crate) last_fired_at_ms: Option<i64>,
    /// Total wakes this monitor has queued (observability, like
    /// `AutonomyLoopRecord.fires_used`).
    pub(crate) fires_used: u32,
    /// `None` for persistent monitors; otherwise the auto-expiry instant.
    pub(crate) expires_at_ms: Option<i64>,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
}

pub(crate) struct ParsedLoopCreate {
    pub(crate) prompt: String,
    pub(crate) mode: String,
    pub(crate) interval_seconds: Option<u64>,
}

pub(crate) struct DueLoopFire {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) loop_id: String,
    pub(crate) prompt: String,
    pub(crate) scheduled_for_ms: i64,
    /// #1135: carries the `MaintenancePromptSource` resolved at fire
    /// time for maintenance loops, so the queued continuation metadata
    /// reports the same `project` / `user` / `built_in` provenance as
    /// the `fire_now` path. `None` for non-maintenance modes — those
    /// fall back to the legacy `"record"` label.
    pub(crate) prompt_source: Option<MaintenancePromptSource>,
}
