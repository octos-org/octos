//! Integration tests for the cost reservation handle pattern (F-003).
//!
//! The reservation handle closes the TOCTOU race in the pre-F-003
//! `CostAccountant::project_dispatch` path, where parallel swarm
//! dispatches each read the same historical spend before any of them
//! hit the ledger. These tests exercise the three invariants that
//! together describe the fix:
//!
//! - Concurrent reservations against the same contract respect the
//!   per-contract cap (the headline property).
//! - Dropping a reservation without committing auto-refunds the
//!   projected amount so the freed budget is immediately re-usable.
//! - `commit` persists the *actual* cost, not the pre-spawn projection,
//!   so over-estimating during reservation never inflates the ledger.

#![cfg(unix)]

use std::sync::Arc;

use octos_agent::cost_ledger::{
    CostAccountant, CostAttributionEvent, CostBudgetPolicy, CostLedger, PersistentCostLedger,
};

fn make_accountant_with_cap(
    ledger: Arc<PersistentCostLedger>,
    per_contract_usd: f64,
) -> Arc<CostAccountant> {
    let policy = CostBudgetPolicy::default().with_per_contract_usd(per_contract_usd);
    Arc::new(CostAccountant::new(ledger, Some(policy)))
}

#[tokio::test]
async fn should_reject_concurrent_dispatches_exceeding_budget() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let accountant = make_accountant_with_cap(ledger, 1.0);

    // 10 concurrent reservations each requesting $0.20. With a $1.00
    // per-contract cap the reservation API must allow at most 5 of
    // them to succeed — the rest observe the in-flight reservations
    // via the shared lock and return `Err(Breach)`.
    let mut join_set = tokio::task::JoinSet::new();
    for _ in 0..10 {
        let accountant = accountant.clone();
        join_set.spawn(async move { accountant.reserve("contract-parallel", 0.20).await.is_ok() });
    }

    let mut allowed = 0usize;
    while let Some(result) = join_set.join_next().await {
        if result.unwrap() {
            allowed += 1;
        }
    }

    assert!(
        allowed <= 5,
        "expected at most 5 reservations to succeed under $1.00 cap, got {allowed}"
    );
    assert!(
        allowed >= 1,
        "expected at least 1 reservation to succeed, got {allowed}"
    );
}

#[tokio::test]
async fn should_refund_reservation_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    // Cap right at the projected amount so a second reservation is
    // only admissible if the first one was refunded.
    let accountant = make_accountant_with_cap(ledger, 0.50);

    {
        let handle = accountant
            .reserve("contract-refund", 0.50)
            .await
            .expect("first reservation must be allowed");
        assert_eq!(handle.contract_id(), "contract-refund");
        assert!((handle.reserved_amount_usd() - 0.50).abs() < f64::EPSILON);
        // Handle goes out of scope here without commit → auto-refund.
    }

    // Give the Drop-spawned refund task a moment to land.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let second = accountant
        .reserve("contract-refund", 0.50)
        .await
        .expect("second reservation must succeed after refund");
    assert_eq!(second.contract_id(), "contract-refund");
}

#[tokio::test]
async fn should_commit_actual_cost_not_projected() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_inner = Arc::new(PersistentCostLedger::open(dir.path()).await.unwrap());
    let ledger: Arc<dyn CostLedger> = ledger_inner.clone();
    let accountant = Arc::new(CostAccountant::new(
        ledger.clone(),
        Some(CostBudgetPolicy::default().with_per_contract_usd(10.0)),
    ));

    let handle = accountant
        .reserve("contract-commit", 0.50)
        .await
        .expect("reservation must succeed under $10 cap");

    let event = CostAttributionEvent::new(
        "session-commit",
        "contract-commit",
        "task-commit",
        "claude-haiku",
        100,
        50,
        0.30, // actual cost, less than the $0.50 projection
    );
    handle.commit(event).await.expect("commit must succeed");

    let rows = ledger
        .list_for_contract("contract-commit")
        .await
        .expect("list must succeed");
    assert_eq!(rows.len(), 1, "expected exactly one attribution row");
    assert!(
        (rows[0].cost_usd - 0.30).abs() < f64::EPSILON,
        "ledger must record actual $0.30, not reserved $0.50; got {}",
        rows[0].cost_usd
    );

    // Reservation should be released so a fresh dispatch sees an
    // empty in-memory reservations map.
    tokio::task::yield_now().await;
    let post_commit = accountant
        .reserve("contract-commit", 9.40)
        .await
        .expect("fresh reservation must succeed; historical 0.30 + reserved 0 + req 9.40 <= 10.0");
    drop(post_commit);
}
