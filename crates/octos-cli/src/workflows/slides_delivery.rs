use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};
use octos_agent::{WorkspacePolicy, WorkspaceProjectKind};

pub fn build() -> WorkflowInstance {
    WorkflowInstance {
        kind: WorkflowKind::Slides,
        label: "Slides deliverable".to_string(),
        ack_message: "Slides generation has started in the background. Only the final deck will be delivered once the workspace contract is satisfied.".to_string(),
        current_phase: WorkflowPhase::new("design"),
        allowed_tools: vec![
            "mofa_slides".into(),
            "read_file".into(),
            "write_file".into(),
            "edit_file".into(),
            "shell".into(),
            "glob".into(),
            "check_background_tasks".into(),
            "check_workspace_contract".into(),
        ],
        limits: WorkflowLimits {
            max_search_passes: None,
            max_pipeline_runs: None,
            max_dialogue_lines: Some(24),
            target_audio_minutes: None,
            max_generate_calls: Some(1),
        },
        terminal_output: WorkflowTerminalOutput {
            deliver_final_artifact_only: true,
            deliver_media_only: false,
            forbid_intermediate_files: true,
            required_artifact_kind: "presentation".into(),
        },
        additional_instructions: "You are a background slides producer. Follow the runtime-owned phases in order: design, generate_deck, deliver_result. Write the slide script first, validate it before generation, call mofa_slides once, and deliver only the final deck artifact. Do not send intermediate previews, scratch PNGs, or alternate deck exports.".to_string(),
    }
}

pub fn workspace_policy() -> WorkspacePolicy {
    WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_slides_workflow_uses_presentation_output_contract() {
        let workflow = build();
        assert_eq!(workflow.kind, WorkflowKind::Slides);
        assert_eq!(workflow.current_phase.as_str(), "design");
        assert_eq!(workflow.terminal_output.required_artifact_kind, "presentation");
        assert!(workflow.terminal_output.deliver_final_artifact_only);
        assert!(!workflow.terminal_output.deliver_media_only);
        assert!(workflow.terminal_output.forbid_intermediate_files);
        assert!(workflow.allowed_tools.iter().any(|tool| tool == "mofa_slides"));
        assert!(workflow
            .allowed_tools
            .iter()
            .any(|tool| tool == "check_workspace_contract"));
    }

    #[test]
    fn slides_workspace_policy_is_standardized() {
        let policy = workspace_policy();
        assert_eq!(policy.workspace.kind, octos_agent::WorkspacePolicyKind::Slides);
        assert!(policy.validation.on_turn_end.contains(&"file_exists:script.js".to_string()));
        assert!(policy
            .validation
            .on_completion
            .contains(&"file_exists:output/*.pptx".to_string()));
    }
}
