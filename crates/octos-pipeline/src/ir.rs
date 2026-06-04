//! Typed intermediate representation (IR) for L2, LLM-authored pipelines.
//!
//! Instead of emitting raw DOT text, an LLM emits a [`PipelineIr`] (as JSON)
//! that names palette node *kinds* from a fixed, closed set. The IR carries no
//! `handler` / `tools` / `model` fields: a node's capability is resolved from a
//! code-owned palette table ([`contract_for`]) at compile time, so the LLM can
//! never widen what a node is allowed to do. [`compile`] lowers the IR directly
//! into a [`PipelineGraph`] — never to DOT text — so the lossy DOT parser is
//! never re-entered on the L2 path.
//!
//! Safety properties:
//! * A closed, `type`-tagged enum makes an unknown kind (e.g. `"shell"`) a
//!   deserialize error — capability-escalating kinds are *unrepresentable*.
//! * The compiler sets `handler` / `tools` / `model` exclusively from the
//!   palette contract, so even a hand-crafted IR cannot select an effectful
//!   handler or widen the tool set.
//! * Every contract uses an explicit, non-empty tool list (an empty
//!   `PipelineNode.tools` means "all builtins"), so no palette node falls back
//!   to the broad default surface.

use std::collections::HashMap;

use eyre::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::graph::{HandlerKind, PipelineEdge, PipelineGraph, PipelineNode};

/// A whole L2 program, authored by an LLM as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineIr {
    /// Graph identifier.
    pub id: String,
    /// Optional human-readable label.
    #[serde(default)]
    pub label: Option<String>,
    /// Palette nodes.
    pub nodes: Vec<IrNode>,
    /// Directed edges. Routing conditions live here, not on nodes.
    #[serde(default)]
    pub edges: Vec<IrEdge>,
}

/// One node: an id plus a palette `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrNode {
    pub id: String,
    pub kind: IrNodeKind,
}

/// The fixed palette. The LLM may ONLY name these kinds. There are deliberately
/// no `handler`, `tools`, or raw `model` fields anywhere — capability is owned
/// by [`contract_for`], not by the IR.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum IrNodeKind {
    /// Read-only research (web + file reads), cheap model.
    Research {
        prompt: String,
        #[serde(default)]
        max_output_tokens: Option<u32>,
    },
    /// Pure transform over prior output, cheap model.
    Transform { prompt: String },
    /// Final synthesis, strong model.
    Synthesize {
        prompt: String,
        #[serde(default)]
        max_output_tokens: Option<u32>,
    },
    /// Pure routing gate (no LLM); outgoing edge conditions decide the branch.
    Gate {},
    /// Dynamic fan-out: plan N worker tasks, run them in parallel, converge.
    Fanout {
        worker_prompt: String,
        converge: String,
        #[serde(default)]
        max_tasks: Option<u32>,
    },
    // NOTE: a `human_gate` kind was intentionally NOT included — the pipeline
    // executor does not route human-input gates through a real approval
    // handler (a bare Gate node defaults its condition to `true` and
    // auto-passes), so advertising one would be a silent approval bypass.
    // Re-add only once a HumanInputProvider-backed handler is wired.
}

/// One directed edge. `condition` is an expression evaluated for routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrEdge {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub condition: Option<String>,
    /// Marks this edge as an intentional retry/guard loop-back. A cycle that
    /// routes through a `back_edge` is permitted by the engine's cycle rule;
    /// an unmarked cycle is rejected. This is the first-class way to express
    /// the retry/revision loops LLMs naturally author (replacing the engine's
    /// magic `retry`/`back_edge` keyword-in-label convention).
    #[serde(default)]
    pub back_edge: bool,
}

/// Fixed, code-owned capability contract for a palette kind. The LLM never sees
/// or sets any of these — they are looked up by kind at compile time.
pub struct PaletteContract {
    pub handler: HandlerKind,
    /// Explicit, never-empty tool allowlist (empty would mean "all builtins").
    pub allowed_tools: &'static [&'static str],
    pub model: Option<&'static str>,
}

