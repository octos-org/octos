//! Wire contract of the autonomy agent orchestrator: continuation-kind and
//! metadata-key constants, dedupe-key helpers, the request records, and the
//! [`AgentOrchestrator`] trait with its uniform `method_not_supported`
//! fallback. Split out of `agent_orchestrator.rs`; every item is re-exported
//! from the parent module so the historical
//! `crate::autonomy::agent_orchestrator::…` paths resolve unchanged.

use std::path::Path;

use octos_core::SessionKey;
use octos_core::ui_protocol::{OutputCursor, RpcError, rpc_error_codes};
use serde_json::{Value, json};
pub(crate) const AUTONOMY_POLICY_ID: &str = "coding-autonomy-v1";
/// #2036 follow-up — how much of a peer's review survives into the durable
/// ledger row. This is the LAST cap before the DB, so it is the one that
/// actually binds, whatever the caller already trimmed.
///
/// Raising the transport-side cap alone was not enough: a real two-peer review
/// soak on merged main still produced findings of exactly 500 characters,
/// because this hard `take(500)` re-truncated them here. The completion
/// verifier reads these rows (#1990), so a review cut to 500 chars mid-token
/// is the same unverifiable fragment the original 400-char cut produced.
///
/// Sized to match `MAX_PEER_FINDING_RECORD_CHARS` in the transport so the two
/// stops on the same path agree; the verifier's own prompt stays bounded
/// separately by `MAX_LEDGER_EVIDENCE_ASSERTION_CHARS` at assembly time.
pub(crate) const MAX_PEER_FINDING_ASSERTION_CHARS: usize = 4_000;
/// Default per-goal continuation token budget when the caller does not
/// specify one. Sized to survive several real turns: each goal turn
/// charges its FULL token cost (input + output + cache reads/writes), so
/// on any non-trivial session a single turn spends 100K–200K+ tokens. The
/// earlier 50K default budget-limited a goal after ~one turn, which read
/// as "the goal stopped counting". Users can override per-goal up to
/// [`GOAL_MAX_TOKEN_BUDGET`]. `pub(crate)` so the capability advertisement
/// (`ui_protocol`) reports the real value instead of a drifting literal.
///
/// Raised 2M -> 100M for PEER FLEETS. A goal that fans out to peers charges
/// every peer's turns to the MASTER's budget (#1965/#1970), so the old 2M
/// default was exhausted by a single realistic fleet: a 5-peer deep-review
/// goal spent 10.65M and flipped to `budget_limited` with all five peers
/// still doing useful work. Enforcement is at the TURN BOUNDARY (mid-turn
/// limiting was deliberately dropped), so the overshoot is unbounded within
/// a turn and a budget only ever acts as a tripwire noticed afterwards —
/// which made 2M a guard that fired on correct behaviour rather than runaway
/// behaviour.
///
/// TRADE-OFF, stated plainly: this is the ceiling on what an UNSPECIFIED
/// goal may spend autonomously. At the ~$1/M-token rate observed in soaks,
/// 100M is on the order of $100 per goal before anything stops it. Set a
/// smaller explicit `--budget` for anything cost-sensitive; the default now
/// optimises for "a legitimate fleet finishes" over "an unattended goal
/// cannot spend much".
pub(crate) const GOAL_DEFAULT_TOKEN_BUDGET: u64 = 100_000_000;
/// Hard ceiling on a caller-supplied goal budget — a sanity limit against
/// typos / overflow, NOT a practical cap (at ~175K tokens/turn this still
/// allows thousands of continuations). The user owns whatever value they
/// set beneath it; the small default above is what guards an unspecified
/// goal from unbounded autonomous spend.
pub(crate) const GOAL_MAX_TOKEN_BUDGET: u64 = 1_000_000_000;
pub(crate) const LOOP_MIN_INTERVAL_SECONDS: u64 = 60;
pub(crate) const LOOP_MAX_INTERVAL_SECONDS: u64 = 86_400;
pub(crate) const LOOP_MAX_AGE_DAYS: i64 = 7;
/// Default max fires for a single loop record before `LoopRuntime` flags
/// budget exhaustion. `AutonomyLoopRecord` already enforces 7-day expiry
/// and a per-session quota, so the per-loop budget is set generously and
/// is intentionally not user-tunable for the M15-D2 cut. (#977)
pub(crate) const LOOP_DEFAULT_MAX_FIRES: u32 = 10_000;
/// Default rescheduling delay when a self-paced loop fires without
/// emitting a `<<loop-next-in: …>>` hint. Caller can override via
/// `apply_self_paced_response` once richer config lands. (#977 bullet 4)
pub(crate) const SELF_PACED_DEFAULT_DELAY_SECONDS: u64 = 60 * 15;
pub(crate) const MAX_OBJECTIVE_BYTES: usize = 8_192;

