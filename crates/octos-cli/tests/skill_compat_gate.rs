//! Harness M4.4: third-party skill compatibility gate.
//!
//! This integration test proves that a custom skill can traverse the full
//! harness lifecycle without any runtime-specific code branches:
//!
//!   install -> run -> deliver -> reload -> remove -> verify-gone
//!
//! It uses a checked-in fixture skill (`e2e/fixtures/compat-test-skill/`)
//! installed from a local path. The test is hermetic: each phase runs in
//! its own temporary directory, and no network or external registry access
//! is performed.
//!
//! Coverage for Required invariants in issue #467:
//!   1. Fixture uses only documented stable fields (manifest/SKILL.md frontmatter).
//!   2. Each lifecycle step is asserted end-to-end.
//!   3. Uninstall is idempotent (removing twice does not error).
//!   4. Secret handling: fixture declares its secret via env-var *name*; the
//!      test verifies the secret value never lands in the installed skill
//!      directory, captured CLI output, or produced artifact.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// Absolute path of the `octos` binary built by the test harness.
fn octos_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // test binary name
    path.pop(); // deps
    path.push("octos");
    path
}

/// Absolute path of the checked-in fixture skill.
fn fixture_skill_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/octos-cli/ during test compilation.
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent() // crates/
        .and_then(Path::parent) // repo root
        .expect("repo root")
        .join("e2e")
        .join("fixtures")
        .join("compat-test-skill")
}

/// Invoke `octos skills <args>` against the given skills dir (via `--cwd`).
///
/// The CLI resolves the skills directory as `<cwd>/.octos/skills/` when no
/// `--profile` is provided, so each test gets its own isolated install root.
fn run_octos_skills(cwd: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(octos_binary());
    cmd.arg("skills").arg("--cwd").arg(cwd).args(args);
    cmd.output().expect("failed to run octos skills")
}

/// Invoke the installed skill's `main` binary directly. Returns stdout+stderr
/// for inspection by the secret-leak assertions.
fn run_skill_main(
    skill_dir: &Path,
    tool: &str,
    input: &Value,
    env: &[(&str, &str)],
) -> (bool, String, String) {
    let main_path = skill_dir.join("main");
    assert!(
        main_path.exists(),
        "installed skill missing binary at {}",
        main_path.display()
    );
    let mut cmd = Command::new(&main_path);
    cmd.arg(tool);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn skill main");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin pipe");
        stdin
            .write_all(input.to_string().as_bytes())
            .expect("write stdin");
    }
    let out = child.wait_with_output().expect("wait_with_output");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Read all text files under a directory and return the concatenated body.
/// Used to assert that no secret value leaked into the installed tree.
fn concat_all_files_under(root: &Path) -> String {
    let mut buf = String::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return buf;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            buf.push_str(&concat_all_files_under(&path));
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            buf.push_str(&text);
            buf.push('\n');
        }
    }
    buf
}

