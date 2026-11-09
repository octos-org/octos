//! M15 production primitive for scheduling automatic master-agent continuation
//! turns. The real turn-loop wiring lands in the next integration step; this
//! module is compiled and covered by unit tests now so the contract is explicit.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::time::{Duration, SystemTime};

/// codex DO-NOT-SHIP TOCTOU window: spawn_only-failure recovery fires the
/// `on_failure` enqueue and the unified `on_terminal` enqueue SEQUENTIALLY
/// inside one `mark_failed` (microseconds apart). Both carry the identical
/// `external/spawn_only_failure/<session>/<task>` dedupe key. If the
/// continuation tick drains the first enqueue before the second runs, the key
/// has already left `pending_by_key` so the existing dedupe misses and one
/// terminal transition would produce TWO recovery turns.
///
/// #2020 widened the blast radius rather than closing it: retiring the
/// gateway's `ActorMessage::RecoveryHint` inbox put the GATEWAY on this same
/// two-producer queue, so this guard is now load-bearing for both runtime
/// modes, not just the WS path it was written for. `on_failure` cannot simply
/// be deleted in favour of `on_terminal` alone — it is the ONLY delivery for a
/// fail-before-ack failure, which `on_terminal` prompt-suppresses.
///
/// The recently-claimed guard records the moment an `External` continuation is
/// claimed (drained/popped) and rejects a re-enqueue of the SAME key inside
/// this window as a duplicate. The window is generous relative to the
/// same-transition gap (sub-millisecond) yet bounded so a genuinely later
/// external occurrence that reuses a key is NOT stranded.
///
/// Scoped to `External` reasons ONLY: those keys are externally-identity-keyed
/// (spawn_only failure embeds the task's UUIDv7, never reused; manual-wakeup is
/// a one-shot), so a same-key re-enqueue inside the window is always the same
/// logical event. The recurring reasons (`LoopFire`/`GoalContinue`/
/// `ChildCompleted`) deliberately re-enqueue the same key tick-after-tick and
/// must stay reusable, so they are never guarded.
pub(crate) const RECENT_CLAIM_GUARD_WINDOW: Duration = Duration::from_secs(30);

/// Refs #2102 (Gap 2): shorter reclaim window for `ChildCompleted` /
/// `ScatterJoinComplete` keys. These reasons have the same
/// drain-between-two-enqueues TOCTOU window the External guard defends
/// against (legacy + unified terminal callbacks firing sequentially inside
/// one terminal transition), but their keys must stay reusable across drain
/// ticks — 2s collapses only a same-transition double-fire and is far below
/// the ~30s AppUI drain tick, so per-tick reuse is unaffected. A straggler
/// continuation carrying corrected metadata produces a different
/// `stable_dedupe_key` (metadata folds into the key) and is NOT suppressed.
const CHILD_SCATTER_RECLAIM_WINDOW: Duration = Duration::from_secs(2);

/// #436 follow-up — max times a popped-but-UNDISPATCHED continuation may be
/// re-inserted for a live re-delivery attempt before it is dropped from the
/// in-memory queue. Without this bound a permanently-undeliverable injection
/// (its target wire gone for good) would be re-queued every drain tick forever
/// and — because [`reinsert`](MasterContinuationScheduler::reinsert) advances
/// the sequence so the item yields to newer work — would churn indefinitely.
/// At the ~2s drain cadence, 5 attempts is ~10s of live retry: long enough to
/// ride out a brief peer reopen/reconnect, short enough not to loop. A drop is
/// in-memory only; the durable record (if any) still replays on the next server
/// restart, a natural point to re-evaluate whether the target came back.
pub(crate) const MAX_REDELIVERY_ATTEMPTS: u32 = 5;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }
    };
}

