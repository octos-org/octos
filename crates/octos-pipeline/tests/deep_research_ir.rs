//! `deep_research` rebuilt as a typed-IR program.
//!
//! Proves the extended palette (Fanout gains web tools + an optional
//! `plan_prompt`; Synthesize gains `write_file`) can express the real bundled
//! `deep_research.dot` workflow — fan-out parallel web search → analyze /
//! cross-reference → synthesize + save — as a closed, capability-locked IR
//! program an LLM (or operator) composes. Capability is taken from
//! `contract_for`, never from the IR.

use octos_pipeline::compose::compose_l2;
use octos_pipeline::graph::HandlerKind;
use std::sync::Arc;

const DEEP_RESEARCH_IR: &str = r#"{
  "id": "deep_research_ir",
  "label": "Deep Research (IR)",
  "nodes": [
    {
      "id": "search",
      "kind": {
        "type": "fanout",
        "plan_prompt": "Generate 4-6 distinct research search angles for the user's query — cover the core topic, alternatives/comparisons, technical detail, and recent developments; do NOT just rephrase the same query. Respond with ONLY a JSON array of {\"task\",\"label\"} objects, each task under 80 chars.",
        "worker_prompt": "You are a research search specialist.\n\n{task}\n\nUse web_search to find the most relevant, authoritative sources, then web_fetch/read_file to read at least 3 of them. Include ALL URLs, specific data points, and quotes in your output.",
        "converge": "analyze",
        "max_tasks": 6
      }
    },
    {
      "id": "analyze",
      "kind": {
        "type": "synthesize",
        "prompt": "You are a research analyst. Cross-reference the search results from all angles: identify the key facts and data, note where sources agree or disagree, organize findings by subtopic, and rate source credibility. Preserve ALL specific numbers, dates, names, quotes, and URLs."
      }
    },
    {
      "id": "synthesize",
      "kind": {
        "type": "report",
        "prompt": "You are a research synthesis expert. Produce a structured report: Executive Summary, Key Findings by topic, Detailed Analysis with [title](url) citations, Areas of Uncertainty, and Conclusions. Include specific numbers/dates/quotes. SAVE the final report with write_file, then return a concise executive summary as text."
      }
    }
  ],
  "edges": [
    {"source": "search", "target": "analyze"},
    {"source": "analyze", "target": "synthesize"}
  ]
}"#;

#[test]
fn deep_research_ir_composes_to_a_dynamic_web_research_pipeline() {
    let graph = compose_l2(DEEP_RESEARCH_IR).unwrap_or_else(|e| {
        panic!(
            "deep_research IR must compose:\n{}",
            e.feedback_lines().join("\n")
        )
    });

    assert_eq!(graph.nodes.len(), 3, "plan-search / analyze / synthesize");

    // Fan-out search: dynamic_parallel, web tools, planner + worker prompts,
    // converging into analyze. Capability comes from the contract, not the IR.
    let search = graph.nodes.get("search").expect("search node");
    assert_eq!(search.handler, HandlerKind::DynamicParallel);
    for tool in ["web_search", "web_fetch", "read_file"] {
        assert!(
            search.tools.iter().any(|t| t == tool),
            "fan-out worker must be allowed {tool}; got {:?}",
            search.tools
        );
    }
    assert!(
        !search.tools.iter().any(|t| t == "shell"),
        "no shell in the contract"
    );
    assert_eq!(search.converge.as_deref(), Some("analyze"));
    assert!(search.worker_prompt.is_some(), "worker prompt set");
    assert!(
        search.prompt.is_some(),
        "planner prompt threaded from plan_prompt"
    );
    assert_eq!(search.model.as_deref(), Some("cheap"));

    // Synthesis: strong model, may SAVE its report (write_file from the
    // extended Synthesize contract).
    let synth = graph.nodes.get("synthesize").expect("synthesize node");
    assert_eq!(synth.handler, HandlerKind::Codergen);
    assert!(
        synth.tools.iter().any(|t| t == "write_file"),
        "synthesize must be allowed write_file; got {:?}",
        synth.tools
    );
    assert_eq!(synth.model.as_deref(), Some("strong"));

    // Flow: search -> analyze -> synthesize.
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.source == "search" && e.target == "analyze")
    );
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.source == "analyze" && e.target == "synthesize")
    );
}

/// Real-LLM end-to-end run of the deep_research IR — `#[ignore]` (needs a live
/// key; a web-search backend if you want real sources, else workers fall back
/// to model knowledge). Runs the COMPOSED IR through the same executor path the
/// bundled DOT uses, proving the rebuild runs, not just composes. Mirrors the
/// DAG real-LLM check.
///
/// Run:
///   DEEPSEEK_API_KEY=... cargo test -p octos-pipeline --test deep_research_ir \
///       -- --ignored --nocapture
#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY (real LLM); optional web-search backend"]
async fn deep_research_ir_runs_end_to_end() {
    use octos_llm::LlmProvider;
    use octos_llm::openai::OpenAIProvider;
    use octos_memory::EpisodeStore;
    use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
    use octos_pipeline::profile::ValidationProfile;

    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY required");
    let dir = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let config = ExecutorConfig {
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        default_provider: Arc::new(
            OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"),
        ) as Arc<dyn LlmProvider>,
        provider_router: None,
        memory,
        working_dir: dir.path().to_path_buf(),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 4,
        max_pipeline_fanout_total: Some(8),
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: octos_pipeline::context::PipelineContext::default(),
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
    };
    let exec = PipelineExecutor::new(config);

    let result = exec
        .run_ir(
            DEEP_RESEARCH_IR,
            &ValidationProfile::l2_default(),
            "The economic and environmental impacts of urban vertical farming.",
            &serde_json::Map::new(),
        )
        .await
        .expect("deep_research IR must run");

    let ran: Vec<String> = result
        .node_summaries
        .iter()
        .map(|s| s.node_id.clone())
        .collect();
    eprintln!("=== nodes ran: {ran:?}");
    eprintln!(
        "=== tokens: {}+{}",
        result.token_usage.input_tokens, result.token_usage.output_tokens
    );
    eprintln!("=== report:\n{}\n===", result.output);

    assert!(ran.iter().any(|n| n == "search"), "fan-out search must run");
    assert!(ran.iter().any(|n| n == "synthesize"), "synthesis must run");
    assert!(
        result.output.len() > 200,
        "must produce a non-trivial report"
    );
}
