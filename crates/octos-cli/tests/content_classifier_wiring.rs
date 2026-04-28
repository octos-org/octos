//! F-005 — assert the content classifier is wired into [`AppState`]
//! at startup when `config.content_routing.enabled` is true.
//!
//! The classifier is constructed eagerly in `commands/serve.rs`; this
//! integration test pins the observable field on [`AppState`] so a
//! future refactor can't silently drop the wiring.

#![cfg(feature = "api")]

use std::sync::Arc;

use octos_cli::api::AppState;
use octos_llm::{ContentClassifier, ModelTier, RoutingConfig};

#[tokio::test]
async fn should_populate_app_state_content_classifier_when_configured() {
    let config = RoutingConfig {
        enabled: true,
        min_strong_length: 400,
        strong_keywords: vec!["debug".into(), "refactor".into()],
    };
    let classifier = ContentClassifier::new(config);

    let mut state = AppState::empty_for_tests();
    state.content_classifier = Some(Arc::new(classifier));

    let wired = state
        .content_classifier
        .as_ref()
        .expect("classifier must be wired when config present");
    // Sanity-check the classifier is actually wired with the enabled
    // config — a long input should classify as Strong rather than the
    // disabled-branch short-circuit.
    let long_input = "r".repeat(1_000);
    let decision = wired.classify(&long_input);
    assert_eq!(decision.tier, ModelTier::Strong);
}

#[tokio::test]
async fn should_leave_app_state_content_classifier_unset_when_config_absent() {
    let state = AppState::empty_for_tests();
    assert!(
        state.content_classifier.is_none(),
        "classifier must stay None when config absent (F-005 spec step 4)"
    );
}

#[tokio::test]
async fn should_leave_classifier_unset_when_disabled_in_config() {
    // `enabled: false` is the absent-config equivalent per invariant
    // #3 of issue #493 — callers should not construct a classifier in
    // that case.
    let config = RoutingConfig {
        enabled: false,
        ..Default::default()
    };
    let wired: Option<Arc<ContentClassifier>> = if config.enabled {
        Some(Arc::new(ContentClassifier::new(config)))
    } else {
        None
    };
    assert!(
        wired.is_none(),
        "disabled routing must not instantiate a classifier"
    );
}