/// #1697 — minimal XML escaping for the goal objective before it is
/// rendered into model-facing prompt text. The objective is USER data; raw
/// interpolation let a crafted objective impersonate the `[system-internal]`
/// framing. Mirrors codex's `<objective>`-fenced, escaped rendering.
pub(crate) fn xml_escape_untrusted(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// #1693 — error→blocked circuit breaker: consecutive zero-token
/// continuation turns before an active goal is parked as `blocked`.
/// codex blocks on the FIRST terminal turn error; three tolerates a
/// transient provider blip without letting a permanently failing goal
/// loop forever.
/// #26b — autonomous-continuation no-progress cap.
pub(crate) const GOAL_NO_PROGRESS_TURN_LIMIT: u32 = 3;
pub(crate) const GOAL_MAX_CONSECUTIVE_FAILED_TURNS: u32 = 3;
pub(crate) const MAX_LOOP_PROMPT_BYTES: usize = 8_192;
pub(crate) const MAX_LOOPS_PER_SESSION: usize = 16;
pub(crate) const AGENT_OUTPUT_CURSOR_INVALID: &str = "agent_output_cursor_invalid";
pub(crate) const AGENT_ARTIFACT_SELECTOR_INVALID: &str = "agent_artifact_selector_invalid";
pub(crate) const AUTONOMY_RECORD_KIND: &str = "autonomy_record_kind";
pub(crate) const AUTONOMY_RECORD_GOAL: &str = "goal";
pub(crate) const AUTONOMY_RECORD_LOOP: &str = "loop";
/// #1977 — supervisor-store record kind for persisted monitor specs.
pub(crate) const AUTONOMY_RECORD_MONITOR: &str = "monitor";
pub(crate) const AUTONOMY_GOAL_CLEARED: &str = "goal_cleared";
/// #979 / M15-C2 — minimum spacing between two goal continuation turns
/// for the same goal. Stops a busy-loop where the model emits an
/// instant tool turn after each continuation and immediately requeues
/// itself. Tuned conservatively at 30s.
pub(crate) const GOAL_MIN_CONTINUATION_INTERVAL_MS: i64 = 30_000;
/// #979 / M15-C2 — sliding-window cap on goal continuation fires per
/// hour. Caps the worst-case spend if the model finds a stable
/// no-progress turn shape.
pub(crate) const GOAL_MAX_CONTINUATIONS_PER_HOUR: u32 = 12;
pub(crate) const GOAL_RATE_WINDOW_MS: i64 = 3_600_000;
/// #979 / M15-C2 — completion sentinels the model can emit at the
/// trailing edge of a goal turn to mark the goal `complete` without
/// requiring an out-of-band RPC. Matched case-insensitively after a
/// whitespace trim of the assistant content.
pub(crate) const GOAL_COMPLETE_SENTINELS: &[&str] = &[
    "<goal:complete>",
    "[goal:complete]",
    "goal-complete",
    "goal_complete",
];
pub(crate) const NATIVE_SPECIALIST_BACKEND_KIND: &str = "native";
pub(crate) const NATIVE_SPECIALIST_SUMMARY_ARTIFACT_ID: &str = "summary";
pub(crate) const NATIVE_SPECIALIST_ARTIFACT_CONTENT_MAX_BYTES: usize = 256 * 1024;

/// #1324 follow-up — kind label for the `External(_)` master continuation
/// reason used to re-inject a `SpawnOnlyFailureSignal` as a synthetic
/// recovery turn.
///
/// #2020 (Gap-1 step 4): this is now the ONLY failure re-entry transport.
/// Both runtime modes enqueue here — the WS / standalone-turn path (drained
/// on the connection's tick and by the connection-independent global drain)
/// and the gateway path (drained by
/// `SessionActor::drain_master_continuations`). The gateway's former
/// `ActorMessage::RecoveryHint` inbox — a SECOND, parallel re-entry channel
/// — is retired.
pub(crate) const SPAWN_ONLY_FAILURE_EXTERNAL_KIND: &str = "spawn_only_failure";
pub(crate) const SPAWN_ONLY_FAILURE_META_TASK_ID: &str = "task_id";
pub(crate) const SPAWN_ONLY_FAILURE_META_TOOL_NAME: &str = "tool_name";
pub(crate) const SPAWN_ONLY_FAILURE_META_ERROR_MESSAGE: &str = "error_message";
pub(crate) const SPAWN_ONLY_FAILURE_META_TOOL_INPUT: &str = "tool_input";
pub(crate) const SPAWN_ONLY_FAILURE_META_ALTERNATIVES: &str = "suggested_alternatives";
pub(crate) const SPAWN_ONLY_FAILURE_META_ORIGINATING_CMID: &str = "originating_client_message_id";
/// Synthetic "group" id stamped onto spawn_only failure continuations so
/// the `External` enqueue path passes the scheduler's required field.
/// Distinct from `coding-autonomy` (loops/goals) so operators can filter
/// the persisted queue by group when triaging recovery turns.
pub(crate) const SPAWN_ONLY_FAILURE_GROUP: &str = "spawn-only-failure-recovery";

/// #436 — kind label for the `External(_)` master continuation reason that
/// delivers a `peer_send_input` injection into a RUNNING serve peer session.
/// The serve process has no gateway `ActorRegistry` to populate the inbox
/// registry, so the tool re-plumbs onto the master continuation queue: the
/// injected text is enqueued under the peer's wire session key and drained
/// as the peer's next turn on its `appui_continuation_tick`.
pub(crate) const PEER_SEND_INPUT_EXTERNAL_KIND: &str = "peer_send_input";
/// Metadata key carrying the verbatim injected message; the prompt renderer
/// emits it as the peer turn's user prompt.
pub(crate) const PEER_SEND_INPUT_META_MESSAGE: &str = "peer_send_input_message";
/// Metadata key carrying the peer slug, so a pending injection can be
/// re-homed to the peer's new wire key on reconnect (#436 P1 #1/#5).
pub(crate) const PEER_SEND_INPUT_META_SLUG: &str = "peer_send_input_slug";
/// Metadata key carrying the unique occurrence id, so a re-home preserves the
/// dedupe identity (a genuine retry still collapses after re-target).
pub(crate) const PEER_SEND_INPUT_META_OCCURRENCE: &str = "peer_send_input_occurrence";
/// Group id stamped onto peer_send_input continuations (queue triage filter).
pub(crate) const PEER_SEND_INPUT_GROUP: &str = "peer-send-input";

/// Peer-fleet auto-synthesis — kind label for the `External(_)` master
/// continuation that fires an AUTONOMOUS synthesis turn on the ORIGINATOR
/// (master) session the moment every peer it handed off has completed. Unlike
/// the passive `peer_results_ready_note` (a mailbox nudge injected only when
/// the user next prompts the master), this actively enqueues a master turn so
/// the fleet's consolidated report is produced with no user prompt. Flows
/// through the same hardened `External` drain path as `peer_send_input`.
pub(crate) const PEER_FLEET_SYNTHESIS_EXTERNAL_KIND: &str = "peer_fleet_synthesis";
/// Metadata key carrying the number of completed peers in the fleet (prompt
/// context only; the synthesis turn gathers results by reading the blackboard).
pub(crate) const PEER_FLEET_SYNTHESIS_META_PEER_COUNT: &str = "peer_fleet_peer_count";
/// Metadata key carrying the comma-separated OWNED peer slugs (this master's
/// fleet). The synthesis prompt directs `peer_gather` at ONLY these slugs, so
/// it reads this master's fleet and never another master's peers that share the
/// same profile `peers/` root (codex #4). Slugs are `peer_slug_is_safe`
/// (`[a-z0-9-]` / `%`-escaped), so a comma join is unambiguous.
pub(crate) const PEER_FLEET_SYNTHESIS_META_SLUGS: &str = "peer_fleet_slugs";
/// Group id stamped onto peer-fleet-synthesis continuations (queue triage).
pub(crate) const PEER_FLEET_SYNTHESIS_GROUP: &str = "peer-fleet-synthesis";

/// Fleet-keeper WAKE (#1857 PR 4a) — kind label for the `External(_)` master
/// continuation the fleet outbox consumer (`autonomy::fleet_wake`) enqueues on a
/// fleet's controller session when a `ChildDone` / `FleetDrained` event lands.
/// It directs the keeper to advance the durable plan by one bounded step. Flows
/// through the same hardened `External` drain path as the peer wakes; the drain
/// dedupes per-occurrence on the outbox `event_id`.
pub(crate) const FLEET_KEEPER_EXTERNAL_KIND: &str = "fleet_keeper_wake";
/// Group id stamped onto fleet-keeper wake continuations (queue triage).
pub(crate) const FLEET_KEEPER_GROUP: &str = "fleet-keeper-wake";
/// Metadata key carrying the woken fleet's id.
pub(crate) const FLEET_KEEPER_META_FLEET_ID: &str = "fleet_id";
/// Metadata key carrying the plan objective (rendered as untrusted data).
pub(crate) const FLEET_KEEPER_META_OBJECTIVE: &str = "objective";
/// Metadata key carrying the pre-rendered per-task plan/status lines.
pub(crate) const FLEET_KEEPER_META_TASK_LINES: &str = "task_lines";
/// Metadata key carrying the comma-separated ids of tasks ready to dispatch.
pub(crate) const FLEET_KEEPER_META_READY: &str = "ready";
/// Metadata key carrying the controller session's persisted workspace root
/// (`FleetRecord.controller_workspace_root`), so a HEADLESS keeper (no live
/// client) can be rehydrated across a serve restart: the global
/// master-continuation drain re-seeds `session_workspaces()` from this before
/// its workspace-known gate (PR 4b). Omitted when the fleet has no persisted
/// root (that keeper is simply not headlessly rehydratable).
pub(crate) const FLEET_KEEPER_META_WORKSPACE_ROOT: &str = "workspace_root";
/// Metadata key preserving whether `workspace_root` originated from an
/// explicit runtime cwd hint. Missing/invalid means legacy unknown and is
/// handled as `false` so a restart never relocates transcripts unsafely.
pub(crate) const FLEET_KEEPER_META_WORKSPACE_HAS_RUNTIME_HINT: &str = "workspace_has_runtime_hint";

/// PR 4b — upper bound on the VALID (rehydratable) fleet-keeper candidates
/// [`crate::autonomy::agent_orchestrator::InProcessAgentOrchestrator::pending_fleet_keeper_seeds`] produces per drain
/// tick. It caps the per-tick clone/allocation so a pathological backlog of
/// stranded headless keepers cannot make the drain's pre-pass unbounded. The cap
/// counts ONLY rooted, existing-directory, deduped candidates — rootless or
/// invalid-root keepers are dropped BEFORE the cap, so noise (a non-rehydratable
/// keeper) can never consume a slot and re-strand a valid keeper behind it.
pub(crate) const FLEET_KEEPER_SEED_CAP: usize = 256;

/// PR 4b — one bounded, validated, PAIRED fleet-keeper rehydration candidate
/// (codex round 2). The workspace root AND the (optional) cwd scope for a wire
/// come from the SAME pending continuation, so the drain's Gate A (workspace
/// known) and Gate D (`goal_target_is_dispatchable`) can never admit a
/// continuation for one folder and execute it in another — the isolation bypass
/// that two independently-selected re-seeds allowed.
///
/// PR 5 MUST bind + validate `controller_session_key` server-side: a
/// corrupt/untrusted scoped controller key could otherwise seed another wire's
/// scope. 4b's require-root + `is_dir` + dedupe validation bounds the damage
/// while the fleet module is dormant (no production create caller yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FleetKeeperSeed {
    /// The plain wire session id (cwd scope stripped) — byte-identical to what
    /// the drain's workspace-known gate probes.
    pub(crate) wire: SessionKey,
    /// The cwd scope hash recovered from a scoped controller key, else `None`
    /// for a plain (unscoped) controller — a plain key needs no Gate-D seed.
    pub(crate) scope: Option<String>,
    /// The persisted controller workspace root, validated to be an existing
    /// directory at selection time.
    pub(crate) root: String,
    /// `Some(true|false)` for new durable records; `None` for legacy/unknown
    /// provenance. Only `Some(true)` may reconstruct a runtime cwd hint.
    pub(crate) workspace_has_runtime_hint: Option<bool>,
}

