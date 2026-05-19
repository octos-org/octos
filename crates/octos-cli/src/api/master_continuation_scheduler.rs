#![allow(dead_code)]
//! M15 production primitive for scheduling automatic master-agent continuation
//! turns. The real turn-loop wiring lands in the next integration step; this
//! module is compiled and covered by unit tests now so the contract is explicit.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::time::SystemTime;

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
    External(String),
}

impl MasterContinuationReason {
    pub(crate) fn priority(&self) -> MasterContinuationPriority {
        match self {
            Self::LoopFire => MasterContinuationPriority::LoopFire,
            Self::ChildCompleted | Self::ScatterJoinComplete => {
                MasterContinuationPriority::ChildOrScatterJoinComplete
            }
            Self::GoalContinue => MasterContinuationPriority::GoalContinue,
            Self::External(_) => MasterContinuationPriority::External,
        }
    }

    fn stable_name(&self) -> &str {
        match self {
            Self::ChildCompleted => "child_completed",
            Self::ScatterJoinComplete => "scatter_join_complete",
            Self::LoopFire => "loop_fire",
            Self::GoalContinue => "goal_continue",
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
}

impl QueuedMasterContinuation {
    pub(crate) fn is_for_session(&self, session_id: &MasterContinuationSessionId) -> bool {
        self.session_id == *session_id
    }

    pub(crate) fn is_for_profile(&self, profile_id: &MasterContinuationProfileId) -> bool {
        self.profile_id == *profile_id
    }
}

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
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.pending_by_key.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending_by_key.is_empty()
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

    pub(crate) fn peek_ready(
        &mut self,
        runtime_state: MasterContinuationRuntimeState,
    ) -> Option<&QueuedMasterContinuation> {
        if !runtime_state.is_idle_eligible() {
            return None;
        }
        self.discard_stale_heap_entries();
        let key = &self.heap.peek()?.dedupe_key;
        self.pending_by_key.get(key)
    }

    pub(crate) fn pop_ready(
        &mut self,
        runtime_state: MasterContinuationRuntimeState,
    ) -> Option<QueuedMasterContinuation> {
        if !runtime_state.is_idle_eligible() {
            return None;
        }

        loop {
            let entry = self.heap.pop()?;
            if self.entry_matches_pending(&entry) {
                if let Some(item) = self.pending_by_key.remove(&entry.dedupe_key) {
                    return Some(item);
                }
            }
        }
    }

    pub(crate) fn drain_ready(
        &mut self,
        runtime_state: MasterContinuationRuntimeState,
        max_items: usize,
    ) -> Vec<QueuedMasterContinuation> {
        if max_items == 0 || !runtime_state.is_idle_eligible() {
            return Vec::new();
        }

        let mut drained = Vec::new();
        while drained.len() < max_items {
            let Some(item) = self.pop_ready(runtime_state) else {
                break;
            };
            drained.push(item);
        }
        drained
    }

    pub(crate) fn drain_ready_for_session(
        &mut self,
        runtime_state: MasterContinuationRuntimeState,
        max_items: usize,
        session_id: &str,
        profile_id: &str,
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
                    item.session_id.as_str() == session_id && item.profile_id.as_str() == profile_id
                });
            if matches_session {
                if let Some(item) = self.pending_by_key.remove(&entry.dedupe_key) {
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

    #[cfg(test)]
    pub(crate) fn pending_count_for_session(&self, session_id: &str, profile_id: &str) -> usize {
        self.pending_by_key
            .values()
            .filter(|item| {
                item.session_id.as_str() == session_id && item.profile_id.as_str() == profile_id
            })
            .count()
    }

    fn discard_stale_heap_entries(&mut self) {
        while self
            .heap
            .peek()
            .is_some_and(|entry| !self.entry_matches_pending(entry))
        {
            self.heap.pop();
        }
    }

    fn entry_matches_pending(&self, entry: &HeapEntry) -> bool {
        self.pending_by_key
            .get(&entry.dedupe_key)
            .is_some_and(|item| item.sequence == entry.sequence && item.priority == entry.priority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn ts(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn request(reason: MasterContinuationReason, suffix: &str) -> MasterContinuationRequest {
        MasterContinuationRequest::new(
            format!("group-{suffix}"),
            "session-1",
            "profile-1",
            reason,
            ts(10),
        )
    }

    fn queued(outcome: MasterContinuationEnqueueOutcome) -> QueuedMasterContinuation {
        match outcome {
            MasterContinuationEnqueueOutcome::Queued(item) => item,
            MasterContinuationEnqueueOutcome::Duplicate { .. } => {
                panic!("expected queued continuation")
            }
        }
    }

    #[test]
    fn priority_order_matches_master_continuation_contract() {
        assert!(
            MasterContinuationReason::LoopFire.priority()
                > MasterContinuationReason::ChildCompleted.priority()
        );
        assert_eq!(
            MasterContinuationReason::ChildCompleted.priority(),
            MasterContinuationReason::ScatterJoinComplete.priority()
        );
        assert!(
            MasterContinuationReason::ScatterJoinComplete.priority()
                > MasterContinuationReason::GoalContinue.priority()
        );

        let mut scheduler = MasterContinuationScheduler::new();
        scheduler.enqueue_at(
            request(MasterContinuationReason::GoalContinue, "goal"),
            ts(20),
        );
        scheduler.enqueue_at(
            request(MasterContinuationReason::ChildCompleted, "child-a"),
            ts(21),
        );
        scheduler.enqueue_at(
            request(MasterContinuationReason::ScatterJoinComplete, "scatter"),
            ts(22),
        );
        scheduler.enqueue_at(request(MasterContinuationReason::LoopFire, "loop"), ts(23));
        scheduler.enqueue_at(
            request(MasterContinuationReason::ChildCompleted, "child-b"),
            ts(24),
        );

        let drained = scheduler.drain_ready(MasterContinuationRuntimeState::idle(), usize::MAX);
        let reasons = drained
            .into_iter()
            .map(|item| item.reason)
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                MasterContinuationReason::LoopFire,
                MasterContinuationReason::ChildCompleted,
                MasterContinuationReason::ScatterJoinComplete,
                MasterContinuationReason::ChildCompleted,
                MasterContinuationReason::GoalContinue,
            ]
        );
    }

    #[test]
    fn duplicate_suppression_uses_stable_dedupe_key() {
        let mut scheduler = MasterContinuationScheduler::new();
        let first = request(MasterContinuationReason::ChildCompleted, "stable")
            .with_child_agent_id("child-1")
            .with_metadata("phase", "summarize");
        let reordered_metadata = request(MasterContinuationReason::ChildCompleted, "stable")
            .with_metadata("phase", "summarize")
            .with_child_agent_id("child-1");

        let first_item = queued(scheduler.enqueue_at(first, ts(20)));
        let duplicate = scheduler.enqueue_at(reordered_metadata, ts(21));

        assert!(duplicate.is_duplicate());
        assert_eq!(scheduler.len(), 1);
        assert!(matches!(
            duplicate,
            MasterContinuationEnqueueOutcome::Duplicate {
                dedupe_key,
                existing_id
            } if dedupe_key == first_item.dedupe_key && existing_id == first_item.id
        ));
    }

    #[test]
    fn external_reason_and_explicit_dedupe_key_are_supported() {
        let mut scheduler = MasterContinuationScheduler::new();
        let first = scheduler.enqueue(
            request(
                MasterContinuationReason::External("manual-wakeup".to_string()),
                "external-a",
            )
            .with_dedupe_key("external/manual-wakeup"),
        );
        let first_item = queued(first);
        assert_eq!(first_item.dedupe_key.as_str(), "external/manual-wakeup");
        assert!(first_item.is_for_session(&MasterContinuationSessionId::from("session-1")));
        assert!(first_item.is_for_profile(&MasterContinuationProfileId::from("profile-1")));

        let duplicate = scheduler.enqueue_at(
            request(
                MasterContinuationReason::External("manual-wakeup".to_string()),
                "external-b",
            )
            .with_dedupe_key("external/manual-wakeup"),
            ts(21),
        );
        assert!(duplicate.is_duplicate());
        assert_eq!(scheduler.len(), 1);
    }

    #[test]
    fn idle_gating_blocks_pop_until_runtime_is_eligible() {
        let mut scheduler = MasterContinuationScheduler::new();
        scheduler.enqueue_at(
            request(MasterContinuationReason::LoopFire, "loop").with_loop_id("loop-1"),
            ts(20),
        );

        assert!(
            scheduler
                .pop_ready(MasterContinuationRuntimeState::busy())
                .is_none()
        );
        assert_eq!(scheduler.len(), 1);
        assert!(
            scheduler
                .pop_ready(MasterContinuationRuntimeState::idle().with_user_input_pending(true))
                .is_none()
        );
        assert_eq!(scheduler.len(), 1);
        assert!(
            scheduler
                .pop_ready(MasterContinuationRuntimeState::idle().with_approval_pending(true))
                .is_none()
        );
        assert_eq!(scheduler.len(), 1);

        let ready = scheduler
            .pop_ready(MasterContinuationRuntimeState::idle())
            .expect("idle runtime should pop queued continuation");
        assert_eq!(ready.reason, MasterContinuationReason::LoopFire);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn drain_ready_obeys_limit_and_releases_dedupe_keys() {
        let mut scheduler = MasterContinuationScheduler::new();
        let first = queued(scheduler.enqueue_at(
            request(MasterContinuationReason::GoalContinue, "goal-a").with_goal_id("goal-a"),
            ts(20),
        ));
        scheduler.enqueue_at(
            request(MasterContinuationReason::GoalContinue, "goal-b").with_goal_id("goal-b"),
            ts(21),
        );
        scheduler.enqueue_at(
            request(MasterContinuationReason::LoopFire, "loop").with_loop_id("loop-1"),
            ts(22),
        );

        let first_batch = scheduler.drain_ready(MasterContinuationRuntimeState::idle(), 2);
        assert_eq!(first_batch.len(), 2);
        assert_eq!(first_batch[0].reason, MasterContinuationReason::LoopFire);
        assert_eq!(first_batch[1].goal_id, Some(GoalId::from("goal-a")));
        assert_eq!(scheduler.len(), 1);

        let requeued = scheduler.enqueue_at(
            request(MasterContinuationReason::GoalContinue, "goal-a").with_goal_id("goal-a"),
            ts(23),
        );
        assert!(
            matches!(&requeued, MasterContinuationEnqueueOutcome::Queued(_)),
            "drained dedupe key should be reusable"
        );
        assert_ne!(
            requeued.queued().unwrap().id.as_u64(),
            first.id.as_u64(),
            "requeued continuation should get a fresh in-process id"
        );

        let remaining = scheduler.drain_ready(MasterContinuationRuntimeState::idle(), usize::MAX);
        assert_eq!(remaining.len(), 2);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn cancel_removes_pending_item_and_leaves_heap_entry_stale_until_next_read() {
        let mut scheduler = MasterContinuationScheduler::new();
        let item = queued(scheduler.enqueue_at(
            request(MasterContinuationReason::GoalContinue, "goal").with_goal_id("goal-1"),
            ts(20),
        ));
        assert!(scheduler.cancel(&item.dedupe_key).is_some());
        assert!(
            scheduler
                .peek_ready(MasterContinuationRuntimeState::idle())
                .is_none()
        );
        assert!(scheduler.is_empty());
    }

    #[test]
    fn reusing_cancelled_dedupe_key_does_not_pop_through_stale_heap_entry() {
        let mut scheduler = MasterContinuationScheduler::new();
        let stale = queued(
            scheduler.enqueue_at(
                request(MasterContinuationReason::LoopFire, "same")
                    .with_loop_id("loop-stale")
                    .with_dedupe_key("same-key"),
                ts(20),
            ),
        );
        scheduler.enqueue_at(
            request(MasterContinuationReason::ChildCompleted, "child")
                .with_child_agent_id("child-1"),
            ts(21),
        );

        assert!(scheduler.cancel(&stale.dedupe_key).is_some());
        let requeued = queued(
            scheduler.enqueue_at(
                request(MasterContinuationReason::GoalContinue, "same")
                    .with_goal_id("goal-1")
                    .with_dedupe_key("same-key"),
                ts(22),
            ),
        );

        let first = scheduler
            .pop_ready(MasterContinuationRuntimeState::idle())
            .expect("child completion should remain first");
        assert_eq!(first.reason, MasterContinuationReason::ChildCompleted);

        let second = scheduler
            .pop_ready(MasterContinuationRuntimeState::idle())
            .expect("requeued continuation should still be pending");
        assert_eq!(second.id, requeued.id);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn drain_ready_for_session_preserves_other_sessions() {
        let mut scheduler = MasterContinuationScheduler::new();
        scheduler.enqueue_at(
            MasterContinuationRequest::new(
                "group-other",
                "session-other",
                "profile-1",
                MasterContinuationReason::LoopFire,
                ts(20),
            ),
            ts(20),
        );
        scheduler.enqueue_at(
            MasterContinuationRequest::new(
                "group-target",
                "session-1",
                "profile-1",
                MasterContinuationReason::ChildCompleted,
                ts(21),
            )
            .with_child_agent_id("child-1"),
            ts(21),
        );

        let drained = scheduler.drain_ready_for_session(
            MasterContinuationRuntimeState::idle(),
            usize::MAX,
            "session-1",
            "profile-1",
        );
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reason, MasterContinuationReason::ChildCompleted);
        assert_eq!(scheduler.len(), 1);

        let remaining = scheduler.drain_ready(MasterContinuationRuntimeState::idle(), usize::MAX);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].reason, MasterContinuationReason::LoopFire);
        assert!(scheduler.is_empty());
    }
}
