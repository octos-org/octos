//! Security integration tests for sandbox and file isolation.
//!
//! These tests exercise the real macOS sandbox-exec (SBPL) and verify that
//! kernel-enforced isolation actually blocks writes, reads, and escapes.
//!
//! Run with: `cargo test -p octos-agent --test security_sandbox`
//! Some tests require macOS with sandbox-exec (skipped on other platforms).

use std::path::Path;
use std::process::Command;

/// Helper: run a shell command inside a sandbox-exec with given SBPL profile.
/// Returns (exit_code, stdout, stderr).
fn run_sandboxed(profile: &str, cmd: &str) -> (i32, String, String) {
    let output = Command::new("sandbox-exec")
        .arg("-p")
        .arg(profile)
        .arg("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .expect("failed to run sandbox-exec");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Helper: build a octos-style SBPL profile for a workspace.
fn octos_sbpl(workspace: &str, allow_network: bool) -> String {
    let network = if allow_network {
        "(allow network*)"
    } else {
        "(deny network*)"
    };
    format!(
        r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow process-info*)
(allow sysctl-read)
(allow mach-lookup)
(allow mach-register)
(allow ipc-posix*)
(allow signal)
(allow file-ioctl)
(allow file-read*)
(allow file-write* (subpath "{workspace}"))
{network}
"#,
    )
}

fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

fn sandbox_exec_available() -> bool {
    if !is_macos() {
        return false;
    }
    assert!(
        Path::new("/usr/bin/sandbox-exec").exists(),
        "macOS sandbox integration tests require /usr/bin/sandbox-exec"
    );
    true
}

// ── Write isolation tests ──────────────────────────────────────────────────

#[test]
fn should_allow_write_inside_workspace() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().to_str().unwrap();
    // macOS: canonicalize to resolve /tmp → /private/tmp
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    let (code, stdout, _stderr) = run_sandboxed(
        &profile,
        &format!("echo 'hello' > {workspace}/test.txt && cat {workspace}/test.txt"),
    );
    assert_eq!(code, 0, "should allow write inside workspace");
    assert!(stdout.contains("hello"));
}

#[test]
fn should_block_write_outside_workspace() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    // Try to write to a temp file outside workspace
    let (code, _stdout, stderr) = run_sandboxed(
        &profile,
        "echo 'escape' > /tmp/octos-security-test-escape.txt",
    );
    assert_ne!(code, 0, "should block write outside workspace");
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "should get permission error, got: {stderr}"
    );
}

#[test]
fn should_block_write_to_etc() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    let (code, _stdout, stderr) =
        run_sandboxed(&profile, "echo 'pwned' > /etc/octos-security-test.txt");
    assert_ne!(code, 0, "should block write to /etc");
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "should get permission error writing to /etc"
    );
}

#[test]
fn should_block_write_to_home() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    let (code, _stdout, _stderr) = run_sandboxed(
        &profile,
        "echo 'escape' > ~/octos-security-test-escape.txt 2>&1 || echo BLOCKED",
    );
    // Either fails (exit 1) or we see BLOCKED
    let (_, stdout2, _) = run_sandboxed(
        &profile,
        "test -f ~/octos-security-test-escape.txt && echo EXISTS || echo MISSING",
    );
    assert!(
        stdout2.contains("MISSING") || code != 0,
        "should not be able to write to home directory"
    );
}

// ── /tmp loophole tests ──────────────────────────────────────────────────

#[test]
fn should_block_tmp_write_without_tmp_allowance() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    // octos SBPL without /private/tmp allowance (the fix)
    let profile = octos_sbpl(&real_workspace, false);
    let (code, _stdout, stderr) = run_sandboxed(
        &profile,
        "mkdir -p /tmp/octos-security-test-tmp && echo 'escape' > /tmp/octos-security-test-tmp/evil.txt",
    );
    assert_ne!(code, 0, "should block writes to /tmp when not allowed");
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "should get permission error for /tmp write"
    );
}

#[test]
fn should_allow_tmp_write_with_explicit_tmp_allowance() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    // Old-style SBPL WITH /private/tmp allowance (demonstrates the loophole)
    let profile = format!(
        r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow sysctl-read)
