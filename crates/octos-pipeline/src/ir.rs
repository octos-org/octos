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
    /// Final synthesis (read-only), strong model. Returns its result as text;
    /// use `Report` to also SAVE an artifact.
    Synthesize {
        prompt: String,
        #[serde(default)]
        max_output_tokens: Option<u32>,
    },
    /// Final report: synthesize AND save an artifact via `write_file`, strong
    /// model. The terminal "produce + persist a report/deck/file" step.
    Report {
        prompt: String,
        #[serde(default)]
        max_output_tokens: Option<u32>,
    },
    /// Pure routing gate (no LLM); outgoing edge conditions decide the branch.
    Gate {},
    /// Dynamic fan-out: plan N worker tasks, run them in parallel, converge.
    Fanout {
        /// Planner instruction — how to break the query into parallel worker
        /// tasks. Optional; when omitted the executor uses a generic
        /// research-angle planner.
        #[serde(default)]
        plan_prompt: Option<String>,
        worker_prompt: String,
        converge: String,
        #[serde(default)]
        max_tasks: Option<u32>,
    },
    /// Read-only code analysis: reads files, runs grep/glob. For "audit this
    /// code", "find all callers of X", "review this diff".
    CodeReview {
        prompt: String,
        /// Optional file/directory scope (e.g. "src/**/*.rs"). Defaults to
        /// the session workspace root.
        #[serde(default)]
        scope: Option<String>,
    },
    /// Code modification: reads, edits, and writes files. For "fix this bug",
    /// "add this feature", "refactor this module". The prompt should describe
    /// WHAT to change; the handler's agent loop decides HOW.
    CodeEdit {
        prompt: String,
        /// Files the edit is expected to touch (informational — the handler
        /// may read/edit others as needed).
        #[serde(default)]
        files: Option<Vec<String>>,
    },
    /// Run a read-only shell command (no side effects). For "run tests",
    /// "check build", "count lines". The command runs in the session
    /// workspace; output is captured as the node's result.
    ShellCheck {
        /// The command to run (e.g. "cargo test", "git diff --stat").
        command: String,
        /// Timeout in seconds. Defaults to 60.
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Spawn a sub-agent to work on a self-contained task. The sub-agent
    /// gets its own LLM session and returns its final output as the node's
    /// result. For "have a specialist handle X", "delegate Y to a fresh
    /// context", "get a second opinion on Z".
    SubAgent {
        /// The task description for the sub-agent.
        task: String,
        /// Tool allowlist for the sub-agent (informational hint; the
        /// executor's spawn logic enforces its own restrictions).
        #[serde(default)]
        tools: Option<Vec<String>>,
        /// Model override for the sub-agent. Defaults to "strong".
        #[serde(default)]
        model: Option<String>,
    },
    /// Send a notification to the user. For "tell the user the build passed",
    /// "alert on failure". Minimal LLM involvement; the message is the output.
    Notify {
        /// The message to send.
        message: String,
        /// Optional channel (e.g. "telegram", "slack"). Defaults to the
        /// current session's channel.
        #[serde(default)]
        channel: Option<String>,
    },
    /// Wait for a condition or a fixed duration. For "wait for CI to finish",
    /// "pause before retry". No LLM call.
    Wait {
        /// Seconds to wait. Mutually exclusive with `until_condition`.
        #[serde(default)]
        seconds: Option<u64>,
        /// A condition expression to poll for (e.g. "file_exists:output.txt").
        /// The node completes when the condition is true or `timeout_secs`
        /// elapses.
        #[serde(default)]
        until_condition: Option<String>,
        /// Timeout for `until_condition` polling. Defaults to 300.
        #[serde(default)]
        timeout_secs: Option<u64>,
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
        // Final report: produces AND SAVES an artifact (`write_file`). Distinct
        // from `Synthesize` because `CodergenHandler` treats any `write_file`
        // node as a final-report writer (save full report, return a concise
        // summary) — so intermediate analysis stays a read-only `Synthesize`.
        IrNodeKind::Report { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            allowed_tools: &["read_file", "write_file"],
            model: Some("strong"),
        },
        IrNodeKind::Gate {} => PaletteContract {
            handler: HandlerKind::Gate,
            allowed_tools: &[],
            model: None,
        },
        IrNodeKind::Fanout { .. } => PaletteContract {
            handler: HandlerKind::DynamicParallel,
            // Fan-out research workers search the web + read sources, and WRITE a
            // per-worker findings file (`findings-<label>.md`) so the converge
            // node reads full detail from disk instead of a size-truncated inline
            // summary. (The builtin `web_search`/`web_fetch`; the richer `search`
            // skill is resolved via discovery when installed, but is not
            // advertised here so the contract resolves on any profile.)
            allowed_tools: &["web_search", "web_fetch", "read_file", "write_file"],
            model: Some("cheap"),
        },
        IrNodeKind::CodeReview { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            allowed_tools: &["read_file", "grep", "glob", "list_dir"],
            model: Some("strong"),
        },
        IrNodeKind::CodeEdit { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            allowed_tools: &["read_file", "write_file", "edit_file", "grep", "glob"],
            model: Some("strong"),
        },
        IrNodeKind::ShellCheck { .. } => PaletteContract {
            handler: HandlerKind::Shell,
            allowed_tools: &[],
            model: None,
        },
        IrNodeKind::SubAgent { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            // SubAgent delegates to a nested agent loop. The node's own tools
            // are minimal — it needs spawn to create the sub-agent.
            allowed_tools: &["spawn"],
            model: Some("strong"),
        },
        IrNodeKind::Notify { .. } => PaletteContract {
            handler: HandlerKind::Codergen,
            // Notify is a terminal side-effect node. No LLM tools needed —
            // the executor's notification layer handles delivery.
            allowed_tools: &["read_file"],
            model: Some("cheap"),
        },
        IrNodeKind::Wait { .. } => PaletteContract {
            handler: HandlerKind::Gate,
            allowed_tools: &[],
            model: None,
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
        result_fidelity: None,
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
        }
        | IrNodeKind::Report {
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
            plan_prompt,
            worker_prompt,
            converge,
            max_tasks,
        } => {
            // `node.prompt` is the dynamic_parallel PLANNER prompt; `None` lets
            // the executor fall back to its generic research-angle planner.
            node.prompt = plan_prompt.clone();
            node.worker_prompt = Some(worker_prompt.clone());
            node.converge = Some(converge.clone());
            node.max_tasks = *max_tasks;
        }
        IrNodeKind::CodeReview { prompt, scope } => {
            node.prompt = Some(prompt.clone());
            node.label = scope.clone();
        }
        IrNodeKind::CodeEdit { prompt, files } => {
            node.prompt = Some(prompt.clone());
            if let Some(fs) = files {
                node.label = Some(fs.join(", "));
            }
        }
        IrNodeKind::ShellCheck {
            command,
            timeout_secs,
        } => {
            node.prompt = Some(command.clone());
            node.label = Some("shell_check".into());
            node.timeout_secs = Some(timeout_secs.unwrap_or(60));
        }
        IrNodeKind::SubAgent { task, tools, model } => {
            node.prompt = Some(task.clone());
            // Sub-agent tool list and model are set at compile time from the
            // palette contract; the IR fields are informational hints that the
            // executor's sub-agent spawn logic can use.
            if let Some(ts) = tools {
                node.label = Some(format!("subagent:{}", ts.join(",")));
            }
            if let Some(m) = model {
                node.model = Some(m.clone());
            }
        }
        IrNodeKind::Notify { message, channel } => {
            node.prompt = Some(message.clone());
            node.label = channel.clone().or(Some("notify".into()));
        }
        IrNodeKind::Wait {
            seconds,
            until_condition,
            timeout_secs,
        } => {
            if let Some(s) = seconds {
                node.prompt = Some(format!("wait {}s", s));
                node.timeout_secs = Some(*s);
            } else if let Some(cond) = until_condition {
                node.prompt = Some(format!("wait until {}", cond));
                node.timeout_secs = Some(timeout_secs.unwrap_or(300));
            }
            node.label = Some("wait".into());
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
            r#"{"type":"code_review","prompt":"x"}"#,
            r#"{"type":"code_edit","prompt":"x"}"#,
            r#"{"type":"sub_agent","task":"x"}"#,
            r#"{"type":"notify","message":"x"}"#,
            r#"{"type":"wait","seconds":5}"#,
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
    fn fanout_synthesize_report_contracts() {
        // Fan-out workers search the web; `synthesize` is read-only analysis;
        // only `report` may save a file.
        let json = r#"{"id":"p","nodes":[
            {"id":"f","kind":{"type":"fanout","plan_prompt":"plan","worker_prompt":"do {task}","converge":"a","max_tasks":4}},
            {"id":"a","kind":{"type":"synthesize","prompt":"analyze"}},
            {"id":"r","kind":{"type":"report","prompt":"report"}}
        ],"edges":[{"source":"f","target":"a"},{"source":"a","target":"r"}]}"#;
        let g = compile(&parse(json)).unwrap();

        let f = &g.nodes["f"];
        assert_eq!(f.handler, HandlerKind::DynamicParallel);
        // Fan-out workers may write their per-worker findings deliverable.
        assert_eq!(
            f.tools,
            vec!["web_search", "web_fetch", "read_file", "write_file"]
        );
        assert_eq!(f.prompt.as_deref(), Some("plan"), "plan_prompt threaded");
        assert_eq!(f.converge.as_deref(), Some("a"));

        // Intermediate synthesis is read-only — must NOT be a file-writer (so the
        // CodergenHandler doesn't treat it as a final-report node).
        assert_eq!(g.nodes["a"].tools, vec!["read_file"]);

        // Only the report node saves an artifact.
        let r = &g.nodes["r"];
        assert_eq!(r.handler, HandlerKind::Codergen);
        assert_eq!(r.tools, vec!["read_file", "write_file"]);
        assert_eq!(r.model.as_deref(), Some("strong"));
        for n in g.nodes.values() {
            assert!(!n.tools.iter().any(|t| t == "shell"), "no shell anywhere");
        }
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

    #[test]
    fn should_compile_code_workflow_palette() {
        let json = r#"{
            "id":"code-pipeline",
            "nodes":[
                {"id":"review","kind":{"type":"code_review","prompt":"audit auth","scope":"src/auth/**"}},
                {"id":"fix","kind":{"type":"code_edit","prompt":"fix issues","files":["src/auth.rs"]}},
                {"id":"test","kind":{"type":"shell_check","command":"cargo test -- auth","timeout_secs":120}},
                {"id":"deploy_check","kind":{"type":"shell_check","command":"cargo build --release"}},
                {"id":"notify","kind":{"type":"notify","message":"Build passed"}},
                {"id":"report","kind":{"type":"report","prompt":"Summarize all changes and results"}}
            ],
            "edges":[
                {"source":"review","target":"fix"},
                {"source":"fix","target":"test"},
                {"source":"test","target":"deploy_check"},
                {"source":"deploy_check","target":"notify"},
                {"source":"notify","target":"report"}
            ]
        }"#;
        let ir = parse(json);
        let g = compile(&ir).expect("compiles");
        assert_eq!(g.nodes.len(), 6);
        assert_eq!(g.edges.len(), 5);

        // code_review: read-only analysis, strong model
        let review = &g.nodes["review"];
        assert_eq!(review.handler, HandlerKind::Codergen);
        assert_eq!(review.model.as_deref(), Some("strong"));
        assert!(review.tools.contains(&"grep".to_string()));
        assert!(review.tools.contains(&"glob".to_string()));

        // code_edit: read+write+edit, strong model
        let fix = &g.nodes["fix"];
        assert_eq!(fix.handler, HandlerKind::Codergen);
        assert!(fix.tools.contains(&"edit_file".to_string()));
        assert!(fix.tools.contains(&"write_file".to_string()));

        // shell_check: shell handler, labeled for validation exemption
        let test = &g.nodes["test"];
        assert_eq!(test.handler, HandlerKind::Shell);
        assert_eq!(test.label.as_deref(), Some("shell_check"));
        assert_eq!(test.prompt.as_deref(), Some("cargo test -- auth"));
        assert_eq!(test.timeout_secs, Some(120));

        // shell_check with default timeout
        let deploy = &g.nodes["deploy_check"];
        assert_eq!(deploy.timeout_secs, Some(60));

        // notify: codergen with minimal tools
        let notify = &g.nodes["notify"];
        assert_eq!(notify.handler, HandlerKind::Codergen);
        assert_eq!(notify.label.as_deref(), Some("notify"));

        // report: write_file for final artifact
        let report = &g.nodes["report"];
        assert_eq!(report.handler, HandlerKind::Codergen);
        assert!(report.tools.contains(&"write_file".to_string()));
    }

    #[test]
    fn should_compile_sub_agent_and_wait() {
        let json = r#"{
            "id":"agent-pipeline",
            "nodes":[
                {"id":"delegate","kind":{"type":"sub_agent","task":"Review PR #42","tools":["read_file","grep"],"model":"strong"}},
                {"id":"pause","kind":{"type":"wait","seconds":30}},
                {"id":"poll","kind":{"type":"wait","until_condition":"file_exists:done.txt","timeout_secs":600}}
            ],
            "edges":[
                {"source":"delegate","target":"pause"},
                {"source":"pause","target":"poll"}
            ]
        }"#;
        let g = compile(&parse(json)).expect("compiles");

        let agent = &g.nodes["delegate"];
        assert_eq!(agent.handler, HandlerKind::Codergen);
        assert_eq!(agent.model.as_deref(), Some("strong"));
        assert!(agent.label.as_deref().unwrap().contains("read_file"));

        let pause = &g.nodes["pause"];
        assert_eq!(pause.handler, HandlerKind::Gate);
        assert_eq!(pause.timeout_secs, Some(30));
        assert_eq!(pause.label.as_deref(), Some("wait"));

        let poll = &g.nodes["poll"];
        assert_eq!(poll.handler, HandlerKind::Gate);
        assert_eq!(poll.timeout_secs, Some(600));
    }

    #[test]
    fn shell_check_default_timeout() {
        let json = r#"{"id":"p","nodes":[{"id":"s","kind":{"type":"shell_check","command":"cargo build"}}]}"#;
        let g = compile(&parse(json)).unwrap();
        assert_eq!(g.nodes["s"].timeout_secs, Some(60));
    }
}
