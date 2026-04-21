//! # Inspection Safety — Tiered Tool Authorization via `ToolPolicy`
//!
//! Demonstrates how a robot integrator expresses supervisory safety tiers
//! *without* adding a trait method to `Tool`. Each tool is registered with a
//! `SafetyTier` through the shared `RobotToolRegistry`; the agent enforces
//! the ceiling by configuring a standard `ToolPolicy` with a single
//! `group:robot:<tier>` entry.
//!
//! Scenario: a gas-pipeline valve inspection. The operator grants the session
//! `group:robot:safe_motion` — cameras and slow moves only. If the LLM asks
//! for `fast_arm_move`, `ToolPolicy::evaluate` returns a deny with
//! `reason = "robot_tier_gate"`, and the existing dispatch site refuses
//! to execute it.
//!
//! ```bash
//! cargo run --example inspection_safety -p octos-agent
//! ```

use octos_agent::permissions::SafetyTier;
use octos_agent::tools::policy::PolicyDecision;
use octos_agent::tools::robot_groups::{self, RobotToolRegistry};
use octos_agent::tools::ToolPolicy;

fn main() -> eyre::Result<()> {
    // Step 1: the robot integrator declares which tools sit at which tier.
    // This is the config layer — no trait default method, no new Tool API.
    let mut registry = RobotToolRegistry::new();
    registry.insert("camera_capture", SafetyTier::Observe);
    registry.insert("sensor_read", SafetyTier::Observe);
    registry.insert("valve_slow_turn", SafetyTier::SafeMotion);
    registry.insert("arm_slow_move", SafetyTier::SafeMotion);
    registry.insert("fast_arm_move", SafetyTier::FullActuation);
    registry.insert("joint_full_actuation", SafetyTier::FullActuation);
    registry.insert("emergency_override", SafetyTier::EmergencyOverride);
    robot_groups::install_registry(registry);

    // Step 2: the operator chooses a session ceiling. Everything below is
    // expressed through the ordinary `ToolPolicy` allow list.
    let policy = ToolPolicy {
        allow: vec!["group:robot:safe_motion".into()],
        ..Default::default()
    };

    println!("Session policy: allow = {:?}", policy.allow);
    println!("(Operator granted SafeMotion — cameras + slow motion only)\n");

    // Step 3: mock a tiny agent loop. For each tool the LLM wants to run,
    // consult `ToolPolicy::evaluate`. The dispatch site is the same one the
    // real `ToolRegistry::execute` uses — no bespoke authorize() call.
    let tool_plan = [
        ("camera_capture", "read pressure gauge V-101"),
        ("valve_slow_turn", "quarter-turn V-101 clockwise"),
        ("fast_arm_move", "retract at max speed"),
        ("emergency_override", "disable force limits"),
    ];

    let mut allowed = 0usize;
    let mut denied = 0usize;
    for (tool, rationale) in tool_plan {
        match policy.evaluate(tool) {
            PolicyDecision::Allow => {
                println!("ALLOW  {tool:<24} — {rationale}");
                allowed += 1;
            }
            PolicyDecision::Deny { reason } => {
                println!("DENY   {tool:<24} — reason: {reason} ({rationale})");
                denied += 1;
            }
        }
    }

    println!("\nSummary: {allowed} allowed, {denied} denied");
    assert_eq!(allowed, 2, "camera_capture and valve_slow_turn must pass");
    assert_eq!(denied, 2, "full_actuation and emergency_override must be gated");

    println!("\nInspection safety demo complete.");
    Ok(())
}
