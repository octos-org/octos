//! One-shot: compose a typed-IR workflow and EXECUTE it against a live model
//! (provider from `DEEPSEEK_API_KEY`, OpenAI-compatible), reusing the same
//! executor + handlers the production `RunPipelineTool` builds.
//!
//! Usage: `run_ir_cli <ir.json> "<input/topic>"`

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use octos_llm::{LlmProvider, openai::OpenAIProvider};
use octos_memory::EpisodeStore;
use octos_pipeline::context::PipelineContext;
use octos_pipeline::executor::{ExecutorConfig, PipelineExecutor};
use octos_pipeline::host_context::PipelineHostContext;
use octos_pipeline::profile::ValidationProfile;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let ir_path = args.next().expect("usage: run_ir_cli <ir.json> <input>");
    let input = args.next().unwrap_or_default();
    let ir_json = std::fs::read_to_string(&ir_path).expect("read ir file");

    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
    let provider: Arc<dyn LlmProvider> = Arc::new(
        OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"),
    );

    let mem_dir = std::env::temp_dir().join("run_ir_cli_episodes");
    let _ = std::fs::create_dir_all(&mem_dir);
    let memory = Arc::new(
        EpisodeStore::open_or_degraded(&mem_dir)
            .await
            .expect("episode store"),
    );

    let config = ExecutorConfig {
        guards: Vec::new(),
        max_concurrent_llm_calls: None,
        default_provider: provider,
        provider_router: None,
        memory,
        working_dir: PathBuf::from("/tmp"),
        provider_policy: None,
        plugin_dirs: vec![],
        plugin_require_signed: false,
        status_bridge: None,
        shutdown: Arc::new(AtomicBool::new(false)),
        max_parallel_workers: 8,
        max_pipeline_fanout_total: None,
        checkpoint_store: None,
        hook_executor: None,
        workspace_context: PipelineContext::default(),
        host_context: PipelineHostContext::default(),
        embedder: None,
        catalog_dir: None,
        // #1607: pipeline validators run under a no-op sandbox in tests
        // (host-independent — command validators run the argv directly).
        sandbox: octos_agent::SandboxConfig::default(),
    };
    let executor = PipelineExecutor::new(config);

    match executor
        .run_ir(
            &ir_json,
            &ValidationProfile::l2_default(),
            &input,
            &serde_json::Map::new(),
        )
        .await
    {
        Ok(r) => {
            eprintln!(
                "=== success={} nodes_run={} ===",
                r.success,
                r.node_summaries.len()
            );
            for n in &r.node_summaries {
                eprintln!(
                    "  node {:<12} {} {}ms {}+{} tok",
                    n.node_id,
                    if n.success { "OK  " } else { "FAIL" },
                    n.duration_ms,
                    n.token_usage.input_tokens,
                    n.token_usage.output_tokens,
                );
            }
            println!("{}", r.output);
        }
        Err(e) => {
            eprintln!("=== ERROR ===\n{e:?}");
            std::process::exit(1);
        }
    }
}
