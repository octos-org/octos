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

#[tokio::test]
async fn project_root_validators_write_to_project_ledger_without_manual_seeding() {
    // octos #997 (round-2 fix): the load-bearing test for codex's review.
    //
    // Codex flagged that pre-round-2, the validator was DECLARED at the
    // slides-kind project policy but never RUN at the project root —
    // production decks that genuinely produce a valid PPTX would still
    // surface `ready = false` because `inspect_workspace_contract` reads
    // `<session>/slides/<slug>/.octos/validator_outcomes.jsonl` but the
    // production code path only wrote to `<session>/.octos/...`. The 9
    // fixture sites that manually `ledger.append(...)` a `Pass` were
    // masking the gap.
    //
    // This test exercises the production code path WITHOUT manually
    // seeding the ledger:
    //
    // 1. Build a slides workspace with a real PPTX and a project-scope
    //    `WorkspacePolicy::for_kind(Slides)` (which declares the
    //    hard-required `slides.mofa_slides.pptx_magic_bytes` validator).
    // 2. Invoke `run_project_root_validators` — the helper wired into the
    //    spawn completion path in this commit.
    // 3. Assert the project-root ledger file exists and contains a `Pass`
    //    for the slides-kind PPTX MagicBytes validator id.
    // 4. Assert `inspect_workspace_contract_at_root` reports `ready = true`
    //    against that project — proving the contract gate sees the run.
    //
    // PRE-FIX FAILURE QUOTE (verified by stubbing
    // `run_project_root_validators` to return an empty report — i.e.
    // mirroring the pre-round-2 state where nothing ran validators at the
    // project root):
    //
    //     assertion `left == right` failed: expected exactly one slides
    //     project to have run validators; got report =
    //     ProjectRootValidatorReport { projects_run: 0, failures: [] }
    //       left: 0
    //      right: 1
    //
    // i.e. the production code path never runs the declared validator at
    // the project root, so the ledger file never exists, and the
    // inspect-contract gate stays `ready = false` even with a genuine deck.
    use octos_agent::ToolRegistry;
    use octos_agent::inspect_workspace_contract_at_root;
    use octos_agent::validators::{ValidatorLedger, ValidatorStatus};
    use octos_agent::workspace_contract::run_project_root_validators;
    use octos_agent::workspace_policy::write_workspace_policy;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let session_root = dir.path();
    let project_root = session_root.join("slides").join("demo");
    let output_dir = project_root.join("output");
    let imgs_dir = output_dir.join("imgs");
    std::fs::create_dir_all(&imgs_dir).unwrap();

    // The slides-kind policy declares the hard-required PPTX MagicBytes
    // validator (octos #997). Persist it under the project root, as
    // `create_slides_project` would in production.
    write_workspace_policy(
        &project_root,
        &WorkspacePolicy::for_kind(WorkspaceProjectKind::Slides),
    )
    .unwrap();

    // Required source files (turn-end checks) + a genuine PPTX (the
    // success case mofa_slides produces).
    std::fs::write(project_root.join("script.js"), "// slides").unwrap();
    std::fs::write(project_root.join("memory.md"), "# memory").unwrap();
    std::fs::write(project_root.join("changelog.md"), "# changelog").unwrap();
    let mut pptx_bytes = vec![0x50, 0x4B, 0x03, 0x04];
    pptx_bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(output_dir.join("deck.pptx"), &pptx_bytes).unwrap();
    std::fs::write(imgs_dir.join("slide-01.png"), b"png").unwrap();

    // PRE-CONDITION: no validator outcome exists at the project root.
    // (Production code path has not been exercised yet.)
    let ledger_path = project_root.join(".octos").join("validator_outcomes.jsonl");
    assert!(
        !ledger_path.exists(),
        "ledger should not exist before the project-root validator run — \
         otherwise the test would not prove the production path writes it"
    );

    // ACT: invoke the production code path. This is what the spawn loop
    // calls after a successful `run_task` for slides workflows. No manual
    // ledger seeding.
    let registry = Arc::new(ToolRegistry::new());
    let report =
        run_project_root_validators(&registry, session_root, Some(WorkspaceProjectKind::Slides))
            .await;

    // The slides project should have been picked up + run.
    assert_eq!(
        report.projects_run, 1,
        "expected exactly one slides project to have run validators; got report = {report:?}"
    );
    assert!(
        report.failures.is_empty(),
        "genuine PPTX should not produce failures; got failures = {:?}",
        report.failures
    );

    // ASSERT 1: the project-root ledger file MUST exist after the
    // production code path runs (without manual seeding).
    assert!(
        ledger_path.exists(),
        "project ledger must exist at {} after the production code path \
         runs (no manual seeding) — this is the gap codex flagged",
        ledger_path.display()
    );

    // ASSERT 2: the ledger must contain a `Pass` for the slides-kind
    // PPTX MagicBytes validator id (the one declared by `for_kind(Slides)`).
    let ledger = ValidatorLedger::open(&ledger_path).expect("open project ledger");
    let entries = ledger.read_all().expect("read project ledger entries");
    let pptx_pass = entries
        .iter()
        .find(|o| {
            o.validator_id == "slides.mofa_slides.pptx_magic_bytes"
                && o.status == ValidatorStatus::Pass
        })
        .unwrap_or_else(|| {
            panic!(
                "project ledger must contain a Pass for \
                 slides.mofa_slides.pptx_magic_bytes; got entries = {entries:?}"
            )
        });
    assert_eq!(pptx_pass.kind, "magic_bytes");

    // ASSERT 3: the contract gate reads the ledger we just wrote and now
    // reports `ready = true`. This is the user-visible behaviour the gap
    // was suppressing.
    let status = inspect_workspace_contract_at_root(&project_root)
        .expect("inspect_workspace_contract_at_root must succeed");
    assert!(
        status.ready,
        "contract gate must report ready = true after project-root \
         validators run; status = {status:?}"
    );
}
