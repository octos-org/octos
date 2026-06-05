//! Live deepseek tool-SELECTION probe for the "prefer run_pipeline for deep
//! research" steering change.
//!
//! The unit tests prove the *description strings* are well-formed; they cannot
//! prove the *behavioral* claim — that a real deepseek model, handed the actual
//! `run_pipeline` + `web_search` + `web_fetch` specs, now CHOOSES `run_pipeline`
//! for a deep-research prompt instead of inlining a single `web_search`. Tool
//! selection is only observable by asking a live model. (`feedback_replay_failing_history`,
//! `feedback_benchmark_doesnt_invalidate_production`.)
//!
//! This is an A/B: same model, same prompts, same competing tools — only the
//! `run_pipeline` description differs (OLD = pre-change permissive text, NEW =
//! current imperative text). It prints a per-prompt table and aggregate rates,
//! and asserts the NEW text routes deep-research prompts to `run_pipeline` at a
//! high absolute rate AND a higher rate than OLD, without over-triggering on
//! single-fact controls.
//!
//! Production deepseek runs with `OCTOS_PIPELINE_IR` OFF, so this probes the
//! NON-IR (`with_ir_enabled(false)`) description branch — the one prod sees.
//!
//! Run:
//!   DEEPSEEK_API_KEY=... cargo test -p octos-pipeline --test deepseek_routing_probe \
//!       -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::Arc;

use octos_agent::Tool;
use octos_agent::{WebFetchTool, WebSearchTool};
use octos_core::Message;
use octos_llm::openai::OpenAIProvider;
use octos_llm::{ChatConfig, LlmProvider, ToolChoice, ToolSpec};
use octos_memory::EpisodeStore;
use octos_pipeline::RunPipelineTool;

/// The pre-change NON-IR `run_pipeline` description (parent of the steering
/// commit). Hardcoded so the A/B runs in a single binary without rebuilding.
const OLD_NON_IR_DESCRIPTION: &str = "Run a sanctioned multi-step pipeline by NAME. The only currently \
     sanctioned pipeline is `deep_research`, which performs MULTI-SOURCE \
     WEB-RESEARCH SYNTHESIS: it fans out parallel web-search workers and \
     synthesizes a source-citing report. Use it ONLY when the user asks \
     for in-depth, multi-source research drawn from the open web. \
     deep_research MUST NOT be used for code review, local-codebase \
     analysis, debugging, or anything answerable from the files already \
     in the working directory — it has no access to your repository and \
     will fabricate or recall unrelated material. For those tasks do NOT \
     call run_pipeline at all; answer directly with the local tools \
     (`read_file`, `grep`, `glob`, `list_dir`, `shell`). Likewise do NOT \
     compose your own inline DOT graph for ad-hoc tasks (slides, media, \
     code edits, partial regenerations, etc.) — those have purpose-built \
     tools (`mofa_slides`, `podcast_generate`, etc.). If no purpose-built \
     tool exists for what the user asked, surface that as a limitation \
     rather than improvising a custom pipeline or force-fitting \
     deep_research.";

/// Neutral, production-shaped system prompt. Deliberately does NOT itself steer
/// toward `run_pipeline` — the whole point is to isolate the *tool description*
/// as the only routing signal.
const SYSTEM_PROMPT: &str = "You are a capable AI assistant operating an agent loop with tools. \
     For each user request, pick the single most appropriate tool to start with \
     (or answer directly if no tool is needed). Call exactly one tool when a tool fits.";

/// Deep / comprehensive / multi-source research requests — the steering target.
/// A well-steered model should open these with `run_pipeline` (deep_research).
const DEEP_RESEARCH_PROMPTS: &[&str] = &[
    "Do a deep research on the current state of solid-state battery commercialization and write me a report.",
    "Research and write a comprehensive, multi-source report on the economic and environmental impacts of urban vertical farming.",
    "I want an in-depth investigation of how RISC-V adoption is progressing in data centers, drawing on multiple sources.",
    "Thoroughly research the latest production approaches to retrieval-augmented generation and synthesize the findings into a report.",
    "深度调研一下中国新能源汽车出口的最新趋势和主要市场，并整理成一份带来源的报告。",
];

