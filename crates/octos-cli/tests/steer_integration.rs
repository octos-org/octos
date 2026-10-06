//! OLP-CTRL steer cross-process integration test (#8b).
//!
//! Moved OUT of `commands/steer.rs`'s inline `#[cfg(test)]` module: the
//! `option_env!("CARGO_BIN_EXE_octos")` + `target/debug/octos` fallback
//! both resolve to nothing in the CI lib-test environment (the binary is
//! not built for lib tests), making the inline test fail spuriously. In
//! the `tests/` integration directory `env!("CARGO_BIN_EXE_octos")` is
//! GUARANTEED to exist — Cargo builds the binary before running
//! integration tests — so no fallback is needed (deleted per #8b).
//!
//! Scope: the subprocess half of the round-4 contract — the REAL `octos`
//! CLI binary, run as a subprocess, queues a steer by writing the
//! reviewer-notes sidecar + session marker into the instance inbox. The
//! serve-side sweep half (`steer_inbox_sweep` → continuation) stays
//! covered by `olp_ctrl_steer_cross_process_sweep_enqueues` in the lib
//! tests (it needs the crate-private orchestrator, unreachable here).

use std::path::{Path, PathBuf};
use std::process::Command;

fn project_sessions_root(canonical_cwd: &Path, profile_id: &str) -> PathBuf {
    canonical_cwd.join(".octos").join(profile_id)
}

fn encode_path_component(s: &str) -> String {
    let mut encoded = String::new();
    for byte in s.as_bytes() {
        if byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_' {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn hash_session_for_inbox(session_id: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[test]
fn olp_ctrl_steer_subprocess_cli_writes_sidecar_and_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("proj");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let session = "prod:local:tui#coding";
    // Seed a REAL session transcript so the CLI's existence check passes.
    let root = project_sessions_root(&cwd, "octos");
    let base = session.split('#').next().expect("base");
    let topic = session.split('#').nth(1).unwrap_or("default");
    let transcript = root
        .join("sessions")
        .join(encode_path_component(base))
        .join("sessions")
        .join(format!("{}.jsonl", encode_path_component(topic)));
    std::fs::create_dir_all(transcript.parent().expect("parent")).expect("mkdir");
    std::fs::write(&transcript, "{}\n").expect("seed transcript");
    // Instance data dir under OCTOS_HOME.
    let state_home = temp.path().join("home");
    let instance_data = state_home.join("profiles").join("octos").join("data");
    std::fs::create_dir_all(&instance_data).expect("instance data");

    let output = Command::new(env!("CARGO_BIN_EXE_octos"))
        .args(["steer", "--session", session, "--text", "读黑板第 7 条"])
        .current_dir(&cwd)
        .env("OCTOS_HOME", &state_home)
        .output()
        .expect("run octos steer");
    assert!(
        output.status.success(),
        "steer CLI must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let safe = hash_session_for_inbox(session);
    let inbox = instance_data.join("inbox");
    let notes = inbox.join(format!("{safe}.reviewer-notes"));
    let marker = inbox.join(format!("{safe}.reviewer-session"));
    assert!(notes.exists(), "reviewer-notes written by the subprocess");
    assert!(marker.exists(), "reviewer-session marker written");
    let body = std::fs::read_to_string(&notes).expect("read notes");
    assert!(
        body.contains("读黑板第 7 条"),
        "steer text persisted: {body}"
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read marker")
            .trim(),
        session,
        "marker carries the raw session id"
    );

    // Negative control: an unknown session is refused with a non-zero exit
    // and writes NOTHING.
    let ghost = Command::new(env!("CARGO_BIN_EXE_octos"))
        .args(["steer", "--session", "ghost:nonexistent", "--text", "hi"])
        .current_dir(&cwd)
        .env("OCTOS_HOME", &state_home)
        .output()
        .expect("run octos steer ghost");
    assert!(!ghost.status.success(), "ghost session must be refused");
    let ghost_safe = hash_session_for_inbox("ghost:nonexistent");
    assert!(
        !inbox.join(format!("{ghost_safe}.reviewer-notes")).exists(),
        "no queue file for an unknown session"
    );
}
