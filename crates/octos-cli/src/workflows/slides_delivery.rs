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
        additional_instructions: "You are a background slides producer. Follow the runtime-owned phases in order: design, generate_deck, deliver_result. Write the slide script first, validate it before generation, call mofa_slides once, and deliver only the final deck artifact. Do not send intermediate previews, scratch PNGs, or alternate deck exports.\n\nM8 Runtime Parity W2.E: at the end of each phase, call check_workspace_contract once and surface the typed result back to the user as a single-line confirmation of the form \"\u{2713} <phase> phase: validators passed (<n>/<n>)\" — for example \"\u{2713} design phase: validators passed (3/3)\". If a phase's validators reject the artifacts, instead emit \"\u{2717} <phase> phase: validators failed (<failed>/<total>) — <reason>\" before moving on.".to_string(),
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
            on_turn_end: vec![
                "file_exists:script.js".into(),
                "file_exists:memory.md".into(),
                "file_exists:changelog.md".into(),
            ],
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
    fn slides_workflow_surfaces_per_phase_validator_status() {
        // M8 Runtime Parity W2.E: the spawned worker must surface a
        // typed phase-completion line — frontend test selectors and
        // the live e2e spec rely on this exact shape.
        let workflow = build();
        assert!(
            workflow
                .additional_instructions
                .contains("phase: validators passed"),
            "missing phase-validator hint: {}",
            workflow.additional_instructions
        );
        assert!(
            workflow.additional_instructions.contains("\u{2713}"),
            "missing checkmark in phase confirmation"
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
