//! Regression coverage for octos #997 — the slides-kind project-scope
//! `WorkspacePolicy` must wire the `mofa_slides` PPTX `MagicBytes` validator
//! into `validation.validators` so the contract-gate rejects HTML "success"
//! decks (the user-visible failure mode where `mofa_slides` writes an HTML
//! error page in place of the `.pptx`).
//!
//! Pre-fix, the `mofa_slides` validator was inserted ONLY into the
//! session-scope spawn_tasks table (`workspace_policy.rs:1127`) and the
//! slides-kind policy declared `spawn_tasks: BTreeMap::new()`. Because
//! `inspect_workspace_contract` reads `validation.validators` (not
//! `spawn_tasks`), the gate silently passed an HTML-as-PPTX deck.
//!
//! Run with `cargo test -p octos-agent --test slides_validator_project_scope`.

use octos_agent::workspace_git::WorkspaceProjectKind;
use octos_agent::workspace_policy::{
    MagicByteKind, ValidatorPhaseKind, ValidatorSpec, WorkspacePolicy,
};

#[test]
fn slides_kind_policy_wires_mofa_slides_pptx_magic_bytes_validator() {
    // Project-scope guarantee: a slides-kind workspace policy must declare
    // a hard-required `MagicBytes` validator for `**/*.pptx` so a downstream
    // `run_declared_validators` call rejects HTML-as-PPTX failure modes.
    let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);

    let pptx_validator = policy
        .validation
        .validators
        .iter()
        .find(|v| matches!(&v.spec, ValidatorSpec::MagicBytes { format, .. } if *format == MagicByteKind::Pptx))
        .expect(
            "slides-kind policy must declare a MagicBytes(Pptx) validator in \
             validation.validators (octos #997)",
        );

    // The validator must be hard-required so it actually demotes a failing
    // delivery — a soft validator would never block the gate.
    assert!(
        pptx_validator.required,
        "PPTX MagicBytes validator must be required = true so the gate blocks"
    );
    assert!(
        !pptx_validator.soft_fail,
        "PPTX MagicBytes validator must be a hard gate (soft_fail = false)"
    );
    assert_eq!(
        pptx_validator.phase,
        ValidatorPhaseKind::Completion,
        "PPTX MagicBytes validator must run at the Completion phase"
    );

    // Sanity: the glob must target `.pptx` files. The validator is glob-
    // based, not template-interpolated, so a recursive PPTX pattern is what
    // we want.
    let glob = match &pptx_validator.spec {
        ValidatorSpec::MagicBytes { glob, .. } => glob.clone(),
        _ => unreachable!("matched MagicBytes above"),
    };
    assert!(
        glob.ends_with(".pptx"),
        "MagicBytes glob should target .pptx files, got {glob:?}"
    );
}

#[tokio::test]
async fn html_pptx_fails_slides_kind_project_scope_validator_gate() {
    // End-to-end: a slides project with an HTML-content `.pptx` (the
    // mofa_slides skill failure mode) must trip the project-scope validator
    // gate via `run_declared_validators`. Pre-fix this passed silently
    // because the slides-kind policy declared no validators.
    use std::sync::Arc;

    use octos_agent::ToolRegistry;
    use octos_agent::validators::ValidatorPhase;
    use octos_agent::workspace_contract::run_declared_validators;

    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path();
    let output_dir = workspace_root.join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Failure mode: mofa_slides wrote an HTML error page in place of the
    // PPTX. The bytes-at-offset-0 are NOT the ZIP local-file-header signature
    // (`PK\x03\x04`) that a real .pptx carries, so MagicBytes(Pptx) must
    // reject this file.
    let html_error_page = b"<!DOCTYPE html><html><body>500 internal error</body></html>";
    std::fs::write(output_dir.join("deck.pptx"), html_error_page).unwrap();

    let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    let registry = Arc::new(ToolRegistry::new());

    let result = run_declared_validators(
        &registry,
        workspace_root,
        &policy.validation.validators,
        "slides/demo",
        ValidatorPhase::Completion,
        None,
    )
    .await;

    let err = result.expect_err(
        "HTML-as-PPTX deck must fail the slides project-scope validator gate (octos #997)",
    );
    let rendered = err.to_string();
    assert!(
        rendered.contains("magic_bytes") || rendered.contains("pptx"),
        "validator failure should call out magic_bytes/pptx, got: {rendered}"
    );
}

#[tokio::test]
async fn valid_pptx_passes_slides_kind_project_scope_validator_gate() {
    // Positive case: a real .pptx (ZIP container with the local-file-header
    // signature at offset 0) must pass the project-scope gate so genuine
    // mofa_slides outputs are not blocked.
    use std::sync::Arc;

    use octos_agent::ToolRegistry;
    use octos_agent::validators::ValidatorPhase;
    use octos_agent::workspace_contract::run_declared_validators;

    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path();
    let output_dir = workspace_root.join("output");
    std::fs::create_dir_all(&output_dir).unwrap();

    // Minimal PPTX header: PK\x03\x04 (ZIP local-file-header magic). The
    // MagicBytes validator only inspects the leading bytes — a full valid
    // archive is not required for this check.
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(output_dir.join("deck.pptx"), pptx_bytes).unwrap();

    let policy = WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides);
    let registry = Arc::new(ToolRegistry::new());

    let outcomes = run_declared_validators(
        &registry,
        workspace_root,
        &policy.validation.validators,
        "slides/demo",
        ValidatorPhase::Completion,
        None,
    )
    .await
    .expect("genuine PPTX must pass the slides project-scope validator gate");

    // Confirm the PPTX MagicBytes validator was actually exercised — not
    // skipped via an empty list.
    let pptx_outcome = outcomes
        .iter()
        .find(|o| o.kind == "magic_bytes")
        .expect("MagicBytes outcome must be recorded for a slides-kind gate run");
    assert_eq!(
        pptx_outcome.status,
        octos_agent::validators::ValidatorStatus::Pass
    );
}