(allow file-read*)
(allow file-write* (subpath "{real_workspace}"))
(allow file-write* (subpath "/private/tmp"))
"#,
    );
    let (code, _stdout, _stderr) = run_sandboxed(
        &profile,
        "echo 'escape' > /tmp/octos-security-test-loophole.txt",
    );
    assert_eq!(
        code, 0,
        "/tmp loophole should exist with explicit /private/tmp allowance"
    );
    // Clean up
    let _ = std::fs::remove_file("/tmp/octos-security-test-loophole.txt");
}

// ── Python escape tests ──────────────────────────────────────────────────

#[test]
fn should_block_python_write_outside_workspace() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    // Check python3 is available
    if !Path::new("/usr/bin/python3").exists()
        && Command::new("which")
            .arg("python3")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
    {
        panic!("macOS sandbox integration tests require python3");
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    let (code, stdout, _stderr) = run_sandboxed(
        &profile,
        r#"python3 -c "
import os
try:
    os.makedirs('/tmp/octos-security-test-python', exist_ok=True)
    open('/tmp/octos-security-test-python/evil.txt', 'w').write('escape')
    print('BAD: escaped')
except PermissionError:
    print('BLOCKED')
""#,
    );
    assert!(
        stdout.contains("BLOCKED") || code != 0,
        "Python should not be able to write outside workspace, got stdout: {stdout}"
    );
}

#[test]
fn should_allow_python_write_inside_workspace() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    if !Path::new("/usr/bin/python3").exists()
        && Command::new("which")
            .arg("python3")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
    {
        panic!("macOS sandbox integration tests require python3");
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().to_str().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    let (code, stdout, _stderr) = run_sandboxed(
        &profile,
        &format!(
            r#"python3 -c "
open('{workspace}/output.txt', 'w').write('allowed')
print('OK')
""#,
        ),
    );
    assert_eq!(code, 0, "Python should write inside workspace");
    assert!(stdout.contains("OK"));
}

#[test]
fn should_redirect_python_tempfile_via_tmpdir() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    if !Path::new("/usr/bin/python3").exists()
        && Command::new("which")
            .arg("python3")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
    {
        panic!("macOS sandbox integration tests require python3");
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().to_str().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    // Create per-user tmp inside workspace
    std::fs::create_dir_all(dir.path().join("tmp")).unwrap();

    let profile = octos_sbpl(&real_workspace, false);
    let (code, stdout, _stderr) = run_sandboxed(
        &profile,
        &format!(
            r#"TMPDIR={workspace}/tmp python3 -c "
import tempfile
with tempfile.NamedTemporaryFile(mode='w', suffix='.tmp', delete=False) as f:
    f.write('temp data')
    inside = f.name.startswith('{workspace}/')
    print(f'TMPDIR_OK:{{inside}}')
    print(f'PATH:{{f.name}}')
""#,
        ),
    );
    assert_eq!(code, 0, "tempfile should work with TMPDIR redirect");
    assert!(
        stdout.contains("TMPDIR_OK:True"),
        "tempfile should be inside workspace, got: {stdout}"
    );
}

// ── Cross-user isolation tests ───────────────────────────────────────────

#[test]
fn should_block_cross_user_write() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let user_a_dir = tempfile::tempdir().unwrap();
    let user_b_dir = tempfile::tempdir().unwrap();
    let real_a = std::fs::canonicalize(user_a_dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let user_b_path = user_b_dir.path().to_str().unwrap();

    // User A's sandbox
    let profile = octos_sbpl(&real_a, false);
    // Try to write to user B's workspace
    let (code, _stdout, stderr) = run_sandboxed(
        &profile,
        &format!("echo 'cross-user' > {user_b_path}/stolen.txt"),
    );
    assert_ne!(code, 0, "user A should not write to user B's workspace");
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "should get permission error for cross-user write"
    );
}

