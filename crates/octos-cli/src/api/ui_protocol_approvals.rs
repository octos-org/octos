use std::collections::HashMap;
use std::sync::RwLock;

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    ApprovalDecidedEvent, ApprovalDecision, ApprovalId, ApprovalRequestedEvent,
    ApprovalRespondParams, ApprovalRespondResult, RpcError, TurnId, methods, rpc_error_codes,
};
use serde_json::json;

#[derive(Debug)]
struct ApprovalEntry {
    session_id: SessionKey,
    state: ApprovalEntryState,
    request: Option<ApprovalRequestedEvent>,
    runtime_resumable: bool,
    response_tx: Option<tokio::sync::oneshot::Sender<ApprovalDecision>>,
}

#[derive(Debug)]
enum ApprovalEntryState {
    #[allow(dead_code)]
    Pending,
    Responded {
        decision: ApprovalDecision,
    },
    /// The server administratively cancelled this approval before any client
    /// could respond. Late `respond` calls now return a typed error so the
    /// client can distinguish "moot" from "approved/denied".
    Cancelled {
        reason: String,
    },
}

/// One cancelled approval surfaced by [`PendingApprovalStore::cancel_pending_for_turn`].
#[derive(Debug, Clone)]
pub(super) struct CancelledApproval {
    pub(super) approval_id: ApprovalId,
    pub(super) turn_id: TurnId,
}

#[derive(Default)]
pub(super) struct PendingApprovalStore {
    entries: RwLock<HashMap<ApprovalId, ApprovalEntry>>,
}

/// Context recovered from the original `ApprovalRequestedEvent` at `respond`
/// time. The scope policy needs `tool_name` and `turn_id` to decide what
/// `MatchKey` to record under, but the client's `ApprovalRespondParams`
/// carries only `approval_id` — we therefore lift the missing fields off the
/// stored entry and hand them back to the caller alongside the existing
/// result. `None` for the legacy `insert_pending` path that never carried
/// a request.
#[derive(Debug, Clone)]
pub(super) struct RespondedApprovalContext {
    pub(super) tool_name: String,
    pub(super) turn_id: TurnId,
}

#[derive(Debug, Clone)]
pub(super) struct RespondOutcome {
    pub(super) result: ApprovalRespondResult,
    pub(super) context: Option<RespondedApprovalContext>,
}