/// Peer awaiting-input WAKE — kind label for the `External(_)` master
/// continuation that WAKES an idle master when one of its staged peers PARKS on
/// an approval/question (i.e. becomes genuinely `awaiting_input`). This closes
/// the "master is the human-in-the-loop" gap: today a peer's block is visible
/// via `peer_list awaiting_input`, but nothing NOTIFIES the master — it has to
/// already be taking turns and choose to check. The wake enqueues an autonomous
/// master turn (drained ONLY when the master is idle-eligible) that directs the
/// master to `peer_list` → `peer_respond`. Sibling of the fleet-synthesis wake;
/// flows through the same hardened `External` drain path.
pub(crate) const PEER_AWAITING_INPUT_EXTERNAL_KIND: &str = "peer_awaiting_input";

/// Peer-agent-based goal: external continuation kind for goal-progress wakes
/// (a goal-scoped peer completed a turn → wake the master so it sees the
/// finding WITHOUT waiting for the next scheduled goal turn).
pub(crate) const GOAL_PROGRESS_EXTERNAL_KIND: &str = "goal_progress";

/// OLP-CTRL — kind label for the `External(_)` continuation an
/// `octos steer` enqueues to wake the steered session (same doorbell +
/// scheduling mechanism as the goal-progress wake).
pub(crate) const STEER_EXTERNAL_KIND: &str = "steer";

