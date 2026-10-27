//! Slides-delivery workflow definition.
//!
//! Declares the expected workspace artifacts and completion validators for
//! the native delivery workflow. DOT execution is not part of this runtime.
use crate::workflow_runtime::{
    WorkflowInstance, WorkflowKind, WorkflowLimits, WorkflowPhase, WorkflowTerminalOutput,
};
use octos_agent::workspace_policy::{
    MagicByteKind, Validator, ValidatorFileSource, ValidatorPhaseKind, ValidatorSpec,
    WorkspacePolicyWorkspace,
};
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
        ack_message: "Slides work has started in the background. The producer will create, inspect and deliver the presentation.".to_string(),
        current_phase: WorkflowPhase::new("design"),
        allowed_tools: vec![
            "mofa_slides".into(),
            "mofa_make".into(),
            "mofa_describe_content_type".into(),
            "group:fs".into(),
            "group:runtime".into(),
            "view_image".into(),
            "glob".into(),
            "grep".into(),
            "discover_tools".into(),
            "run_tool".into(),
            "check_background_tasks".into(),
            "check_workspace_contract".into(),
        ],
        limits: WorkflowLimits::default(),
        terminal_output: WorkflowTerminalOutput {
            deliver_final_artifact_only: true,
            forbid_intermediate_files: true,
            required_artifact_kind: "presentation".into(),
        },
        additional_instructions: "You are a background slides producer. Own planning, authoring, visual review and repairs according to the user's brief. Choose the installed authoring and inspection tools that fit the task. Read actual rendered PPTX images alongside native text and structural measurements; batch related repairs and reuse valid evidence for unchanged content. Use task notifications for long-running tools. An optional reviewer supplies evidence, not mandatory approval or a prescribed repair sequence. Workspace checks report mechanical artifact state and do not establish visual quality. Report the checks actually performed and material limitations. Runtime delivers the final deck; keep inspection images and scratch exports available for your own review without sending them as final deliverables.".to_string(),
    }
}

/// Build the slides workspace policy. When `slug` is `Some`, the
/// artifact globs are baked with the project's slug so they point at
/// the canonical post-rebind location
/// (`skill-output/slides/<slug>/output/...`). When `slug` is `None`,
/// generic patterns are used (suitable for the `for_kind(Slides)`
/// default and tests that don't know the slug).
pub fn workspace_policy_for_slug(slug: Option<&str>) -> WorkspacePolicy {
    let (primary, deck, previews) = match slug {
        Some(s) => (
            format!("skill-output/slides/{s}/output/deck.pptx"),
            format!("skill-output/slides/{s}/output/deck.pptx"),
            format!("skill-output/slides/{s}/output/**/slide-*.png"),
        ),
        None => (
            "output/deck.pptx".into(),
            "output/deck.pptx".into(),
            "output/**/slide-*.png".into(),
        ),
    };
    let mut policy = workspace_policy();
    policy.artifacts = WorkspaceArtifactsPolicy {
        entries: BTreeMap::from([
            ("primary".into(), primary),
            ("deck".into(), deck),
            ("previews".into(), previews),
        ]),
    };
    policy
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
            on_turn_end: Vec::new(),
            on_source_change: Vec::new(),
            // Path-based `file_exists` checks were removed: the deck
            // lands under `<workspace>/skill-output/slides/<slug>/` via
            // the host's plugin work-dir rebind (outside the project
            // dir), so the previous `file_exists:output/...` could
            // never match in production. The MagicBytes validator
            // below now consumes the plugin's `files_to_send` list via
            // the SpawnOnlyFiles source.
            on_completion: Vec::new(),
            // octos #997: gate the slides project on the PPTX
            // magic-bytes signature so an HTML "success" deck trips
            // the contract. Uses `SpawnOnlyFiles` source — the spawn
            // loop wires `files_to_send` through to
            // `run_project_root_validators`, which filters to files
            // belonging to this project
            // (`<session>/skill-output/slides/<slug>/` or the legacy
            // `<session>/slides/<slug>/`) before passing them to the
            // validator runner.
            validators: vec![Validator {
                id: "slides.mofa_slides.pptx_magic_bytes".into(),
                required: true,
                soft_fail: false,
                timeout_ms: None,
                phase: ValidatorPhaseKind::Completion,
                spec: ValidatorSpec::MagicBytes {
                    glob: String::new(),
                    format: MagicByteKind::Pptx,
                    source: ValidatorFileSource::SpawnOnlyFiles,
                    extension: Some("pptx".into()),
                },
            }],
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
    fn should_allow_artifact_review_tools_without_unrelated_delivery_tools() {
        let workflow = build();
        let policy = octos_agent::tools::ToolPolicy {
            allow: workflow.allowed_tools,
            ..Default::default()
        };
        for tool in [
            "read_file",
            "view_image",
            "edit_file",
            "exec_command",
            "write_stdin",
            "watch_external_job",
            "mofa_make",
            "check_workspace_contract",
        ] {
            assert!(
                policy.is_allowed(tool),
                "missing artifact review capability: {tool}"
            );
        }
        for tool in ["send_email", "mofa_publish"] {
            assert!(
                !policy.is_allowed(tool),
                "unrelated capability exposed: {tool}"
            );
        }
    }

    #[test]
    fn should_keep_explicit_tool_denials_effective_during_slide_review() {
        let policy = octos_agent::tools::ToolPolicy {
            allow: build().allowed_tools,
            deny: vec!["view_image".into(), "group:runtime".into()],
            ..Default::default()
        };
        assert!(policy.is_allowed("read_file"));
        for tool in ["view_image", "shell", "exec_command", "write_stdin"] {
            assert!(!policy.is_allowed(tool));
        }
    }

    #[test]
    fn should_leave_generation_and_review_budgets_to_the_session() {
        let workflow = build();
        assert_eq!(workflow.limits.max_generate_calls, None);
        assert_eq!(workflow.limits.max_dialogue_lines, None);
    }

    #[test]
    fn slides_workspace_policy_is_standardized() {
        let policy = workspace_policy();
        assert_eq!(
            policy.workspace.kind,
            octos_agent::WorkspacePolicyKind::Slides
        );
        // Model-selected authoring routes need not create a specific script
        // language or boilerplate journals to inspect and deliver a PPTX.
        assert!(policy.validation.on_turn_end.is_empty());
        // `on_completion` no longer declares project-relative
        // `file_exists` checks — the deck lands under
        // `<workspace>/skill-output/...` via the host's plugin
        // work-dir rebind, outside the project dir. Artifact gating
        // moved to the SpawnOnlyFiles MagicBytes validator below.
        assert!(policy.validation.on_completion.is_empty());
        // The bare `MagicBytes(Pptx)` HTML-pptx safety net is retained,
        // now using `SpawnOnlyFiles` source.
        assert_eq!(policy.validation.validators.len(), 1);
        assert_eq!(
            policy.validation.validators[0].id,
            "slides.mofa_slides.pptx_magic_bytes"
        );
    }
}
