//! Site-delivery workflow definition.
//!
//! Declares the expected workspace artifacts and completion validators for
//! the native delivery workflow. DOT execution is not part of this runtime.
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
        additional_instructions: "You are a background site builder. Follow the runtime-owned phases in order: scaffold, build, deliver_result. Read the session metadata to discover the selected template and build output directory, keep edits inside the project root, and deliver only the final built site entrypoint. Do not send intermediate logs, scratch files, or alternate build artifacts.\n\nM8 Runtime Parity W2.E: at the end of each phase, call check_workspace_contract once and surface the typed result back to the user as a single-line confirmation of the form \"\u{2713} <phase> phase: validators passed (<n>/<n>)\" — for example \"\u{2713} scaffold phase: validators passed (3/3)\". If a phase's validators reject the artifacts, instead emit \"\u{2717} <phase> phase: validators failed (<failed>/<total>) — <reason>\" before moving on.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every known template must pair its build output directory with the
    /// completion validator and the published artifact entrypoint — this is
    /// the cross-system acceptance contract the executor checks, not an
    /// output-string mirror.
    #[test]
    fn workspace_policy_completion_validators_cover_every_template() {
        for (slug, output_dir) in [
            ("astro-site", "dist"),
            ("nextjs-app", "out"),
            ("react-vite", "dist"),
            ("quarto-lesson", "docs"),
        ] {
            let policy = workspace_policy_for_template(slug);
            assert_eq!(
                policy.validation.on_completion,
                vec![format!("file_exists:{output_dir}/index.html")],
                "slug: {slug}"
            );
            let entrypoint = format!("{output_dir}/index.html");
            assert_eq!(
                policy.artifacts.entries.get("entrypoint"),
                Some(&entrypoint),
                "slug: {slug}"
            );
        }
    }
}