/// Borderline / ambiguous research-ish requests that do NOT contain the explicit
/// "deep research / comprehensive report / in-depth investigation" trigger words.
/// A model could reasonably answer these with one `web_search` OR escalate to the
/// pipeline. THIS is the class where steering moves the needle — explicit prompts
/// already route to the pipeline regardless. The change earns its keep here or
/// nowhere.
const BORDERLINE_PROMPTS: &[&str] = &[
    "What's the latest on small modular nuclear reactors?",
    "Tell me everything about the current EV fast-charging standards landscape.",
    "Give me a rundown of how different countries are regulating stablecoins.",
    "Compare the leading open-source vector databases and their tradeoffs.",
    "What's the state of play with on-device LLM inference these days?",
];

/// Single-fact / quick-lookup controls — the model should NOT escalate these to
/// the heavyweight pipeline (a plain `web_search` or a direct answer is right).
const CONTROL_PROMPTS: &[&str] = &[
    "What's the capital of Australia?",
    "Look up the current stable version number of the Rust compiler.",
    "What time zone is Tokyo in?",
];

fn deepseek() -> Arc<dyn LlmProvider> {
    let key = std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY required");
    Arc::new(OpenAIProvider::new(key, "deepseek-chat").with_base_url("https://api.deepseek.com/v1"))
        as Arc<dyn LlmProvider>
}

fn spec_of(t: &dyn Tool) -> ToolSpec {
    ToolSpec {
        name: t.name().to_string(),
        description: t.description().to_string(),
        input_schema: t.input_schema(),
    }
}

