//! Acceptance tests for RP01: SafetyTier as ToolPolicy group family.
//!
//! These tests lock in the behaviour a robot integrator depends on —
//! tiered tools must be gated by `ToolPolicy::evaluate` through the
//! existing allow/deny pipeline, with no new trait method on `Tool`.

use std::process::Command;
use std::sync::{Mutex, OnceLock};

use octos_agent::permissions::SafetyTier;
use octos_agent::tools::policy::PolicyDecision;
use octos_agent::tools::robot_groups::{self, RobotToolRegistry};
use octos_agent::tools::ToolPolicy;

/// The robot-group registry is process-wide state. Serialize tests that
/// mutate it so they don't race against each other.
fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn install_demo_registry() {
    let mut reg = RobotToolRegistry::new();
    reg.insert("camera_read", SafetyTier::Observe);
    reg.insert("sensor_probe", SafetyTier::Observe);
    reg.insert("slow_move", SafetyTier::SafeMotion);
    reg.insert("valve_turn", SafetyTier::SafeMotion);
    reg.insert("fast_move", SafetyTier::FullActuation);
    reg.insert("e_stop_bypass", SafetyTier::EmergencyOverride);
    robot_groups::install_registry(reg);
}

#[test]
fn should_deny_safe_motion_tool_when_policy_allows_only_observe() {
    let _g = registry_guard();
    install_demo_registry();

    let policy = ToolPolicy {
        allow: vec!["group:robot:observe".into()],
        ..Default::default()
    };

    match policy.evaluate("slow_move") {
        PolicyDecision::Deny { reason } => assert_eq!(reason, "robot_tier_gate"),
        PolicyDecision::Allow => panic!("slow_move (safe_motion) must be denied under observe-only policy"),
    }
    assert!(policy.evaluate("camera_read") == PolicyDecision::Allow);
}

#[test]
fn should_allow_observe_tool_when_policy_is_empty() {
    let _g = registry_guard();
    install_demo_registry();

    let policy = ToolPolicy::default();
    assert_eq!(policy.evaluate("camera_read"), PolicyDecision::Allow);
    assert_eq!(policy.evaluate("sensor_probe"), PolicyDecision::Allow);
    // Empty policy must also allow non-robot tools.
    assert_eq!(policy.evaluate("read_file"), PolicyDecision::Allow);
}

#[test]
fn should_allow_full_actuation_tool_when_policy_grants_full_actuation_group() {
    let _g = registry_guard();
    install_demo_registry();

    let policy = ToolPolicy {
        allow: vec!["group:robot:full_actuation".into()],
        ..Default::default()
    };

    assert_eq!(policy.evaluate("fast_move"), PolicyDecision::Allow);
    // Subset semantics: lower-tier tools also pass.
    assert_eq!(policy.evaluate("slow_move"), PolicyDecision::Allow);
    assert_eq!(policy.evaluate("camera_read"), PolicyDecision::Allow);
    // Higher-tier tool must remain blocked.
    match policy.evaluate("e_stop_bypass") {
        PolicyDecision::Deny { reason } => assert_eq!(reason, "robot_tier_gate"),
        PolicyDecision::Allow => panic!("emergency_override tool must stay gated"),
    }
}

#[test]
fn should_reject_invalid_tier_string_in_from_str() {
    // from_str is case-insensitive but strict about the set of accepted names.
    let err = "dangerous".parse::<SafetyTier>().unwrap_err();
    assert_eq!(err.input, "dangerous");
    assert!(
        err.to_string().contains("invalid safety tier"),
        "error must identify itself: {err}"
    );
    assert!("".parse::<SafetyTier>().is_err());
    assert!("safe-motion".parse::<SafetyTier>().is_err());
    // Canonical names still parse.
    assert_eq!(
        "Observe".parse::<SafetyTier>().unwrap(),
        SafetyTier::Observe
    );
}

#[test]
#[ignore = "runs the example binary — enable with `cargo test -p octos-agent -- --ignored`"]
fn inspection_safety_example_runs_end_to_end_against_policy() {
    let status = Command::new(env!("CARGO"))
        .args([
            "run",
            "--quiet",
            "--example",
            "inspection_safety",
            "-p",
            "octos-agent",
        ])
        .status()
        .expect("spawn cargo run --example inspection_safety");
    assert!(
        status.success(),
        "inspection_safety example should exit cleanly"
    );
}
