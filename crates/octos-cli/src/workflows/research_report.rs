use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};

pub fn build() -> WorkflowInstance {
    WorkflowInstance {
        kind: WorkflowKind::DeepResearch,
        label: "Deep research".to_string(),
        ack_message: "深度研究已在后台启动。完成后会把最终研究结果发回当前会话。".to_string(),
        current_phase: WorkflowPhase::new("research"),
        allowed_tools: vec!["run_pipeline".into()],
        limits: WorkflowLimits {
            max_search_passes: Some(6),
            max_pipeline_runs: Some(1),
            ..Default::default()
        },
        terminal_output: WorkflowTerminalOutput {
            deliver_final_artifact_only: true,
            deliver_media_only: false,
            forbid_intermediate_files: true,
            required_artifact_kind: "report".into(),
        },
        additional_instructions: "You are a background research analyst. Follow the runtime-owned workflow phases in order: research, draft_report, deliver_result. Use run_pipeline with an inline DOT graph to complete the research in the background. Build one bounded pipeline run only, keep the evidence-gathering depth to roughly 4-6 search/evidence passes, and then draft a single final report. Deliver exactly one final user-facing report. Do not emit intermediate status chatter or send intermediate files.".to_string(),
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
        assert_eq!(workflow.limits.max_pipeline_runs, Some(1));
        assert_eq!(workflow.terminal_output.required_artifact_kind, "report");
        assert!(workflow.terminal_output.deliver_final_artifact_only);
        assert!(workflow.terminal_output.forbid_intermediate_files);
    }
}
