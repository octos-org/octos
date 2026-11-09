//! Approval scope policy table.
//!
//! Records per-session decisions whose `approval_scope` allows the runtime to
//! auto-resolve future matching approval requests, instead of forcing the user
//! to re-prompt every time. Counterpart to the `respond` flow in
//! `ui_protocol_approvals.rs`: when the client sends an `approval_scope`
//! stronger than the default `approve_once`, we insert an entry here; when a
//! new tool approval comes in, we look here first and short-circuit with
//! `approval/auto_resolved` if a matching entry resolves the decision.
//!
//! See spec `M9-FIX-06-approval-scope.md`. Tracks #644.
//!
//! Concurrency: per-session granularity. Each session has its own `Mutex` so
//! concurrent activity in unrelated sessions does not contend on a shared
//! lock. Sessions themselves are looked up under a `RwLock<HashMap<...>>`.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use octos_core::SessionKey;
use octos_core::ui_protocol::{ApprovalDecision, ApprovalScopeEntry, TurnId, approval_scopes};

/// Recognised scope kinds; unknown strings collapse to `ApproveOnce` per the
/// open-registry rule (caller should fall back to a normal `approval/requested`
/// flow when the lookup tells it not to record).
///
/// The shared `Approve*` prefix matches the spec's `approve_for_*` aliases —
/// silencing `enum_variant_names` because the prefix is load-bearing
/// documentation, not a missed naming mistake.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ApprovalScopeKind {
    /// `approve_once` / `request` — never recorded; one-shot only.
    ApproveOnce,
    /// `approve_for_turn` / `turn` — auto-resolves within the same `turn_id`.
    ApproveForTurn,
    /// `approve_for_session` / `session` — auto-resolves across the session
    /// until session/close.
    ApproveForSession,
    /// `approve_for_tool` / `tool` — auto-resolves any future call to the
    /// same `tool_name` until session/close.
    ApproveForTool,
}

impl ApprovalScopeKind {
    /// Open-registry parser: maps the various aliases the spec accepts. Any
    /// unknown string becomes `ApproveOnce` rather than an error, so a forward-
    /// compatible client can keep sending its own scope strings without
    /// breaking the server.
    pub(crate) fn from_scope_str(scope: &str) -> Self {
        match scope {
            // Default / "request" / "approve_once" — never recorded.
            approval_scopes::REQUEST | "approve_once" | "once" => Self::ApproveOnce,
            approval_scopes::TURN | "approve_for_turn" => Self::ApproveForTurn,
            approval_scopes::SESSION | "approve_for_session" => Self::ApproveForSession,
            approval_scopes::TOOL | "approve_for_tool" => Self::ApproveForTool,
            _ => Self::ApproveOnce,
        }
    }

    /// Canonical wire string used in the auto-resolved notification. Always
    /// the spec's short alias (`request`/`turn`/`session`/`tool`).
    pub(crate) fn as_wire_str(self) -> &'static str {
        match self {
            Self::ApproveOnce => approval_scopes::REQUEST,
            Self::ApproveForTurn => approval_scopes::TURN,
            Self::ApproveForSession => approval_scopes::SESSION,
            Self::ApproveForTool => approval_scopes::TOOL,
        }
    }

    /// Whether this scope kind should be recorded in the policy table.
    /// `ApproveOnce` is one-shot — no entry is needed.
    pub(crate) fn is_recordable(self) -> bool {
        !matches!(self, Self::ApproveOnce)
    }
}

/// What the policy is matching against to decide an entry applies.
///
/// `Tool(name)` for `approve_for_tool`, `Turn(id)` for `approve_for_turn`,
/// and `Session` for `approve_for_session` (the whole session, no extra key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum MatchKey {
    Session,
    Turn(TurnId),
    Tool(String),
}

impl MatchKey {
    fn as_wire_str(&self) -> String {
        match self {
            Self::Session => "*".to_owned(),
            Self::Turn(turn_id) => turn_id.0.to_string(),
            Self::Tool(name) => name.clone(),
        }
    }
}