impl PendingApprovalStore {
    /// Decide an approval and snapshot the metadata the audit/ledger path
    /// needs. Equivalent to [`Self::respond`] for callers that don't care
    /// about the captured request.
    pub(super) fn respond_with_context(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<RespondOutcome, RpcError> {
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        let Some(entry) = entries.get_mut(&params.approval_id) else {
            return Err(approval_not_found_error(&params));
        };

        if entry.session_id != params.session_id {
            return Err(approval_not_found_error(&params));
        }

        match &entry.state {
            ApprovalEntryState::Pending => {
                // FIX-01 made `ApprovalDecision` non-Copy (added `Unknown(String)`
                // for forward-compat); clone the decision out so we can both
                // store it on the entry and forward it to the runtime channel.
                let decision_for_state = params.decision.clone();
                let decision_for_runtime = params.decision.clone();
                entry.state = ApprovalEntryState::Responded {
                    decision: decision_for_state,
                };
                let runtime_resumed = entry
                    .response_tx
                    .take()
                    // FIX-01 made `ApprovalDecision` non-Copy; FIX-06 needs
                    // the decision to live across recording + return. Use
                    // the pre-cloned `decision_for_runtime`.
                    .is_some_and(|tx| tx.send(decision_for_runtime).is_ok());
                let context = entry
                    .request
                    .as_ref()
                    .map(|request| RespondedApprovalContext {
                        tool_name: request.tool_name.clone(),
                        turn_id: request.turn_id.clone(),
                    });
                Ok(RespondOutcome {
                    result: ApprovalRespondResult::accepted_with_runtime_resumed(
                        params.approval_id,
                        entry.runtime_resumable && runtime_resumed,
                    ),
                    context,
                })
            }
            ApprovalEntryState::Responded { decision } => {
                let request_title = entry.request.as_ref().map(|request| request.title.as_str());
                Err(approval_not_pending_error(
                    &params,
                    decision.clone(),
                    request_title,
                ))
            }
            ApprovalEntryState::Cancelled { reason } => Err(approval_cancelled_error(
                &params,
                reason,
                entry.request.as_ref().map(|request| &request.turn_id),
            )),
        }
    }

    #[cfg(test)]
    pub(super) fn respond(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<RespondOutcome, RpcError> {
        self.respond_with_context(params)
    }

    /// Atomically cancel every still-pending approval that belongs to the
    /// given turn. Idempotent: a second call after all entries are already
    /// `Cancelled`/`Responded` returns an empty list.
    ///
    /// FIX-06 interaction: this only touches per-call pending entries. Scope
    /// entries (`approve_for_session`) live in a separate store and are not
    /// affected here; `approve_for_turn` scopes are evicted by the caller via
    /// `evict_turn` already wired into `handle_turn_interrupt`.
    ///
    /// TODO(M9-FIX-07-followup): emit an audit entry per cancellation
    /// (`decision: "cancelled"` with `reason`) so the audit log mirrors the
    /// durable ledger. Out of scope for FIX-08 — flagged so a follow-up can
    /// pick it up without re-reading the spec.
    pub(super) fn cancel_pending_for_turn(
        &self,
        session_id: &SessionKey,
        turn_id: &TurnId,
        reason: &str,
    ) -> Vec<CancelledApproval> {
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        let mut cancelled = Vec::new();
        for (approval_id, entry) in entries.iter_mut() {
            if entry.session_id != *session_id {
                continue;
            }
            if !matches!(&entry.state, ApprovalEntryState::Pending) {
                continue;
            }
            let entry_turn_id = match entry.request.as_ref().map(|request| &request.turn_id) {
                Some(turn) => turn,
                None => continue,
            };
            if entry_turn_id != turn_id {
                continue;
            }
            entry.state = ApprovalEntryState::Cancelled {
                reason: reason.to_owned(),
            };
            // Drop any pending runtime waiter; the aborted task will see the
            // closed receiver and treat it as a denial — matching pre-fix
            // behaviour for the runtime side of the channel.
            entry.response_tx = None;
            cancelled.push(CancelledApproval {
                approval_id: approval_id.clone(),
                turn_id: entry_turn_id.clone(),
            });
        }
        cancelled
    }

    pub(super) fn cancel_pending_approval(
        &self,
        session_id: &SessionKey,
        approval_id: &ApprovalId,
        fallback_turn_id: &TurnId,
        reason: &str,
    ) -> Option<CancelledApproval> {
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        let entry = entries.get_mut(approval_id)?;
        if entry.session_id != *session_id {
            return None;
        }
        if !matches!(&entry.state, ApprovalEntryState::Pending) {
            return None;
        }
        let turn_id = entry
            .request
            .as_ref()
            .map(|request| request.turn_id.clone())
            .unwrap_or_else(|| fallback_turn_id.clone());
        entry.state = ApprovalEntryState::Cancelled {
            reason: reason.to_owned(),
        };
        // Drop any pending runtime waiter, but keep the approval entry so a
        // late client response receives the typed cancellation error.
        entry.response_tx = None;
        Some(CancelledApproval {
            approval_id: approval_id.clone(),
            turn_id,
        })
    }

    #[allow(dead_code)]
    pub(super) fn insert_pending(&self, session_id: SessionKey, approval_id: ApprovalId) {
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        entries.insert(
            approval_id,
            ApprovalEntry {
                session_id,
                state: ApprovalEntryState::Pending,
                request: None,
                runtime_resumable: false,
                response_tx: None,
            },
        );
    }

    pub(super) fn request(&self, event: ApprovalRequestedEvent) -> ApprovalRequestedEvent {
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        entries.insert(
            event.approval_id.clone(),
            ApprovalEntry {
                session_id: event.session_id.clone(),
                state: ApprovalEntryState::Pending,
                request: Some(event.clone()),
                runtime_resumable: false,
                response_tx: None,
            },
        );
        event
    }

    pub(super) fn request_runtime(
        &self,
        event: ApprovalRequestedEvent,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        entries.insert(
            event.approval_id.clone(),
            ApprovalEntry {
                session_id: event.session_id.clone(),
                state: ApprovalEntryState::Pending,
                request: Some(event),
                runtime_resumable: true,
                response_tx: Some(tx),
            },
        );
        rx
    }

    pub(super) fn pending_for_session(
        &self,
        session_id: &SessionKey,
    ) -> Vec<ApprovalRequestedEvent> {
        let entries = self
            .entries
            .read()
            .expect("pending approval store poisoned");
        entries
            .values()
            .filter(|entry| {
                entry.session_id == *session_id
                    && matches!(&entry.state, ApprovalEntryState::Pending)
            })
            .filter_map(|entry| entry.request.clone())
            .collect()
    }

    #[allow(dead_code)]
    pub(super) fn remove_pending(&self, session_id: &SessionKey, approval_id: &ApprovalId) -> bool {
        let mut entries = self
            .entries
            .write()
            .expect("pending approval store poisoned");
        let should_remove = entries.get(approval_id).is_some_and(|entry| {
            entry.session_id == *session_id && matches!(&entry.state, ApprovalEntryState::Pending)
        });
        if should_remove {
            entries.remove(approval_id);
        }
        should_remove
    }

    #[cfg(test)]
    pub(super) fn requested_event(
        &self,
        approval_id: &ApprovalId,
    ) -> Option<ApprovalRequestedEvent> {
        let entries = self
            .entries
            .read()
            .expect("pending approval store poisoned");
        entries
            .get(approval_id)
            .and_then(|entry| entry.request.clone())
    }
}

/// Build the [`ApprovalDecidedEvent`] that gets durably appended to the
/// ledger and (separately) recorded in the audit log.
///
/// Callers populate `decided_by` from their auth context. For auto-resolved
/// decisions (M9-FIX-06's path), set `auto_resolved = true` and supply a
/// `policy_id` after construction — see the auto-resolved emission site in
/// `UiProtocolApprovalRequester::request_approval`.
///
/// `outcome.context` is `None` for the legacy `insert_pending` test path
/// that never carried a request; in that case we synthesize a fresh
/// `TurnId` so the event still serializes.
pub(super) fn build_decided_event(
    params: &ApprovalRespondParams,
    outcome: &RespondOutcome,
    decided_by: impl Into<String>,
    decided_at: chrono::DateTime<chrono::Utc>,
) -> ApprovalDecidedEvent {
    let turn_id = outcome
        .context
        .as_ref()
        .map(|ctx| ctx.turn_id.clone())
        .unwrap_or_else(TurnId::new);
    ApprovalDecidedEvent {
        session_id: params.session_id.clone(),
        approval_id: params.approval_id.clone(),
        turn_id,
        // FIX-01: `ApprovalDecision` is non-Copy (`Unknown(String)`); clone
        // for the event so the caller can keep using `params`.
        decision: params.decision.clone(),
        scope: params.approval_scope.clone(),
        decided_at,
        decided_by: decided_by.into(),
        auto_resolved: false,
        policy_id: None,
        client_note: params.client_note.clone(),
    }
}

fn approval_not_found_error(params: &ApprovalRespondParams) -> RpcError {
    RpcError::new(
        rpc_error_codes::UNKNOWN_APPROVAL_ID,
        "approval/respond target was not found for this session",
    )
    .with_data(json!({
        "kind": "unknown_approval",
        "method": methods::APPROVAL_RESPOND,
        "session_id": params.session_id,
        "approval_id": params.approval_id,
        "legacy_kind": "approval_not_found",
    }))
}

fn approval_not_pending_error(
    params: &ApprovalRespondParams,
    recorded_decision: ApprovalDecision,
    request_title: Option<&str>,
) -> RpcError {
    RpcError::new(
        rpc_error_codes::APPROVAL_NOT_PENDING,
        "approval/respond target is no longer pending",
    )
    .with_data(json!({
        "kind": "approval_not_pending",
        "method": methods::APPROVAL_RESPOND,
        "session_id": params.session_id,
        "approval_id": params.approval_id,
        "recorded_decision": recorded_decision,
        "request_title": request_title,
    }))
}

fn approval_cancelled_error(
    params: &ApprovalRespondParams,
    reason: &str,
    turn_id: Option<&TurnId>,
) -> RpcError {
    RpcError::new(
        rpc_error_codes::APPROVAL_CANCELLED,
        "approval/respond target was cancelled before a response arrived",
    )
    .with_data(json!({
        "kind": "approval_cancelled",
        "method": methods::APPROVAL_RESPOND,
        "session_id": params.session_id,
        "approval_id": params.approval_id,
        "turn_id": turn_id,
        "reason": reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{ApprovalRespondStatus, TurnId};

    #[test]
    fn known_pending_approval_accepts_once() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        store.insert_pending(session_id.clone(), approval_id.clone());

        let outcome = store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect("pending approval should accept");

        assert!(outcome.result.accepted);
        assert_eq!(outcome.result.status, ApprovalRespondStatus::Accepted);
        assert!(!outcome.result.runtime_resumed);
        // `insert_pending` doesn't carry a request — context is `None`.
        assert!(outcome.context.is_none());

        let error = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Deny,
            ))
            .expect_err("responded approval is not pending");

        assert_eq!(error.code, rpc_error_codes::APPROVAL_NOT_PENDING);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("approval_not_pending"))
        );
    }