#[test]
fn should_install_run_reload_remove_fixture_skill_when_driven_by_harness_contract() {
    let fixture = fixture_skill_dir();
    assert!(
        fixture.join("SKILL.md").exists(),
        "fixture missing at {}",
        fixture.display()
    );
    assert!(
        fixture.join("manifest.json").exists(),
        "fixture manifest missing"
    );
    assert!(fixture.join("main").exists(), "fixture binary missing");

    let octos = octos_binary();
    assert!(
        octos.exists(),
        "octos binary not built at {} — run `cargo build -p octos-cli`",
        octos.display()
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path();
    let skills_dir = cwd.join(".octos").join("skills");
    let installed_skill = skills_dir.join("compat-test-skill");

    // ── Phase 1: install ───────────────────────────────────────────────
    let out = run_octos_skills(cwd, &["install", fixture.to_str().expect("fixture utf-8")]);
    assert!(
        out.status.success(),
        "install failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        installed_skill.join("SKILL.md").exists(),
        "installed SKILL.md missing"
    );
    assert!(
        installed_skill.join("manifest.json").exists(),
        "installed manifest.json missing"
    );
    assert!(
        installed_skill.join("main").exists(),
        "installed main binary missing"
    );

    // ── Phase 2: list shows the skill (harness discovery) ──────────────
    let list_out = run_octos_skills(cwd, &["list"]);
    assert!(list_out.status.success(), "list failed");
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(
        list_stdout.contains("compat-test-skill"),
        "list output missing skill: {list_stdout}"
    );

    // ── Phase 3: run the skill through its documented binary protocol ──
    let input_text_path = cwd.join("input.txt");
    let output_summary_path = cwd.join("summary.md");
    std::fs::write(
        &input_text_path,
        "alpha line\nbeta line\ngamma line\ndelta line\nepsilon line\nzeta line\n",
    )
    .unwrap();

    // Token is delivered via the manifest-allowlisted env-var name.
    // It MUST NOT appear in any logged output or produced artifact.
    let secret_value = "harness-compat-secret-2026-04-21-canary";

    let (ok, stdout, stderr) = run_skill_main(
        &installed_skill,
        "summarize_text",
        &json!({
            "input_path": input_text_path.to_string_lossy(),
            "output_path": output_summary_path.to_string_lossy(),
        }),
        &[("COMPAT_SUMMARY_TOKEN", secret_value)],
    );
    assert!(ok, "skill run failed: stdout={stdout} stderr={stderr}");

    // Response is JSON with success, output, files_to_send.
    let response: Value =
        serde_json::from_str(stdout.trim()).expect("skill stdout must be valid JSON");
    assert_eq!(response["success"], Value::Bool(true));
    let delivered = response["files_to_send"]
        .as_array()
        .expect("files_to_send array");
    assert_eq!(
        delivered.len(),
        1,
        "expected exactly one delivered artifact"
    );
    assert_eq!(
        delivered[0].as_str().unwrap(),
        output_summary_path.to_string_lossy(),
        "declared artifact must match output_path"
    );

    // Delivered artifact exists and carries the expected summary shape.
    assert!(
        output_summary_path.exists(),
        "declared artifact missing on disk"
    );
    let summary = std::fs::read_to_string(&output_summary_path).unwrap();
    assert!(
        summary.contains("Compat Summary"),
        "summary missing heading: {summary}"
    );
    assert!(
        summary.contains("COMPAT_SUMMARY_TOKEN: declared"),
        "summary should mark the allowlisted env var as declared: {summary}"
    );

    // ── Secret invariant: value must never leak into any captured surface ──
    let installed_tree = concat_all_files_under(&installed_skill);
    assert!(
        !installed_tree.contains(secret_value),
        "secret VALUE leaked into installed skill tree"
    );
    assert!(
        !stdout.contains(secret_value),
        "secret VALUE leaked into skill stdout"
    );
    assert!(
        !stderr.contains(secret_value),
        "secret VALUE leaked into skill stderr"
    );
    assert!(
        !summary.contains(secret_value),
        "secret VALUE leaked into produced artifact"
    );

    // ── Phase 4: reload — artifacts and skill survive a fresh scan ─────
    // Simulate a runtime reload by re-running `octos skills list` (fresh process)
    // and re-invoking the skill against the already-delivered artifact.
    let list_after_run = run_octos_skills(cwd, &["list"]);
    assert!(list_after_run.status.success());
    assert!(
        String::from_utf8_lossy(&list_after_run.stdout).contains("compat-test-skill"),
        "skill disappeared after reload"
    );
    assert!(
        output_summary_path.exists(),
        "delivered artifact must survive reload"
    );

    // Rerun: the skill must be idempotent and still produce the contract shape.
    let rerun_output = cwd.join("summary-2.md");
    let (ok2, stdout2, _) = run_skill_main(
        &installed_skill,
        "summarize_text",
        &json!({
            "input_path": input_text_path.to_string_lossy(),
            "output_path": rerun_output.to_string_lossy(),
        }),
        &[],
    );
    assert!(ok2, "rerun after reload failed: {stdout2}");
    let response2: Value = serde_json::from_str(stdout2.trim()).unwrap();
    assert_eq!(response2["success"], Value::Bool(true));
    assert!(rerun_output.exists(), "rerun artifact missing");

    // ── Phase 5: remove — skill state is fully cleaned up ──────────────
    let remove_out = run_octos_skills(cwd, &["remove", "compat-test-skill"]);
    assert!(
        remove_out.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove_out.stderr)
    );
    assert!(
        !installed_skill.exists(),
        "skill directory must be gone after remove"
    );

    // `octos skills list` no longer reports the skill.
    let list_after_remove = run_octos_skills(cwd, &["list"]);
    assert!(list_after_remove.status.success());
    assert!(
        !String::from_utf8_lossy(&list_after_remove.stdout).contains("compat-test-skill"),
        "skill still listed after remove"
    );

    // ── Phase 6: idempotent uninstall ──────────────────────────────────
    // Required invariant 3: removing an already-absent skill must not error.
    let remove_twice = run_octos_skills(cwd, &["remove", "compat-test-skill"]);
    assert!(
        remove_twice.status.success(),
        "second remove must be idempotent: stdout={} stderr={}",
        String::from_utf8_lossy(&remove_twice.stdout),
        String::from_utf8_lossy(&remove_twice.stderr),
    );
}

#[test]
fn should_reject_missing_required_fields_when_fixture_is_invoked_without_input() {
    // Documents the failure contract — actionable errors, not silent success.
    let fixture = fixture_skill_dir();
    let (ok, stdout, _) = run_skill_main(&fixture, "summarize_text", &json!({}), &[]);
    assert!(!ok, "missing fields must fail");
    let response: Value = serde_json::from_str(stdout.trim()).expect("JSON error response");
    assert_eq!(response["success"], Value::Bool(false));
    let msg = response["output"].as_str().unwrap_or_default();
    assert!(
        msg.contains("Missing required field"),
        "error message must be actionable: {msg}"
    );
}

#[test]
fn should_reject_unknown_tool_with_actionable_message() {
    let fixture = fixture_skill_dir();
    let (ok, stdout, _) = run_skill_main(&fixture, "not_a_tool", &json!({}), &[]);
    assert!(!ok);
    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["success"], Value::Bool(false));
    let msg = response["output"].as_str().unwrap_or_default();
    assert!(
        msg.contains("Unknown tool") && msg.contains("summarize_text"),
        "error must name the expected tool: {msg}"
    );
}
