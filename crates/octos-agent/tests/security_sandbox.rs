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

fn skip_without_sandbox() -> bool {
    if sandbox_exec_available() {
        return false;
    }
    eprintln!("SKIP: sandbox-exec not available");
    true
}

fn canonical_workspace(dir: &tempfile::TempDir) -> String {
    std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string()
}

struct SandboxWorkspace {
    _dir: tempfile::TempDir,
    real_workspace: String,
}

impl SandboxWorkspace {
    fn new() -> Option<Self> {
        if skip_without_sandbox() {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let real_workspace = canonical_workspace(&dir);
        Some(Self {
            _dir: dir,
            real_workspace,
        })
    }

    fn path(&self) -> &str {
        self._dir.path().to_str().unwrap()
    }

    fn run(&self, cmd: &str) -> (i32, String, String) {
        let profile = octos_sbpl(&self.real_workspace, false);
        run_sandboxed(&profile, cmd)
    }
}

fn require_python3() {
    if !Path::new("/usr/bin/python3").exists()
        && Command::new("which")
            .arg("python3")
            .output()
            .map(|output| !output.status.success())
            .unwrap_or(true)
    {
        panic!("macOS sandbox integration tests require python3");
    }
}

fn assert_permission_denied(stderr: &str, message: &str) {
    assert!(
        stderr.contains("Operation not permitted") || stderr.contains("Permission denied"),
        "{message}, got: {stderr}"
    );
}

// ── Write isolation tests ──────────────────────────────────────────────────

#[test]
fn should_allow_write_inside_workspace() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    let path = workspace.path();
    let (code, stdout, _stderr) = workspace.run(&format!(
        "echo 'hello' > {path}/test.txt && cat {path}/test.txt"
    ));
    assert_eq!(code, 0, "should allow write inside workspace");
    assert!(stdout.contains("hello"));
}

#[test]
fn should_block_write_outside_workspace() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    // Try to write to a temp file outside workspace
    let (code, _stdout, stderr) =
        workspace.run("echo 'escape' > /tmp/octos-security-test-escape.txt");
    assert_ne!(code, 0, "should block write outside workspace");
    assert_permission_denied(&stderr, "should get permission error");
}

#[test]
fn should_block_write_to_etc() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    let (code, _stdout, stderr) = workspace.run("echo 'pwned' > /etc/octos-security-test.txt");
    assert_ne!(code, 0, "should block write to /etc");
    assert_permission_denied(&stderr, "should get permission error writing to /etc");
}

#[test]
fn should_block_write_to_home() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    let (code, _stdout, _stderr) =
        workspace.run("echo 'escape' > ~/octos-security-test-escape.txt 2>&1 || echo BLOCKED");
    // Either fails (exit 1) or we see BLOCKED
    let (_, stdout2, _) =
        workspace.run("test -f ~/octos-security-test-escape.txt && echo EXISTS || echo MISSING");
    assert!(
        stdout2.contains("MISSING") || code != 0,
        "should not be able to write to home directory"
    );
}

// ── /tmp loophole tests ──────────────────────────────────────────────────

#[test]
fn should_block_tmp_write_without_tmp_allowance() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    // octos SBPL without /private/tmp allowance (the fix)
    let (code, _stdout, stderr) = workspace.run(
        "mkdir -p /tmp/octos-security-test-tmp && echo 'escape' > /tmp/octos-security-test-tmp/evil.txt",
    );
    assert_ne!(code, 0, "should block writes to /tmp when not allowed");
    assert_permission_denied(&stderr, "should get permission error for /tmp write");
}

#[test]
fn should_allow_tmp_write_with_explicit_tmp_allowance() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    // Old-style SBPL WITH /private/tmp allowance (demonstrates the loophole)
    let real_workspace = &workspace.real_workspace;
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
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    require_python3();
    let (code, stdout, _stderr) = workspace.run(
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
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    require_python3();
    let path = workspace.path();
    let (code, stdout, _stderr) = workspace.run(&format!(
        r#"python3 -c "
open('{path}/output.txt', 'w').write('allowed')
print('OK')
""#,
    ));
    assert_eq!(code, 0, "Python should write inside workspace");
    assert!(stdout.contains("OK"));
}

#[test]
fn should_redirect_python_tempfile_via_tmpdir() {
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    require_python3();
    let path = workspace.path();
    // Create per-user tmp inside workspace
    std::fs::create_dir_all(workspace._dir.path().join("tmp")).unwrap();
    let (code, stdout, _stderr) = workspace.run(&format!(
        r#"TMPDIR={path}/tmp python3 -c "
import tempfile
with tempfile.NamedTemporaryFile(mode='w', suffix='.tmp', delete=False) as f:
    f.write('temp data')
    inside = f.name.startswith('{path}/')
    print(f'TMPDIR_OK:{{inside}}')
    print(f'PATH:{{f.name}}')
""#,
    ));
    assert_eq!(code, 0, "tempfile should work with TMPDIR redirect");
    assert!(
        stdout.contains("TMPDIR_OK:True"),
        "tempfile should be inside workspace, got: {stdout}"
    );
}

// ── Cross-user isolation tests ───────────────────────────────────────────

#[test]
fn should_isolate_two_users_simultaneously() {
    let Some(user_a) = SandboxWorkspace::new() else {
        return;
    };
    let user_b = SandboxWorkspace::new().unwrap();
    let workspace_a = user_a.path();
    let workspace_b = user_b.path();

    let (code_a, _, _) = user_a.run(&format!("echo 'user_a_data' > {workspace_a}/data.txt"));
    assert_eq!(code_a, 0, "user A should write to own workspace");

    let (code_b, _, _) = user_b.run(&format!("echo 'user_b_data' > {workspace_b}/data.txt"));
    assert_eq!(code_b, 0, "user B should write to own workspace");

    // User A cannot read user B's data (file-read* is global, but writing was blocked above)
    // User A tries to write to user B
    let (code_cross, _stdout, stderr) =
        user_a.run(&format!("echo 'stolen' > {workspace_b}/evil.txt"));
    assert_ne!(code_cross, 0, "user A should not write to user B");
    assert_permission_denied(&stderr, "should get permission error for cross-user write");
}

// ── Read isolation tests ──────────────────────────────────────────────────

#[test]
fn should_restrict_reads_when_configured() {
    if skip_without_sandbox() {
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
    let Some(workspace) = SandboxWorkspace::new() else {
        return;
    };
    // Try to make a network connection
    let (code, _stdout, _stderr) =
        workspace.run("curl -s --max-time 3 http://example.com > /dev/null 2>&1");
    // Should fail (network denied)
    assert_ne!(
        code, 0,
        "network should be blocked when allow_network=false"
    );
}

#[test]
fn should_reject_absolute_paths() {
    use octos_agent::tools::resolve_path;
    let base = Path::new("/home/user/workspace");
    assert!(resolve_path(base, "/etc/passwd").is_err());
    assert!(resolve_path(base, "/tmp/evil").is_err());
}

#[test]
fn should_reject_path_traversal() {
    use octos_agent::tools::resolve_path;
    let base = Path::new("/home/user/workspace");
    assert!(resolve_path(base, "../../etc/passwd").is_err());
    assert!(resolve_path(base, "src/../../etc/passwd").is_err());
    assert!(resolve_path(base, "../other-user/secrets").is_err());
}
