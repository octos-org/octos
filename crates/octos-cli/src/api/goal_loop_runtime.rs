#![allow(dead_code)]
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
    pub fn new(max: u32) -> Self {
        Self { max, used: 0 }
    }

    pub fn with_used(max: u32, used: u32) -> Self {
        Self { max, used }
    }

    pub fn remaining(&self) -> u32 {
        self.max.saturating_sub(self.used)
    }

    pub fn is_exhausted(&self) -> bool {
        self.used >= self.max
    }

    pub fn record_use(&mut self) {
        self.used = self.used.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextDueState {
    Ready,
    ScheduledAt(SystemTime),
    WaitingForSelfPacedSignal,
}

impl NextDueState {
    pub fn is_due(&self, now: SystemTime) -> bool {
        match self {
            Self::Ready => true,
            Self::ScheduledAt(due_at) => *due_at <= now,
            Self::WaitingForSelfPacedSignal => false,
        }
    }
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

    pub fn busy() -> Self {
        Self {
            activity: RuntimeActivity::Busy,
            ..Self::idle()
        }
    }

    pub fn closed() -> Self {
        Self {
            session_open: false,
            ..Self::idle()
        }
    }

    pub fn with_user_input_pending(mut self, pending: bool) -> Self {
        self.user_input_pending = pending;
        self
    }

    pub fn with_approval_pending(mut self, pending: bool) -> Self {
        self.approval_pending = pending;
        self
    }

    pub fn with_request_user_input_pending(mut self, pending: bool) -> Self {
        self.request_user_input_pending = pending;
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

    pub fn is_idle_eligible(self) -> bool {
        self.idle_blocker().is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeSchedulePriority {
    GoalContinuation,
    LoopFire,
}

impl RuntimeSchedulePriority {
    pub fn rank(self) -> u8 {
        match self {
            Self::GoalContinuation => 10,
            Self::LoopFire => 30,
        }
    }
}

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
    Failed(String),
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
            Self::Failed(reason) => write!(f, "runtime failed: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCadence {
    FixedInterval(Duration),
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

    pub fn self_paced(max_continuations: u32) -> Self {
        Self {
            cadence: GoalCadence::SelfPaced,
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
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalBudgetResolution {
    pub goal_id: GoalId,
    pub reason: DenyReason,
    pub wrap_up_prompt: String,
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

    pub fn without_policy(goal_id: impl Into<String>, objective: impl Into<String>) -> Self {
        Self {
            goal_id: GoalId::new(goal_id),
            objective: objective.into(),
            policy: None,
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
            GoalRuntimeState::Failed(reason) => {
                return GoalPolicyDecision::Denied(DenyReason::Failed(reason.clone()));
            }
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

    pub fn continuation_priority(&self) -> RuntimeSchedulePriority {
        RuntimeSchedulePriority::GoalContinuation
    }

    pub fn runtime_budget(&self) -> Option<RuntimeBudget> {
        self.policy
            .as_ref()
            .map(|policy| policy.budget(self.continuations_used))
    }

    pub fn budget_resolution(&self) -> Option<GoalBudgetResolution> {
        let budget = self.runtime_budget()?;
        if !budget.is_exhausted() {
            return None;
        }

        Some(GoalBudgetResolution {
            goal_id: self.goal_id.clone(),
            reason: DenyReason::ExhaustedBudget,
            wrap_up_prompt: format!(
                "Goal `{}` has exhausted its continuation budget. Summarize the current state, call out remaining work, and stop starting new work.",
                self.goal_id
            ),
        })
    }

    pub fn mark_due(&mut self) {
        self.next_due = NextDueState::Ready;
    }

    pub fn schedule_at(&mut self, due_at: SystemTime) {
        self.next_due = NextDueState::ScheduledAt(due_at);
    }

    pub fn wait_for_self_paced_signal(&mut self) {
        self.next_due = NextDueState::WaitingForSelfPacedSignal;
    }

    pub fn record_continuation(&mut self, now: SystemTime) -> Result<(), DenyReason> {
        let Some(policy) = &self.policy else {
            return Err(DenyReason::MissingPolicy);
        };

        if !policy.cadence.is_valid() {
            return Err(DenyReason::InvalidInterval);
        }

        let budget = policy.budget(self.continuations_used);
        if budget.is_exhausted() {
            return Err(DenyReason::ExhaustedBudget);
        }

        self.continuations_used = self.continuations_used.saturating_add(1);
        self.next_due = next_due_after_fire(&policy.cadence, now);
        Ok(())
    }

    pub fn pause(&mut self) {
        self.state = GoalRuntimeState::Paused;
    }

    pub fn resume(&mut self) {
        if self.state == GoalRuntimeState::Paused {
            self.state = GoalRuntimeState::Active;
        }
    }

    pub fn complete(&mut self) {
        self.state = GoalRuntimeState::Completed;
    }

    pub fn fail(&mut self, reason: impl Into<String>) {
        self.state = GoalRuntimeState::Failed(reason.into());
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

    pub fn deny() -> Self {
        Self {
            allow_slash_commands: false,
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

    pub fn with_slash_command_policy(mut self, policy: LoopSlashCommandPolicy) -> Self {
        self.slash_command_policy = policy;
        self
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

    pub fn denied() -> Self {
        Self {
            authorized_at_creation: false,
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

    pub fn with_runtime_state(mut self, runtime_state: RuntimeIdleState) -> Self {
        self.runtime_state = runtime_state;
        self
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

    pub fn without_policy(loop_id: impl Into<String>, invocation: LoopInvocation) -> Self {
        Self {
            loop_id: LoopId::new(loop_id),
            invocation,
            policy: None,
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

    pub fn decide(&self, now: SystemTime) -> LoopPolicyDecision {
        match self.state {
            LoopRuntimeState::Deleted => return LoopPolicyDecision::Denied(DenyReason::Deleted),
            LoopRuntimeState::Paused => return LoopPolicyDecision::Denied(DenyReason::Paused),
            LoopRuntimeState::Active => {}
        }

        if self.runtime_busy {
            return LoopPolicyDecision::Denied(DenyReason::RuntimeBusy);
        }

        let Some(policy) = &self.policy else {
            return LoopPolicyDecision::Denied(DenyReason::MissingPolicy);
        };

        if !policy.cadence.is_valid() {
            return LoopPolicyDecision::Denied(DenyReason::InvalidInterval);
        }

        let budget = policy.budget(self.fires_used);
        if budget.is_exhausted() {
            return LoopPolicyDecision::Exhausted {
                reason: DenyReason::ExhaustedBudget,
            };
        }

        match &self.next_due {
            NextDueState::Ready => LoopPolicyDecision::FireNow {
                loop_id: self.loop_id.clone(),
                invocation: self.invocation.clone(),
                remaining_budget: budget.remaining(),
            },
            NextDueState::ScheduledAt(due_at) if *due_at <= now => LoopPolicyDecision::FireNow {
                loop_id: self.loop_id.clone(),
                invocation: self.invocation.clone(),
                remaining_budget: budget.remaining(),
            },
            NextDueState::ScheduledAt(due_at) => {
                LoopPolicyDecision::WaitUntil(WaitUntil::At(*due_at))
            }
            NextDueState::WaitingForSelfPacedSignal => {
                LoopPolicyDecision::WaitUntil(WaitUntil::SelfPacedSignal)
            }
        }
    }

    pub fn decide_when_idle(
        &self,
        now: SystemTime,
        runtime_state: RuntimeIdleState,
    ) -> LoopPolicyDecision {
        match self.decide(now) {
            LoopPolicyDecision::FireNow {
                loop_id,
                invocation,
                remaining_budget,
            } => {
                if let Some(blocker) = runtime_state.idle_blocker() {
                    LoopPolicyDecision::WaitUntil(WaitUntil::RuntimeIdle(blocker))
                } else {
                    LoopPolicyDecision::FireNow {
                        loop_id,
                        invocation,
                        remaining_budget,
                    }
                }
            }
            decision => decision,
        }
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

    pub fn runtime_budget(&self) -> Option<RuntimeBudget> {
        self.policy
            .as_ref()
            .map(|policy| policy.budget(self.fires_used))
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

    pub fn mark_due(&mut self) {
        self.next_due = NextDueState::Ready;
    }

    pub fn schedule_at(&mut self, due_at: SystemTime) {
        self.next_due = NextDueState::ScheduledAt(due_at);
    }

    pub fn wait_for_self_paced_signal(&mut self) {
        self.next_due = NextDueState::WaitingForSelfPacedSignal;
    }

    pub fn record_fire(&mut self, now: SystemTime) -> Result<(), DenyReason> {
        let Some(policy) = &self.policy else {
            return Err(DenyReason::MissingPolicy);
        };

        if !policy.cadence.is_valid() {
            return Err(DenyReason::InvalidInterval);
        }

        let budget = policy.budget(self.fires_used);
        if budget.is_exhausted() {
            return Err(DenyReason::ExhaustedBudget);
        }

        self.fires_used = self.fires_used.saturating_add(1);
        self.next_due = next_due_after_fire(&policy.cadence, now);
        Ok(())
    }

    pub fn pause(&mut self) {
        if self.state == LoopRuntimeState::Active {
            self.state = LoopRuntimeState::Paused;
        }
    }

    pub fn resume(&mut self) {
        if self.state == LoopRuntimeState::Paused {
            self.state = LoopRuntimeState::Active;
        }
    }

    pub fn delete(&mut self) {
        self.state = LoopRuntimeState::Deleted;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopPolicyDecision {
    FireNow {
        loop_id: LoopId,
        invocation: LoopInvocation,
        remaining_budget: u32,
    },
    WaitUntil(WaitUntil),
    Denied(DenyReason),
    Exhausted {
        reason: DenyReason,
    },
}

trait RuntimeCadence {
    fn next_due_after_fire(&self, now: SystemTime) -> NextDueState;
}

impl RuntimeCadence for GoalCadence {
    fn next_due_after_fire(&self, now: SystemTime) -> NextDueState {
        match self {
            Self::FixedInterval(interval) => NextDueState::ScheduledAt(now + *interval),
            Self::SelfPaced => NextDueState::WaitingForSelfPacedSignal,
        }
    }
}

impl RuntimeCadence for LoopCadence {
    fn next_due_after_fire(&self, now: SystemTime) -> NextDueState {
        match self {
            Self::FixedInterval(interval) => NextDueState::ScheduledAt(now + *interval),
            Self::SelfPaced => NextDueState::WaitingForSelfPacedSignal,
        }
    }
}

fn next_due_after_fire(cadence: &impl RuntimeCadence, now: SystemTime) -> NextDueState {
    cadence.next_due_after_fire(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn interval_scheduling_waits_until_due_and_advances_after_fire() {
        let now = at(100);
        let due_at = at(130);
        let interval = Duration::from_secs(30);
        let mut runtime = LoopRuntime::new(
            "loop-1",
            LoopInvocation::slash_command("/status"),
            LoopRuntimePolicy::fixed_interval(interval, 3),
        );

        runtime.schedule_at(due_at);

        assert_eq!(
            runtime.decide(now),
            LoopPolicyDecision::WaitUntil(WaitUntil::At(due_at))
        );
        assert_eq!(
            runtime.decide(due_at),
            LoopPolicyDecision::FireNow {
                loop_id: LoopId::new("loop-1"),
                invocation: LoopInvocation::slash_command("/status"),
                remaining_budget: 3,
            }
        );

        runtime.record_fire(due_at).unwrap();

        assert_eq!(runtime.fires_used, 1);
        assert_eq!(runtime.next_due, NextDueState::ScheduledAt(at(160)));
    }

    #[test]
    fn self_paced_gating_requires_due_signal() {
        let now = at(200);
        let mut runtime = GoalRuntime::new(
            "goal-1",
            "keep tests green",
            GoalRuntimePolicy::self_paced(2),
        );

        runtime.wait_for_self_paced_signal();

        assert_eq!(
            runtime.decide(now),
            GoalPolicyDecision::WaitUntil(WaitUntil::SelfPacedSignal)
        );

        runtime.mark_due();

        assert_eq!(
            runtime.decide(now),
            GoalPolicyDecision::ContinueNow {
                goal_id: GoalId::new("goal-1"),
                remaining_budget: 2,
            }
        );
    }

    #[test]
    fn budget_exhaustion_denies_goal_and_exhausts_loop() {
        let now = at(300);
        let mut goal = GoalRuntime::new(
            "goal-2",
            "finish migration",
            GoalRuntimePolicy::fixed_interval(Duration::from_secs(60), 1),
        );
        goal.continuations_used = 1;

        assert_eq!(
            goal.decide(now),
            GoalPolicyDecision::Denied(DenyReason::ExhaustedBudget)
        );

        let loop_runtime = LoopRuntime::new(
            "loop-2",
            LoopInvocation::prompt("summarize activity"),
            LoopRuntimePolicy::self_paced(0),
        );

        assert_eq!(
            loop_runtime.decide(now),
            LoopPolicyDecision::Exhausted {
                reason: DenyReason::ExhaustedBudget,
            }
        );
    }

    #[test]
    fn pause_resume_delete_state_controls_loop_decisions() {
        let now = at(400);
        let mut runtime = LoopRuntime::new(
            "loop-3",
            LoopInvocation::prompt("check blockers"),
            LoopRuntimePolicy::self_paced(5),
        );

        runtime.pause();
        assert_eq!(
            runtime.decide(now),
            LoopPolicyDecision::Denied(DenyReason::Paused)
        );

        runtime.resume();
        runtime.mark_due();
        assert_eq!(
            runtime.decide(now),
            LoopPolicyDecision::FireNow {
                loop_id: LoopId::new("loop-3"),
                invocation: LoopInvocation::prompt("check blockers"),
                remaining_budget: 5,
            }
        );

        runtime.delete();
        runtime.resume();
        assert_eq!(
            runtime.decide(now),
            LoopPolicyDecision::Denied(DenyReason::Deleted)
        );
    }

    #[test]
    fn denial_formatting_and_required_denials_are_explicit() {
        let now = at(500);

        assert_eq!(DenyReason::MissingPolicy.to_string(), "missing policy");
        assert_eq!(DenyReason::ExhaustedBudget.to_string(), "exhausted budget");
        assert_eq!(DenyReason::Paused.to_string(), "runtime paused");
        assert_eq!(DenyReason::Deleted.to_string(), "runtime deleted");
        assert_eq!(DenyReason::InvalidInterval.to_string(), "invalid interval");
        assert_eq!(DenyReason::RuntimeBusy.to_string(), "runtime busy");

        let missing_policy =
            GoalRuntime::without_policy("goal-missing-policy", "missing policy test");
        assert_eq!(
            missing_policy.decide(now),
            GoalPolicyDecision::Denied(DenyReason::MissingPolicy)
        );

        let invalid_interval = GoalRuntime::new(
            "goal-invalid-interval",
            "invalid interval test",
            GoalRuntimePolicy::fixed_interval(Duration::ZERO, 1),
        );
        assert_eq!(
            invalid_interval.decide(now),
            GoalPolicyDecision::Denied(DenyReason::InvalidInterval)
        );

        let mut busy = LoopRuntime::new(
            "loop-busy",
            LoopInvocation::prompt("busy test"),
            LoopRuntimePolicy::self_paced(1),
        );
        busy.runtime_busy = true;
        assert_eq!(
            busy.decide(now),
            LoopPolicyDecision::Denied(DenyReason::RuntimeBusy)
        );
    }

    #[test]
    fn idle_gating_blocks_goal_and_loop_until_session_is_eligible() {
        let now = at(600);
        let goal = GoalRuntime::new(
            "goal-idle",
            "continue only when idle",
            GoalRuntimePolicy::self_paced(1),
        );
        assert_eq!(
            goal.decide_when_idle(
                now,
                RuntimeIdleState::idle().with_request_user_input_pending(true),
            ),
            GoalPolicyDecision::WaitUntil(WaitUntil::RuntimeIdle(
                RuntimeIdleBlocker::RequestUserInputPending
            ))
        );
        assert_eq!(
            goal.decide_when_idle(now, RuntimeIdleState::idle()),
            GoalPolicyDecision::ContinueNow {
                goal_id: GoalId::new("goal-idle"),
                remaining_budget: 1,
            }
        );

        let loop_runtime = LoopRuntime::new(
            "loop-idle",
            LoopInvocation::prompt("check idle"),
            LoopRuntimePolicy::self_paced(1),
        );
        assert_eq!(
            loop_runtime.decide_when_idle(now, RuntimeIdleState::busy()),
            LoopPolicyDecision::WaitUntil(WaitUntil::RuntimeIdle(RuntimeIdleBlocker::ActiveTurn))
        );
    }

    #[test]
    fn runtime_priority_puts_loop_fire_ahead_of_goal_continuation() {
        let goal = GoalRuntime::new(
            "goal-priority",
            "priority test",
            GoalRuntimePolicy::self_paced(1),
        );
        let loop_runtime = LoopRuntime::new(
            "loop-priority",
            LoopInvocation::prompt("priority test"),
            LoopRuntimePolicy::self_paced(1),
        );

        assert!(loop_runtime.fire_priority() > goal.continuation_priority());
        assert!(
            RuntimeSchedulePriority::LoopFire.rank()
                > RuntimeSchedulePriority::GoalContinuation.rank()
        );
    }

    #[test]
    fn exhausted_goal_budget_returns_wrap_up_contract() {
        let mut goal = GoalRuntime::new(
            "goal-budget",
            "budget test",
            GoalRuntimePolicy::fixed_interval(Duration::from_secs(30), 1),
        );
        goal.continuations_used = 1;

        let resolution = goal
            .budget_resolution()
            .expect("exhausted goal should have resolution");

        assert_eq!(resolution.goal_id, GoalId::new("goal-budget"));
        assert_eq!(resolution.reason, DenyReason::ExhaustedBudget);
        assert!(resolution.wrap_up_prompt.contains("exhausted"));
        assert!(resolution.wrap_up_prompt.contains("stop starting new work"));
    }

    #[test]
    fn loop_fire_triggers_share_policy_and_fire_now_does_not_bypass_idle_or_pause() {
        let now = at(700);
        let due_at = at(900);
        let mut runtime = LoopRuntime::new(
            "loop-fire",
            LoopInvocation::prompt("run loop"),
            LoopRuntimePolicy::fixed_interval(Duration::from_secs(60), 2),
        );
        runtime.schedule_at(due_at);

        assert_eq!(
            runtime.decide_fire(now, LoopFireTrigger::ScheduledDue, LoopFireContext::idle(),),
            LoopFireDecision::WaitUntil(WaitUntil::At(due_at))
        );

        assert_eq!(
            runtime.decide_fire(
                now,
                LoopFireTrigger::FireNow,
                LoopFireContext::idle().with_runtime_state(RuntimeIdleState::busy()),
            ),
            LoopFireDecision::Denied(DenyReason::RuntimeBusy)
        );

        match runtime.decide_fire(now, LoopFireTrigger::FireNow, LoopFireContext::idle()) {
            LoopFireDecision::Fire(plan) => {
                assert_eq!(plan.loop_id, LoopId::new("loop-fire"));
                assert_eq!(plan.trigger, LoopFireTrigger::FireNow);
                assert_eq!(plan.priority, RuntimeSchedulePriority::LoopFire);
                assert_eq!(plan.remaining_budget, 2);
            }
            other => panic!("expected fire-now plan, got {other:?}"),
        }

        runtime.pause();
        assert_eq!(
            runtime.decide_fire(now, LoopFireTrigger::FireNow, LoopFireContext::idle()),
            LoopFireDecision::Denied(DenyReason::Paused)
        );
    }

    #[test]
    fn loop_creation_immediate_fire_uses_same_budget_and_policy_path() {
        let now = at(800);
        let runtime = LoopRuntime::new(
            "loop-create",
            LoopInvocation::maintenance_prompt(),
            LoopRuntimePolicy::maintenance(1),
        );

        match runtime.decide_fire(
            now,
            LoopFireTrigger::CreationImmediate,
            LoopFireContext::idle(),
        ) {
            LoopFireDecision::Fire(plan) => {
                assert_eq!(plan.loop_id, LoopId::new("loop-create"));
                assert_eq!(plan.invocation, LoopInvocation::MaintenancePrompt);
                assert_eq!(plan.trigger, LoopFireTrigger::CreationImmediate);
                assert_eq!(plan.remaining_budget, 1);
            }
            other => panic!("expected create-immediate fire plan, got {other:?}"),
        }

        let exhausted = LoopRuntime::new(
            "loop-create-exhausted",
            LoopInvocation::prompt("never fire"),
            LoopRuntimePolicy::self_paced(0),
        );
        assert_eq!(
            exhausted.decide_fire(
                now,
                LoopFireTrigger::CreationImmediate,
                LoopFireContext::idle(),
            ),
            LoopFireDecision::Exhausted {
                reason: DenyReason::ExhaustedBudget,
            }
        );
    }

    #[test]
    fn maintenance_prompt_resolution_uses_project_user_builtin_order_at_fire_time() {
        assert_eq!(
            resolve_maintenance_prompt(
                Some(" project maintenance "),
                Some("user maintenance"),
                BUILT_IN_MAINTENANCE_PROMPT,
            )
            .unwrap(),
            MaintenancePromptResolution {
                source: MaintenancePromptSource::Project,
                prompt: "project maintenance".to_owned(),
            }
        );

        assert_eq!(
            resolve_maintenance_prompt(
                Some(" "),
                Some("user maintenance"),
                BUILT_IN_MAINTENANCE_PROMPT,
            )
            .unwrap(),
            MaintenancePromptResolution {
                source: MaintenancePromptSource::User,
                prompt: "user maintenance".to_owned(),
            }
        );

        assert_eq!(
            resolve_maintenance_prompt(Some(""), Some(""), BUILT_IN_MAINTENANCE_PROMPT).unwrap(),
            MaintenancePromptResolution {
                source: MaintenancePromptSource::BuiltIn,
                prompt: BUILT_IN_MAINTENANCE_PROMPT.to_owned(),
            }
        );

        assert_eq!(
            resolve_maintenance_prompt(Some("old project"), None, BUILT_IN_MAINTENANCE_PROMPT)
                .unwrap()
                .prompt,
            "old project"
        );
        assert_eq!(
            resolve_maintenance_prompt(Some("new project"), None, BUILT_IN_MAINTENANCE_PROMPT)
                .unwrap()
                .prompt,
            "new project"
        );
    }

    #[test]
    fn slash_commands_are_reauthorized_on_every_loop_fire() {
        let now = at(900);
        let runtime = LoopRuntime::new(
            "loop-slash",
            LoopInvocation::slash_command("/status"),
            LoopRuntimePolicy::self_paced(2),
        );

        assert_eq!(
            runtime.decide_fire(
                now,
                LoopFireTrigger::FireNow,
                LoopFireContext::idle().with_slash_authorization(
                    SlashCommandAuthorization::authorized_at_creation_only()
                ),
            ),
            LoopFireDecision::Denied(DenyReason::SlashCommandDenied)
        );

        match runtime.decide_fire(
            now,
            LoopFireTrigger::FireNow,
            LoopFireContext::idle()
                .with_slash_authorization(SlashCommandAuthorization::authorized_now()),
        ) {
            LoopFireDecision::Fire(plan) => {
                assert!(plan.slash_authorization.required);
                assert!(plan.slash_authorization.authorized_now);
                assert!(plan.slash_authorization.reauthorized_each_fire);
            }
            other => panic!("expected slash fire plan, got {other:?}"),
        }

        let denied_by_policy = LoopRuntime::new(
            "loop-slash-denied",
            LoopInvocation::slash_command("/status"),
            LoopRuntimePolicy::self_paced(2)
                .with_slash_command_policy(LoopSlashCommandPolicy::deny()),
        );
        assert_eq!(
            denied_by_policy.decide_fire(
                now,
                LoopFireTrigger::FireNow,
                LoopFireContext::idle()
                    .with_slash_authorization(SlashCommandAuthorization::authorized_now()),
            ),
            LoopFireDecision::Denied(DenyReason::SlashCommandDenied)
        );
    }
}
