//! Real-LLM validation of the ready-set DAG scheduler.
//!
//! A DAG-schedulable "deep research" shape — `plan → {angle_a, angle_b} →
//! synthesize` — run through the REAL `CodergenHandler` against a live model.
//! Each angle ends with a unique marker; a correct DAG JOIN puts BOTH markers
//! in synthesize's input, so the final report must contain both. On the
//! single-path walk only one angle runs, so only one marker survives — the
//! real-LLM contrast that proves the fix.
//!
//! `#[ignore]` (needs a live key + network). Run:
//!   DEEPSEEK_API_KEY=... cargo test -p octos-pipeline --test dag_real_llm \
//!       -- --ignored --nocapture

use std::sync::Arc;

use octos_llm::LlmProvider;
use octos_llm::openai::OpenAIProvider;
use octos_memory::EpisodeStore;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use tempfile::TempDir;

const TOPIC: &str = "The economic and environmental impacts of urban vertical farming.";

// Text-only nodes (`tools=""` = deny-all sentinel) so the run is deterministic
// LLM reasoning with no tool-calling. `plan` fans out to two angles; both
// converge on `synthesize` — a heterogeneous diamond the single-path walk
// half-runs.
const RESEARCH_DIAMOND: &str = r#"digraph deep_research_dag {
    plan       [handler=codergen, tools="", prompt="You are a research planner. Restate the user's topic in one sentence and name two complementary angles to investigate."]
    angle_a    [handler=codergen, tools="", prompt="Research the FIRST angle of the topic in your input. Reply with two concise sentences of findings, then on a new line output exactly this token and nothing else: MARKER_ALPHA"]
    angle_b    [handler=codergen, tools="", prompt="Research the SECOND angle of the topic in your input. Reply with two concise sentences of findings, then on a new line output exactly this token and nothing else: MARKER_BRAVO"]
    synthesize [handler=codergen, tools="", prompt="You are a research synthesizer. Your input contains findings from two research angles, each ending in a MARKER_ token. Write a 3-4 sentence synthesis of the COMBINED findings. You MUST reproduce, verbatim, EVERY token beginning with MARKER_ that appears anywhere in your input."]
    plan -> angle_a
    plan -> angle_b
    angle_a -> synthesize
    angle_b -> synthesize
}"#;

fn deepseek() -> Arc<dyn LlmProvider> {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY required");
    Arc::new(OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"))
}

async fn make_executor(dir: &TempDir, dag: bool) -> PipelineExecutor {
    let memory = Arc::new(EpisodeStore::open(dir.path().join(".octos")).await.unwrap());
    let config = ExecutorConfig {
        default_provider: deepseek(),
        provider_router: None,
        memory,
        working_dir: dir.path().to_path_buf(),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        max_parallel_workers: 4,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: octos_pipeline::context::PipelineContext::default(),
        host_context: octos_pipeline::host_context::PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
    };
    PipelineExecutor::new(config).with_dag_scheduler(dag)
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY (real LLM + network)"]
async fn dag_deep_research_diamond_joins_both_angles() {
    let dir = TempDir::new().unwrap();
    let exec = make_executor(&dir, true).await;

    let result = exec
        .run(RESEARCH_DIAMOND, TOPIC, &serde_json::Map::new())
        .await
        .expect("DAG research pipeline run");

    let ran: Vec<String> = result
        .node_summaries
        .iter()
        .map(|s| s.node_id.clone())
        .collect();
    eprintln!("=== DAG nodes ran: {ran:?}");
    eprintln!(
        "=== DAG tokens: {}+{}",
        result.token_usage.input_tokens, result.token_usage.output_tokens
    );
    eprintln!("=== DAG synthesis output:\n{}\n===", result.output);

    assert!(result.success, "DAG pipeline failed: {}", result.output);
    for n in ["plan", "angle_a", "angle_b", "synthesize"] {
        assert!(
            ran.contains(&n.to_string()),
            "node {n} must run; ran={ran:?}"
        );
    }
    let out = result.output.to_uppercase();
    assert!(
        out.contains("MARKER_ALPHA"),
        "synthesis is missing MARKER_ALPHA — the angle_a branch did not reach the join:\n{}",
        result.output
    );
    assert!(
        out.contains("MARKER_BRAVO"),
        "synthesis is missing MARKER_BRAVO — the angle_b branch did not reach the join:\n{}",
        result.output
    );
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY (real LLM + network)"]
async fn single_path_half_runs_research_diamond() {
    // The same graph on the single-path walk: one angle is skipped, so the
    // synthesis joins only one branch — at most one marker survives. This is
    // the real-LLM proof of the latent bug the DAG scheduler fixes.
    let dir = TempDir::new().unwrap();
    let legacy = make_executor(&dir, false).await;

    let result = legacy
        .run(RESEARCH_DIAMOND, TOPIC, &serde_json::Map::new())
        .await
        .expect("single-path research pipeline run");

    let ran: Vec<String> = result
        .node_summaries
        .iter()
        .map(|s| s.node_id.clone())
        .collect();
    eprintln!("=== single-path nodes ran: {ran:?}");
    eprintln!("=== single-path output:\n{}\n===", result.output);

    assert!(
        ran.len() < 4,
        "single-path walk must skip a diamond branch; it ran all of {ran:?}"
    );
    let out = result.output.to_uppercase();
    assert!(
        !(out.contains("MARKER_ALPHA") && out.contains("MARKER_BRAVO")),
        "single-path walk should NOT have joined both angles, but both markers appeared:\n{}",
        result.output
    );
}