/// OLP-CTRL — metadata key carrying the steer text on a steer
/// continuation, so the continuation turn's user message IS the steer
/// (回合 3 整改: standalone user message body, never a prompt appendix).
pub(crate) const STEER_META_TEXT: &str = "steer_text";

/// OLP-CTRL #8c — the steer line's enqueue timestamp (unix secs string),
/// carried so the steer_consumed receipt names WHEN it was queued.
pub(crate) const STEER_META_ENQUEUED_TS: &str = "steer_enqueued_ts";

/// OLP-CTRL #8c — the content hash of (session, ts, text) that gives each
/// steer line its exactly-once identity (dedupe key + receipt join).
pub(crate) const STEER_META_LINE_HASH: &str = "steer_line_hash";

/// #1977 Monitor WAKE — kind label for the `External(_)` master continuation a
/// [`crate::autonomy::monitor_runtime`] watcher enqueues when its filtered probe
/// output changes (poll) or a stream batch lands. Rides the SAME hardened
/// `External` drain path (idle-gated, deduped, rate-disciplined) as the peer
/// wakes — no new scheduler. The matched lines are staged for prompt injection
/// via the monitor-notes sidecar (`inbox/<hash>.monitor-notes`, the
/// goal-progress-notes idiom) AND carried as a capped preview in metadata so
/// the wake turn is self-contained even if the notes read fails.
pub(crate) const MONITOR_FIRED_EXTERNAL_KIND: &str = "monitor_fired";
/// Group id stamped onto monitor wakes (queue triage filter).
pub(crate) const MONITOR_FIRED_GROUP: &str = "monitor-watch";
/// Metadata key carrying the firing monitor's id (schedulability gate + UI).
pub(crate) const MONITOR_META_ID: &str = "monitor_id";
/// Metadata key carrying the monitor's human-readable name.
pub(crate) const MONITOR_META_NAME: &str = "monitor_name";
/// Metadata key carrying the number of matched lines in the batch.
pub(crate) const MONITOR_META_LINE_COUNT: &str = "line_count";
/// Metadata key carrying a capped preview of the matched lines
/// (newline-joined; the full batch is in the monitor-notes sidecar).
pub(crate) const MONITOR_META_LINES_PREVIEW: &str = "lines_preview";
/// Metadata key carrying the bound goal id, when the monitor is
/// goal-scoped. Informational: token charging rides the existing
/// per-session goal accountant (#1647), not a new charge path.
pub(crate) const MONITOR_META_GOAL_ID: &str = "goal_id";
/// Byte cap for the metadata lines preview.
pub(crate) const MONITOR_LINES_PREVIEW_CAP: usize = 2 * 1024;
/// Backend cap on live (non-deleted) monitors per session, mirroring
/// [`MAX_LOOPS_PER_SESSION`].
pub(crate) const MAX_MONITORS_PER_SESSION: usize = 16;

