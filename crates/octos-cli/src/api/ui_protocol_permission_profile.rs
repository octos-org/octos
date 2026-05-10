//! M9-β-2 — `permission/profile/list` and `permission/profile/set`
//! handler module.
//!
//! Background: PR #858 stubbed both methods with `method_not_supported`
//! to keep the merge train green while α-5/α-6 (atomic SSE delete)
//! landed. This module restores them as the v1 server slice's
//! authoritative implementation.
//!
//! ## Wire shape (re-stating the core spec for context)
//!
//! - **`permission/profile/list`** — input is `PermissionProfileListParams
//!   { session_id }`; output is `PermissionProfileListResult { session_id,
//!   current, profiles }`. `current` is the selection currently in effect
//!   for the session; `profiles` is the canonical list of preset
//!   selections the UI can offer (one per `PermissionProfileMode`, all
//!   with `network: Deny` as the safe default).
//!
//! - **`permission/profile/set`** — input is `PermissionProfileSetParams
//!   { session_id, update }`; output is `PermissionProfileSetResult {
//!   session_id, current, applied }`. `applied` is `true` when the
//!   stored selection actually changed and `false` when the update was
//!   a no-op (idempotent). `current` is the post-update selection
//!   regardless of `applied`.
//!
//! ## Storage model
//!
//! Selection is per-session, stored in a process-global
//! `PermissionProfileStore` table keyed by `SessionKey`. A session
//! whose selection has not been set yet returns
//! `PermissionProfileSelection::default()` (workspace-write + network
//! denied). The store survives connection drops but not process
//! restarts — it is RAM-only by design, so a daemon restart resets
//! every selection to the default. This matches the existing
//! `ApprovalScopeKind` policy table contract (see
//! `ui_protocol_scope.rs`), and is intentional: permission profile
//! preferences should not silently outlive a server they were chosen
//! against.
//!
//! ## Out-of-scope (intentionally deferred)
//!
//! - Wiring the selection into the agent execution path (`ToolPolicy`
//!   gating, sandbox network policy). The selection is a session-scoped
//!   preference; the wiring is independent feature work tracked
//!   separately. Today's `permission/profile/set` records the choice;
//!   the agent loop continues to use its config-driven policy.
//! - Per-user profile presets beyond the three canonical modes. The
//!   `profiles` list is intentionally minimal so clients have a stable,
//!   non-customizable enumeration to render selection chips against.

use std::collections::HashMap;
use std::sync::Mutex;

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    PermissionNetworkPolicy, PermissionProfileListParams, PermissionProfileListResult,
    PermissionProfileMode, PermissionProfileSelection, PermissionProfileSetParams,
    PermissionProfileSetResult, PermissionProfileUpdate,
};

/// Process-global store of per-session permission profile selections.
///
/// Lookups are O(1). The mutex granularity is deliberately coarse —
/// the store sits off the hot path (a session opens it once, then
/// flips it occasionally), so a single mutex outperforms a sharded
/// store and is simpler to reason about.
#[derive(Debug, Default)]
pub(crate) struct PermissionProfileStore {
    inner: Mutex<HashMap<SessionKey, PermissionProfileSelection>>,
}

