//! Append-only context audit: does a turn's request history only ever GROW?
//!
//! ## Why this exists
//!
//! The messages handed to the model and the messages written to the durable
//! session are two different objects. Nothing compares them, so they drift —
//! and the drift is silent at the moment it happens, surfacing later as a
//! resumed session the model never had, a context gauge over 100%, or a cost
//! figure computed from a request nobody can reconstruct.
//!
//! The `pi` harness avoids the whole family by never rewriting history: the
//! request prefix only grows, and output is bounded where it is PRODUCED
//! rather than trimmed after the fact. `deepseek-harness` enforces the same
//! property structurally, asserting that every loop-built request equals the
//! projection of its durable log.
//!
//! Adopting either is a large change. This module is the cheap precursor that
//! tells us whether it is warranted: it MEASURES how append-only we actually
//! are today, without changing what is sent.
//!
//! ## What it checks
//!
//! Within one turn, successive requests must be prefix-extensions of each
//! other: iteration N's message list must be an exact prefix of iteration
//! N+1's. Any index that changes content, or a list that shrinks, is a
//! REWRITE — the sent history at that point is no longer reconstructable by
//! replaying what came before.
//!
//! ## What it deliberately does NOT do
//!
//! - It never mutates, blocks, or fails a request. Findings are reported.
//! - It does not compare against the durable session. That lives in another
//!   crate; this is the in-turn half, which needs no new plumbing and which
//!   catches the known rewrite path (`truncate_old_tool_results`).
//! - It hashes content rather than retaining it: an audit that copies every
//!   request would be a memory regression on long turns.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
#[cfg(test)]
use std::sync::Mutex;

use octos_core::{Message, MessageRole};

/// Process-global findings collector.
///
/// The audit also logs through `tracing`, but a log line is not a measurement
/// you can verify: octos-agent's lib tests install no subscriber, so `warn!`
/// output goes nowhere and an empty run is indistinguishable from a clean one.
/// This gives the measurement a channel whose silence can be checked — drain
/// it and you know whether the audit ran at all.
///
/// Bounded so an armed long-running process cannot grow it without limit.
#[cfg(test)]
static FINDINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Retained-findings ceiling; further findings still log, and still count.
#[cfg(test)]
const MAX_RETAINED_FINDINGS: usize = 512;

/// Number of findings recorded since the process started, retained or not.
#[cfg(test)]
static FINDING_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Record one finding for later inspection.
///
/// Test-only on purpose. In production the channel is `tracing`, whose
/// subscriber the operator installs and can therefore verify; in tests no
/// subscriber exists, so the audit needs somewhere its silence can be checked.
#[cfg(test)]
pub(crate) fn record_finding(description: String) {
    FINDING_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut findings = FINDINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if findings.len() < MAX_RETAINED_FINDINGS {
        findings.push(description);
    }
}

/// Test-only arming override, so a test never has to mutate the environment
/// (`set_var` is `unsafe` under edition 2024, and this workspace denies unsafe).
#[cfg(test)]
static FORCED_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the audit is armed.
///
/// Opt-in because it is a measurement, not a guard: it exists to establish how
/// far octos already is from the append-only prefix `pi` keeps and
/// `deepseek-harness` enforces, before committing to either.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if FORCED_ON.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    std::env::var("OCTOS_APPEND_ONLY_AUDIT").is_ok_and(|value| value == "1")
}

/// Arm the audit for one test.
#[cfg(test)]
pub(crate) fn arm_for_test() {
    FORCED_ON.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Disarm after a test.
#[cfg(test)]
pub(crate) fn disarm_for_test() {
    FORCED_ON.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Total findings recorded, including any past the retention ceiling.
#[cfg(test)]
pub(crate) fn finding_count() -> usize {
    FINDING_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Drain retained findings, leaving the counter intact.
#[cfg(test)]
pub(crate) fn drain_findings() -> Vec<String> {
    let mut findings = FINDINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut *findings)
}

/// Cheap positional fingerprint of one message.
///
/// Content is hashed rather than kept so the audit's footprint stays flat on
/// long turns; `len` rides along because a length delta is the single most
/// useful thing in a report ("4,812 chars became 800" names the mechanism).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MessageFingerprint {
    role: MessageRole,
    len: usize,
    hash: u64,
}

impl MessageFingerprint {
    fn of(message: &Message) -> Self {
        let mut hasher = DefaultHasher::new();
        message.content.hash(&mut hasher);
        message.tool_call_id.hash(&mut hasher);
        // Tool calls are part of what the model saw; a rewritten argument list
        // is as much a divergence as rewritten prose.
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                call.id.hash(&mut hasher);
                call.name.hash(&mut hasher);
                call.arguments.to_string().hash(&mut hasher);
            }
        }
        Self {
            role: message.role,
            len: message.content.len(),
            hash: hasher.finish(),
        }
    }
}

/// One way a turn's history stopped being append-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Rewrite {
    /// A message that had already been sent came back with different content.
    Modified {
        index: usize,
        role: MessageRole,
        previous_len: usize,
        current_len: usize,
    },
    /// The history got SHORTER: messages already shown to the model are gone.
    Shrunk {
        previous_len: usize,
        current_len: usize,
    },
}

