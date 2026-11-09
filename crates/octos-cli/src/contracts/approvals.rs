use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, RwLock};

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
    /// The UI Protocol connection whose turn raised the approval (its
    /// `ConnectionId`), when known. `octos serve --host-managed` lets an
    /// external connection answer only approvals it owns (UPCR-2026-036).
    owner_connection: Option<u64>,
    /// Covers exactly one call: no remembered scope may be recorded from it
    /// (UPCR-2026-035 host-routed app tools).
    once_only: bool,
    /// For a host-routed app tool's call (UPCR-2026-035): the peer's route
    /// key (`crate::peers::host_tools::route_key`). Only the owning
    /// connection or the peer's current host connection may answer it.
    host_route: Option<String>,
    /// Raised by an external client's turn (`serve --host-managed`,
    /// UPCR-2026-036): only `owner_connection` sees or answers it. Kept on
    /// the entry so the rule holds even if the transport's side table has
    /// evicted the id.
    external: bool,
}

#[derive(Debug)]
enum ApprovalEntryState {
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
pub(crate) struct CancelledApproval {
    pub(crate) approval_id: ApprovalId,
    pub(crate) turn_id: TurnId,
}

const TERMINAL_APPROVAL_CAP: usize = 1024;

#[derive(Default)]
pub(crate) struct PendingApprovalStore {
    entries: RwLock<HashMap<ApprovalId, ApprovalEntry>>,
    /// FIFO of terminal ids used only for bounded late-response errors.
    /// Pending approvals are never evicted.
    terminal_order: Mutex<VecDeque<ApprovalId>>,
}

/// Context recovered from the original `ApprovalRequestedEvent` at `respond`
/// time. The scope policy needs `tool_name` and `turn_id` to decide what
/// `MatchKey` to record under, but the client's `ApprovalRespondParams`
/// carries only `approval_id` — we therefore lift the missing fields off the
/// stored entry and hand them back to the caller alongside the existing
/// result. `None` for the legacy `insert_pending` path that never carried
/// a request.
#[derive(Debug, Clone)]
pub(crate) struct RespondedApprovalContext {
    pub(crate) tool_name: String,
    pub(crate) turn_id: TurnId,
    /// The approval covers exactly one call; record no scope from it.
    pub(crate) once_only: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct RespondOutcome {
    pub(crate) result: ApprovalRespondResult,
    pub(crate) context: Option<RespondedApprovalContext>,
}

pub(crate) enum ApprovalRespondPersistError<E> {
    Rpc(RpcError),
    Persist(E),
}

impl PendingApprovalStore {
    fn record_terminal(
        &self,
        entries: &mut HashMap<ApprovalId, ApprovalEntry>,
        approval_id: ApprovalId,
    ) {
        let mut order = self
            .terminal_order
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        order.push_back(approval_id);
        while order.len() > TERMINAL_APPROVAL_CAP {
            let Some(expired) = order.pop_front() else {
                break;
            };
            if entries.get(&expired).is_some_and(|entry| {
                matches!(
                    entry.state,
                    ApprovalEntryState::Responded { .. } | ApprovalEntryState::Cancelled { .. }
                )
            }) {
                entries.remove(&expired);
            }
        }
    }

    /// Decide an approval and snapshot the metadata the audit/ledger path
    /// needs. Equivalent to [`Self::respond`] for callers that don't care
    /// about the captured request.
    #[cfg(test)]
    pub(crate) fn respond_with_context(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<RespondOutcome, RpcError> {
        self.respond_with_context_and_persist(params, |_| Ok::<(), std::convert::Infallible>(()))
            .map_err(|error| match error {
                ApprovalRespondPersistError::Rpc(error) => error,
                ApprovalRespondPersistError::Persist(never) => match never {},
            })
    }

    /// Persist a decision BEFORE changing its pending state or waking the
    /// waiting runtime. A persistence error leaves both untouched so the same
    /// response can be retried without executing an unlogged approval.
    pub(crate) fn respond_with_context_and_persist<E>(
        &self,
        params: ApprovalRespondParams,
        persist: impl FnOnce(&RespondOutcome) -> Result<(), E>,
    ) -> Result<RespondOutcome, ApprovalRespondPersistError<E>> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries.get_mut(&params.approval_id) else {
            return Err(ApprovalRespondPersistError::Rpc(approval_not_found_error(
                &params,
            )));
        };
        if entry.session_id != params.session_id {
            return Err(ApprovalRespondPersistError::Rpc(approval_not_found_error(
                &params,
            )));
        }

        match &entry.state {
            ApprovalEntryState::Pending => {
                let context = entry
                    .request
                    .as_ref()
                    .map(|request| RespondedApprovalContext {
                        tool_name: request.tool_name.clone(),
                        turn_id: request.turn_id.clone(),
                        once_only: entry.once_only,
                    });
                let preview = RespondOutcome {
                    result: ApprovalRespondResult::accepted_with_runtime_resumed(
                        params.approval_id.clone(),
                        false,
                    ),
                    context: context.clone(),
                };
                persist(&preview).map_err(ApprovalRespondPersistError::Persist)?;
                entry.state = ApprovalEntryState::Responded {
                    decision: params.decision.clone(),
                };
                let runtime_resumed = entry
                    .response_tx
                    .take()
                    .is_some_and(|tx| tx.send(params.decision.clone()).is_ok());
                let outcome = RespondOutcome {
                    result: ApprovalRespondResult::accepted_with_runtime_resumed(
                        params.approval_id.clone(),
                        entry.runtime_resumable && runtime_resumed,
                    ),
                    context,
                };
                self.record_terminal(&mut entries, params.approval_id.clone());
                Ok(outcome)
            }
            ApprovalEntryState::Responded { decision } => {
                let request_title = entry.request.as_ref().map(|request| request.title.as_str());
                Err(ApprovalRespondPersistError::Rpc(
                    approval_not_pending_error(&params, decision.clone(), request_title),
                ))
            }
            ApprovalEntryState::Cancelled { reason } => {
                Err(ApprovalRespondPersistError::Rpc(approval_cancelled_error(
                    &params,
                    reason,
                    entry.request.as_ref().map(|request| &request.turn_id),
                )))
            }
        }
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
    pub(crate) fn cancel_pending_for_turn(
        &self,
        session_id: &SessionKey,
        turn_id: &TurnId,
        reason: &str,
    ) -> Vec<CancelledApproval> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
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
        for cancelled_approval in &cancelled {
            self.record_terminal(&mut entries, cancelled_approval.approval_id.clone());
        }
        cancelled
    }