/// The dedupe key for a monitor wake — the issue-#1977 `monitor:<id>:<line-hash>`
/// key in the canonical `external/{kind}/…` producer form. Keyed on the
/// monitor id AND the batch content hash, so a retry of the SAME observation
/// (or a poll re-reporting an unchanged state that raced the change-dedupe)
/// collapses, while distinct observations each wake the master. The batch
/// hash is the unique per-occurrence id the `External`-producer invariant
/// requires: a hash only ever re-fires when the observed state genuinely
/// recurs, and the recent-claim guard window then correctly treats an A→B→A
/// flap within seconds as one wake.
pub(crate) fn monitor_fired_dedupe_key(
    session: &SessionKey,
    monitor_id: &str,
    batch_hash: &str,
) -> String {
    format!("external/{MONITOR_FIRED_EXTERNAL_KIND}/{session}/{monitor_id}/{batch_hash}")
}
/// Metadata key carrying the parked peer's slug (names the peer in the nudge).
pub(crate) const PEER_AWAITING_INPUT_META_SLUG: &str = "peer_awaiting_input_slug";
/// Metadata key carrying the park kind — `"approval"` or `"question"`.
pub(crate) const PEER_AWAITING_INPUT_META_KIND: &str = "peer_awaiting_input_kind";
/// Metadata key carrying a short one-line summary of what the peer is blocked
/// on (prompt context only; the master reads the authoritative parked set via
/// `peer_list`).
pub(crate) const PEER_AWAITING_INPUT_META_PROMPT: &str = "peer_awaiting_input_prompt";
/// Group id stamped onto peer-awaiting-input wakes (queue triage).
pub(crate) const PEER_AWAITING_INPUT_GROUP: &str = "peer-awaiting-input";