/// Resolve the capability contract for a palette kind. `shell` is never
/// reachable; tool lists are explicit and non-effectful.
pub fn contract_for(kind: &IrNodeKind) -> PaletteContract {
    match kind {
        IrNodeKind::Research { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            allowed_tools: &["web_search", "web_fetch", "read_file"],
            model: Some("cheap"),
        },
        IrNodeKind::Transform { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            allowed_tools: &["read_file"],
            model: Some("cheap"),
        },
        IrNodeKind::Synthesize { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            allowed_tools: &["read_file"],
            model: Some("strong"),
        },
        IrNodeKind::Gate {} => PaletteContract {
            handler: HandlerKind::Gate,
            allowed_tools: &[],
            model: None,
        },
        IrNodeKind::Fanout { .. } => PaletteContract {
            handler: HandlerKind::DynamicParallel,
            allowed_tools: &["read_file"],
            model: Some("cheap"),
        },
    }
}

/// Lower a typed IR program into an executable [`PipelineGraph`].
///
/// Capability (`handler` / `tools` / `model`) is taken EXCLUSIVELY from
/// [`contract_for`]; nothing capability-bearing is read from the IR.
pub fn compile(ir: &PipelineIr) -> Result<PipelineGraph> {
    let mut nodes: HashMap<String, PipelineNode> = HashMap::new();
    for n in &ir.nodes {
        if nodes.contains_key(&n.id) {
            bail!("duplicate node id '{}'", n.id);
        }
        nodes.insert(n.id.clone(), compile_node(n));
    }

    let mut edges = Vec::with_capacity(ir.edges.len());
    for e in &ir.edges {
        if !nodes.contains_key(&e.source) {
            bail!("edge source '{}' is not a declared node", e.source);
        }
        if !nodes.contains_key(&e.target) {
            bail!("edge target '{}' is not a declared node", e.target);
        }
        edges.push(PipelineEdge {
            source: e.source.clone(),
            target: e.target.clone(),
            // A back_edge is marked with the engine-recognized "back_edge"
            // label so the cycle rule permits the loop.
            label: e.back_edge.then(|| "back_edge".to_string()),
            condition: e.condition.clone(),
            weight: 1.0,
        });
    }

    Ok(PipelineGraph {
        id: ir.id.clone(),
        label: ir.label.clone(),
        default_model: None,
        max_total_tokens: None,
        default_timeout_secs: None,
        nodes,
        edges,
        subgraphs: Vec::new(),
    })
}

