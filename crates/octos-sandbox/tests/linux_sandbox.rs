#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_octos-sandbox")
}

fn test_dir(name: &str) -> PathBuf {
    std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("{name}-{}", std::process::id()))
}

fn run_script(work: &Path, program: &str, script: &str, readonly: bool) -> ExitStatus {
    let mut command = Command::new(helper());
    command.arg("--cwd").arg(work);
    if readonly {
        command.arg("--readonly-cwd");
    }
    command
        .arg("--")
        .arg(program)
        .arg("-c")
        .arg(script)
        .status()
        .expect("octos-sandbox command should run")
}

fn readonly_write_probe(work: &Path, failure_exit_code: u8) -> (PathBuf, String) {
    let existing = work.join("existing.txt");
    std::fs::write(&existing, "readable\n").unwrap();
    let new_file = work.join("newfile.txt");
    let script = format!(
        "cat '{}' >/dev/null && if touch '{}' 2>/dev/null; then exit {failure_exit_code}; else test ! -e '{}'; fi",
        existing.display(),
        new_file.display(),
        new_file.display(),
    );
    (new_file, script)
}

fn require_linux_sandbox_supported() {
    let output = Command::new(helper())
        .arg("--probe-linux")
        .output()
        .expect("octos-sandbox probe should run");
    assert!(
        output.status.success(),
        "Linux Landlock/seccomp probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn linux_probe_succeeds_when_kernel_supports_landlock_seccomp() {
    require_linux_sandbox_supported();
}

#[test]
fn linux_sandbox_allows_cwd_write_and_blocks_sibling_write() {
    require_linux_sandbox_supported();

    let root = test_dir("linux-sandbox-test");
    let work = root.join("work");
    let outside = root.join("outside");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let inside_file = work.join("inside.txt");
    let outside_file = outside.join("escape.txt");
    let script = format!(
        "echo ok > '{}' && if echo bad > '{}'; then exit 42; else test ! -e '{}'; fi",
        inside_file.display(),
        outside_file.display(),
        outside_file.display(),
    );

    let status = run_script(&work, "sh", &script, false);

    assert!(status.success(), "sandboxed command should pass");
    assert_eq!(std::fs::read_to_string(&inside_file).unwrap(), "ok\n");
    assert!(
        !outside_file.exists(),
        "Landlock should block writes outside cwd and /tmp"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn linux_sandbox_readonly_cwd_blocks_cwd_write() {
    // P1 (codex): with `--readonly-cwd` the helper must grant the workspace
    // cwd READ-ONLY, so `touch newfile` inside the cwd fails (the
    // `--sandbox read-only` shell-write hole). Reads inside the cwd still work.
    require_linux_sandbox_supported();

    let root = test_dir("linux-sandbox-ro-test");
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // Reading the existing file must succeed (exit 0); creating a new file must
    // fail; and after the attempt the new file must NOT exist.
    let (new_file, script) = readonly_write_probe(&work, 43);

    let status = run_script(&work, "sh", &script, true);

    assert!(
        status.success(),
        "read-only cwd must allow reads but block writes"
    );
    assert!(
        !new_file.exists(),
        "Landlock read-only cwd must block workspace writes"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn linux_sandbox_readonly_cwd_under_tmp_blocks_cwd_write() {
    // P1 (codex, round 3): Landlock rules are ADDITIVE and cannot subtract a
    // subtree, so if the read-only workspace lives UNDER /tmp, a broad write
    // grant to /tmp would re-permit `touch $cwd/newfile`. The helper must drop
    // that overlapping grant. A read-only run whose cwd is under /tmp must
    // block writes to the cwd, while a dedicated scratch dir (TMPDIR) outside
    // the workspace still works.
    require_linux_sandbox_supported();

    // Workspace lives directly under /tmp (the hazardous case).
    let work =
        std::path::PathBuf::from("/tmp").join(format!("octos-ro-under-tmp-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    // Reads inside cwd must succeed; writing a NEW file inside cwd must fail;
    // and the new file must not exist afterwards.
    let (new_file, script) = readonly_write_probe(&work, 44);

    let status = run_script(&work, "sh", &script, true);

    assert!(
        status.success(),
        "read-only cwd under /tmp must allow reads but block workspace writes"
    );
    assert!(
        !new_file.exists(),
        "Landlock must block writes to a read-only cwd even when it is under /tmp"
    );

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn linux_sandbox_blocks_internet_socket_when_network_disabled() {
    require_linux_sandbox_supported();

    let Some(python_path) = ["/usr/bin/python3", "/bin/python3"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        eprintln!("SKIP: python3 not available in allowed exec dirs");
        return;
    };

    let work = test_dir("linux-sandbox-net-test");
    std::fs::create_dir_all(&work).unwrap();

    let script = r#"
import errno
import socket
import sys

try:
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
except OSError as exc:
    sys.exit(0 if exc.errno == errno.EPERM else 2)
else:
    sys.exit(42)
"#;

    let status = run_script(&work, python_path, script, false);

    assert!(
        status.success(),
        "seccomp should deny AF_INET sockets with EPERM"
    );

    let _ = std::fs::remove_dir_all(work);
}