/// First tool deepseek calls for `prompt` given `tools`, or `"<none/text>"`.
async fn first_tool_choice(
    provider: &Arc<dyn LlmProvider>,
    tools: &[ToolSpec],
    prompt: &str,
) -> String {
    let messages = vec![Message::system(SYSTEM_PROMPT), Message::user(prompt)];
    let config = ChatConfig {
        max_tokens: Some(600),
        temperature: Some(0.0),
        tool_choice: ToolChoice::Auto,
        ..Default::default()
    };
    match provider.chat(&messages, tools, &config).await {
        Ok(resp) => resp
            .tool_calls
            .first()
            .map(|tc| tc.name.clone())
            .unwrap_or_else(|| "<none/text>".to_string()),
        Err(e) => format!("<error: {e}>"),
    }
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY (real LLM); A/B tool-selection probe"]
async fn deepseek_prefers_run_pipeline_for_deep_research() {
    let provider = deepseek();

    // Real competing tools, exactly as production wires them.
    let web_search = WebSearchTool::new();
    let web_fetch = WebFetchTool::new();
    let dir = tempfile::TempDir::new().unwrap();
    let data = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(
        EpisodeStore::open(data.path().join(".octos"))
            .await
            .unwrap(),
    );
    let run_pipeline = RunPipelineTool::new(
        provider.clone(),
        memory,
        PathBuf::from(dir.path()),
        PathBuf::from(data.path()),
    )
    .with_ir_enabled(false); // production default: probe the NON-IR description

    let web_search_spec = spec_of(&web_search);
    let web_fetch_spec = spec_of(&web_fetch);
    let new_pipeline_spec = spec_of(&run_pipeline);
    assert_eq!(new_pipeline_spec.name, "run_pipeline");
    assert!(
        new_pipeline_spec.description.contains("PREFER")
            || new_pipeline_spec
                .description
                .contains("ALWAYS use `deep_research`"),
        "NEW spec must carry the imperative steering — got: {}",
        new_pipeline_spec.description
    );

    let mut old_pipeline_spec = new_pipeline_spec.clone();
    old_pipeline_spec.description = OLD_NON_IR_DESCRIPTION.to_string();

    let variants: [(&str, ToolSpec); 2] = [("OLD", old_pipeline_spec), ("NEW", new_pipeline_spec)];

    // (label, prompt, expect_pipeline)
    let mut cases: Vec<(&str, &str, bool)> = Vec::new();
    for p in DEEP_RESEARCH_PROMPTS {
        cases.push(("deep", p, true));
    }
    for p in BORDERLINE_PROMPTS {
        cases.push(("bord", p, true)); // "ideal" is pipeline; informational only
    }
    for p in CONTROL_PROMPTS {
        cases.push(("ctrl", p, false));
    }

    eprintln!("\n================= deepseek tool-selection A/B =================");
    eprintln!("model=deepseek-chat  tool_choice=Auto  temp=0  ir_enabled=false");
    eprintln!(
        "tools offered: run_pipeline, {}, {}\n",
        web_search_spec.name, web_fetch_spec.name
    );

    let mut deep_pipeline = [0usize; 2];
    let mut bord_pipeline = [0usize; 2];
    let mut ctrl_pipeline = [0usize; 2];
    let deep_total = DEEP_RESEARCH_PROMPTS.len();
    let bord_total = BORDERLINE_PROMPTS.len();
    let ctrl_total = CONTROL_PROMPTS.len();

    for (vi, (vname, pipeline_spec)) in variants.iter().enumerate() {
        let tools = vec![
            pipeline_spec.clone(),
            web_search_spec.clone(),
            web_fetch_spec.clone(),
        ];
        eprintln!("----- variant {vname} -----");
        for (label, prompt, expect_pipeline) in &cases {
            let choice = first_tool_choice(&provider, &tools, prompt).await;
            let routed = choice == "run_pipeline";
            match *label {
                "deep" if routed => deep_pipeline[vi] += 1,
                "bord" if routed => bord_pipeline[vi] += 1,
                "ctrl" if routed => ctrl_pipeline[vi] += 1,
                _ => {}
            }
            let want = if *expect_pipeline {
                "→run_pipeline"
            } else {
                "→inline/direct"
            };
            // borderline has no single "right" answer, so don't mark it ok/!!
            let mark = if *label == "bord" {
                "   "
            } else if routed == *expect_pipeline {
                "ok "
            } else {
                "!! "
            };
            let shown: String = prompt.chars().take(58).collect();
            eprintln!("  {mark}[{label} {want:<15}] chose={choice:<14} | {shown}");
        }
        eprintln!(
            "  >>> {vname}: deep→pipeline {}/{}, BORDERLINE→pipeline {}/{}, ctrl→pipeline {}/{}\n",
            deep_pipeline[vi],
            deep_total,
            bord_pipeline[vi],
            bord_total,
            ctrl_pipeline[vi],
            ctrl_total
        );
    }

    eprintln!("================= summary =================");
    eprintln!(
        "explicit deep-research → run_pipeline:  OLD {}/{}   NEW {}/{}",
        deep_pipeline[0], deep_total, deep_pipeline[1], deep_total
    );
    eprintln!(
        "BORDERLINE research-ish → run_pipeline:  OLD {}/{}   NEW {}/{}   <-- where steering matters",
        bord_pipeline[0], bord_total, bord_pipeline[1], bord_total
    );
    eprintln!(
        "single-fact controls → run_pipeline:    OLD {}/{}   NEW {}/{}   (lower is better)",
        ctrl_pipeline[0], ctrl_total, ctrl_pipeline[1], ctrl_total
    );
    eprintln!("==========================================\n");

    // Behavioral assertions (lenient enough for a real model, strict enough to
    // mean something):
    // 1. NEW must route a strong majority of deep-research prompts to the pipeline.
    assert!(
        deep_pipeline[1] * 2 > deep_total, // > 50%
        "NEW steering should route most deep-research prompts to run_pipeline; \
         got {}/{}",
        deep_pipeline[1],
        deep_total
    );
    // 2. NEW must be no worse than OLD on the steering target (the change exists
    //    to INCREASE pipeline routing for deep research).
    assert!(
        deep_pipeline[1] >= deep_pipeline[0],
        "NEW must not route deep-research to run_pipeline LESS than OLD; \
         OLD={}/{}, NEW={}/{}",
        deep_pipeline[0],
        deep_total,
        deep_pipeline[1],
        deep_total
    );
    // 3. NEW must not over-escalate quick single-fact lookups to the pipeline.
    assert!(
        ctrl_pipeline[1] * 2 <= ctrl_total, // <= 50%
        "NEW must not over-trigger run_pipeline on single-fact controls; \
         got {}/{}",
        ctrl_pipeline[1],
        ctrl_total
    );
    // 4. On the BORDERLINE class — the only place steering can move the needle —
    //    NEW must route at least as many to the pipeline as OLD (no regression).
    //    Whether it routes MORE is the informational signal printed above.
    assert!(
        bord_pipeline[1] >= bord_pipeline[0],
        "NEW must not route borderline research LESS than OLD; OLD={}/{}, NEW={}/{}",
        bord_pipeline[0],
        bord_total,
        bord_pipeline[1],
        bord_total
    );
}