string_id!(MasterContinuationGroupId);
string_id!(MasterContinuationSessionId);
string_id!(MasterContinuationProfileId);
string_id!(ChildAgentId);
string_id!(GoalId);
string_id!(LoopId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct MasterContinuationId(u64);

impl MasterContinuationId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MasterContinuationDedupeKey(String);

impl MasterContinuationDedupeKey {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for MasterContinuationDedupeKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for MasterContinuationDedupeKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MasterContinuationReason {
    ChildCompleted,
    ScatterJoinComplete,
    LoopFire,
    GoalContinue,
    /// #1131 — terminal "summarize and stop" turn enqueued when a
    /// goal exhausts its token budget. Carries the wrap-up directive
    /// in `wrap_up_prompt` metadata; the prompt renderer must use
    /// that text verbatim instead of the standard "Advance the
    /// goal..." template.
    GoalWrapUp,
    External(String),
}

impl MasterContinuationReason {
    pub(crate) fn priority(&self) -> MasterContinuationPriority {
        match self {
            Self::LoopFire => MasterContinuationPriority::LoopFire,
            Self::ChildCompleted | Self::ScatterJoinComplete => {
                MasterContinuationPriority::ChildOrScatterJoinComplete
            }
            // #1131 — wrap-up rides the same priority lane as a
            // regular goal continuation. It is the LAST goal turn
            // before the session pauses, not a privileged one.
            Self::GoalContinue | Self::GoalWrapUp => MasterContinuationPriority::GoalContinue,
            Self::External(_) => MasterContinuationPriority::External,
        }
    }

    fn stable_name(&self) -> &str {
        match self {
            Self::ChildCompleted => "child_completed",
            Self::ScatterJoinComplete => "scatter_join_complete",
            Self::LoopFire => "loop_fire",
            Self::GoalContinue => "goal_continue",
            Self::GoalWrapUp => "goal_wrap_up",
            Self::External(_) => "external",
        }
    }

    fn external_kind(&self) -> Option<&str> {
        match self {
            Self::External(kind) => Some(kind.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MasterContinuationPriority {
    /// Generic external wakeups are intentionally lowest unless future wiring
    /// maps them to a typed internal reason.
    External,
    GoalContinue,
    ChildOrScatterJoinComplete,
    LoopFire,
}

impl MasterContinuationPriority {
    pub(crate) fn rank(self) -> u8 {
        match self {
            Self::External => 0,
            Self::GoalContinue => 10,
            Self::ChildOrScatterJoinComplete => 20,
            Self::LoopFire => 30,
        }
    }
}

impl Ord for MasterContinuationPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for MasterContinuationPriority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) type MasterContinuationMetadata = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MasterContinuationRequest {
    pub(crate) group_id: MasterContinuationGroupId,
    pub(crate) session_id: MasterContinuationSessionId,
    pub(crate) profile_id: MasterContinuationProfileId,
    pub(crate) reason: MasterContinuationReason,
    pub(crate) child_agent_id: Option<ChildAgentId>,
    pub(crate) goal_id: Option<GoalId>,
    pub(crate) loop_id: Option<LoopId>,
    pub(crate) metadata: MasterContinuationMetadata,
    pub(crate) created_at: SystemTime,
    pub(crate) dedupe_key: Option<MasterContinuationDedupeKey>,
}

impl MasterContinuationRequest {
    pub(crate) fn new(
        group_id: impl Into<MasterContinuationGroupId>,
        session_id: impl Into<MasterContinuationSessionId>,
        profile_id: impl Into<MasterContinuationProfileId>,
        reason: MasterContinuationReason,
        created_at: SystemTime,
    ) -> Self {
        Self {
            group_id: group_id.into(),
            session_id: session_id.into(),
            profile_id: profile_id.into(),
            reason,
            child_agent_id: None,
            goal_id: None,
            loop_id: None,
            metadata: BTreeMap::new(),
            created_at,
            dedupe_key: None,
        }
    }

    pub(crate) fn with_child_agent_id(mut self, child_agent_id: impl Into<ChildAgentId>) -> Self {
        self.child_agent_id = Some(child_agent_id.into());
        self
    }

    pub(crate) fn with_goal_id(mut self, goal_id: impl Into<GoalId>) -> Self {
        self.goal_id = Some(goal_id.into());
        self
    }

    pub(crate) fn with_loop_id(mut self, loop_id: impl Into<LoopId>) -> Self {
        self.loop_id = Some(loop_id.into());
        self
    }

    pub(crate) fn with_metadata(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub(crate) fn with_dedupe_key(
        mut self,
        dedupe_key: impl Into<MasterContinuationDedupeKey>,
    ) -> Self {
        self.dedupe_key = Some(dedupe_key.into());
        self
    }

    pub(crate) fn stable_dedupe_key(&self) -> MasterContinuationDedupeKey {
        if let Some(key) = &self.dedupe_key {
            return key.clone();
        }

        let mut key = String::new();
        push_key_part(&mut key, "group", self.group_id.as_str());
        push_key_part(&mut key, "session", self.session_id.as_str());
        push_key_part(&mut key, "profile", self.profile_id.as_str());
        push_key_part(&mut key, "reason", self.reason.stable_name());
        if let Some(kind) = self.reason.external_kind() {
            push_key_part(&mut key, "external", kind);
        }
        push_optional_key_part(
            &mut key,
            "child",
            self.child_agent_id.as_ref().map(ChildAgentId::as_str),
        );
        push_optional_key_part(&mut key, "goal", self.goal_id.as_ref().map(GoalId::as_str));
        push_optional_key_part(&mut key, "loop", self.loop_id.as_ref().map(LoopId::as_str));
        for (metadata_key, metadata_value) in &self.metadata {
            push_key_part(&mut key, "metadata_key", metadata_key);
            push_key_part(&mut key, "metadata_value", metadata_value);
        }
        MasterContinuationDedupeKey::new(key)
    }
}

fn push_optional_key_part(output: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value {
        push_key_part(output, label, value);
    }
}

fn push_key_part(output: &mut String, label: &str, value: &str) {
    output.push_str(&label.len().to_string());
    output.push(':');
    output.push_str(label);
    output.push('=');
    output.push_str(&value.len().to_string());
    output.push(':');
    output.push_str(value);
    output.push(';');
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedMasterContinuation {
    pub(crate) id: MasterContinuationId,
    pub(crate) dedupe_key: MasterContinuationDedupeKey,
    pub(crate) priority: MasterContinuationPriority,
    pub(crate) sequence: u64,
    pub(crate) group_id: MasterContinuationGroupId,
    pub(crate) session_id: MasterContinuationSessionId,
    pub(crate) profile_id: MasterContinuationProfileId,
    pub(crate) reason: MasterContinuationReason,
    pub(crate) child_agent_id: Option<ChildAgentId>,
    pub(crate) goal_id: Option<GoalId>,
    pub(crate) loop_id: Option<LoopId>,
    pub(crate) metadata: MasterContinuationMetadata,
    pub(crate) created_at: SystemTime,
    pub(crate) enqueued_at: SystemTime,
    /// #436 follow-up — how many times this item has been re-inserted after a
    /// popped-but-undispatched delivery attempt. Starts at 0 on enqueue, is
    /// incremented by [`reinsert`](MasterContinuationScheduler::reinsert), and
    /// bounds live re-delivery at [`MAX_REDELIVERY_ATTEMPTS`]. It travels with
    /// the item (pop→reinsert→pop preserves it) and dies when the item is
    /// consumed or re-homed to a fresh continuation.
    pub(crate) redelivery_attempts: u32,
    /// #26 (round-4, #18 B4) — the DURABLE revision (`attempt`) this queued
    /// item's payload was persisted at, so the delivery path can stamp
    /// Started/Completed with the revision it actually resolved. 0 = not
    /// (yet) durable / legacy shape (the lifecycle event then applies
    /// unconditionally, matching pre-#26 behavior). Set by the persist
    /// helpers and by restart restore (the record's own `attempt`); NOT
    /// part of dedupe identity.
    pub(crate) persisted_attempt: u32,
}

/// Canonical workspace identity, with legacy `workspace` fallback for old
/// callers. Producer/restore boundaries normalize known peer wire spellings.
/// Empty strings remain absent so an unstamped item keeps the None scope.
pub(crate) fn item_workspace(item: &QueuedMasterContinuation) -> Option<&str> {
    item.metadata
        .get("workspace_scope")
        .or_else(|| item.metadata.get("workspace"))
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

impl QueuedMasterContinuation {}

// `Queued` carries the full continuation record by value so enqueue returns
// the stored item without a clone; the enum is matched once per enqueue
// (cold path), so the size difference is immaterial.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MasterContinuationEnqueueOutcome {
    Queued(QueuedMasterContinuation),
    Duplicate {
        dedupe_key: MasterContinuationDedupeKey,
        existing_id: MasterContinuationId,
    },
}

impl MasterContinuationEnqueueOutcome {
    pub(crate) fn queued(&self) -> Option<&QueuedMasterContinuation> {
        match self {
            Self::Queued(item) => Some(item),
            Self::Duplicate { .. } => None,
        }
    }

    pub(crate) fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate { .. })
    }
}

/// Result of [`reinsert`](MasterContinuationScheduler::reinsert).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReinsertOutcome {
    /// Re-queued for another live delivery attempt; its sequence was advanced
    /// so it yields to newer injections instead of starving them.
    Requeued,
    /// The key was already pending (a concurrent path re-queued it first); the
    /// existing entry was kept and this stale copy discarded.
    AlreadyPending,
    /// Exceeded [`MAX_REDELIVERY_ATTEMPTS`] and was dropped from the in-memory
    /// queue so it can no longer churn or starve newer work. Best-effort by
    /// design; the durable record (if any) still replays on the next restart.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeActivity {
    Idle,
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MasterContinuationRuntimeState {
    pub(crate) activity: RuntimeActivity,
    pub(crate) user_input_pending: bool,
    pub(crate) approval_pending: bool,
}

impl MasterContinuationRuntimeState {
    pub(crate) fn idle() -> Self {
        Self {
            activity: RuntimeActivity::Idle,
            user_input_pending: false,
            approval_pending: false,
        }
    }

    pub(crate) fn busy() -> Self {
        Self {
            activity: RuntimeActivity::Busy,
            user_input_pending: false,
            approval_pending: false,
        }
    }

    pub(crate) fn with_user_input_pending(mut self, pending: bool) -> Self {
        self.user_input_pending = pending;
        self
    }

    pub(crate) fn with_approval_pending(mut self, pending: bool) -> Self {
        self.approval_pending = pending;
        self
    }

    pub(crate) fn is_idle_eligible(self) -> bool {
        self.activity == RuntimeActivity::Idle && !self.user_input_pending && !self.approval_pending
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeapEntry {
    priority: MasterContinuationPriority,
    sequence: u64,
    dedupe_key: MasterContinuationDedupeKey,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
            .then_with(|| self.dedupe_key.cmp(&other.dedupe_key))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug)]
pub(crate) struct MasterContinuationScheduler {
    heap: BinaryHeap<HeapEntry>,
    pending_by_key: HashMap<MasterContinuationDedupeKey, QueuedMasterContinuation>,
    next_id: u64,
    next_sequence: u64,
    /// codex DO-NOT-SHIP TOCTOU guard: timestamp (the claimed item's
    /// `enqueued_at`) at which an `External` continuation was last
    /// claimed (drained/popped) out of `pending_by_key`. A re-enqueue of
    /// the same key inside [`RECENT_CLAIM_GUARD_WINDOW`] is rejected as a
    /// duplicate so a drain that races between the legacy `on_failure` and
    /// unified `on_terminal` enqueues of one terminal transition cannot
    /// double-deliver. Only `External` keys are recorded (see the constant
    /// doc) so recurring loop/goal/child continuations stay reusable.
    ///
    /// INVARIANT for future `External` producers: this guard assumes an
    /// `External` dedupe key identifies ONE occurrence (today: spawn_only
    /// failure keyed by task UUIDv7, one-shot — no other production producer).
    /// Any new `External` producer that can legitimately re-enqueue the same
    /// key within the window MUST embed a unique occurrence id in the key, or
    /// the second enqueue will be wrongly dropped.
    recently_claimed_external: HashMap<MasterContinuationDedupeKey, SystemTime>,
}

impl Default for MasterContinuationScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl MasterContinuationScheduler {
    pub(crate) fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            pending_by_key: HashMap::new(),
            next_id: 1,
            next_sequence: 0,
            recently_claimed_external: HashMap::new(),
        }
    }

    pub(crate) fn enqueue(
        &mut self,
        request: MasterContinuationRequest,
    ) -> MasterContinuationEnqueueOutcome {
        self.enqueue_at(request, SystemTime::now())
    }

    pub(crate) fn enqueue_at(
        &mut self,
        request: MasterContinuationRequest,
        enqueued_at: SystemTime,
    ) -> MasterContinuationEnqueueOutcome {
        let dedupe_key = request.stable_dedupe_key();
        if let Some(existing) = self.pending_by_key.get(&dedupe_key) {
            return MasterContinuationEnqueueOutcome::Duplicate {
                dedupe_key,
                existing_id: existing.id,
            };
        }

        // codex DO-NOT-SHIP TOCTOU guard: reject a same-transition re-enqueue
        // of an External key that was claimed (drained/popped) moments ago.
        // The legacy `on_failure` and unified `on_terminal` enqueues fire
        // sequentially inside one `mark_failed`; a drain between them removes
        // the key from `pending_by_key` so the check above misses. The
        // recently-claimed window catches it. Pruning runs lazily here so the
        // map stays bounded by the live churn rate. Scoped to External so
        // recurring loop/goal/child keys stay reusable across ticks.
        self.prune_recently_claimed(enqueued_at);
        let reclaim_window = match request.reason {
            MasterContinuationReason::External(_) => Some(RECENT_CLAIM_GUARD_WINDOW),
            MasterContinuationReason::ChildCompleted
            | MasterContinuationReason::ScatterJoinComplete => Some(CHILD_SCATTER_RECLAIM_WINDOW),
            // LoopFire / GoalContinue: recurrence across ticks is the
            // feature — no reclaim guard.
            _ => None,
        };
        if let Some(window) = reclaim_window
            && let Some(claimed_at) = self.recently_claimed_external.get(&dedupe_key)
            && within_claim_window(*claimed_at, enqueued_at, window)
        {
            return MasterContinuationEnqueueOutcome::Duplicate {
                dedupe_key,
                // No live pending item to point at; the guard collapses the
                // re-enqueue onto the already-claimed continuation.
                existing_id: MasterContinuationId::new(0),
            };
        }

        let id = MasterContinuationId::new(self.next_id);
        self.next_id += 1;
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let priority = request.reason.priority();
        let item = QueuedMasterContinuation {
            id,
            dedupe_key: dedupe_key.clone(),
            priority,
            sequence,
            group_id: request.group_id,
            session_id: request.session_id,
            profile_id: request.profile_id,
            reason: request.reason,
            child_agent_id: request.child_agent_id,
            goal_id: request.goal_id,
            loop_id: request.loop_id,
            metadata: request.metadata,
            created_at: request.created_at,
            enqueued_at,
            redelivery_attempts: 0,
            persisted_attempt: 0,
        };

        self.heap.push(HeapEntry {
            priority,
            sequence,
            dedupe_key: dedupe_key.clone(),
        });
        self.pending_by_key.insert(dedupe_key, item.clone());
        MasterContinuationEnqueueOutcome::Queued(item)
    }

    pub(crate) fn cancel(
        &mut self,
        dedupe_key: &MasterContinuationDedupeKey,
    ) -> Option<QueuedMasterContinuation> {
        self.pending_by_key.remove(dedupe_key)
    }

    /// #26 (round-4, #18 B4) — stamp the durable revision onto the still-
    /// pending item so any later drain of it carries the attempt its
    /// payload was persisted at (the delivery path's Started/Completed
    /// resolve that revision). A no-op when the key is no longer pending
    /// (already drained/cancelled — nothing left to stamp). Never lowers an
    /// existing stamp: a retry's re-persist of the same payload at a higher
    /// allocator attempt only lifts it.
    pub(crate) fn stamp_persisted_attempt(
        &mut self,
        dedupe_key: &MasterContinuationDedupeKey,
        attempt: u32,
    ) {
        if let Some(item) = self.pending_by_key.get_mut(dedupe_key) {
            item.persisted_attempt = item.persisted_attempt.max(attempt);
        }
    }

    /// #436 P1 #2/#4 — RE-INSERT a continuation that was popped (claimed) but
    /// NOT consumed: an injection whose turn failed to dispatch, or whose
    /// target wire went obsolete before dispatch. Restores it to the pending
    /// set + heap and CLEARS its recently-claimed guard entry so it is
    /// immediately drainable again. That guard exists to collapse a re-enqueue
    /// racing a claim of a WILL-BE-delivered item; a not-delivered restore is
    /// the opposite and must be redrained, so bypassing the guard here is
    /// correct. Idempotent: a key already pending is left untouched.
    ///
    /// #436 follow-up (round-3 starvation regression) — a naive restore that
    /// preserved the item's original (low) sequence would keep WINNING the
    /// oldest-first heap every drain tick, so a permanently-undeliverable
    /// injection would starve every newer injection to its session forever.
    /// Two coupled bounds fix that: (1) ADVANCE the sequence so a repeatedly
    /// re-inserted item moves to the BACK of the FIFO and yields to newer work,
    /// and (2) CAP re-delivery at [`MAX_REDELIVERY_ATTEMPTS`], dropping the item
    /// past the cap so it cannot churn indefinitely. Returns the outcome so the
    /// caller can log a drop rather than silently losing a peer message.
    pub(crate) fn reinsert(&mut self, mut item: QueuedMasterContinuation) -> ReinsertOutcome {
        self.recently_claimed_external.remove(&item.dedupe_key);
        if self.pending_by_key.contains_key(&item.dedupe_key) {
            return ReinsertOutcome::AlreadyPending;
        }
        item.redelivery_attempts = item.redelivery_attempts.saturating_add(1);
        if item.redelivery_attempts > MAX_REDELIVERY_ATTEMPTS {
            return ReinsertOutcome::Dropped;
        }
        // Advance to a fresh sequence so a repeatedly-failing item sinks to the
        // back of the oldest-first heap. The heap entry and the stored item MUST
        // carry the SAME sequence — `entry_matches_pending` compares them and a
        // mismatch would discard the entry as stale.
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        item.sequence = sequence;
        self.heap.push(HeapEntry {
            priority: item.priority,
            sequence,
            dedupe_key: item.dedupe_key.clone(),
        });
        self.pending_by_key.insert(item.dedupe_key.clone(), item);
        ReinsertOutcome::Requeued
    }

    /// #1707 (codex Blocker 1) — put back an item REMOVED by
    /// [`Self::take_pending_terminal_for_scope`] when its fold aborts (the
    /// carrier's durable persist failed, so nothing was tombstoned and the
    /// batch must deliver as-is). Unlike [`Self::reinsert`], this is a
    /// crash-safe RESTORE, not a delivery attempt: it clears the claim guard
    /// and restores the item to pending + heap with a FRESH sequence (back of
    /// the FIFO) WITHOUT counting a redelivery attempt — the fold abort is a
    /// durability failure, not a failed delivery. Idempotent: a key already
    /// pending is left untouched.
    pub(crate) fn requeue_taken(&mut self, mut item: QueuedMasterContinuation) {
        self.recently_claimed_external.remove(&item.dedupe_key);
        if self.pending_by_key.contains_key(&item.dedupe_key) {
            return;
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        item.sequence = sequence;
        self.heap.push(HeapEntry {
            priority: item.priority,
            sequence,
            dedupe_key: item.dedupe_key.clone(),
        });
        self.pending_by_key.insert(item.dedupe_key.clone(), item);
    }

    /// #1707 round 3 (codex Blocker 3) — STATUS-CORRECTION enqueue: a
    /// terminal re-forward whose STATUS differs from the pending/drained one
    /// (a `failed` → `completed` correction on the same identity dedupe key)
    /// must REPLACE any still-pending older payload instead of collapsing as
    /// a `Duplicate`, and must bypass the reclaim window when the older item
    /// was claimed moments ago (`CHILD_SCATTER_RECLAIM_WINDOW` —
    /// the correction is a DIFFERENT occurrence of the same key, not the
    /// double-enqueue the guard exists to collapse).
    ///
    /// Mechanics mirror [`Self::requeue_taken`] but from a request: remove
    /// any pending item with the same dedupe key (its heap entry goes stale
    /// and is skipped by `entry_matches_pending`), clear the key's
    /// recently-claimed guard entry, then enqueue the fresh request with a
    /// fresh sequence and NO redelivery counting. Returns the outcome like
    /// [`Self::enqueue_at`].
    pub(crate) fn replace_pending_payload(
        &mut self,
        request: MasterContinuationRequest,
    ) -> MasterContinuationEnqueueOutcome {
        let dedupe_key = request.stable_dedupe_key();
        self.pending_by_key.remove(&dedupe_key);
        self.recently_claimed_external.remove(&dedupe_key);
        self.enqueue_at(request, SystemTime::now())
    }

    pub(crate) fn drain_ready_for_session_if(
        &mut self,
        runtime_state: MasterContinuationRuntimeState,
        max_items: usize,
        session_id: &str,
        profile_id: &str,
        mut eligible: impl FnMut(&QueuedMasterContinuation) -> bool,
    ) -> Vec<QueuedMasterContinuation> {
        if max_items == 0 || !runtime_state.is_idle_eligible() {
            return Vec::new();
        }

        let mut drained = Vec::new();
        let mut held = Vec::new();
        while drained.len() < max_items {
            let Some(entry) = self.heap.pop() else {
                break;
            };
            if !self.entry_matches_pending(&entry) {
                continue;
            }
            let matches_session = self
                .pending_by_key
                .get(&entry.dedupe_key)
                .is_some_and(|item| {
                    item.session_id.as_str() == session_id
                        && item.profile_id.as_str() == profile_id
                        && eligible(item)
                });
            if matches_session {
                if let Some(item) = self.pending_by_key.remove(&entry.dedupe_key) {
                    self.record_external_claim(&item);
                    drained.push(item);
                }
            } else {
                held.push(entry);
            }
        }
        for entry in held {
            self.heap.push(entry);
        }
        drained
    }

    /// Number of pending `ChildCompleted` items for `session_id` +
    /// `profile_id` scoped to one continuation group and workspace (#1707).
    /// `workspace` is matched against the item's `workspace` metadata with an
    /// empty string treated as absent (`None == None` matches).
    pub(crate) fn pending_child_completed_count_for_scope(
        &self,
        session_id: &str,
        profile_id: &str,
        group_id: &str,
        workspace: Option<&str>,
    ) -> usize {
        self.pending_by_key
            .values()
            .filter(|item| {
                item.session_id.as_str() == session_id
                    && item.profile_id.as_str() == profile_id
                    && item.reason == MasterContinuationReason::ChildCompleted
                    && item.group_id.as_str() == group_id
                    && item_workspace(item) == workspace
            })
            .count()
    }

    /// Remove every pending `ChildCompleted` / `ScatterJoinComplete` item for
    /// `session_id` + `profile_id` + `group_id` + `workspace` (#1707:
    /// terminal coalescing is scope-batched so a burst from another group or
    /// another workspace is never tombstoned into a carrier whose prompt only
    /// describes the carrier's own group). Heap entries for the removed keys
    /// go stale and are skipped by `entry_matches_pending`; claims are
    /// recorded so a same-key re-enqueue inside the reclaim window collapses.
    /// Returned oldest-first.
    pub(crate) fn take_pending_terminal_for_scope(
        &mut self,
        session_id: &str,
        profile_id: &str,
        group_id: &str,
        workspace: Option<&str>,
        mut eligible: impl FnMut(&QueuedMasterContinuation) -> bool,
    ) -> Vec<QueuedMasterContinuation> {
        let keys = self
            .pending_by_key
            .values()
            .filter(|item| {
                item.session_id.as_str() == session_id
                    && item.profile_id.as_str() == profile_id
                    && matches!(
                        item.reason,
                        MasterContinuationReason::ChildCompleted
                            | MasterContinuationReason::ScatterJoinComplete
                    )
                    && item.group_id.as_str() == group_id
                    && item_workspace(item) == workspace
                    && eligible(item)
            })
            .map(|item| item.dedupe_key.clone())
            .collect::<Vec<_>>();
        let mut taken = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(item) = self.pending_by_key.remove(&key) {
                self.record_external_claim(&item);
                taken.push(item);
            }
        }
        taken.sort_by_key(|item| item.sequence);
        taken
    }

    #[cfg(test)]
    pub(crate) fn pending_count_for_session(&self, session_id: &str, profile_id: &str) -> usize {
        self.pending_by_key
            .values()
            .filter(|item| {
                item.session_id.as_str() == session_id && item.profile_id.as_str() == profile_id
            })
            .count()
    }

    /// #1145 codex P1 follow-up — iterate over every pending
    /// continuation so callers can filter by reason/goal_id/loop_id
    /// before deciding to schedule a tick. The orchestrator uses this
    /// to skip stale `GoalContinue`/`LoopFire` items whose owning
    /// goal/loop has since been paused, cleared, or deleted.
    pub(crate) fn pending_items(&self) -> impl Iterator<Item = &QueuedMasterContinuation> + '_ {
        self.pending_by_key.values()
    }

    pub(crate) fn pending_item(
        &self,
        key: &MasterContinuationDedupeKey,
    ) -> Option<&QueuedMasterContinuation> {
        self.pending_by_key.get(key)
    }

    fn entry_matches_pending(&self, entry: &HeapEntry) -> bool {
        self.pending_by_key
            .get(&entry.dedupe_key)
            .is_some_and(|item| item.sequence == entry.sequence && item.priority == entry.priority)
    }

    /// codex DO-NOT-SHIP TOCTOU guard: record that `item` was just claimed
    /// (drained/popped) so a same-transition re-enqueue of its key is
    /// rejected inside [`RECENT_CLAIM_GUARD_WINDOW`]. Only `External` claims
    /// are recorded — recurring loop/goal/child keys re-enqueue legitimately
    /// every tick and must stay reusable. The claim time is the item's
    /// `enqueued_at` so the comparison is deterministic in tests (which thread
    /// explicit timestamps) and matches wall-clock `now()` in production
    /// (where both enqueue and drain use `SystemTime::now()`).
    fn record_external_claim(&mut self, item: &QueuedMasterContinuation) {
        // Refs #2102 (Gap 2): Child/Scatter claims are recorded too, gated at
        // enqueue time by the shorter CHILD_SCATTER_RECLAIM_WINDOW (see
        // enqueue_at). LoopFire / GoalContinue are never recorded — their
        // recurrence across ticks is legitimate.
        let guardable = matches!(
            item.reason,
            MasterContinuationReason::External(_)
                | MasterContinuationReason::ChildCompleted
                | MasterContinuationReason::ScatterJoinComplete
        );
        if guardable {
            self.recently_claimed_external
                .insert(item.dedupe_key.clone(), item.enqueued_at);
        }
    }

    /// Drop recently-claimed entries older than the guard window relative to
    /// `now`. Keeps the map bounded to live churn. `now` is the enqueue
    /// timestamp of the call that triggered the prune.
    fn prune_recently_claimed(&mut self, now: SystemTime) {
        self.recently_claimed_external
            .retain(|_, claimed_at| within_recent_claim_window(*claimed_at, now));
    }

    /// True when an `External` continuation whose dedupe key STARTS WITH
    /// `key_prefix` was CLAIMED (popped by a drain) within
    /// [`RECENT_CLAIM_GUARD_WINDOW`] of `now`. Reads the same
    /// `recently_claimed_external` map the double-enqueue guard maintains.
    ///
    /// The fleet-synthesis gate uses this to treat a peer whose `peer_send_input`
    /// was just popped by the drain — but whose turn has not yet registered in
    /// the active-turn map — as still busy, closing the pop-vs-active-snapshot
    /// TOCTOU: the pop removes the item from `pending_by_key` AND records the
    /// claim here in ONE critical section, so a caller that finds no pending
    /// item is guaranteed to find the claim instead. `key_prefix` is the
    /// per-session key stem (e.g. `external/peer_send_input/<session>/`) so a
    /// distinct peer's claim never matches.
    pub(crate) fn has_recent_external_claim_with_prefix(
        &self,
        key_prefix: &str,
        now: SystemTime,
    ) -> bool {
        self.recently_claimed_external
            .iter()
            .any(|(key, claimed_at)| {
                key.as_str().starts_with(key_prefix) && within_recent_claim_window(*claimed_at, now)
            })
    }

    /// Drop a SPECIFIC External key from the recent-claim guard so a subsequent
    /// enqueue of that exact key is NOT collapsed as a recent duplicate. Returns
    /// whether an entry was removed.
    ///
    /// Used by the peer-fleet-synthesis RESET: that key is STABLE per master, so
    /// after a fleet is cleared a fresh fleet completing within
    /// [`RECENT_CLAIM_GUARD_WINDOW`] would otherwise have its enqueue rejected as
    /// a duplicate of the just-claimed PRIOR synthesis — dropping the fresh
    /// continuation and leaving the fresh fleet marked-but-unsynthesized. Clearing
    /// the entry on reset re-opens the key for the next legitimate fire. Distinct
    /// from [`reinsert`](Self::reinsert)'s guard-clear (an undelivered restore);
    /// this is a deliberate reset of a delivered, now-obsolete claim.
    pub(crate) fn clear_recent_external_claim(
        &mut self,
        dedupe_key: &MasterContinuationDedupeKey,
    ) -> bool {
        self.recently_claimed_external.remove(dedupe_key).is_some()
    }
}

/// True when `candidate` falls within [`RECENT_CLAIM_GUARD_WINDOW`] after
/// `claimed_at`. Wall-clock based (`SystemTime`), so NOT fully clock-skew
/// proof: a backward delta (`candidate` at/before `claimed_at`) is treated as
/// in-window — the safe direction, collapsing a re-enqueue that races the
/// claim — but a forward wall-clock jump larger than the window would let the
/// guard MISS and fall back to the (rare) double-delivery. Acceptable today
/// because production `External` keys are one-shot (see field doc); revisit
/// with a monotonic/injected clock if that ever changes.
fn within_recent_claim_window(claimed_at: SystemTime, candidate: SystemTime) -> bool {
    within_claim_window(claimed_at, candidate, RECENT_CLAIM_GUARD_WINDOW)
}

fn within_claim_window(claimed_at: SystemTime, candidate: SystemTime, window: Duration) -> bool {
    match candidate.duration_since(claimed_at) {
        Ok(elapsed) => elapsed <= window,
        // `candidate` is at or before `claimed_at`: same instant or reordered
        // sampling within one transition — always in-window.
        Err(_) => true,
    }
}