/// #436 P1 (#3) — real delivery status for a `peer_send_input` injection so the
/// tool never acks success on a durable-persist failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerSendInputEnqueueOutcome {
    /// Newly queued (durably persisted, or in-memory-only when no store).
    Queued,
    /// Collapsed onto an already-queued injection with the SAME occurrence id
    /// (a genuine retry) — already queued for delivery.
    Duplicate,
    /// Enqueued in-memory but the durable store write failed; the enqueue was
    /// rolled back so it is NOT queued. The caller MUST surface an error.
    PersistFailed,
}

impl PeerSendInputEnqueueOutcome {
    /// The tool callback maps a real delivery status to its `Result`: a
    /// persist failure is an ERROR (do not ack success); a genuine retry of
    /// the same call is reported as already queued, not as a fresh send.
    pub(crate) fn into_callback_result(
        self,
        slug: &str,
    ) -> Result<octos_agent::PeerSendInputDelivery, String> {
        match self {
            Self::Queued => Ok(octos_agent::PeerSendInputDelivery::Queued),
            Self::Duplicate => Ok(octos_agent::PeerSendInputDelivery::AlreadyQueued),
            Self::PersistFailed => Err(format!(
                "failed to durably queue input for peer '{slug}' (storage write \
                 error) — the injection was not delivered; try again"
            )),
        }
    }
}

/// The dedupe key for a `peer_send_input` continuation. Keyed on the peer's
/// wire session AND the unique per-call occurrence id, so distinct calls never
/// collapse while a same-call retry (or a re-home under the same occurrence)
/// dedups.
pub(crate) fn peer_send_input_dedupe_key(session: &SessionKey, occurrence_id: &str) -> String {
    format!("external/{PEER_SEND_INPUT_EXTERNAL_KIND}/{session}/{occurrence_id}")
}

/// The dedupe key for a peer-fleet-synthesis continuation — PER-MASTER only.
/// A master's fleet synthesizes EXACTLY ONCE (the `.synthesized` existence
/// marker gates the enqueue, and the marker persists for the life of the
/// fleet), so a stable per-master key is exactly right: a second enqueue for the
/// same master collapses onto the first. No mtime/turns occurrence id — there is
/// no re-arm to distinguish, and the `RECENT_CLAIM_GUARD_WINDOW` can only ever
/// see a benign duplicate here (a genuine re-fire requires the fleet to be fully
/// cleared and a fresh one completed, far beyond the 30s window).
pub(crate) fn peer_fleet_synthesis_dedupe_key(master_session: &SessionKey) -> String {
    format!("external/{PEER_FLEET_SYNTHESIS_EXTERNAL_KIND}/{master_session}")
}

/// The dedupe key for a peer awaiting-input WAKE — keyed on the master
/// (originator) session AND the park's unique pending id (the `ApprovalId` /
/// `QuestionId`, a fresh UUID minted per park). PER-PENDING-ID by design:
///
/// * Two DISTINCT parks carry two distinct pending ids → two distinct keys →
///   two wakes, so each block wakes the master at least once while it is still
///   pending.
/// * A retry of the SAME park re-uses its pending id → the second enqueue
///   collapses onto the first (either as a live pending duplicate, or via the
///   `RECENT_CLAIM_GUARD_WINDOW` once the wake has been drained).
///
/// The pending id is the unique per-occurrence id the `External`-producer
/// invariant requires (a park's id never re-fires under a different park), so
/// there is NO re-arm hazard: the recent-claim guard can only ever suppress the
/// SAME park's key, never a different peer's fresh park. The tradeoff is that N
/// simultaneous parks under one master enqueue N wakes; the first woken turn
/// handles the whole `peer_list` batch and any surplus wakes are harmless
/// no-ops (the master calls `peer_list`, finds nothing pending, ends).
pub(crate) fn peer_awaiting_input_dedupe_key(
    master_session: &SessionKey,
    pending_id: &str,
) -> String {
    format!("external/{PEER_AWAITING_INPUT_EXTERNAL_KIND}/{master_session}/{pending_id}")
}

