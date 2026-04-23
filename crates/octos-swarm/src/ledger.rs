//! Cost ledger stub coordinated with M7.4.
//!
//! M7.5 depends on per-subtask cost attribution, but M7.4 (the real
//! `CostLedger` implementation + typed `CostAttribution` event) is being
//! implemented in parallel on `harness-m7/04-cost-ledger`. Until M7.4
//! lands, this module provides:
//!
//! - A minimal [`CostLedger`] trait with a single async attribution
//!   method. M7.4 will replace this with the shared trait.
//! - A [`NoopCostLedger`] default that records nothing and returns
//!   zero cost. Used when no ledger is wired.
//!
//! Every call site is tagged with `// TODO(M7.4): wire real ledger` so
//! a small reconciliation commit can replace the stub once M7.4 merges.
//! The primitive itself rolls up a numeric `total_cost_usd: Option<f64>`
//! in [`SwarmResult`](crate::result::SwarmResult) — `None` today, set by
//! M7.4 once it wires `CostLedger::summarize`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Minimal attribution record written to the cost ledger for each
/// sub-contract dispatch. M7.4's real [`CostAttributionEvent`] is a
/// superset of this (adds supervisor_session_id, provider/model, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwarmCostAttribution {
    pub dispatch_id: String,
    pub contract_id: String,
    pub backend: String,
    pub endpoint: String,
    pub outcome: String,
    /// Optional attempt counter (1-indexed). M7.4 extends this with
    /// token counts once the real ledger exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Abstract cost ledger. M7.4 will replace this with the shared trait
/// on `octos-agent`. Until then, implementers only need the single
/// `attribute` method — M7.5 does not read back from the ledger
/// directly.
#[async_trait]
pub trait CostLedger: Send + Sync {
    /// Record one sub-contract dispatch attempt against the ledger. The
    /// primitive calls this once per attempted dispatch (including
    /// retries). The ledger is responsible for idempotency across its
    /// own storage.
    async fn attribute(&self, record: &SwarmCostAttribution);

    /// Summarize the total cost attributed to a dispatch. The primitive
    /// rolls this into [`SwarmResult::total_cost_usd`]. Default returns
    /// `None` so the [`NoopCostLedger`] and any M7.4-unaware
    /// implementation stay compatible.
    async fn summarize(&self, _dispatch_id: &str) -> Option<f64> {
        None
    }
}

/// No-op ledger used when no cost attribution is wired. Records
/// nothing, summarizes to `None`.
#[derive(Debug, Default, Clone)]
pub struct NoopCostLedger;

#[async_trait]
impl CostLedger for NoopCostLedger {
    async fn attribute(&self, _record: &SwarmCostAttribution) {
        // TODO(M7.4): wire real ledger — this is intentionally a no-op
        // today so the swarm primitive compiles independently of
        // M7.4's shared trait.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Default)]
    struct SpyLedger {
        records: Mutex<Vec<SwarmCostAttribution>>,
    }

    #[async_trait]
    impl CostLedger for SpyLedger {
        async fn attribute(&self, record: &SwarmCostAttribution) {
            self.records
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(record.clone());
        }
    }

    #[tokio::test]
    async fn noop_ledger_returns_zero_summary() {
        let ledger = NoopCostLedger;
        assert!(ledger.summarize("dispatch").await.is_none());
    }

    #[tokio::test]
    async fn attribute_dispatches_to_implementer() {
        let spy: Arc<dyn CostLedger> = Arc::new(SpyLedger::default());
        spy.attribute(&SwarmCostAttribution {
            dispatch_id: "d1".into(),
            contract_id: "c1".into(),
            backend: "local".into(),
            endpoint: "claude".into(),
            outcome: "success".into(),
            attempt: Some(1),
        })
        .await;
        // Downcast round-trip isn't supported on Arc<dyn Trait>; the
        // test exists to prove the trait object dispatches without
        // panicking. The NoopCostLedger test above validates the
        // default summarize path.
    }
}
