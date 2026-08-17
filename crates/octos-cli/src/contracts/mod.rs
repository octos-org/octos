//! Process-global interactive-contract stores.
//!
//! Lifted verbatim out of the `api`-gated `api::ui_protocol` tree (Phase 3 of
//! bringing peer-agent goal into `octos chat`, mirroring the Phase 0 autonomy
//! extraction). Nothing here touches axum, `AppState`, or `WsConnection`: an
//! approval / user-question / diff-preview / approval-scope entry is a plain
//! in-memory registry keyed by `SessionKey`, holding a `tokio::sync::oneshot`
//! the blocked tool awaits.
//!
//! Why it had to move: [`contract_stores`] is the single PROCESS-GLOBAL
//! authority for "which session is parked on which prompt". `peer_respond`
//! resolves a peer's parked oneshot through the very same store the peer's own
//! requester registered it in — so the master and the peer must share one
//! registry inside one process. `octos serve` got that for free because both
//! sides lived behind the `api` feature; `octos chat --peers` hosts its peers
//! in-process too, and needs the identical guarantee WITHOUT the web surface.
//!
//! `api::ui_protocol` keeps working unchanged through the re-exports in
//! `api::mod` (`ui_protocol_approvals`, `ui_protocol_questions`, …).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use tracing::info;

use crate::approvals_audit::{ApprovalsAuditConfig, ApprovalsAuditLog};
use approvals::PendingApprovalStore;
use diff::{DiffPreviewConfig, PendingDiffPreviewStore};
use questions::PendingQuestionStore;
use scope::ScopePolicy;

pub(crate) mod approvals;
pub(crate) mod diff;
pub(crate) mod questions;
pub(crate) mod sanitize;
pub(crate) mod scope;

#[derive(Default)]
pub(crate) struct UiProtocolContractStores {
    pub(crate) approvals: PendingApprovalStore,
    /// UPCR-2026-023: pending structured user-questions, keyed by
    /// `question_id`. Mirrors `approvals`; the blocked `ask_user_question`
    /// tool awaits a oneshot resolved by `user_question/respond`.
    pub(crate) user_questions: PendingQuestionStore,
    /// Lazily-initialized pending diff-preview store. With a `data_dir`
    /// the first call hydrates from disk and subsequent inserts
    /// write-ahead before returning, so `diff/preview/get` survives
    /// daemon restart (mirrors the M9.6 ledger durability pattern).
    /// Without a `data_dir` (unit tests, headless smoke) we fall back
    /// to an ephemeral RAM-only store via `Default`.
    diff_previews: OnceLock<Arc<PendingDiffPreviewStore>>,
    /// Per-session approval-scope policy table — stores future-call gating
    /// rules registered by `respond` when the user picks a scope stronger
    /// than `approve_once`. See `scope.rs`.
    pub(crate) scopes: ScopePolicy,
    /// Lazily-initialized append-only audit log for approval decisions
    /// (FIX-07). The first decision creates the log under
    /// `<data_dir>/audit/approvals-<epoch>.log`; subsequent decisions reuse
    /// the same writer.
    audit: OnceLock<Arc<ApprovalsAuditLog>>,
}

impl UiProtocolContractStores {
    pub(crate) fn audit_log(&self, data_dir: &Path) -> Arc<ApprovalsAuditLog> {
        self.audit
            .get_or_init(|| {
                Arc::new(ApprovalsAuditLog::new(
                    data_dir,
                    ApprovalsAuditConfig::from_env(),
                ))
            })
            .clone()
    }

    /// Lazily build the durable diff-preview store. The first caller
    /// with a `data_dir` wins and runs disk recovery; without a
    /// `data_dir` we install an ephemeral store. Subsequent calls
    /// always return the same `Arc`.
    pub(crate) fn diff_previews(&self, data_dir: Option<&Path>) -> Arc<PendingDiffPreviewStore> {
        self.diff_previews
            .get_or_init(|| {
                let config = match data_dir {
                    Some(dir) => DiffPreviewConfig::durable(dir.to_path_buf()),
                    None => DiffPreviewConfig::ephemeral(),
                };
                if config.data_dir.is_some() {
                    let outcome = PendingDiffPreviewStore::recover(config);
                    info!(
                        target = "octos::diff_preview",
                        sessions_recovered = outcome.sessions_recovered,
                        entries_recovered = outcome.entries_recovered,
                        "ui protocol diff-preview store initialized with durable backing"
                    );
                    Arc::new(outcome.store)
                } else {
                    Arc::new(PendingDiffPreviewStore::with_config(config))
                }
            })
            .clone()
    }
}

/// The one registry every parked prompt in this process lands in.
///
/// A `OnceLock` process-global on purpose: `peer_respond` (master side) and the
/// peer's own approval / question requester run on different tasks — often
/// different sessions — and must see the SAME `HashMap`, or the master resolves
/// a oneshot nobody is awaiting. `octos chat --peers` depends on exactly this.
pub(crate) fn contract_stores() -> Arc<UiProtocolContractStores> {
    static CONTRACT_STORES: OnceLock<Arc<UiProtocolContractStores>> = OnceLock::new();
    CONTRACT_STORES
        .get_or_init(|| Arc::new(UiProtocolContractStores::default()))
        .clone()
}