    #[test]
    fn approval_request_is_stored_and_can_be_responded_to() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        let turn_id = TurnId::new();

        let event = store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Run command",
            "cargo test",
        ));

        assert_eq!(event.approval_id, approval_id);
        assert_eq!(
            store
                .requested_event(&approval_id)
                .as_ref()
                .map(|event| event.title.as_str()),
            Some("Run command")
        );

        let outcome = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect("stored approval request should accept");

        assert!(outcome.result.accepted);
        assert_eq!(outcome.result.status, ApprovalRespondStatus::Accepted);
        assert!(!outcome.result.runtime_resumed);
        // Context recovered from the stored `ApprovalRequestedEvent`.
        let context = outcome.context.expect("context should be present");
        assert_eq!(context.tool_name, "shell");
        assert_eq!(context.turn_id, turn_id);
    }

    #[tokio::test]
    async fn runtime_approval_response_resumes_waiting_tool() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        let response_rx = store.request_runtime(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            TurnId::new(),
            "shell",
            "Run command",
            "printf approved",
        ));

        let outcome = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect("runtime approval should accept");

        assert!(outcome.result.runtime_resumed);
        assert_eq!(
            response_rx.await.expect("approval receiver"),
            ApprovalDecision::Approve
        );
    }

    #[test]
    fn missing_approval_is_typed_not_found() {
        let store = PendingApprovalStore::default();
        let error = store
            .respond(ApprovalRespondParams::new(
                SessionKey("local:test".into()),
                ApprovalId::new(),
                ApprovalDecision::Approve,
            ))
            .expect_err("missing approval should fail");

        assert_eq!(error.code, rpc_error_codes::UNKNOWN_APPROVAL_ID);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("unknown_approval"))
        );
    }

    #[test]
    fn pending_approval_survives_cross_session_reconnect_probe() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let other_session_id = SessionKey("local:other".into());
        let approval_id = ApprovalId::new();
        store.insert_pending(session_id.clone(), approval_id.clone());

        let wrong_session = store
            .respond(ApprovalRespondParams::new(
                other_session_id,
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect_err("approval must be scoped to its owning session");
        assert_eq!(wrong_session.code, rpc_error_codes::UNKNOWN_APPROVAL_ID);

        let outcome = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect("owning session can still approve after reconnect");
        assert_eq!(outcome.result.status, ApprovalRespondStatus::Accepted);
    }

    #[test]
    fn pending_for_session_returns_only_unanswered_requests() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let other_session_id = SessionKey("local:other".into());
        let pending_id = ApprovalId::new();
        let answered_id = ApprovalId::new();

        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            pending_id.clone(),
            TurnId::new(),
            "shell",
            "Pending command",
            "cargo test",
        ));
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            answered_id.clone(),
            TurnId::new(),
            "shell",
            "Answered command",
            "cargo fmt",
        ));
        store.request(ApprovalRequestedEvent::generic(
            other_session_id,
            ApprovalId::new(),
            TurnId::new(),
            "shell",
            "Other session",
            "cargo check",
        ));
        store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                answered_id,
                ApprovalDecision::Deny,
            ))
            .expect("answer one approval");

        let pending = store.pending_for_session(&session_id);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].approval_id, pending_id);
        assert_eq!(pending[0].title, "Pending command");
    }

    #[test]
    fn removed_pending_approval_is_not_found_for_late_response() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        store.insert_pending(session_id.clone(), approval_id.clone());

        assert!(store.remove_pending(&session_id, &approval_id));
        let error = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect_err("late response after timeout removal should miss");

        assert_eq!(error.code, rpc_error_codes::UNKNOWN_APPROVAL_ID);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("unknown_approval"))
        );
    }

    #[test]
    fn cancel_pending_for_turn_returns_only_matching_pending_entries() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let other_session = SessionKey("local:other".into());
        let interrupted_turn = TurnId::new();
        let surviving_turn = TurnId::new();

        let cancel_target = ApprovalId::new();
        let other_turn = ApprovalId::new();
        let other_session_target = ApprovalId::new();
        let already_responded = ApprovalId::new();

        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            cancel_target.clone(),
            interrupted_turn.clone(),
            "shell",
            "Should cancel",
            "rm -rf /tmp/x",
        ));
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            other_turn.clone(),
            surviving_turn.clone(),
            "shell",
            "Different turn",
            "ls",
        ));
        store.request(ApprovalRequestedEvent::generic(
            other_session.clone(),
            other_session_target.clone(),
            interrupted_turn.clone(),
            "shell",
            "Different session, same turn id",
            "ls",
        ));
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            already_responded.clone(),
            interrupted_turn.clone(),
            "shell",
            "Already approved",
            "ls",
        ));
        store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                already_responded.clone(),
                ApprovalDecision::Approve,
            ))
            .expect("approve before cancel");

        let cancelled =
            store.cancel_pending_for_turn(&session_id, &interrupted_turn, "turn_interrupted");

        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].approval_id, cancel_target);
        assert_eq!(cancelled[0].turn_id, interrupted_turn);

        // The previously-responded entry keeps its recorded decision.
        let recorded = store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                already_responded,
                ApprovalDecision::Deny,
            ))
            .expect_err("recorded decision is preserved");
        assert_eq!(recorded.code, rpc_error_codes::APPROVAL_NOT_PENDING);

        // Surviving turn (same session, different turn id) is untouched.
        let survive = store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                other_turn,
                ApprovalDecision::Approve,
            ))
            .expect("surviving turn still pending");
        // FIX-06 wrapped the result in `RespondOutcome { result, context }`.
        assert!(survive.result.accepted);

        // Other session with the same turn id is untouched.
        let foreign = store
            .respond(ApprovalRespondParams::new(
                other_session,
                other_session_target,
                ApprovalDecision::Approve,
            ))
            .expect("foreign session still pending");
        assert!(foreign.result.accepted);
    }

    #[test]
    fn cancel_pending_for_turn_is_idempotent() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));

        let first = store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].approval_id, approval_id);

        let second = store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");
        assert!(
            second.is_empty(),
            "second cancel must be a no-op for already-cancelled entries",
        );
    }

    #[test]
    fn cancel_with_no_pending_approvals_is_noop() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();

        let cancelled = store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");
        assert!(
            cancelled.is_empty(),
            "interrupt on a session with no pending approvals must be a no-op",
        );
    }

    #[test]
    fn respond_to_cancelled_approval_returns_typed_error() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));

        store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");

        let err = store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect_err("late respond against cancelled approval must fail");
        assert_eq!(err.code, rpc_error_codes::APPROVAL_CANCELLED);
        let data = err.data.expect("typed error data");
        assert_eq!(data["kind"], json!("approval_cancelled"));
        assert_eq!(data["reason"], json!("turn_interrupted"));
        assert_eq!(data["approval_id"], json!(approval_id));
        assert_eq!(data["turn_id"], json!(turn_id));
    }

    #[tokio::test]
    async fn cancel_drops_runtime_waiter_so_it_resolves_to_deny() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        let rx = store.request_runtime(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));

        store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");

        // Runtime waiter sees the receiver close as Err, which the agent code
        // unwraps to Deny — preserving pre-fix runtime semantics for the
        // already-aborted task.
        assert!(
            rx.await.is_err(),
            "cancel must drop the runtime sender so the receiver errors",
        );
    }

    #[test]
    fn cancelled_approval_is_excluded_from_pending_for_session() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id,
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));

        store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");
        assert!(
            store.pending_for_session(&session_id).is_empty(),
            "cancelled approvals must not replay as fresh pending cards"
        );
    }

    #[test]
    fn exact_cancel_preserves_cancelled_error_for_late_respond() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let turn_id = TurnId::new();
        let approval_id = ApprovalId::new();
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Pending",
            "ls",
        ));

        let cancelled = store
            .cancel_pending_approval(&session_id, &approval_id, &turn_id, "request_send_failed")
            .expect("approval cancelled");
        assert_eq!(cancelled.approval_id, approval_id);
        assert_eq!(cancelled.turn_id, turn_id);

        let error = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect_err("late response should see cancelled state");

        assert_eq!(error.code, rpc_error_codes::APPROVAL_CANCELLED);
        assert_eq!(error.data.as_ref().unwrap()["kind"], "approval_cancelled");
        assert_eq!(
            error.data.as_ref().unwrap()["reason"],
            "request_send_failed"
        );
    }

    #[test]
    fn reconnect_retry_preserves_recorded_approval_decision() {
        let store = PendingApprovalStore::default();
        let session_id = SessionKey("local:test".into());
        let approval_id = ApprovalId::new();
        store.insert_pending(session_id.clone(), approval_id.clone());

        store
            .respond(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Deny,
            ))
            .expect("first response records decision");
        let error = store
            .respond(ApprovalRespondParams::new(
                session_id,
                approval_id,
                ApprovalDecision::Approve,
            ))
            .expect_err("reconnect retry should see recorded decision");

        assert_eq!(error.code, rpc_error_codes::APPROVAL_NOT_PENDING);
        assert_eq!(
            error.data.as_ref().unwrap()["recorded_decision"],
            json!(ApprovalDecision::Deny)
        );
    }

    fn pending_request_fixture(store: &PendingApprovalStore) -> (SessionKey, ApprovalId, TurnId) {
        let session_id = SessionKey("local:audit".into());
        let approval_id = ApprovalId::new();
        let turn_id = TurnId::new();
        store.request(ApprovalRequestedEvent::generic(
            session_id.clone(),
            approval_id.clone(),
            turn_id.clone(),
            "shell",
            "Run command",
            "cargo test",
        ));
        (session_id, approval_id, turn_id)
    }

    #[test]
    fn decision_emits_approval_decided_durable_notification() {
        let store = PendingApprovalStore::default();
        let (session_id, approval_id, turn_id) = pending_request_fixture(&store);

        let mut params = ApprovalRespondParams::new(
            session_id.clone(),
            approval_id.clone(),
            ApprovalDecision::Approve,
        );
        params.approval_scope = Some("session".into());
        params.client_note = Some("ok".into());
        let outcome = store
            .respond_with_context(params.clone())
            .expect("decide manually");
        let event = build_decided_event(&params, &outcome, "user:tester", chrono::Utc::now());

        assert_eq!(event.turn_id, turn_id);
        assert_eq!(event.scope.as_deref(), Some("session"));
        assert_eq!(event.client_note.as_deref(), Some("ok"));
        assert_eq!(event.decided_by, "user:tester");
        assert!(!event.auto_resolved);
        assert_eq!(
            outcome.context.as_ref().map(|c| c.tool_name.as_str()),
            Some("shell")
        );
        assert!(store.pending_for_session(&session_id).is_empty());
        // Round-trips through the wire-shaped UiNotification carrier.
        let notification = octos_core::ui_protocol::UiNotification::ApprovalDecided(event.clone());
        let wire = notification
            .clone()
            .into_rpc_notification()
            .expect("serialize");
        assert_eq!(wire.method, methods::APPROVAL_DECIDED);
        assert_eq!(
            octos_core::ui_protocol::UiNotification::from_rpc_notification(wire).expect("decode"),
            notification
        );
    }

    #[test]
    fn auto_resolved_emits_approval_decided_with_auto_resolved_true() {
        // The auto-resolved emission lives at the request site (see
        // `UiProtocolApprovalRequester`); this unit test exercises just the
        // shape-side helper: an auto-resolved decision is built exactly
        // like a manual decision plus `auto_resolved = true` and a
        // `policy_id` set on the constructed event.
        let store = PendingApprovalStore::default();
        let (session_id, approval_id, _) = pending_request_fixture(&store);
        let outcome = store
            .respond_with_context(ApprovalRespondParams::new(
                session_id.clone(),
                approval_id.clone(),
                ApprovalDecision::Approve,
            ))
            .expect("decide auto");
        let mut event = build_decided_event(
            &ApprovalRespondParams::new(session_id, approval_id, ApprovalDecision::Approve),
            &outcome,
            "",
            chrono::Utc::now(),
        );
        event.auto_resolved = true;
        event.policy_id = Some("policy:trusted_shell".into());

        let wire = octos_core::ui_protocol::UiNotification::ApprovalDecided(event.clone())
            .into_rpc_notification()
            .expect("serialize");
        assert_eq!(wire.params["auto_resolved"], json!(true));
        assert_eq!(wire.params["policy_id"], json!("policy:trusted_shell"));
    }
}