/// One recorded decision with its match scope. The `turn_id` is held alongside
/// `kind=ApproveForTurn` so eviction-by-turn is exact.
#[derive(Debug, Clone)]
struct ScopeEntry {
    decision: ApprovalDecision,
    match_key: MatchKey,
    /// For turn-scoped entries we also store the turn id so we can evict on
    /// `turn/completed`. For other kinds this is `None`.
    turn_id: Option<TurnId>,
}

/// Per-session map keyed by `(scope_kind, match_key)`. Wrapped in a `Mutex`
/// because every operation either rewrites or scans the whole map; per-key
/// striping would be premature.
#[derive(Debug, Default)]
struct SessionScopes {
    entries: HashMap<(ApprovalScopeKind, MatchKey), ScopeEntry>,
}

/// The full policy table.
///
/// `sessions` is `RwLock<HashMap<SessionKey, Mutex<SessionScopes>>>` so that
/// reads against unrelated sessions do not block each other; only the inner
/// session-level `Mutex` is taken for reads/writes inside a single session,
/// avoiding a global write-lock for every approval-request lookup.
#[derive(Debug, Default)]
pub(crate) struct ScopePolicy {
    sessions: RwLock<HashMap<SessionKey, Mutex<SessionScopes>>>,
}

impl ScopePolicy {
    /// Records a decision under `(scope_kind, match_key)` for the given
    /// session. Idempotent: if an entry already exists it is overwritten —
    /// the most recent user choice always wins.
    ///
    /// Returns `true` if the scope was actually recorded (recordable kind),
    /// `false` if the scope is one-shot and there is nothing to do.
    pub(crate) fn record(
        &self,
        session_id: &SessionKey,
        scope_kind: ApprovalScopeKind,
        match_key: MatchKey,
        decision: ApprovalDecision,
    ) -> bool {
        if !scope_kind.is_recordable() {
            return false;
        }
        let turn_id = if let MatchKey::Turn(t) = &match_key {
            Some(t.clone())
        } else {
            None
        };
        // Slow path: insert a new session entry. We need a write lock on
        // `sessions` only when the session isn't there yet.
        let needs_insert = {
            let sessions = self.sessions.read().unwrap_or_else(|p| p.into_inner());
            !sessions.contains_key(session_id)
        };
        if needs_insert {
            let mut sessions = self.sessions.write().unwrap_or_else(|p| p.into_inner());
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| Mutex::new(SessionScopes::default()));
        }

