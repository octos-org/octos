use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};
use octos_agent::WorkspacePolicy;

pub fn build_output_dir_for_template(template: &str) -> &'static str {
    match template {
        "astro-site" => "dist",
        "nextjs-app" => "out",
        "react-vite" => "dist",
        _ => "docs",
    }
}

pub fn workspace_policy_for_template(template: &str) -> WorkspacePolicy {
    WorkspacePolicy::for_site_build_output(build_output_dir_for_template(template))
}

pub fn build() -> WorkflowInstance {
    WorkflowInstance {
        kind: WorkflowKind::Site,
        label: "Site deliverable".to_string(),
        ack_message: "Site generation has started in the background. Only the final verified site entrypoint will be delivered once the workspace contract is satisfied.".to_string(),
        current_phase: WorkflowPhase::new("scaffold"),
        allowed_tools: vec![
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
            required_artifact_kind: "site".into(),
        },
        additional_instructions: "You are a background site builder. Follow the runtime-owned phases in order: scaffold, build, deliver_result. Read the session metadata to discover the selected template and build output directory, keep edits inside the project root, and deliver only the final built site entrypoint. Do not send intermediate logs, scratch files, or alternate build artifacts.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_site_workflow_uses_site_output_contract() {
        let workflow = build();
        assert_eq!(workflow.kind, WorkflowKind::Site);
        assert_eq!(workflow.current_phase.as_str(), "scaffold");
        assert_eq!(workflow.terminal_output.required_artifact_kind, "site");
        assert!(workflow.terminal_output.deliver_final_artifact_only);
        assert!(!workflow.terminal_output.deliver_media_only);
        assert!(workflow.terminal_output.forbid_intermediate_files);
        assert!(workflow
            .allowed_tools
            .iter()
            .any(|tool| tool == "check_workspace_contract"));
    }

    #[test]
    fn build_output_dir_is_template_aware() {
        assert_eq!(build_output_dir_for_template("astro-site"), "dist");
        assert_eq!(build_output_dir_for_template("nextjs-app"), "out");
        assert_eq!(build_output_dir_for_template("react-vite"), "dist");
        assert_eq!(build_output_dir_for_template("other"), "docs");
    }

    #[test]
    fn workspace_policy_tracks_template_entrypoint() {
        let policy = workspace_policy_for_template("nextjs-app");
        assert_eq!(
            policy.validation.on_completion,
            vec!["file_exists:out/index.html"]
        );
        assert_eq!(
            policy.artifacts.entries.get("entrypoint").map(String::as_str),
            Some("out/index.html")
        );
    }
}
