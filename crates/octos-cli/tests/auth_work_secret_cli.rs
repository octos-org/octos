//! Issue #2444: work-secret CLI stdout stays machine-readable and list output
//! never exposes the bearer token.

use std::path::Path;
use std::process::Command;

use octos_agent::bridge::work_secret::WorkSecret;

fn run_octos(args: &[&str], data_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_octos"))
        .args(args)
        .arg("--data-dir")
        .arg(data_dir)
        .env("OCTOS_HOME", data_dir)
        .output()
        .expect("spawn octos")
}

#[test]
fn issue_stdout_stays_the_secret_and_list_never_shows_the_token() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path();
    let out = run_octos(
        &[
            "auth",
            "issue-work-secret",
            "--session",
            "cli:audit",
            "--ttl",
            "1h",
        ],
        data,
    );
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    let mut lines = stdout.lines();
    let encoded = lines.next().unwrap();
    assert!(
        lines.next().is_none(),
        "stdout must be the secret only, got: {stdout:?}"
    );
    let secret = WorkSecret::decode(encoded).unwrap();
    assert!(!secret.session_ingress_token.is_empty());

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("expires"), "stderr: {stderr:?}");
    assert!(stderr.contains("replaces"), "stderr: {stderr:?}");
    assert!(!stdout.contains("expires"));

    let list = run_octos(&["auth", "list-work-secrets"], data);
    assert!(list.status.success());
    let listed = String::from_utf8(list.stdout).unwrap();
    assert!(listed.contains("cli:audit"), "listed: {listed:?}");
    assert!(!listed.contains(&secret.session_ingress_token));
}
