//! A receipt-bound external operation may span multiple worker invocations.
//! An adapter supplies a private, append-only journal of validated receipts and
//! binds each worker to one operation generation. Capture the cursor when the
//! worker is admitted. A physical return is an input to reconciliation, and may
//! arrive before or after the operation's receipt. Each independent operation
//! advances on its own stream, including during fan-out.
//!
//! The host persists the return before acknowledging the worker, rechecks the
//! cursor before dispatch, and persists the chosen effect before acknowledging
//! the return. Replaying a return yields the same successor identity. External
//! systems with side effects must accept an idempotency key for the operation
//! if a process can stop before its receipt is recorded. OctoScript controls
//! workflow dependencies and joins. Retry bounds belong to the admitting adapter.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityAttempt {
    pub operation: String,
    pub attempt: u16,
    pub max_resumes: u16,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Observation {
    Pending,
    ReceiptAccepted,
    Superseded,
    /// A required input changed without an accepted transition.
    Inconsistent,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerExit {
    Returned,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Settled,
    Superseded,
    Interrupted,
    Resume(ActivityAttempt),
    Blocked(&'static str),
}

impl ActivityAttempt {
    /// This identity is independent of turn IDs, clocks and message delivery counts.
    pub fn occurrence(&self) -> String {
        if self.attempt == 0 {
            self.operation.clone()
        } else {
            format!("{}/attempt/{}", self.operation, self.attempt)
        }
    }

    pub fn resolve(&self, observed: Observation, exit: WorkerExit) -> Resolution {
        match (observed, exit) {
            (Observation::ReceiptAccepted, _) => Resolution::Settled,
            (Observation::Superseded, _) => Resolution::Superseded,
            (_, WorkerExit::Interrupted) => Resolution::Interrupted,
            (Observation::Inconsistent, _) => Resolution::Blocked("workflow evidence diverged"),
            (Observation::Pending, WorkerExit::Failed) => {
                Resolution::Blocked("worker failed before receipt")
            }
            (Observation::Pending, WorkerExit::Returned) if self.attempt < self.max_resumes => {
                Resolution::Resume(Self {
                    attempt: self.attempt + 1,
                    ..self.clone()
                })
            }
            (Observation::Pending, WorkerExit::Returned) => {
                Resolution::Blocked("receipt missing after final attempt")
            }
        }
    }
}

/// Append-only journal of domain-validated events. A cursor belongs to one
/// frozen workflow generation. Hosts bind that generation to an execution scope.
#[derive(Debug, Deserialize)]
pub struct ReceiptJournal {
    pub next: String,
    pub events: Vec<JournalReceipt>,
}

#[derive(Debug, Deserialize)]
pub struct JournalReceipt {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub previous_wait: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptCursor {
    offset: usize,
    predecessor: Option<String>,
    phase: String,
    expected_kind: String,
    adopted: bool,
}

/// An authorized domain adapter may attach to verified work that predates the journal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Awaited,
    AdoptExisting,
}

impl ReceiptCursor {
    pub fn at(
        journal: &ReceiptJournal,
        phase: &str,
        expected_kind: &str,
        admission: Admission,
    ) -> Option<Self> {
        let adopted = journal.next != phase && admission == Admission::AdoptExisting;
        (journal.next == phase || adopted).then(|| Self {
            offset: journal.events.len(),
            predecessor: journal.events.last().map(|event| event.id.clone()),
            phase: journal.next.clone(),
            adopted,
            expected_kind: expected_kind.to_owned(),
        })
    }

    pub fn observe(&self, journal: &ReceiptJournal) -> Observation {
        if journal.events.len() < self.offset
            || self
                .offset
                .checked_sub(1)
                .and_then(|p| journal.events.get(p))
                .map(|e| &e.id)
                != self.predecessor.as_ref()
        {
            return Observation::Inconsistent;
        }
        match journal.events.get(self.offset) {
            Some(event)
                if event.kind == self.expected_kind
                    && (event.previous_wait.is_none()
                        || (self.adopted && event.previous_wait.as_ref() == Some(&self.phase))) =>
            {
                Observation::ReceiptAccepted
            }
            Some(_) => Observation::Superseded,
            None if journal.next == self.phase => Observation::Pending,
            None => Observation::Inconsistent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(id: &str) -> ActivityAttempt {
        ActivityAttempt {
            operation: id.into(),
            attempt: 0,
            max_resumes: 1,
        }
    }

    #[test]
    fn independent_import_shards_and_page_review_share_the_same_lifecycle() {
        let import = first("import/dataset-v3/shard-7");
        let review = first("review/page-2/candidate-18");
        let retry = import.resolve(Observation::Pending, WorkerExit::Returned);
        assert_eq!(
            retry,
            import.resolve(Observation::Pending, WorkerExit::Returned)
        );
        let Resolution::Resume(second) = retry else {
            panic!("missing retry")
        };
        assert_eq!(second.occurrence(), "import/dataset-v3/shard-7/attempt/1");
        assert_eq!(
            review.resolve(Observation::ReceiptAccepted, WorkerExit::Returned),
            Resolution::Settled
        );
        assert!(matches!(
            second.resolve(Observation::Pending, WorkerExit::Returned),
            Resolution::Blocked(_)
        ));
        assert_eq!(
            second.resolve(Observation::ReceiptAccepted, WorkerExit::Failed),
            Resolution::Settled
        );
    }

    #[test]
    fn cancelled_failed_and_obsolete_work_cannot_launch_a_worker() {
        let work = first("dataset/generation-2");
        assert_eq!(
            work.resolve(Observation::Pending, WorkerExit::Interrupted),
            Resolution::Interrupted
        );
        assert_eq!(
            work.resolve(Observation::Superseded, WorkerExit::Returned),
            Resolution::Superseded
        );
        assert!(matches!(
            work.resolve(Observation::Pending, WorkerExit::Failed),
            Resolution::Blocked(_)
        ));
        assert!(matches!(
            work.resolve(Observation::Inconsistent, WorkerExit::Returned),
            Resolution::Blocked(_)
        ));
    }

    fn journal(next: &str, receipts: &[(&str, &str, Option<&str>)]) -> ReceiptJournal {
        ReceiptJournal {
            next: next.into(),
            events: receipts
                .iter()
                .map(|(id, kind, previous)| JournalReceipt {
                    id: (*id).into(),
                    kind: (*kind).into(),
                    previous_wait: previous.map(str::to_owned),
                })
                .collect(),
        }
    }

    #[test]
    fn late_worker_observes_its_own_receipt_even_after_the_next_stage_advances() {
        let import = journal("import", &[("p1", "allocated", None)]);
        let cursor = ReceiptCursor::at(&import, "import", "committed", Admission::Awaited).unwrap();
        assert_eq!(cursor.observe(&import), Observation::Pending);
        let verified = journal(
            "finished",
            &[
                ("p1", "allocated", None),
                ("tx7", "committed", None),
                ("ok", "verified", None),
            ],
        );
        assert_eq!(cursor.observe(&verified), Observation::ReceiptAccepted);
        assert_eq!(
            cursor.observe(&journal("import", &[])),
            Observation::Inconsistent
        );
        assert_eq!(
            cursor.observe(&journal("import", &[("different", "allocated", None)])),
            Observation::Inconsistent
        );
    }

    #[test]
    fn unrelated_and_rebased_receipts_cannot_settle_an_ordinary_wait() {
        let initial = journal("review", &[]);
        let cursor = ReceiptCursor::at(&initial, "review", "verdict", Admission::Awaited).unwrap();
        assert_eq!(
            cursor.observe(&journal("repair", &[("r", "candidate", None)])),
            Observation::Superseded
        );
        assert_eq!(
            cursor.observe(&journal("repair", &[("v", "verdict", Some("review"))])),
            Observation::Superseded
        );
    }

    #[test]
    fn explicit_adoption_anchors_the_next_verified_receipt_to_the_existing_history() {
        let existing = journal("review", &[("v1", "candidate", None)]);
        assert!(ReceiptCursor::at(&existing, "repair", "candidate", Admission::Awaited).is_none());
        let cursor =
            ReceiptCursor::at(&existing, "repair", "candidate", Admission::AdoptExisting).unwrap();
        assert_eq!(cursor.observe(&existing), Observation::Pending);
        let recovered = journal(
            "review",
            &[
                ("v1", "candidate", None),
                ("v2", "candidate", Some("review")),
            ],
        );
        assert_eq!(cursor.observe(&recovered), Observation::ReceiptAccepted);
    }
}
