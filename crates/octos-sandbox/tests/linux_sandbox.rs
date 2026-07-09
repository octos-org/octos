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