impl Rewrite {
    /// One-line report for a log line.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Modified {
                index,
                role,
                previous_len,
                current_len,
            } => format!(
                "message[{index}] ({role}) rewritten in place: {previous_len} -> {current_len} chars"
            ),
            Self::Shrunk {
                previous_len,
                current_len,
            } => format!("history shrank: {previous_len} -> {current_len} messages"),
        }
    }
}

/// Per-turn append-only auditor.
///
/// Holds only the previous request's fingerprints, so its cost is one small
/// vector per live turn regardless of how long the conversation runs.
#[derive(Default, Clone, Debug)]
pub(crate) struct AppendOnlyAudit {
    previous: Option<Vec<MessageFingerprint>>,
}

impl AppendOnlyAudit {
    /// Record this iteration's request and report how it broke append-only.
    ///
    /// The first call of a turn establishes the baseline and can report
    /// nothing — there is no earlier request to have diverged from.
    pub(crate) fn observe(&mut self, messages: &[Message]) -> Vec<Rewrite> {
        let current: Vec<MessageFingerprint> =
            messages.iter().map(MessageFingerprint::of).collect();
        let Some(previous) = self.previous.replace(current.clone()) else {
            return Vec::new();
        };

        let mut rewrites = Vec::new();
        if current.len() < previous.len() {
            rewrites.push(Rewrite::Shrunk {
                previous_len: previous.len(),
                current_len: current.len(),
            });
        }

        // Compare only the overlap: everything past it is legitimate growth.
        for (index, (before, after)) in previous.iter().zip(current.iter()).enumerate() {
            if before != after {
                rewrites.push(Rewrite::Modified {
                    index,
                    role: after.role,
                    previous_len: before.len,
                    current_len: after.len,
                });
            }
        }
        rewrites
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message_repair::truncate_old_tool_results;

    fn tool_result(id: &str, content: &str) -> Message {
        let mut message = Message::user(content);
        message.role = MessageRole::Tool;
        message.tool_call_id = Some(id.to_string());
        message
    }

    #[test]
    fn should_report_nothing_when_the_turn_only_appends() {
        let mut audit = AppendOnlyAudit::default();
        let mut messages = vec![Message::system("prompt"), Message::user("do the thing")];
        assert!(
            audit.observe(&messages).is_empty(),
            "the baseline cannot diverge"
        );

        messages.push(Message::assistant("calling a tool"));
        messages.push(tool_result("call-1", "a result"));
        assert!(
            audit.observe(&messages).is_empty(),
            "appending to the end is exactly what an append-only turn does"
        );
    }

    #[test]
    fn should_report_a_rewrite_when_an_earlier_message_changes_in_place() {
        let mut audit = AppendOnlyAudit::default();
        let messages = vec![Message::system("prompt"), Message::user("first")];
        audit.observe(&messages);

        let mut rewritten = messages.clone();
        rewritten[1].content = "first, edited".into();
        rewritten.push(Message::assistant("reply"));

        let rewrites = audit.observe(&rewritten);
        assert_eq!(
            rewrites,
            vec![Rewrite::Modified {
                index: 1,
                role: MessageRole::User,
                previous_len: 5,
                current_len: 13,
            }],
            "an edit under an index the model already saw is not an append"
        );
    }

    #[test]
    fn should_report_a_shrink_when_history_is_dropped() {
        let mut audit = AppendOnlyAudit::default();
        let messages = vec![
            Message::system("prompt"),
            Message::user("first"),
            Message::assistant("reply"),
        ];
        audit.observe(&messages);

        let rewrites = audit.observe(&messages[..2]);
        assert!(
            rewrites.contains(&Rewrite::Shrunk {
                previous_len: 3,
                current_len: 2,
            }),
            "messages the model already saw disappearing is the loudest possible divergence; got {rewrites:?}"
        );
    }

    /// The measurement this module was built to take.
    ///
    /// `truncate_old_tool_results` collapses tool results older than the last
    /// user message to 800 chars, in place, on the vector that is about to be
    /// sent — while the durable session keeps the full text. If the audit
    /// cannot see THAT, it cannot see anything that matters.
    #[test]
    fn should_report_the_real_in_place_truncation_octos_performs_today() {
        let mut audit = AppendOnlyAudit::default();
        let mut messages = vec![
            Message::system("prompt"),
            Message::user("first question"),
            Message::assistant("looking it up"),
            tool_result("call-1", &"x".repeat(4_000)),
            Message::user("second question"),
        ];
        audit.observe(&messages);

        let changed = truncate_old_tool_results(&mut messages);
        assert!(
            changed,
            "fixture must actually trip the production truncation"
        );

        let rewrites = audit.observe(&messages);
        let modified: Vec<_> = rewrites
            .iter()
            .filter(|r| matches!(r, Rewrite::Modified { .. }))
            .collect();
        assert_eq!(
            modified.len(),
            1,
            "exactly the collapsed tool result should be reported; got {rewrites:?}"
        );
        let Rewrite::Modified {
            index,
            role,
            previous_len,
            current_len,
        } = modified[0]
        else {
            unreachable!("filtered above")
        };
        assert_eq!(*index, 3);
        assert_eq!(*role, MessageRole::Tool);
        assert_eq!(*previous_len, 4_000);
        assert!(
            *current_len < *previous_len,
            "the production path collapses the result, so the length must fall"
        );
    }
}