#[test]
fn should_isolate_two_users_simultaneously() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let user_a_dir = tempfile::tempdir().unwrap();
    let user_b_dir = tempfile::tempdir().unwrap();
    let real_a = std::fs::canonicalize(user_a_dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let real_b = std::fs::canonicalize(user_b_dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let workspace_a = user_a_dir.path().to_str().unwrap();
    let workspace_b = user_b_dir.path().to_str().unwrap();

    // User A writes to own workspace
    let profile_a = octos_sbpl(&real_a, false);
    let (code_a, _, _) = run_sandboxed(
        &profile_a,
        &format!("echo 'user_a_data' > {workspace_a}/data.txt"),
    );
    assert_eq!(code_a, 0, "user A should write to own workspace");

    // User B writes to own workspace
    let profile_b = octos_sbpl(&real_b, false);
    let (code_b, _, _) = run_sandboxed(
        &profile_b,
        &format!("echo 'user_b_data' > {workspace_b}/data.txt"),
    );
    assert_eq!(code_b, 0, "user B should write to own workspace");

    // User A cannot read user B's data (file-read* is global, but writing was blocked above)
    // User A tries to write to user B
    let (code_cross, _stdout, _stderr) = run_sandboxed(
        &profile_a,
        &format!("echo 'stolen' > {workspace_b}/evil.txt"),
    );
    assert_ne!(code_cross, 0, "user A should not write to user B");
}

// ── Read isolation tests ──────────────────────────────────────────────────

#[test]
fn should_restrict_reads_when_configured() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }

    // Verify the MacosSandbox generates restricted read rules when read_allow_paths is set.
    // Note: macOS SBPL with subpath-only reads prevents even `sh` from starting
    // (needs mach-lookup, dyld shared cache access, etc.), so we test the profile
    // generation rather than live execution with restricted reads.
    let config = octos_agent::SandboxConfig {
        allow_toolchains: true,
        enabled: true,
        mode: octos_agent::sandbox::SandboxMode::Macos,
        fail_closed: false,
        allow_network: false,
        workspace_write: true,
        repo_git_write: None,
        docker: octos_agent::sandbox::DockerConfig::default(),
        read_allow_paths: vec!["/usr".into(), "/opt/custom".into()],
        write_allow_globs: None,
        profile_name: None,
        build_cache_slot: None,
        read_only_view: None,
    };
    let sandbox = octos_agent::create_sandbox(&config);
    let dir = tempfile::tempdir().unwrap();
    let cmd = sandbox.wrap_command("echo test", dir.path());
    let args: Vec<String> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    // The SBPL profile is the second arg (after -p flag)
    // args: ["-p", "<profile>", "sh", "-c", "<cmd>"]
    assert!(args.len() >= 2, "expected at least 2 args, got: {args:?}");
    let profile = &args[1];
    // Should NOT contain "(allow file-read*)" as a standalone global rule
    assert!(
        !profile.contains("(allow file-read*)\n"),
        "restricted read mode should not have global file-read*"
    );
    // Should contain subpath rules for /usr and /opt/custom
    assert!(
        profile.contains(r#"(allow file-read* (subpath "/usr"))"#),
        "should have /usr read rule"
    );
    assert!(
        profile.contains(r#"(allow file-read* (subpath "/opt/custom"))"#),
        "should have /opt/custom read rule"
    );

    // Also verify that deny-list approach works in practice (live test)
    // Using SBPL (allow default) then (deny file-read* (subpath ...))
    let deny_profile = r#"(version 1)
(deny default)
(allow default)
(deny file-read* (subpath "/Users"))
(deny network*)
"#;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/nobody".into());
    if std::path::Path::new(&home).exists() {
        let (code, _, _) = run_sandboxed(deny_profile, &format!("ls {home}"));
        assert_ne!(code, 0, "should deny reading /Users with deny-list SBPL");
    }
}

// ── Network isolation tests ──────────────────────────────────────────────

#[test]
fn should_block_network_when_denied() {
    if !sandbox_exec_available() {
        eprintln!("SKIP: sandbox-exec not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let real_workspace = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();

    let profile = octos_sbpl(&real_workspace, false);
    // Try to make a network connection
    let (code, _stdout, _stderr) = run_sandboxed(
        &profile,
        "curl -s --max-time 3 http://example.com > /dev/null 2>&1",
    );
    // Should fail (network denied)
    assert_ne!(
        code, 0,
        "network should be blocked when allow_network=false"
    );
}