    pub(crate) fn cancel_pending_approval(
        &self,
        session_id: &SessionKey,
        approval_id: &ApprovalId,
        fallback_turn_id: &TurnId,
        reason: &str,
    ) -> Option<CancelledApproval> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
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
        let cancelled = CancelledApproval {
            approval_id: approval_id.clone(),
            turn_id,
        };
        self.record_terminal(&mut entries, approval_id.clone());
        Some(cancelled)
    }

    pub(crate) fn request(&self, event: ApprovalRequestedEvent) -> ApprovalRequestedEvent {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        entries.insert(
            event.approval_id.clone(),
            ApprovalEntry {
                session_id: event.session_id.clone(),
                state: ApprovalEntryState::Pending,
                request: Some(event.clone()),
                runtime_resumable: false,
                response_tx: None,
                owner_connection: None,
                once_only: false,
                host_route: None,
                external: false,
            },
        );
        event
    }

    pub(crate) fn request_runtime(
        &self,
        event: ApprovalRequestedEvent,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        self.request_runtime_owned(event, None)
    }

    /// [`Self::request_runtime`], recording the connection that owns the
    /// approval (see [`Self::pending_owner`]).
    pub(crate) fn request_runtime_owned(
        &self,
        event: ApprovalRequestedEvent,
        owner_connection: Option<u64>,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        self.request_runtime_entry(event, owner_connection, false, None)
    }

    /// [`Self::request_runtime_owned`] with the UPCR-2026-035 flags: a
    /// `once_only` approval never records a remembered scope, and a
    /// `host_route` one (a host-routed app tool's call) is answered only by
    /// its owning connection or the peer's current host connection.
    pub(crate) fn request_runtime_entry(
        &self,
        event: ApprovalRequestedEvent,
        owner_connection: Option<u64>,
        once_only: bool,
        host_route: Option<String>,
    ) -> tokio::sync::oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        entries.insert(
            event.approval_id.clone(),
            ApprovalEntry {
                session_id: event.session_id.clone(),
                state: ApprovalEntryState::Pending,
                request: Some(event),
                runtime_resumable: true,
                response_tx: Some(tx),
                owner_connection,
                once_only,
                host_route,
                external: false,
            },
        );
        rx
    }

    /// The owning connection of a PENDING approval of `session_id`: `None`
    /// when no such approval is pending, `Some(None)` when it has no recorded
    /// owner.
    pub(crate) fn pending_owner(
        &self,
        session_id: &SessionKey,
        approval_id: &ApprovalId,
    ) -> Option<Option<u64>> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        entries
            .get(approval_id)
            .filter(|entry| {
                entry.session_id == *session_id
                    && matches!(&entry.state, ApprovalEntryState::Pending)
            })
            .map(|entry| entry.owner_connection)
    }

    /// Mark approval `approval_id` as raised by an external client's turn
    /// (UPCR-2026-036): see [`Self::external_owner`].
    pub(crate) fn mark_external(&self, approval_id: &ApprovalId) {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = entries.get_mut(approval_id) {
            entry.external = true;
        }
    }

    /// For an approval raised by an external client's turn (in any state):
    /// `Some(owning connection)`, which alone may see or answer it
    /// (`Some(None)`: nobody may). `None` for every other approval.
    pub(crate) fn external_owner(&self, approval_id: &ApprovalId) -> Option<Option<u64>> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        entries
            .get(approval_id)
            .filter(|entry| entry.external)
            .map(|entry| entry.owner_connection)
    }

    /// For a host-routed call's approval (in any state): its peer route and
    /// owning connection. `None` for every other approval.
    pub(crate) fn host_route_owner(
        &self,
        approval_id: &ApprovalId,
    ) -> Option<(String, Option<u64>)> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        let entry = entries.get(approval_id)?;
        entry
            .host_route
            .clone()
            .map(|route| (route, entry.owner_connection))
    }

    pub(crate) fn pending_for_session(
        &self,
        session_id: &SessionKey,
    ) -> Vec<ApprovalRequestedEvent> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        entries
            .values()
            .filter(|entry| {
                entry.session_id == *session_id
                    && matches!(&entry.state, ApprovalEntryState::Pending)
            })
            .filter_map(|entry| entry.request.clone())
            .collect()
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
pub(crate) fn build_decided_event(
    params: &ApprovalRespondParams,
    outcome: &RespondOutcome,
    decided_by: impl Into<String>,
    decided_at: chrono::DateTime<chrono::Utc>,
) -> ApprovalDecidedEvent {
    let turn_id = outcome
        .context
        .as_ref()
        .map(|ctx| ctx.turn_id.clone())
        .unwrap_or_default();
    ApprovalDecidedEvent {
        session_id: params.session_id.clone(),
        topic: params.session_id.topic().map(ToOwned::to_owned),
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
