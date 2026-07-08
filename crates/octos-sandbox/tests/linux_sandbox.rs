#![cfg(target_os = "linux")]

use std::process::Command;

fn helper() -> &'static str {
    env!("CARGO_BIN_EXE_octos-sandbox")
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

    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("linux-sandbox-test-{}", std::process::id()));
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

    let status = Command::new(helper())
        .arg("--cwd")
        .arg(&work)
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .status()
        .expect("octos-sandbox command should run");

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

    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("linux-sandbox-ro-test-{}", std::process::id()));
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // A pre-existing file we should still be able to READ under read-only cwd.
    let existing = work.join("existing.txt");
    std::fs::write(&existing, "readable\n").unwrap();
    let new_file = work.join("newfile.txt");

    // Reading the existing file must succeed (exit 0); creating a new file must
    // fail; and after the attempt the new file must NOT exist.
    let script = format!(
        "cat '{}' >/dev/null && if touch '{}' 2>/dev/null; then exit 43; else test ! -e '{}'; fi",
        existing.display(),
        new_file.display(),
        new_file.display(),
    );

    let status = Command::new(helper())
        .arg("--cwd")
        .arg(&work)
        .arg("--readonly-cwd")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .status()
        .expect("octos-sandbox --readonly-cwd command should run");

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
fn linux_sandbox_blocks_internet_socket_when_network_disabled() {
    require_linux_sandbox_supported();

    let Some(python_path) = ["/usr/bin/python3", "/bin/python3"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        eprintln!("SKIP: python3 not available in allowed exec dirs");
        return;
    };

    let work = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("linux-sandbox-net-test-{}", std::process::id()));
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

    let status = Command::new(helper())
        .arg("--cwd")
        .arg(&work)
        .arg("--")
        .arg(python_path)
        .arg("-c")
        .arg(script)
        .status()
        .expect("octos-sandbox network test should run");

    assert!(
        status.success(),
        "seccomp should deny AF_INET sockets with EPERM"
    );

    let _ = std::fs::remove_dir_all(work);
}
