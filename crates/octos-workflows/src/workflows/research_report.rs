use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};

pub fn build() -> WorkflowInstance {
    WorkflowInstance {
        kind: WorkflowKind::DeepResearch,
        label: "Deep research".to_string(),
        ack_message: "深度研究已在后台启动。完成后会把最终研究结果发回当前会话。".to_string(),
        current_phase: WorkflowPhase::new("research"),
        allowed_tools: vec![
            "search".into(),
            "deep_crawl".into(),
            "read_file".into(),
            "write_file".into(),
        ],
        limits: WorkflowLimits {
            max_search_passes: Some(6),
            max_pipeline_runs: Some(0),
            ..Default::default()
        },
        terminal_output: WorkflowTerminalOutput {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "report".into(),
        },
        additional_instructions: "You are a background research analyst. Use the native Rust `search` tool for multi-round research, parallel source reading, reference chasing, and a cited report. It uses OctoScript metasearch engines internally. Call search with the user's question, depth 2 by default (1 for a quick bounded check, 3 only when thorough coverage is needed), and output=report. Use `deep_crawl` for a specific documentation site or linked sources that need rendered pages; bound max_pages and max_depth to the task. Read the saved source files when checking claims. Preserve failed reads, contradictions, dates, source URLs and gaps; a search snippet is not confirmation that a source was read. Deliver exactly one final report, using write_file only when the research report needs consolidation with crawled evidence. Do not use run_pipeline, DOT, or a graph IR for this research workflow. If search or deep_crawl is unavailable, report the missing native tool rather than claiming research completed. Do not emit intermediate status chatter or send intermediate files.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_report_workflow_uses_report_output_contract() {
        let workflow = build();
        assert_eq!(workflow.kind, WorkflowKind::DeepResearch);
        assert_eq!(workflow.current_phase.as_str(), "research");
        assert_eq!(workflow.limits.max_search_passes, Some(6));
        assert_eq!(workflow.limits.max_pipeline_runs, Some(0));
        assert!(workflow.allowed_tools.iter().any(|tool| tool == "search"));
        assert!(
            workflow
                .allowed_tools
                .iter()
                .any(|tool| tool == "deep_crawl")
        );
        assert!(
            !workflow
                .allowed_tools
                .iter()
                .any(|tool| tool == "run_pipeline")
        );
        assert_eq!(workflow.terminal_output.required_artifact_kind, "report");
        assert!(workflow.terminal_output.deliver_final_artifact_only);
        assert!(workflow.terminal_output.forbid_intermediate_files);
    }
}