#[derive(Debug, Clone)]
pub(crate) struct AgentListRequest {
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) connection_profile_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentOutputRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) cursor: Option<OutputCursor>,
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentArtifactReadRequest {
    pub(crate) agent_id: String,
    pub(crate) artifact_id: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GoalSessionRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct GoalSetRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) objective: String,
    pub(crate) status: Option<String>,
    pub(crate) token_budget: Option<u64>,
    pub(crate) transition_actor: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopCreateRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) prompt: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) interval_seconds: Option<u64>,
    pub(crate) mode: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopListRequest {
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopControlKind {
    Delete,
    Pause,
    Resume,
    FireNow,
}

#[derive(Debug, Clone)]
pub(crate) struct LoopControlRequest {
    pub(crate) loop_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) kind: LoopControlKind,
}

/// #1977 — create one monitor. The validated [`crate::autonomy::monitor_runtime::MonitorSpec`] carries the
/// probe shape; `data_dir` (the profile's persistent dir) roots the
/// monitor-notes sidecar the wake path stages matched lines into.
#[derive(Debug, Clone)]
pub(crate) struct MonitorCreateRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) spec: crate::autonomy::monitor_runtime::MonitorSpec,
    pub(crate) data_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct MonitorListRequest {
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorControlKind {
    Pause,
    Resume,
    Delete,
}

#[derive(Debug, Clone)]
pub(crate) struct MonitorControlRequest {
    pub(crate) monitor_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) kind: MonitorControlKind,
}

/// #991 / M15-B — scope for `spawn_agent`. The trait keeps the request
/// surface narrow because the orchestrator-owned launcher is the source
/// of truth for backend kind, sandbox stamp, and policy stamp — the
/// caller only declares which child it wants and the task that child
/// should drive. Optional fields are accepted but always re-validated:
/// client-supplied `agent_id`, `parent_agent_id`, and policy stamps are
/// rejected or ignored as effective state per the M15-B acceptance
/// criteria. The default trait impl returns the
/// `method_not_supported` shape so wire-level callers can detect the
/// orchestrator-not-wired condition without panicking.
#[derive(Debug, Clone)]
#[allow(dead_code)] // #991 wiring never landed: constructed only in tests, kept for the M15-B spawn surface
pub(crate) struct SpawnAgentRequest {
    pub(crate) session_id: SessionKey,
    pub(crate) profile_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) backend_kind: String,
    pub(crate) role: String,
    pub(crate) nickname: String,
    pub(crate) task: String,
    pub(crate) cwd: Option<String>,
}

/// #991 / M15-B — scope for `send_input` (push a user message into a
/// running child) and `wait_agent` (block until terminal). Keeping the
/// two requests identical right now avoids leaking transport details
/// (timeout, cursor) into the trait surface; M15-C will refine wait
/// semantics with streaming once a backend implements it.
#[derive(Debug, Clone)]
#[allow(dead_code)] // #991 wiring never landed: constructed only in tests, kept for the M15-B spawn surface
pub(crate) struct AgentInputRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
    pub(crate) input: String,
}

/// #991 / M15-B — scope for `resume_agent` (re-attach to an existing
/// child by id). Resume is a read-mostly operation today: it returns
/// the agent record so the caller can re-wire its dispatch context
/// without a fresh `agent_list` round-trip.
#[derive(Debug, Clone)]
#[allow(dead_code)] // #991 wiring never landed: constructed only in tests, kept for the M15-B spawn surface
pub(crate) struct ResumeAgentRequest {
    pub(crate) agent_id: String,
    pub(crate) session_id: Option<SessionKey>,
    pub(crate) profile_id: String,
}