fn compile_node(n: &IrNode) -> PipelineNode {
    let contract = contract_for(&n.kind);
    let mut node = PipelineNode {
        id: n.id.clone(),
        handler: contract.handler.clone(),
        // capability locked from the contract, NEVER from the IR:
        tools: contract
            .allowed_tools
            .iter()
            .map(|s| s.to_string())
            .collect(),
        model: contract.model.map(|s| s.to_string()),
        ..PipelineNode::default()
    };
    match &n.kind {
        IrNodeKind::Research {
            prompt,
            max_output_tokens,
        }
        | IrNodeKind::Synthesize {
            prompt,
            max_output_tokens,
        } => {
            node.prompt = Some(prompt.clone());
            node.max_output_tokens = *max_output_tokens;
        }
        IrNodeKind::Transform { prompt } => {
            node.prompt = Some(prompt.clone());
        }
        IrNodeKind::Gate {} => {}
        IrNodeKind::Fanout {
            worker_prompt,
            converge,
            max_tasks,
        } => {
            node.worker_prompt = Some(worker_prompt.clone());
            node.converge = Some(converge.clone());
            node.max_tasks = *max_tasks;
        }
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> PipelineIr {
        serde_json::from_str(json).expect("valid IR")
    }

    #[test]
    fn should_reject_unknown_node_kind() {
        // "shell" is not a palette kind -> deserialize error (unrepresentable).
        let json = r#"{"id":"p","nodes":[{"id":"n","kind":{"type":"shell"}}]}"#;
        assert!(serde_json::from_str::<PipelineIr>(json).is_err());
    }

    #[test]
    fn should_deserialize_and_compile_full_palette() {
        let json = r#"{
            "id":"demo",
            "nodes":[
                {"id":"r","kind":{"type":"research","prompt":"find X"}},
                {"id":"t","kind":{"type":"transform","prompt":"clean"}},
                {"id":"g","kind":{"type":"gate"}},
                {"id":"f","kind":{"type":"fanout","worker_prompt":"do {task}","converge":"s"}},
                {"id":"s","kind":{"type":"synthesize","prompt":"write"}}
            ],
            "edges":[{"source":"r","target":"s"}]
        }"#;
        let ir = parse(json);
        assert_eq!(ir.nodes.len(), 5);
        let g = compile(&ir).expect("compiles");
        assert_eq!(g.nodes.len(), 5);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn should_never_compile_to_shell_handler() {
        let kinds = [
            r#"{"type":"research","prompt":"x"}"#,
            r#"{"type":"transform","prompt":"x"}"#,
            r#"{"type":"synthesize","prompt":"x"}"#,
            r#"{"type":"gate"}"#,
            r#"{"type":"fanout","worker_prompt":"x","converge":"c"}"#,
        ];
        for k in kinds {
            let json = format!(r#"{{"id":"p","nodes":[{{"id":"n","kind":{k}}}]}}"#);
            let ir = parse(&json);
            let g = compile(&ir).expect("compiles");
            for node in g.nodes.values() {
                assert_ne!(node.handler, HandlerKind::Shell, "kind {k} -> Shell");
            }
        }
    }

    #[test]
    fn should_lock_tools_from_contract_not_ir() {
        let json = r#"{"id":"p","nodes":[{"id":"r","kind":{"type":"research","prompt":"x"}}]}"#;
        let g = compile(&parse(json)).unwrap();
        let r = &g.nodes["r"];
        assert_eq!(r.handler, HandlerKind::Codergen);
        assert_eq!(r.tools, vec!["web_search", "web_fetch", "read_file"]);
        assert_eq!(r.model.as_deref(), Some("cheap"));
        assert!(!r.tools.is_empty(), "empty tools would mean all builtins");
    }

    #[test]
    fn should_lock_capability_even_if_ir_smuggles_a_tools_field() {
        // A research node trying to smuggle `tools:["shell"]`. Internally-tagged
        // enums + deny_unknown_fields may or may not reject the extra key; either
        // way the compiler never reads `tools` from the IR, so capability stays
        // locked to the contract.
        let json = r#"{"id":"p","nodes":[{"id":"n","kind":{"type":"research","prompt":"x","tools":["shell"]}}]}"#;
        if let Ok(ir) = serde_json::from_str::<PipelineIr>(json) {
            let g = compile(&ir).unwrap();
            assert!(!g.nodes["n"].tools.iter().any(|t| t == "shell"));
        }
        // (Rejection at deserialize time is strictly better and also acceptable.)
    }

    #[test]
    fn should_reject_dangling_edge() {
        let json = r#"{"id":"p","nodes":[{"id":"a","kind":{"type":"gate"}}],"edges":[{"source":"a","target":"ghost"}]}"#;
        assert!(compile(&parse(json)).is_err());
    }

    #[test]
    fn should_reject_duplicate_node_id() {
        let json = r#"{"id":"p","nodes":[{"id":"a","kind":{"type":"gate"}},{"id":"a","kind":{"type":"gate"}}]}"#;
        assert!(compile(&parse(json)).is_err());
    }

    #[test]
    fn should_mark_back_edge_label_on_compile() {
        let json = r#"{"id":"p","nodes":[{"id":"a","kind":{"type":"gate"}},{"id":"b","kind":{"type":"gate"}}],
            "edges":[{"source":"a","target":"b"},{"source":"b","target":"a","back_edge":true}]}"#;
        let g = compile(&parse(json)).unwrap();
        let back = g.edges.iter().find(|e| e.source == "b").unwrap();
        assert_eq!(back.label.as_deref(), Some("back_edge"));
        let fwd = g.edges.iter().find(|e| e.source == "a").unwrap();
        assert_eq!(fwd.label, None);
    }
}