impl PermissionProfileStore {
    /// Return the current selection for `session_id`, or the default
    /// (`workspace-write` + `network: deny`) if the session has not
    /// yet stored one.
    pub(crate) fn current(&self, session_id: &SessionKey) -> PermissionProfileSelection {
        let guard = self.inner.lock().expect("permission profile store poisoned");
        guard
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    /// Apply `update` to the session's stored selection and return the
    /// post-update value plus whether it differed from the prior one.
    ///
    /// `applied = false` means the `update` was a no-op against the
    /// existing entry — the wire is idempotent so clients can safely
    /// retry on transient failures without flipping flags.
    pub(crate) fn apply(
        &self,
        session_id: &SessionKey,
        update: PermissionProfileUpdate,
    ) -> (PermissionProfileSelection, bool) {
        let mut guard = self.inner.lock().expect("permission profile store poisoned");
        let previous = guard.get(session_id).copied().unwrap_or_default();
        let next = update.apply_to(previous);
        let applied = next != previous;
        guard.insert(session_id.clone(), next);
        (next, applied)
    }
}

/// Build the canonical list of selectable profile presets.
///
/// Returns one entry per `PermissionProfileMode`, all with
/// `network: Deny` as the safe default. Clients render this as the
/// available choices; the actual `current` field on the wire reports
/// what the session has stored (which may differ from any of these
/// presets along the network axis once the user has overridden it).
fn canonical_profiles() -> Vec<PermissionProfileSelection> {
    [
        PermissionProfileMode::ReadOnly,
        PermissionProfileMode::WorkspaceWrite,
        PermissionProfileMode::DangerFullAccess,
    ]
    .into_iter()
    .map(|mode| PermissionProfileSelection {
        mode,
        network: PermissionNetworkPolicy::Deny,
    })
    .collect()
}

/// Handle `permission/profile/list`.
pub(crate) fn handle_list(
    store: &PermissionProfileStore,
    params: PermissionProfileListParams,
) -> PermissionProfileListResult {
    let current = store.current(&params.session_id);
    PermissionProfileListResult {
        session_id: params.session_id,
        current,
        profiles: canonical_profiles(),
    }
}

/// Handle `permission/profile/set`.
pub(crate) fn handle_set(
    store: &PermissionProfileStore,
    params: PermissionProfileSetParams,
) -> PermissionProfileSetResult {
    let (current, applied) = store.apply(&params.session_id, params.update);
    PermissionProfileSetResult {
        session_id: params.session_id,
        current,
        applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> SessionKey {
        SessionKey::new("api", id)
    }

    /// β-2 acceptance gate (A): a fresh session reports the default
    /// selection (workspace-write + network deny) without ever having
    /// stored one. This is the contract clients block on at first
    /// render — a `current` of `None`-shaped JSON would force them to
    /// pick a value, which is the wrong place for that decision.
    #[test]
    fn should_list_default_selection_for_unseen_session() {
        let store = PermissionProfileStore::default();
        let result = handle_list(
            &store,
            PermissionProfileListParams {
                session_id: session("fresh"),
            },
        );

        assert_eq!(result.session_id, session("fresh"));
        assert_eq!(result.current, PermissionProfileSelection::default());
        assert_eq!(result.current.mode, PermissionProfileMode::WorkspaceWrite);
        assert_eq!(result.current.network, PermissionNetworkPolicy::Deny);
    }

    /// β-2 acceptance gate (B): the canonical preset list is exactly
    /// the three modes, all with network denied. Without this the UI
    /// has nothing stable to render selection chips against, and the
    /// preset list would silently drift if a future commit added a
    /// fourth mode without updating this handler.
    #[test]
    fn should_list_canonical_three_profile_presets() {
        let store = PermissionProfileStore::default();
        let result = handle_list(
            &store,
            PermissionProfileListParams {
                session_id: session("any"),
            },
        );

        assert_eq!(result.profiles.len(), 3);
        let modes: Vec<_> = result.profiles.iter().map(|p| p.mode).collect();
        assert!(modes.contains(&PermissionProfileMode::ReadOnly));
        assert!(modes.contains(&PermissionProfileMode::WorkspaceWrite));
        assert!(modes.contains(&PermissionProfileMode::DangerFullAccess));
        assert!(
            result
                .profiles
                .iter()
                .all(|p| p.network == PermissionNetworkPolicy::Deny),
            "every preset must default to network-denied per the safe-default contract"
        );
    }

    /// β-2 acceptance gate (C): a `set` that flips the mode is
    /// recorded, and the very next `list` reflects it. This is the
    /// round-trip that clients depend on for their "you chose X"
    /// confirmation UI.
    #[test]
    fn should_round_trip_mode_change_through_set_and_list() {
        let store = PermissionProfileStore::default();
        let session_id = session("rt");

        let set_result = handle_set(
            &store,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(PermissionProfileMode::ReadOnly),
                    network: None,
                },
            },
        );
        assert!(set_result.applied, "first non-default set must be applied");
        assert_eq!(set_result.current.mode, PermissionProfileMode::ReadOnly);
        assert_eq!(set_result.current.network, PermissionNetworkPolicy::Deny);

        let list_result = handle_list(
            &store,
            PermissionProfileListParams {
                session_id: session_id.clone(),
            },
        );
        assert_eq!(list_result.current, set_result.current);
    }

