//! Slides-delivery workflow definition.
//!
//! Pipeline-contract enforcement (coding-blue FA-7): the
//! `workspace_policy()` helper below returns a typed
//! [`WorkspacePolicy`] with `validation.on_completion` declaring the
//! deck artifact + preview PNGs as required. When the session's
//! working directory is initialised with `write_workspace_policy`,
//! [`octos_pipeline::RunPipelineTool::build_workspace_context`] reads
//! that policy on every `run_pipeline` invocation and propagates the
//! validator block to the pipeline executor. Pipeline completion fails
//! if the deck is missing — no new opt-in needed.
use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};
use octos_agent::workspace_policy::WorkspacePolicyWorkspace;
use octos_agent::{
    ValidationPolicy, WorkspaceArtifactsPolicy, WorkspacePolicy, WorkspacePolicyKind,
    WorkspaceSnapshotTrigger, WorkspaceTrackingPolicy, WorkspaceVersionControlPolicy,
    WorkspaceVersionControlProvider,
};
use std::collections::BTreeMap;

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
            forbid_intermediate_files: true,
            required_artifact_kind: "presentation".into(),
        },
        additional_instructions: "You are a background slides producer. Follow the runtime-owned phases in order: design, generate_deck, deliver_result. Write the slide script first, validate it before generation, call mofa_slides once, and deliver only the final deck artifact. Do not send intermediate previews, scratch PNGs, or alternate deck exports.".to_string(),
    }
}

pub fn workspace_policy() -> WorkspacePolicy {
    WorkspacePolicy {
        schema_version: octos_agent::WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Slides,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: true,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: true,
        },
        tracking: WorkspaceTrackingPolicy {
            ignore: vec![
                "history/**".into(),
                "output/**".into(),
                "skill-output/**".into(),
                "*.pptx".into(),
                "*.tmp".into(),
                ".DS_Store".into(),
            ],
        },
        validation: ValidationPolicy {
            on_turn_end: vec![
                "file_exists:script.js".into(),
                "file_exists:memory.md".into(),
                "file_exists:changelog.md".into(),
            ],
            on_source_change: Vec::new(),
            on_completion: vec![
                "file_exists:output/deck.pptx".into(),
                "file_exists:output/**/slide-*.png".into(),
            ],
            validators: Vec::new(),
        },
        artifacts: WorkspaceArtifactsPolicy {
            entries: BTreeMap::from([
                ("primary".into(), "output/deck.pptx".into()),
                ("deck".into(), "output/deck.pptx".into()),
                ("previews".into(), "output/**/slide-*.png".into()),
            ]),
        },
        spawn_tasks: BTreeMap::new(),
        compaction: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_slides_workflow_uses_presentation_output_contract() {
        let workflow = build();
        assert_eq!(workflow.kind, WorkflowKind::Slides);
        assert_eq!(workflow.current_phase.as_str(), "design");
        assert_eq!(
            workflow.terminal_output.required_artifact_kind,
            "presentation"
        );
        assert!(workflow.terminal_output.deliver_final_artifact_only);
        assert!(workflow.terminal_output.forbid_intermediate_files);
        assert!(
            workflow
                .allowed_tools
                .iter()
                .any(|tool| tool == "mofa_slides")
        );
        assert!(
            workflow
                .allowed_tools
                .iter()
                .any(|tool| tool == "check_workspace_contract")
        );
    }

    #[test]
    fn slides_workspace_policy_is_standardized() {
        let policy = workspace_policy();
        assert_eq!(
            policy.workspace.kind,
            octos_agent::WorkspacePolicyKind::Slides
        );
        assert!(
            policy
                .validation
                .on_turn_end
                .contains(&"file_exists:script.js".to_string())
        );
        assert!(
            policy
                .validation
                .on_completion
                .contains(&"file_exists:output/deck.pptx".to_string())
        );
    }
}
