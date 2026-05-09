//! The `examples/dora-bridge-config/inspection_mission.dot` file in the repo
//! root is a documentation artifact that ships next to the dora-mcp bridge.
//! It must use only parser-recognised handlers + deadline-action keywords,
//! and any `gate` node must carry an executable predicate so the example
//! demonstrates real branching rather than degenerating into an
//! always-pass / unconditional-edge no-op (codex round-3 P2 + round-4 P2s).
//!
//! Specifically:
//!   * Round-3: handlers/actions had to live in the parser's set
//!     (`codergen|gate|noop|...`, `abort|skip|escalate|retry:N`).
//!   * Round-4: every `gate` node also needs `prompt=...` (the
//!     `GateHandler` defaults to `"true"` when the prompt is absent), and
//!     every gate-outgoing edge needs an explicit `condition=` so routing
//!     does not fall through to the executor's label-substring fallback.

use octos_pipeline::{DeadlineAction, HandlerKind, condition, parse_dot};

const DOT_PATH: &str = "../../examples/dora-bridge-config/inspection_mission.dot";

#[test]
fn should_parse_inspection_mission_dot_with_supported_handlers() {
    let src = std::fs::read_to_string(DOT_PATH).unwrap_or_else(|e| panic!("read {DOT_PATH}: {e}"));
    let graph = parse_dot(&src).expect("parse_dot must accept the example");

    for node in graph.nodes.values() {
        assert!(
            matches!(
                node.handler,
                HandlerKind::Codergen | HandlerKind::Gate | HandlerKind::Noop
            ),
            "node {} uses unsupported handler {:?}",
            node.id,
            node.handler,
        );

        if let Some(action) = node.deadline_action {
            assert!(
                matches!(
                    action,
                    DeadlineAction::Abort
                        | DeadlineAction::Skip
                        | DeadlineAction::Escalate
                        | DeadlineAction::Retry { .. }
                ),
                "node {} has unsupported deadline_action {:?}",
                node.id,
                action,
            );
        }
    }
}

#[test]
fn should_wire_every_gate_node_with_an_executable_predicate() {
    // GateHandler treats a missing/`"true"` prompt as unconditional pass, so
    // a `gate` node without a real prompt is a documentation lie. Codex
    // round-4 caught both `safety_gate` and `result_gate` in this state.

    let src = std::fs::read_to_string(DOT_PATH).unwrap_or_else(|e| panic!("read {DOT_PATH}: {e}"));
    let graph = parse_dot(&src).expect("parse_dot must accept the example");

    let gate_nodes: Vec<_> = graph
        .nodes
        .values()
        .filter(|n| matches!(n.handler, HandlerKind::Gate))
        .collect();

    assert!(
        !gate_nodes.is_empty(),
        "the example is supposed to demonstrate gates; none found",
    );

    for node in gate_nodes {
        let prompt = node
            .prompt
            .as_deref()
            .unwrap_or_else(|| panic!("gate node {} has no prompt", node.id));
        assert_ne!(
            prompt.trim(),
            "true",
            "gate node {} has a no-op `true` predicate",
            node.id,
        );
        assert!(
            !prompt.trim().is_empty(),
            "gate node {} has an empty predicate",
            node.id,
        );
    }
}

#[test]
fn should_route_every_gate_outgoing_edge_via_explicit_condition() {
    // The executor's edge-selection step (executor.rs::next_edge) prefers
    // `condition=`-matched edges; falling back to label substring or the
    // first unconditional edge erases the gate's branching intent. Every
    // gate-outgoing edge must therefore carry a real `condition=`.

    let src = std::fs::read_to_string(DOT_PATH).unwrap_or_else(|e| panic!("read {DOT_PATH}: {e}"));
    let graph = parse_dot(&src).expect("parse_dot must accept the example");

    let gate_ids: Vec<&str> = graph
        .nodes
        .values()
        .filter(|n| matches!(n.handler, HandlerKind::Gate))
        .map(|n| n.id.as_str())
        .collect();

    for gate_id in gate_ids {
        let outgoing: Vec<_> = graph.edges.iter().filter(|e| e.source == gate_id).collect();
        assert!(
            outgoing.len() >= 2,
            "gate {} should have at least two outgoing edges (pass/fail), found {}",
            gate_id,
            outgoing.len(),
        );
        for edge in outgoing {
            let cond = edge.condition.as_deref().unwrap_or_else(|| {
                panic!(
                    "gate-outgoing edge {} -> {} has no condition= attribute",
                    edge.source, edge.target,
                )
            });
            assert!(
                !cond.trim().is_empty(),
                "edge {} -> {} has an empty condition",
                edge.source,
                edge.target,
            );
        }
    }
}

#[test]
fn should_compile_every_gate_predicate_and_edge_condition_via_the_dsl() {
    // A `gate` prompt or edge condition that looks plausible but does not
    // actually parse (e.g. plain English, missing quotes) silently degrades:
    // GateHandler returns an Err for a bad prompt, and `next_edge` bubbles
    // the parse error up, so a doc artifact that ships unparseable
    // predicates is a runtime time-bomb. This test compiles every predicate
    // through the real DSL.

    let src = std::fs::read_to_string(DOT_PATH).unwrap_or_else(|e| panic!("read {DOT_PATH}: {e}"));
    let graph = parse_dot(&src).expect("parse_dot must accept the example");

    for node in graph.nodes.values() {
        if !matches!(node.handler, HandlerKind::Gate) {
            continue;
        }
        let prompt = node.prompt.as_deref().expect("gate prompt asserted above");
        condition::parse_condition(prompt).unwrap_or_else(|e| {
            panic!(
                "gate {} prompt did not compile via parse_condition: {} (source: {})",
                node.id, e, prompt,
            )
        });
    }

    for edge in &graph.edges {
        let Some(cond) = edge.condition.as_deref() else {
            continue;
        };
        condition::parse_condition(cond).unwrap_or_else(|e| {
            panic!(
                "edge {} -> {} condition did not compile: {} (source: {})",
                edge.source, edge.target, e, cond,
            )
        });
    }
}
