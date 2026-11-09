//! M15 production primitives for goal and loop scheduling policy. These are
//! intentionally independent from AppUI handlers until the runtime scheduler
//! wiring is added.

use std::fmt;
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GoalId(pub String);

impl GoalId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for GoalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LoopId(pub String);

impl LoopId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for LoopId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBudget {
    pub max: u32,
    pub used: u32,
}

impl RuntimeBudget {
    pub fn with_used(max: u32, used: u32) -> Self {
        Self { max, used }
    }

    pub fn remaining(&self) -> u32 {
        self.max.saturating_sub(self.used)
    }

    pub fn is_exhausted(&self) -> bool {
        self.used >= self.max
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextDueState {
    Ready,
    ScheduledAt(SystemTime),
    /// Runtime state entered when a self-paced scheduler waits for its signal.
    /// The scheduler is wired outside this primitive; keep the state contract
    /// visible in non-test builds rather than gating it on its tests.
    #[allow(dead_code)]
    WaitingForSelfPacedSignal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitUntil {
    At(SystemTime),
    SelfPacedSignal,
    RuntimeIdle(RuntimeIdleBlocker),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeActivity {
    Idle,
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeIdleBlocker {
    SessionClosed,
    ActiveTurn,
    UserInputPending,
    ApprovalPending,
    RequestUserInputPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeIdleState {
    pub session_open: bool,
    pub activity: RuntimeActivity,
    pub user_input_pending: bool,
    pub approval_pending: bool,
    pub request_user_input_pending: bool,
}

impl RuntimeIdleState {
    pub fn idle() -> Self {
        Self {
            session_open: true,
            activity: RuntimeActivity::Idle,
            user_input_pending: false,
            approval_pending: false,
            request_user_input_pending: false,
        }
    }

    pub fn with_user_input_pending(mut self, pending: bool) -> Self {
        self.user_input_pending = pending;
        self
    }

    pub fn idle_blocker(self) -> Option<RuntimeIdleBlocker> {
        if !self.session_open {
            return Some(RuntimeIdleBlocker::SessionClosed);
        }
        if self.activity == RuntimeActivity::Busy {
            return Some(RuntimeIdleBlocker::ActiveTurn);
        }
        if self.user_input_pending {
            return Some(RuntimeIdleBlocker::UserInputPending);
        }
        if self.approval_pending {
            return Some(RuntimeIdleBlocker::ApprovalPending);
        }
        if self.request_user_input_pending {
            return Some(RuntimeIdleBlocker::RequestUserInputPending);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeSchedulePriority {
    /// Goal continuations precede loop fires in the scheduler contract.
    #[allow(dead_code)]
    GoalContinuation,
    LoopFire,
}

impl RuntimeSchedulePriority {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    MissingPolicy,
    ExhaustedBudget,
    Paused,
    Deleted,
    InvalidInterval,
    RuntimeBusy,
    SlashCommandDenied,
    PromptResolutionFailed,
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPolicy => f.write_str("missing policy"),
            Self::ExhaustedBudget => f.write_str("exhausted budget"),
            Self::Paused => f.write_str("runtime paused"),
            Self::Deleted => f.write_str("runtime deleted"),
            Self::InvalidInterval => f.write_str("invalid interval"),
            Self::RuntimeBusy => f.write_str("runtime busy"),
            Self::SlashCommandDenied => f.write_str("slash command denied"),
            Self::PromptResolutionFailed => f.write_str("prompt resolution failed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCadence {
    FixedInterval(Duration),
    /// Signal-driven goals are part of the runtime policy contract; the
    /// scheduler wiring constructs this variant outside this module.
    #[allow(dead_code)]
    SelfPaced,
}

impl GoalCadence {
    pub fn is_valid(&self) -> bool {
        match self {
            Self::FixedInterval(interval) => !interval.is_zero(),
            Self::SelfPaced => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRuntimePolicy {
    pub cadence: GoalCadence,
    pub max_continuations: u32,
}

impl GoalRuntimePolicy {
    pub fn fixed_interval(interval: Duration, max_continuations: u32) -> Self {
        Self {
            cadence: GoalCadence::FixedInterval(interval),
            max_continuations,
        }
    }

    fn budget(&self, used: u32) -> RuntimeBudget {
        RuntimeBudget::with_used(self.max_continuations, used)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalRuntimeState {
    Active,
    Paused,
    Completed,
}

/// Outcome of the INDEPENDENT goal-completion verifier.
///
/// Loop-engineering (addyosmani.com/blog/loop-engineering): a `/goal` should run
/// "until a verifiable stopping condition is met, with a separate model checking
/// completion rather than the agent self-grading." This is the goal-level
/// analogue of [`octos_agent`]'s `VerifierVerdict::ReadyToAnswer`: the agent's
/// own `<goal:complete>` sentinel is only a *claim*; a separate cheap-lane
/// verifier must independently return [`GoalCompletionVerdict::Done`] before the
/// goal may transition to `Completed`. Any `NotDone` keeps the goal `Active`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCompletionVerdict {
    /// The separate verifier independently confirmed the objective is met.
    Done,
    /// The verifier judged the objective not yet met (or the check could not be
    /// run); the goal stays `Active`. Carries a short reason for the ledger.
    NotDone { reason: String },
}

impl GoalCompletionVerdict {
    /// Whether the independent verifier confirmed completion.
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done)
    }
}

/// Structured classification of why a goal-completion verifier outcome was
/// not a clean `Done` — the orthogonal axis to [`GoalCompletionVerdict`].
///
/// The verdict answers "is the objective met?"; this kind answers "how
/// healthy was the verification call itself?". Mixing the two (the
/// pre-evo-verifier `NotDone { reason }`) made infrastructure failures
/// indistinguishable from semantic evidence gaps, so callers could neither
/// retry sensibly nor report what was missing.
///
/// `None` on the outcome ⟺ `Done` (a successful verification has no failure
/// kind — the illegal `Done + CallFailed` state is unrepresentable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalVerifierFailureKind {
    /// The provider call itself failed (`chat` returned `Err`). Transient —
    /// retried at most once when the underlying error is retryable.
    CallFailed,
    /// Provider returned `Ok` with empty/whitespace content (including the
    /// reasoning-only case: `reasoning_content` non-empty, `content` empty —
    /// thinking burned the budget, #23 family). Transient — retried once.
    EmptyResponse,
    /// Non-empty reply that violates the verdict protocol: `DONE` with
    /// trailing text, multi-line residue, unpaired backticks, prose
    /// negation, `Done.` — anything that is not an exact verdict. NOT
    /// retried (same prompt ⇒ same malformed answer; blind retry just burns
    /// 2× tokens). The reason embeds a truncated quote of the raw reply so
    /// the agent can self-correct next turn.
    InvalidResponse,
    /// The verifier answered `NOT_DONE: <reason>` — a *semantic* verdict
    /// that the objective is not yet met. Never retried; `missing_evidence`
    /// carries what the verifier found lacking.
    InsufficientEvidence,
}

impl GoalVerifierFailureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CallFailed => "call_failed",
            Self::EmptyResponse => "empty_response",
            Self::InvalidResponse => "invalid_response",
            Self::InsufficientEvidence => "insufficient_evidence",
        }
    }
}

impl fmt::Display for GoalVerifierFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ledger `outcome` value: the failure kinds plus the success value, so a
/// `Done` verdict is representable in the persistent verifier ledger.
pub const GOAL_VERIFIER_OUTCOME_DONE: &str = "done";

/// Structured result of one bounded goal-completion verification run.
///
/// Wraps the (unchanged) [`GoalCompletionVerdict`] with the orthogonal
/// failure classification, attempt count, semantic gap, and replay provenance.
/// Token usage is persisted in the verifier ledger; it is not duplicated in
/// this runtime outcome.
#[derive(Debug, Clone)]
pub struct GoalVerifierOutcome {
    /// The semantic verdict — unchanged type, so `maybe_complete_goal_from_model`
    /// and its guards see exactly what they saw before.
    pub verdict: GoalCompletionVerdict,
    /// `None` ⟺ `verdict` is `Done`.
    pub kind: Option<GoalVerifierFailureKind>,
    /// Number of provider calls made in this run (≤2; 0 on a gate replay).
    pub attempts: u32,
    /// True when no new call was made — the persistent same-evidence gate
    /// replayed a ledger verdict (`replayed_of_ts` is that record's ts).
    pub replayed: bool,
    pub replayed_of_ts_ms: Option<u64>,
    /// Composite diagnostic (k3 round-3 B): set when the verification could
    /// not complete its bookkeeping — e.g. a Done verdict whose ledger write
    /// failed (fail-closed), or an unreadable gate ledger. The goal stays
    /// open; the message names the remediation path.
    pub diagnostic: Option<String>,
}

impl GoalVerifierOutcome {
    pub fn is_done(&self) -> bool {
        self.verdict.is_done()
    }

    /// NotDone reason for call sites: classification + attempts, never the
    /// bare `verifier returned: ` of the pre-evo-verifier era.
    pub fn not_done_reason(&self) -> Option<&str> {
        match &self.verdict {
            GoalCompletionVerdict::Done => None,
            GoalCompletionVerdict::NotDone { reason } => Some(reason),
        }
    }
}

/// evo-goal-verifier (spec Decision 1/6, M5): ONE canonical human-readable
/// NotDone line shared by every call site — goal_tool's ToolResult output
/// and the three sentinel stations' failure notes — so the structured
/// `{kind} (attempt n/2): reason [replayed…][diagnostic]` format cannot
/// drift between the four entry points. Done renders as `done`.
impl fmt::Display for GoalVerifierOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_done() {
            return f.write_str("done");
        }
        let kind = self.kind.map(|k| k.as_str()).unwrap_or("unknown");
        write!(
            f,
            "verifier {kind} (attempt {}/{}): {}",
            self.attempts,
            crate::autonomy::agent_orchestrator::VERIFIER_MAX_ATTEMPTS,
            self.not_done_reason().unwrap_or("unknown")
        )?;
        if self.replayed {
            match self.replayed_of_ts_ms {
                Some(ts) => write!(f, " [replayed verdict from {ts}]")?,
                None => f.write_str(" [replayed verdict from history]")?,
            }
        }
        if let Some(d) = &self.diagnostic {
            write!(f, " [diagnostic: {d}]")?;
        }
        Ok(())
    }
}

/// Parsed verdict of a raw verifier reply — the pure, sync, provider-free
/// core of the strict DONE protocol (spec Decision 2, final rule).
///
/// Deterministic ladder (no heuristics):
/// 1. `None`/whitespace-only content → `EmptyResponse` (reasoning-only
///    replies land here too — thinking burned the budget).
/// 2. Strip at most one layer of a *paired* markdown fence.
/// 3. Strip *paired* backticks (repeatable; unpaired are left alone).
/// 4. Let L = the non-blank lines.
/// 5. `|L| == 1` and `L[0]` case-insensitively `DONE` → `Done`.
/// 6. Else `L[0]` (after paired-backtick strip) case-insensitively starts
///    with `NOT_DONE` → `InsufficientEvidence`, missing = text after the
///    colon (capped at 200 chars).
/// 7. Else (trailing text, multi-line residue, `Done.`, prose) →
///    `InvalidResponse` with a truncated quote of the raw reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedVerifierReply {
    pub verdict: GoalCompletionVerdict,
    pub kind: Option<GoalVerifierFailureKind>,
    pub missing_evidence: Option<String>,
}

/// Max chars of the raw reply quoted into an InvalidResponse reason —
/// enough for the agent to see and fix its format next turn (spec Decision 2).
pub const VERIFIER_INVALID_QUOTE_CHARS: usize = 120;
/// Max chars of the NOT_DONE tail kept as missing evidence (pre-existing
/// `take(200)` cap, preserved).
pub const VERIFIER_MISSING_EVIDENCE_CHARS: usize = 200;

fn strip_paired_backticks(mut text: &str) -> &str {
    loop {
        let bytes = text.as_bytes();
        if bytes.len() >= 2 && bytes.first() == Some(&b'`') && bytes.last() == Some(&b'`') {
            text = &text[1..text.len() - 1];
        } else {
            return text;
        }
    }
}

/// The strict verdict protocol parser (pure; see [`ParsedVerifierReply`]).
pub fn classify_verifier_reply(
    content: Option<&str>,
    _reasoning_present: bool,
) -> ParsedVerifierReply {
    let raw = content.unwrap_or("");
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ParsedVerifierReply {
            verdict: GoalCompletionVerdict::NotDone {
                reason: "verifier returned EMPTY content (Ok but no text)".to_owned(),
            },
            kind: Some(GoalVerifierFailureKind::EmptyResponse),
            missing_evidence: None,
        };
    }
    // At most one layer of a paired ``` fence.
    let mut t = trimmed;
    if let Some(inner) = t.strip_prefix("```") {
        if let Some(body) = inner.strip_suffix("```") {
            t = body.trim();
        }
    }
    t = strip_paired_backticks(t);
    let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() == 1 && lines[0].eq_ignore_ascii_case("done") {
        return ParsedVerifierReply {
            verdict: GoalCompletionVerdict::Done,
            kind: None,
            missing_evidence: None,
        };
    }
    let first = strip_paired_backticks(lines.first().copied().unwrap_or(""));
    // Safe prefix match: NO arbitrary byte slicing (a multi-byte reply like
    // 💡💡💡 has no char boundary at any fixed byte index — the previous
    // `first[..9]` sliced into a code point and panicked). Also NOT_DONE is
    // 8 bytes, not 9: match the exact prefix case-INSENSITIVELY over ASCII
    // (spec Decision 2 keeps `done`/`Done` Done — the same leniency applies
    // to `not_done`/`nOt_DoNe`; outer defect ⑥), and require the next char
    // to be a delimiter (`:`/space/end) so fused tokens like NOT_DONEgarbage
    // stay protocol violations (InvalidResponse), not semantic NOT_DONEs.
    let bytes = first.as_bytes();
    let prefix_matches = bytes.len() >= 8
        && bytes[..8]
            .iter()
            .zip(b"NOT_DONE")
            .all(|(b, p)| b.eq_ignore_ascii_case(p));
    let not_done_tail = if prefix_matches {
        Some(&first[8..])
    } else {
        None
    }
    .filter(|tail| tail.is_empty() || tail.starts_with(':') || tail.starts_with(' '));
    if let Some(tail) = not_done_tail {
        let missing = tail.trim_start_matches([':', ' ']).trim();
        let missing: String = missing
            .chars()
            .take(VERIFIER_MISSING_EVIDENCE_CHARS)
            .collect();
        return ParsedVerifierReply {
            verdict: GoalCompletionVerdict::NotDone {
                reason: format!("insufficient evidence: {missing}"),
            },
            kind: Some(GoalVerifierFailureKind::InsufficientEvidence),
            missing_evidence: Some(missing),
        };
    }
    let quote: String = trimmed.chars().take(VERIFIER_INVALID_QUOTE_CHARS).collect();
    ParsedVerifierReply {
        verdict: GoalCompletionVerdict::NotDone {
            reason: format!("verifier reply violated the verdict protocol: {quote}"),
        },
        kind: Some(GoalVerifierFailureKind::InvalidResponse),
        missing_evidence: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRuntime {
    pub goal_id: GoalId,
    pub objective: String,
    pub policy: Option<GoalRuntimePolicy>,
    pub next_due: NextDueState,
    pub state: GoalRuntimeState,
    pub continuations_used: u32,
    pub runtime_busy: bool,
}

impl GoalRuntime {
    pub fn new(
        goal_id: impl Into<String>,
        objective: impl Into<String>,
        policy: GoalRuntimePolicy,
    ) -> Self {
        Self {
            goal_id: GoalId::new(goal_id),
            objective: objective.into(),
            policy: Some(policy),
            next_due: NextDueState::Ready,
            state: GoalRuntimeState::Active,
            continuations_used: 0,
            runtime_busy: false,
        }
    }

    pub fn decide(&self, now: SystemTime) -> GoalPolicyDecision {
        match &self.state {
            GoalRuntimeState::Completed => return GoalPolicyDecision::Completed,
            GoalRuntimeState::Paused => return GoalPolicyDecision::Denied(DenyReason::Paused),
            GoalRuntimeState::Active => {}
        }

        if self.runtime_busy {
            return GoalPolicyDecision::Denied(DenyReason::RuntimeBusy);
        }

        let Some(policy) = &self.policy else {
            return GoalPolicyDecision::Denied(DenyReason::MissingPolicy);
        };

        if !policy.cadence.is_valid() {
            return GoalPolicyDecision::Denied(DenyReason::InvalidInterval);
        }

        let budget = policy.budget(self.continuations_used);
        if budget.is_exhausted() {
            return GoalPolicyDecision::Denied(DenyReason::ExhaustedBudget);
        }

        match &self.next_due {
            NextDueState::Ready => GoalPolicyDecision::ContinueNow {
                goal_id: self.goal_id.clone(),
                remaining_budget: budget.remaining(),
            },
            NextDueState::ScheduledAt(due_at) if *due_at <= now => {
                GoalPolicyDecision::ContinueNow {
                    goal_id: self.goal_id.clone(),
                    remaining_budget: budget.remaining(),
                }
            }
            NextDueState::ScheduledAt(due_at) => {
                GoalPolicyDecision::WaitUntil(WaitUntil::At(*due_at))
            }
            NextDueState::WaitingForSelfPacedSignal => {
                GoalPolicyDecision::WaitUntil(WaitUntil::SelfPacedSignal)
            }
        }
    }

    pub fn decide_when_idle(
        &self,
        now: SystemTime,
        runtime_state: RuntimeIdleState,
    ) -> GoalPolicyDecision {
        match self.decide(now) {
            GoalPolicyDecision::ContinueNow {
                goal_id,
                remaining_budget,
            } => {
                if let Some(blocker) = runtime_state.idle_blocker() {
                    GoalPolicyDecision::WaitUntil(WaitUntil::RuntimeIdle(blocker))
                } else {
                    GoalPolicyDecision::ContinueNow {
                        goal_id,
                        remaining_budget,
                    }
                }
            }
            decision => decision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalPolicyDecision {
    ContinueNow {
        goal_id: GoalId,
        remaining_budget: u32,
    },
    WaitUntil(WaitUntil),
    Denied(DenyReason),
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopCadence {
    FixedInterval(Duration),
    SelfPaced,
}

impl LoopCadence {
    pub fn is_valid(&self) -> bool {
        match self {
            Self::FixedInterval(interval) => !interval.is_zero(),
            Self::SelfPaced => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopInvocation {
    Prompt(String),
    SlashCommand(String),
    MaintenancePrompt,
}

impl LoopInvocation {
    pub fn prompt(prompt: impl Into<String>) -> Self {
        Self::Prompt(prompt.into())
    }

    pub fn slash_command(command: impl Into<String>) -> Self {
        Self::SlashCommand(command.into())
    }

    pub fn maintenance_prompt() -> Self {
        Self::MaintenancePrompt
    }

    pub fn requires_slash_authorization(&self) -> bool {
        matches!(self, Self::SlashCommand(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopRuntimeMode {
    FixedInterval,
    SelfPaced,
    Maintenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopSlashCommandPolicy {
    pub allow_slash_commands: bool,
    pub reauthorize_each_fire: bool,
}

impl LoopSlashCommandPolicy {
    pub fn allow_with_reauthorization() -> Self {
        Self {
            allow_slash_commands: true,
            reauthorize_each_fire: true,
        }
    }
}

impl Default for LoopSlashCommandPolicy {
    fn default() -> Self {
        Self::allow_with_reauthorization()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopRuntimePolicy {
    pub cadence: LoopCadence,
    pub max_fires: u32,
    pub mode: LoopRuntimeMode,
    pub slash_command_policy: LoopSlashCommandPolicy,
}

impl LoopRuntimePolicy {
    pub fn fixed_interval(interval: Duration, max_fires: u32) -> Self {
        Self {
            cadence: LoopCadence::FixedInterval(interval),
            max_fires,
            mode: LoopRuntimeMode::FixedInterval,
            slash_command_policy: LoopSlashCommandPolicy::default(),
        }
    }

    pub fn self_paced(max_fires: u32) -> Self {
        Self {
            cadence: LoopCadence::SelfPaced,
            max_fires,
            mode: LoopRuntimeMode::SelfPaced,
            slash_command_policy: LoopSlashCommandPolicy::default(),
        }
    }

    pub fn maintenance(max_fires: u32) -> Self {
        Self {
            cadence: LoopCadence::SelfPaced,
            max_fires,
            mode: LoopRuntimeMode::Maintenance,
            slash_command_policy: LoopSlashCommandPolicy::default(),
        }
    }

    fn budget(&self, used: u32) -> RuntimeBudget {
        RuntimeBudget::with_used(self.max_fires, used)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopRuntimeState {
    Active,
    Paused,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopFireTrigger {
    /// Immediate creation fire, distinct from a later scheduled fire in the
    /// runtime audit trail; construction belongs to the scheduler adapter.
    #[allow(dead_code)]
    CreationImmediate,
    ScheduledDue,
    FireNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommandAuthorization {
    pub authorized_at_creation: bool,
    pub authorized_now: bool,
}

impl SlashCommandAuthorization {
    pub fn authorized_now() -> Self {
        Self {
            authorized_at_creation: false,
            authorized_now: true,
        }
    }

    pub fn authorized_at_creation_only() -> Self {
        Self {
            authorized_at_creation: true,
            authorized_now: false,
        }
    }
}

impl Default for SlashCommandAuthorization {
    fn default() -> Self {
        Self::authorized_now()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopFireContext {
    pub runtime_state: RuntimeIdleState,
    pub slash_authorization: SlashCommandAuthorization,
}

impl LoopFireContext {
    pub fn idle() -> Self {
        Self {
            runtime_state: RuntimeIdleState::idle(),
            slash_authorization: SlashCommandAuthorization::default(),
        }
    }

    pub fn with_slash_authorization(
        mut self,
        slash_authorization: SlashCommandAuthorization,
    ) -> Self {
        self.slash_authorization = slash_authorization;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopSlashAuthorization {
    pub required: bool,
    pub authorized_at_creation: bool,
    pub authorized_now: bool,
    pub reauthorized_each_fire: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopFirePlan {
    pub loop_id: LoopId,
    pub invocation: LoopInvocation,
    pub trigger: LoopFireTrigger,
    pub priority: RuntimeSchedulePriority,
    pub remaining_budget: u32,
    pub slash_authorization: LoopSlashAuthorization,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopFireDecision {
    Fire(LoopFirePlan),
    WaitUntil(WaitUntil),
    Denied(DenyReason),
    Exhausted { reason: DenyReason },
}

pub const BUILT_IN_MAINTENANCE_PROMPT: &str = "run maintenance checks";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenancePromptSource {
    Project,
    User,
    BuiltIn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenancePromptResolution {
    pub source: MaintenancePromptSource,
    pub prompt: String,
}

pub fn resolve_maintenance_prompt(
    project_prompt: Option<&str>,
    user_prompt: Option<&str>,
    built_in_prompt: &str,
) -> Result<MaintenancePromptResolution, DenyReason> {
    if let Some(prompt) = nonempty_prompt(project_prompt) {
        return Ok(MaintenancePromptResolution {
            source: MaintenancePromptSource::Project,
            prompt,
        });
    }
    if let Some(prompt) = nonempty_prompt(user_prompt) {
        return Ok(MaintenancePromptResolution {
            source: MaintenancePromptSource::User,
            prompt,
        });
    }
    if let Some(prompt) = nonempty_prompt(Some(built_in_prompt)) {
        return Ok(MaintenancePromptResolution {
            source: MaintenancePromptSource::BuiltIn,
            prompt,
        });
    }

    Err(DenyReason::PromptResolutionFailed)
}

fn nonempty_prompt(prompt: Option<&str>) -> Option<String> {
    let prompt = prompt?.trim();
    if prompt.is_empty() {
        None
    } else {
        Some(prompt.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopRuntime {
    pub loop_id: LoopId,
    pub invocation: LoopInvocation,
    pub policy: Option<LoopRuntimePolicy>,
    pub next_due: NextDueState,
    pub state: LoopRuntimeState,
    pub fires_used: u32,
    pub runtime_busy: bool,
}

impl LoopRuntime {
    pub fn new(
        loop_id: impl Into<String>,
        invocation: LoopInvocation,
        policy: LoopRuntimePolicy,
    ) -> Self {
        Self {
            loop_id: LoopId::new(loop_id),
            invocation,
            policy: Some(policy),
            next_due: NextDueState::Ready,
            state: LoopRuntimeState::Active,
            fires_used: 0,
            runtime_busy: false,
        }
    }

    /// #1130 — seed the runtime view with a previously persisted
    /// `fires_used` counter. `loop_runtime_view` (the
    /// `AutonomyLoopRecord` → `LoopRuntime` adapter in
    /// `agent_orchestrator.rs`) was rebuilding a fresh runtime on every
    /// decision call, so `decide_fire` always saw `fires_used == 0` and
    /// the `LOOP_DEFAULT_MAX_FIRES` budget never tripped — any loop that
    /// out-lived its budget through repeated `fire_now` calls or a
    /// long-running fixed schedule kept firing forever. This setter is
    /// the production wire-through: callers pass the
    /// `AutonomyLoopRecord.fires_used` they read from the supervisor
    /// store, and the runtime's budget gate then sees the real count.
    pub fn with_fires_used(mut self, fires: u32) -> Self {
        self.fires_used = fires;
        self
    }

    pub fn decide_fire(
        &self,
        now: SystemTime,
        trigger: LoopFireTrigger,
        context: LoopFireContext,
    ) -> LoopFireDecision {
        match self.state {
            LoopRuntimeState::Deleted => return LoopFireDecision::Denied(DenyReason::Deleted),
            LoopRuntimeState::Paused => return LoopFireDecision::Denied(DenyReason::Paused),
            LoopRuntimeState::Active => {}
        }

        if self.runtime_busy {
            return LoopFireDecision::Denied(DenyReason::RuntimeBusy);
        }

        let Some(policy) = &self.policy else {
            return LoopFireDecision::Denied(DenyReason::MissingPolicy);
        };

        if !policy.cadence.is_valid() {
            return LoopFireDecision::Denied(DenyReason::InvalidInterval);
        }

        let budget = policy.budget(self.fires_used);
        if budget.is_exhausted() {
            return LoopFireDecision::Exhausted {
                reason: DenyReason::ExhaustedBudget,
            };
        }

        if trigger == LoopFireTrigger::ScheduledDue {
            match &self.next_due {
                NextDueState::Ready => {}
                NextDueState::ScheduledAt(due_at) if *due_at <= now => {}
                NextDueState::ScheduledAt(due_at) => {
                    return LoopFireDecision::WaitUntil(WaitUntil::At(*due_at));
                }
                NextDueState::WaitingForSelfPacedSignal => {
                    return LoopFireDecision::WaitUntil(WaitUntil::SelfPacedSignal);
                }
            }
        }

        if let Some(blocker) = context.runtime_state.idle_blocker() {
            return match trigger {
                LoopFireTrigger::FireNow => LoopFireDecision::Denied(DenyReason::RuntimeBusy),
                LoopFireTrigger::CreationImmediate | LoopFireTrigger::ScheduledDue => {
                    LoopFireDecision::WaitUntil(WaitUntil::RuntimeIdle(blocker))
                }
            };
        }

        let slash_authorization =
            match self.authorize_invocation_for_fire(context.slash_authorization) {
                Ok(authorization) => authorization,
                Err(reason) => return LoopFireDecision::Denied(reason),
            };

        LoopFireDecision::Fire(LoopFirePlan {
            loop_id: self.loop_id.clone(),
            invocation: self.invocation.clone(),
            trigger,
            priority: self.fire_priority(),
            remaining_budget: budget.remaining(),
            slash_authorization,
        })
    }

    pub fn fire_priority(&self) -> RuntimeSchedulePriority {
        RuntimeSchedulePriority::LoopFire
    }

    pub fn authorize_invocation_for_fire(
        &self,
        slash_authorization: SlashCommandAuthorization,
    ) -> Result<LoopSlashAuthorization, DenyReason> {
        let Some(policy) = &self.policy else {
            return Err(DenyReason::MissingPolicy);
        };

        let required = self.invocation.requires_slash_authorization();
        let resolution = LoopSlashAuthorization {
            required,
            authorized_at_creation: slash_authorization.authorized_at_creation,
            authorized_now: slash_authorization.authorized_now,
            reauthorized_each_fire: policy.slash_command_policy.reauthorize_each_fire,
        };

        if !required {
            return Ok(resolution);
        }

        if !policy.slash_command_policy.allow_slash_commands {
            return Err(DenyReason::SlashCommandDenied);
        }

        if policy.slash_command_policy.reauthorize_each_fire {
            if slash_authorization.authorized_now {
                Ok(resolution)
            } else {
                Err(DenyReason::SlashCommandDenied)
            }
        } else if slash_authorization.authorized_now || slash_authorization.authorized_at_creation {
            Ok(resolution)
        } else {
            Err(DenyReason::SlashCommandDenied)
        }
    }

    pub fn pause(&mut self) {
        if self.state == LoopRuntimeState::Active {
            self.state = LoopRuntimeState::Paused;
        }
    }

    pub fn delete(&mut self) {
        self.state = LoopRuntimeState::Deleted;
    }
}