#[allow(dead_code)] // spawn/send_input/wait/resume have no production dispatch caller yet (#991 never landed); exercised only by the embedded test suite
pub(crate) trait AgentOrchestrator: Send + Sync {
    fn list_agents(&self, request: AgentListRequest) -> Result<Value, RpcError>;
    fn read_agent_status(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn read_agent_output(&self, request: AgentOutputRequest) -> Result<Value, RpcError>;
    fn list_agent_artifacts(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn read_agent_artifact(&self, request: AgentArtifactReadRequest) -> Result<Value, RpcError>;
    fn interrupt_agent(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn close_agent(&self, request: AgentRequest) -> Result<Value, RpcError>;
    fn get_goal(&self, request: GoalSessionRequest) -> Result<Value, RpcError>;
    fn set_goal(&self, request: GoalSetRequest) -> Result<Value, RpcError>;
    fn clear_goal(&self, request: GoalSessionRequest) -> Result<Value, RpcError>;

    fn operator_transition_goal_with_ledger_sync(
        &self,
        request: GoalSessionRequest,
        goal_id: &str,
        action: &str,
        reason: &str,
        ledger_data_dir: Option<&Path>,
    ) -> Result<Value, RpcError> {
        let _ = (goal_id, action, reason, ledger_data_dir);
        Err(method_not_supported_error(
            "session/goal/operator_transition",
            "goal_operator_transition",
            Some(&request.session_id),
            Some(&request.profile_id),
        ))
    }

    /// #1973 fix B — [`Self::clear_goal`] plus a best-effort sync of the
    /// cleared status into the durable per-goal SQLite ledger under
    /// `ledger_data_dir` (the PROFILE data dir), so the goals-row stops
    /// claiming `active` after a user clear. The RPC dispatch resolves the
    /// dir from the profile store and calls this; a `None` dir (or this
    /// default impl, kept so test mocks stay untouched) is a plain clear.
    fn clear_goal_with_ledger_sync(
        &self,
        request: GoalSessionRequest,
        ledger_data_dir: Option<&Path>,
    ) -> Result<Value, RpcError> {
        let _ = ledger_data_dir;
        self.clear_goal(request)
    }

    fn create_loop(&self, request: LoopCreateRequest) -> Result<Value, RpcError>;
    fn list_loops(&self, request: LoopListRequest) -> Result<Value, RpcError>;
    fn control_loop(&self, request: LoopControlRequest) -> Result<Value, RpcError>;
    /// #1977 monitor runtime CRUD, mirroring the loop family.
    fn create_monitor(&self, request: MonitorCreateRequest) -> Result<Value, RpcError>;
    fn list_monitors(&self, request: MonitorListRequest) -> Result<Value, RpcError>;
    fn control_monitor(&self, request: MonitorControlRequest) -> Result<Value, RpcError>;

    /// #991 / M15-B — kick off a new native/CLI/MCP child via the
    /// orchestrator-owned specialist runner. Default impl returns
    /// `method_not_supported` so existing in-process impls stay
    /// buildable; production implementations override this.
    fn spawn_agent(&self, request: SpawnAgentRequest) -> Result<Value, RpcError> {
        let _ = request;
        Err(method_not_supported_error(
            "agent/spawn",
            "spawn_agent",
            None,
            None,
        ))
    }

    /// #991 / M15-B — push a user input into a running child. Default
    /// impl returns `method_not_supported`; production implementations
    /// route to the supervised process / MCP transport.
    fn send_input(&self, request: AgentInputRequest) -> Result<Value, RpcError> {
        Err(method_not_supported_error(
            "agent/send_input",
            "send_input",
            request.session_id.as_ref(),
            Some(&request.profile_id),
        ))
    }

    /// #991 / M15-B — block on or stream the terminal transition of an
    /// agent. The default impl returns `method_not_supported`; in-
    /// process orchestrators can satisfy this synchronously by reading
    /// the current agent record when the agent is already terminal.
    fn wait_agent(&self, request: AgentRequest) -> Result<Value, RpcError> {
        Err(method_not_supported_error(
            "agent/wait",
            "wait_agent",
            request.session_id.as_ref(),
            Some(&request.profile_id),
        ))
    }

    /// #991 / M15-B — re-attach to an existing child by id. Default
    /// impl returns `method_not_supported`.
    fn resume_agent(&self, request: ResumeAgentRequest) -> Result<Value, RpcError> {
        Err(method_not_supported_error(
            "agent/resume",
            "resume_agent",
            request.session_id.as_ref(),
            Some(&request.profile_id),
        ))
    }
}

/// #991 / M15-B — uniform error shape for trait methods that have a
/// declared default impl but are not implemented on the current
/// orchestrator. Uses the spec §3 `UNSUPPORTED_CAPABILITY` slot so
/// AppUI clients can distinguish "method exists but not wired" from
/// the `METHOD_NOT_FOUND` JSON-RPC dispatch miss.
pub(crate) fn method_not_supported_error(
    method: &str,
    capability: &str,
    session_id: Option<&SessionKey>,
    profile_id: Option<&str>,
) -> RpcError {
    let mut data = serde_json::Map::new();
    data.insert("kind".into(), json!("agent_method_not_supported"));
    data.insert("method".into(), json!(method));
    data.insert("capability".into(), json!(capability));
    data.insert("recoverable".into(), json!(false));
    if let Some(session_id) = session_id {
        data.insert("session_id".into(), json!(session_id));
    }
    if let Some(profile_id) = profile_id {
        data.insert("profile_id".into(), json!(profile_id));
    }
    RpcError::new(
        rpc_error_codes::UNSUPPORTED_CAPABILITY,
        format!("{method} is not implemented on this orchestrator"),
    )
    .with_data(Value::Object(data))
}
