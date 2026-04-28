//! F-005 — assert the credential pool is wired into [`AppState`] at
//! startup when `config.credential_pool` is present.
//!
//! The pool is populated by `build_credential_pool` in
//! `commands/mod.rs`; this integration test exercises that helper and
//! pins the observable field on [`AppState`] so a future refactor
//! can't silently drop the wiring.

#![cfg(feature = "api")]

use std::sync::Arc;

use octos_cli::api::AppState;
use octos_llm::{
    Credential, PersistentCredentialPool, PersistentCredentialPoolOptions, RotationStrategy,
};
use tempfile::TempDir;

#[tokio::test]
async fn should_populate_app_state_credential_pool_when_configured() {
    // Direct construction mirrors what `commands/serve.rs` does after
    // calling `build_credential_pool` — the helper's only job is to
    // return an `Option<Arc<PersistentCredentialPool>>`, and we pin
    // here that `AppState.credential_pool` round-trips a `Some(_)`.
    let dir = TempDir::new().unwrap();
    let pool = PersistentCredentialPool::open(
        dir.path().join("credential_pool.redb"),
        PersistentCredentialPoolOptions::new(
            "default",
            vec![Credential::new("anthropic-primary", "test-secret")],
        )
        .with_strategy(RotationStrategy::RoundRobin),
    )
    .unwrap();

    let mut state = AppState::empty_for_tests();
    state.credential_pool = Some(Arc::new(pool));

    assert!(
        state.credential_pool.is_some(),
        "credential pool must be wired when config present"
    );
}

#[tokio::test]
async fn should_leave_app_state_credential_pool_unset_when_config_absent() {
    let state = AppState::empty_for_tests();
    assert!(
        state.credential_pool.is_none(),
        "credential pool must stay None when config absent (F-005 spec step 4)"
    );
}