        let sessions = self.sessions.read().unwrap_or_else(|p| p.into_inner());
        let session = sessions
            .get(session_id)
            .expect("session entry created above");
        let mut session = session.lock().unwrap_or_else(|p| p.into_inner());
        session.entries.insert(
            (scope_kind, match_key.clone()),
            ScopeEntry {
                decision,
                match_key,
                turn_id,
            },
        );
        true
    }

    /// Looks up a recorded decision for the given session and tool/turn
    /// context. `tool_name` and `turn_id` together drive the three lookups
    /// in priority order:
    ///
    ///   1. `(ApproveForTurn, turn_id)` — most specific
    ///   2. `(ApproveForTool, tool_name)`
    ///   3. `(ApproveForSession, *)`
    ///
    /// Earliest hit wins; any matching `Deny` short-circuits exactly the
    /// same way an `Approve` does — symmetry is intentional, the spec says
    /// `deny_*` analogs auto-deny.
    pub(crate) fn lookup(
        &self,
        session_id: &SessionKey,
        tool_name: &str,
        turn_id: &TurnId,
    ) -> Option<ScopeHit> {
        let sessions = self.sessions.read().unwrap_or_else(|p| p.into_inner());
        let session = sessions.get(session_id)?;
        let session = session.lock().unwrap_or_else(|p| p.into_inner());
        let probes: [(ApprovalScopeKind, MatchKey); 3] = [
            (
                ApprovalScopeKind::ApproveForTurn,
                MatchKey::Turn(turn_id.clone()),
            ),
            (
                ApprovalScopeKind::ApproveForTool,
                MatchKey::Tool(tool_name.to_owned()),
            ),
            (ApprovalScopeKind::ApproveForSession, MatchKey::Session),
        ];
        for (kind, key) in probes {
            if let Some(entry) = session.entries.get(&(kind, key)) {
                return Some(ScopeHit {
                    scope_kind: kind,
                    // FIX-01: `ApprovalDecision` is non-Copy because of
                    // `Unknown(String)`; clone out of the borrowed entry.
                    decision: entry.decision.clone(),
                    scope_match: entry.match_key.as_wire_str(),
                });
            }
        }
        None
    }

    /// Drops every entry recorded for `session_id`. Called on session/close
    /// (and also exposed for tests / future explicit revoke flows).
    ///
    /// `session/close` is not yet a wire event in the v1alpha1 protocol —
    /// see M9-FIX-06 § "Out of scope" — so this is currently invoked from
    /// `abort_connection_turns` (best-effort connection-close hook) and
    /// directly from tests. The method is kept on the public-to-module
    /// surface so the future `session/close` handler can call it without
    /// further refactoring.
    pub(crate) fn evict_session(&self, session_id: &SessionKey) {
        let mut sessions = self.sessions.write().unwrap_or_else(|p| p.into_inner());
        sessions.remove(session_id);
    }

    /// Drops every `ApproveForTurn` entry for `(session_id, turn_id)`. Other
    /// scope kinds (`session`, `tool`) are unaffected — they outlive the turn.
    pub(crate) fn evict_turn(&self, session_id: &SessionKey, turn_id: &TurnId) {
        let sessions = self.sessions.read().unwrap_or_else(|p| p.into_inner());
        let Some(session) = sessions.get(session_id) else {
            return;
        };
        let mut session = session.lock().unwrap_or_else(|p| p.into_inner());
        session.entries.retain(|_, entry| {
            entry
                .turn_id
                .as_ref()
                .is_none_or(|recorded| recorded != turn_id)
        });
    }

    /// Snapshot of every recorded scope for a session — wire-shaped so the
    /// `approval/scopes/list` handler can return it directly. Sorted for
    /// deterministic output.
    pub(crate) fn list_for_session(&self, session_id: &SessionKey) -> Vec<ApprovalScopeEntry> {
        let sessions = self.sessions.read().unwrap_or_else(|p| p.into_inner());
        let Some(session) = sessions.get(session_id) else {
            return Vec::new();
        };
        let session = session.lock().unwrap_or_else(|p| p.into_inner());
        let mut entries: Vec<ApprovalScopeEntry> = session
            .entries
            .iter()
            .map(|((kind, _), entry)| ApprovalScopeEntry {
                session_id: session_id.clone(),
                scope: kind.as_wire_str().to_owned(),
                scope_match: entry.match_key.as_wire_str(),
                // FIX-01: `ApprovalDecision` is non-Copy; clone out of the
                // borrowed entry into the wire payload.
                decision: entry.decision.clone(),
                turn_id: entry.turn_id.clone(),
            })
            .collect();
        entries.sort_by(|a, b| {
            a.scope
                .cmp(&b.scope)
                .then_with(|| a.scope_match.cmp(&b.scope_match))
        });
        entries
    }
}

/// Result of a successful `lookup` — what the auto-resolved notification
/// needs to know to render itself, plus the canonical scope wire string.
#[derive(Debug, Clone)]
pub(crate) struct ScopeHit {
    pub(crate) scope_kind: ApprovalScopeKind,
    pub(crate) decision: ApprovalDecision,
    pub(crate) scope_match: String,
}

impl ScopeHit {
    pub(crate) fn scope_wire(&self) -> &'static str {
        self.scope_kind.as_wire_str()
    }
}

/// Decide which `MatchKey` corresponds to a given scope kind given the
/// approval that was just decided. Centralising this keeps the `respond`
/// site small.
pub(crate) fn match_key_for(
    scope_kind: ApprovalScopeKind,
    tool_name: &str,
    turn_id: &TurnId,
) -> MatchKey {
    match scope_kind {
        ApprovalScopeKind::ApproveOnce => MatchKey::Session, // unused; recorded == false
        ApprovalScopeKind::ApproveForTurn => MatchKey::Turn(turn_id.clone()),
        ApprovalScopeKind::ApproveForTool => MatchKey::Tool(tool_name.to_owned()),
        ApprovalScopeKind::ApproveForSession => MatchKey::Session,
    }
}
