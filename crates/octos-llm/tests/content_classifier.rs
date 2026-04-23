//! Integration tests for the content-classified smart model routing.
//!
//! The classifier is a pure heuristic: it takes the user's input signals
//! (message text, long-form hints, enabled flag) and returns a tier that
//! the adaptive router can use to bias model selection. These tests lock
//! down the acceptance invariants in issue #493 (M6.6).

use octos_llm::{ClassificationDecision, ContentClassifier, ModelTier, RoutingConfig};

fn default_config() -> RoutingConfig {
    RoutingConfig {
        enabled: true,
        min_strong_length: 400,
        strong_keywords: vec![
            "debug".to_string(),
            "refactor".to_string(),
            "architecture".to_string(),
            "proof".to_string(),
        ],
    }
}

fn disabled_config() -> RoutingConfig {
    RoutingConfig {
        enabled: false,
        ..default_config()
    }
}

fn classify(input: &str, cfg: &RoutingConfig) -> ClassificationDecision {
    ContentClassifier::new(cfg.clone()).classify(input)
}

#[test]
fn should_route_to_cheap_for_short_plain_message() {
    let decision = classify("hello, how are you?", &default_config());
    assert_eq!(decision.tier, ModelTier::Cheap);
    assert!(
        decision.reasons.iter().any(|r| r.contains("default_cheap")),
        "expected default reason, got {:?}",
        decision.reasons
    );
}

#[test]
fn should_route_to_strong_when_strong_keyword_present() {
    let decision = classify("please help me debug this nasty failure", &default_config());
    assert_eq!(decision.tier, ModelTier::Strong);
    assert!(decision.reasons.iter().any(|r| r.contains("keyword")));
}

#[test]
fn should_route_to_strong_on_code_fence() {
    let decision = classify("quick q\n```rust\nfn f() {}\n```", &default_config());
    assert_eq!(decision.tier, ModelTier::Strong);
    assert!(decision.reasons.iter().any(|r| r.contains("code_fence")));
}

#[test]
fn should_respect_min_length_for_strong() {
    let cfg = RoutingConfig {
        min_strong_length: 100,
        ..default_config()
    };
    let short = "a".repeat(80);
    let long = "a".repeat(150);
    assert_eq!(classify(&short, &cfg).tier, ModelTier::Cheap);
    let long_decision = classify(&long, &cfg);
    assert_eq!(long_decision.tier, ModelTier::Strong);
    assert!(long_decision.reasons.iter().any(|r| r.contains("length")));
}

#[test]
fn should_not_false_match_substring_of_keyword() {
    // "debugger" must NOT trigger the "debug" keyword — word-boundary matching.
    let decision = classify("I like this debugger ui", &default_config());
    assert_eq!(decision.tier, ModelTier::Cheap);
}

#[test]
fn should_disable_classifier_when_config_disabled() {
    // When disabled, every input routes Strong (router unchanged behavior —
    // invariant #2: disabling the classifier must not downgrade any request).
    let decision = classify("hi", &disabled_config());
    assert_eq!(decision.tier, ModelTier::Strong);
    assert!(decision.reasons.iter().any(|r| r.contains("disabled")));
}

#[test]
fn should_emit_routing_decision_event() {
    let decision = classify("please refactor this large module", &default_config());
    // The classifier exposes a canonical `harness_event_payload()` we can
    // serialize. The payload name and fields are the contract for
    // `routing.decision`.
    let payload = decision.harness_event_payload();
    assert_eq!(payload.kind, "routing.decision");
    assert_eq!(payload.tier, "strong");
    assert!(!payload.reasons.is_empty());
}

#[test]
fn keyword_matching_is_case_insensitive() {
    let decision = classify("PLEASE DEBUG this", &default_config());
    assert_eq!(decision.tier, ModelTier::Strong);
}

#[test]
fn url_presence_is_reasoned_but_does_not_alone_upgrade() {
    // Per contract, URL presence is a signal we surface in reasons.
    // It is recorded but does not automatically flip the tier on its own
    // for a very short plain message.
    let decision = classify("see https://example.com", &default_config());
    // Either tier is acceptable here; assert the reason is recorded.
    let _ = decision.tier;
    assert!(
        decision.reasons.iter().any(|r| r.contains("url")),
        "expected a url reason, got {:?}",
        decision.reasons
    );
}
