//! The `examples/dora-bridge-config/inspection_mission.dot` file in the repo
//! root is a documentation artifact that ships next to the dora-mcp bridge.
//! It must use only parser-recognised handlers + deadline-action keywords;
//! otherwise the example silently degrades to default `Codergen` behaviour
//! and the README's claims diverge from runtime semantics.
//!
//! Codex round-3 P2: an earlier revision used CamelCase / unsupported names
//! (`SensorCheck`, `Motion`, `SafetyGate`, `Abort`, `Skip`, `EmergencyStop`)
//! which all parsed as `Codergen` / `None`.

use octos_pipeline::{parse_dot, DeadlineAction, HandlerKind};

const DOT_PATH: &str = "../../examples/dora-bridge-config/inspection_mission.dot";

#[test]
fn should_parse_inspection_mission_dot_with_supported_handlers() {
    let src = std::fs::read_to_string(DOT_PATH)
        .unwrap_or_else(|e| panic!("read {DOT_PATH}: {e}"));
    let graph = parse_dot(&src).expect("parse_dot must accept the example");

    for node in graph.nodes.values() {
        assert!(
            matches!(
                node.handler,
                HandlerKind::Codergen | HandlerKind::Gate
            ),
            "node {} uses unsupported handler {:?}",
            node.id,
            node.handler,
        );

        if let Some(action) = node.deadline_action {
            assert!(
                matches!(
                    action,
                    DeadlineAction::Abort | DeadlineAction::Skip | DeadlineAction::Escalate | DeadlineAction::Retry { .. }
                ),
                "node {} has unsupported deadline_action {:?}",
                node.id,
                action,
            );
        }
    }
}