    /// β-2 acceptance gate (D): a `set` that does not change the stored
    /// selection reports `applied: false`. Without this, a client that
    /// retries on flaky transport would see a sequence of "you changed
    /// X" confirmations for an unchanged value, which produces a
    /// jittery UI on reconnects.
    #[test]
    fn should_report_applied_false_for_idempotent_set() {
        let store = PermissionProfileStore::default();
        let session_id = session("idem");

        // First set lands the value.
        let _ = handle_set(
            &store,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(PermissionProfileMode::DangerFullAccess),
                    network: Some(PermissionNetworkPolicy::Allow),
                },
            },
        );

        // Second set with the same value is a no-op on the data, but
        // must still answer with the current (unchanged) selection.
        let result = handle_set(
            &store,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(PermissionProfileMode::DangerFullAccess),
                    network: Some(PermissionNetworkPolicy::Allow),
                },
            },
        );
        assert!(!result.applied, "no-op set must report applied=false");
        assert_eq!(result.current.mode, PermissionProfileMode::DangerFullAccess);
        assert_eq!(result.current.network, PermissionNetworkPolicy::Allow);
    }

    /// β-2 acceptance gate (E): selections are isolated per session.
    /// Without this a multi-tab user toggling the network axis on tab
    /// A would silently change the policy on tab B's open session,
    /// which is the cross-session leak we explicitly guard against.
    #[test]
    fn should_isolate_selections_across_sessions() {
        let store = PermissionProfileStore::default();
        let a = session("iso-a");
        let b = session("iso-b");

        let _ = handle_set(
            &store,
            PermissionProfileSetParams {
                session_id: a.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(PermissionProfileMode::ReadOnly),
                    network: Some(PermissionNetworkPolicy::Allow),
                },
            },
        );

        let list_a = handle_list(
            &store,
            PermissionProfileListParams {
                session_id: a.clone(),
            },
        );
        let list_b = handle_list(
            &store,
            PermissionProfileListParams {
                session_id: b.clone(),
            },
        );

        assert_eq!(list_a.current.mode, PermissionProfileMode::ReadOnly);
        assert_eq!(list_a.current.network, PermissionNetworkPolicy::Allow);

        // B is unchanged — full default, regardless of A's overrides.
        assert_eq!(list_b.current, PermissionProfileSelection::default());
    }

    /// β-2 acceptance gate (F): a partial `update` (only `mode`, no
    /// `network`) preserves the prior `network` axis. This is the
    /// `apply_to` contract from the core type but exercised through
    /// the handler so a future regression that bypassed
    /// `PermissionProfileUpdate::apply_to` would be caught here.
    #[test]
    fn should_preserve_unchanged_axis_on_partial_set() {
        let store = PermissionProfileStore::default();
        let session_id = session("partial");

        // Stash a full selection.
        let _ = handle_set(
            &store,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(PermissionProfileMode::WorkspaceWrite),
                    network: Some(PermissionNetworkPolicy::Allow),
                },
            },
        );

        // Now flip only `mode` — `network` must stay at `Allow`.
        let result = handle_set(
            &store,
            PermissionProfileSetParams {
                session_id: session_id.clone(),
                update: PermissionProfileUpdate {
                    mode: Some(PermissionProfileMode::ReadOnly),
                    network: None,
                },
            },
        );
        assert!(result.applied);
        assert_eq!(result.current.mode, PermissionProfileMode::ReadOnly);
        assert_eq!(
            result.current.network,
            PermissionNetworkPolicy::Allow,
            "partial set must preserve the unchanged axis"
        );
    }
}
