use crate::workflow_runtime::workflow_families::SiteTemplate;
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

pub fn build_output_dir_for_template_kind(template: SiteTemplate) -> &'static str {
    template.output_dir()
}

pub fn build_output_dir_for_template(template: &str) -> &'static str {
    build_output_dir_for_template_kind(SiteTemplate::from_slug(template))
}

pub fn workspace_policy_for_template_kind(template: SiteTemplate) -> WorkspacePolicy {
    let build_output_dir = build_output_dir_for_template_kind(template);
    WorkspacePolicy {
        schema_version: octos_agent::WORKSPACE_POLICY_SCHEMA_VERSION,
        workspace: WorkspacePolicyWorkspace {
            kind: WorkspacePolicyKind::Sites,
        },
        version_control: WorkspaceVersionControlPolicy {
            provider: WorkspaceVersionControlProvider::Git,
            auto_init: true,
            trigger: WorkspaceSnapshotTrigger::TurnEnd,
            fail_on_error: true,
        },
        tracking: WorkspaceTrackingPolicy {
            ignore: vec![
                "node_modules/**".into(),
                "dist/**".into(),
                "out/**".into(),
                "docs/**".into(),
                "build/**".into(),
                ".astro/**".into(),
                ".next/**".into(),
                ".quarto/**".into(),
                "*.log".into(),
                ".DS_Store".into(),
            ],
        },
        validation: ValidationPolicy {
            on_turn_end: vec![
                "file_exists:mofa-site-session.json".into(),
                "file_exists:site-plan.json".into(),
                "file_exists:optimized-prompt.md".into(),
            ],
            on_source_change: Vec::new(),
            on_completion: vec![format!("file_exists:{build_output_dir}/index.html")],
            validators: Vec::new(),
        },
        artifacts: WorkspaceArtifactsPolicy {
            entries: BTreeMap::from([
                ("primary".into(), format!("{build_output_dir}/index.html")),
                (
                    "entrypoint".into(),
                    format!("{build_output_dir}/index.html"),
                ),
            ]),
        },
        spawn_tasks: BTreeMap::new(),
        compaction: None,
    }
}

pub fn workspace_policy_for_template(template: &str) -> WorkspacePolicy {
    workspace_policy_for_template_kind(SiteTemplate::from_slug(template))
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
        assert!(workflow.terminal_output.forbid_intermediate_files);
        assert!(
            workflow
                .allowed_tools
                .iter()
                .any(|tool| tool == "check_workspace_contract")
        );
    }

    #[test]
    fn build_output_dir_is_template_aware() {
        assert_eq!(build_output_dir_for_template("astro-site"), "dist");
        assert_eq!(build_output_dir_for_template("nextjs-app"), "out");
        assert_eq!(build_output_dir_for_template("react-vite"), "dist");
        assert_eq!(build_output_dir_for_template("other"), "docs");
    }

    #[test]
    fn build_output_dir_is_template_typed() {
        assert_eq!(
            build_output_dir_for_template_kind(SiteTemplate::AstroSite),
            "dist"
        );
        assert_eq!(
            build_output_dir_for_template_kind(SiteTemplate::NextjsApp),
            "out"
        );
        assert_eq!(
            build_output_dir_for_template_kind(SiteTemplate::Docs),
            "docs"
        );
    }

    #[test]
    fn workspace_policy_tracks_template_entrypoint() {
        let policy = workspace_policy_for_template("nextjs-app");
        assert_eq!(
            policy.validation.on_completion,
            vec!["file_exists:out/index.html"]
        );
        assert_eq!(
            policy
                .artifacts
                .entries
                .get("entrypoint")
                .map(String::as_str),
            Some("out/index.html")
        );
    }
}
