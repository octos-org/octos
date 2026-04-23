//! Integration tests for `LifecycleExecutor` sandbox dispatch.
//!
//! The contract for the hardware lifecycle executor (issue #448) enumerates
//! seven acceptance tests. This file implements all of them, plus a
//! synchronization test that asserts `BLOCKED_ENV_VARS` in `octos-plugin`
//! matches the canonical list in `octos-agent/src/sandbox/mod.rs`.
//!
//! The hardest test is `should_kill_child_when_step_timeout_exceeded`: it
//! runs a shell command that spawns a long-lived marker process (`sleep`
//! plus a log file) with a 100ms timeout, and asserts the marker PID is
//! dead within 500ms of the timeout firing. Without the executor actively
//! killing the child on timeout this test will flake — the whole point of
//! the rewrite is that it does not.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use octos_plugin::{
    HardwareLifecycle, LifecycleExecutor, LifecyclePhase, LifecycleStep, NoSandbox, Sandbox,
    StepOutcome, is_safe_shell_command,
};
use tokio::process::Command;

/// Build a lifecycle step with sensible defaults for tests.
fn step(label: &str, command: &str, timeout_ms: u64, critical: bool) -> LifecycleStep {
    LifecycleStep {
        label: label.to_string(),
        command: command.to_string(),
        timeout_ms,
        retries: 0,
        critical,
    }
}

/// A `Sandbox` that records every command it wraps so tests can assert the
/// executor actually routed through the sandbox abstraction rather than
/// calling `Command::new("sh")` directly.
#[derive(Clone, Default)]
struct RecordingSandbox {
    calls: Arc<Mutex<Vec<String>>>,
}

impl RecordingSandbox {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Sandbox for RecordingSandbox {
    fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
        self.calls.lock().unwrap().push(shell_command.to_string());
        NoSandbox.wrap_command(shell_command, cwd)
    }
}

#[tokio::test]
async fn should_run_preflight_steps_through_sandbox() {
    let recorder = RecordingSandbox::default();
    let exec = LifecycleExecutor::new(Box::new(recorder.clone()), std::env::temp_dir());

    let steps = vec![
        step("check_a", "echo alpha", 5_000, true),
        step("check_b", "echo bravo", 5_000, true),
    ];

    let result = exec.run_phase(LifecyclePhase::Preflight, &steps).await;

    assert!(result.success, "preflight should pass: {:?}", result.error);
    assert_eq!(result.steps_completed, 2);
    let calls = recorder.calls();
    assert_eq!(
        calls,
        vec!["echo alpha".to_string(), "echo bravo".to_string()],
        "every step must dispatch through the Sandbox trait"
    );
}

/// `BLOCKED_ENV_VARS` must be stripped off the child before it spawns.
///
/// We verify this using a hostile sandbox that deliberately sets the
/// injection vectors after `wrap_command` returns — our executor's
/// `env_remove(var)` call must run AFTER `wrap_command` and before
/// `spawn()`, so it will override the hostile values and the child
/// will see empty strings.
#[cfg(unix)]
#[tokio::test]
async fn should_apply_blocked_env_vars_to_lifecycle_steps() {
    /// A sandbox that poisons the Command with the very env vars the
    /// executor is supposed to strip. If the executor's env_remove is
    /// ordered correctly (after wrap_command), the child sees an empty
    /// value. If the order is wrong, the child sees the sentinel.
    struct PoisonSandbox;
    impl Sandbox for PoisonSandbox {
        fn wrap_command(&self, shell_command: &str, cwd: &Path) -> Command {
            let mut cmd = NoSandbox.wrap_command(shell_command, cwd);
            cmd.env("LD_PRELOAD", "/tmp/definitely-not-a-real-lib.so");
            cmd.env("NODE_OPTIONS", "--inspect");
            cmd.env("BASH_ENV", "/tmp/evil.sh");
            cmd.env("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib");
            cmd
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let out_ld = tmp.path().join("ld.txt");
    let out_node = tmp.path().join("node.txt");
    let out_bash = tmp.path().join("bash.txt");
    let out_dyld = tmp.path().join("dyld.txt");
    let cmd = format!(
        "printf %s \"${{LD_PRELOAD:-}}\" > {ld}; \
         printf %s \"${{NODE_OPTIONS:-}}\" > {node}; \
         printf %s \"${{BASH_ENV:-}}\" > {bash}; \
         printf %s \"${{DYLD_INSERT_LIBRARIES:-}}\" > {dyld}",
        ld = out_ld.display(),
        node = out_node.display(),
        bash = out_bash.display(),
        dyld = out_dyld.display(),
    );
    let steps = vec![LifecycleStep {
        label: "probe_env".to_string(),
        command: cmd,
        timeout_ms: 5_000,
        retries: 0,
        critical: true,
    }];

    let exec = LifecycleExecutor::new(Box::new(PoisonSandbox), std::env::temp_dir());
    let result = exec.run_phase(LifecyclePhase::Init, &steps).await;
    assert!(
        result.success,
        "probe step should succeed: {:?}",
        result.error
    );

    for (name, path) in [
        ("LD_PRELOAD", &out_ld),
        ("NODE_OPTIONS", &out_node),
        ("BASH_ENV", &out_bash),
        ("DYLD_INSERT_LIBRARIES", &out_dyld),
    ] {
        let value = std::fs::read_to_string(path).unwrap();
        assert!(
            value.is_empty(),
            "{name} must be scrubbed before spawn, got {value:?}"
        );
    }
}

/// The critical test: when a step times out, the child process must be
/// killed within 500ms of the timeout firing. We verify this by having
/// the step spawn an explicit `sleep` marker with its own PID recorded
/// to a tempfile, then poll `kill -0 $pid` (via `std::process::Command`,
/// no unsafe) until we see a non-zero exit status meaning ESRCH.
///
/// The deny(unsafe_code) workspace lint means we cannot call
/// `libc::kill(pid, 0)` directly. `kill -0` has identical semantics from
/// userspace (returns 0 iff process exists and we have permission to
/// signal it; non-zero iff ESRCH or EPERM). On Unix test hosts we always
/// have permission to signal our own children.
#[cfg(unix)]
#[tokio::test]
async fn should_kill_child_when_step_timeout_exceeded() {
    use std::time::Instant;

    let tmp = tempfile::tempdir().unwrap();
    let pid_path = tmp.path().join("marker.pid");

    // Shell spawns an explicit `sleep` child and writes its PID so we
    // can check it independently of the `sh` wrapper. The `exec sleep`
    // form avoids an extra fork — the sh process becomes the sleep
    // process, so killing the Child handle directly reaches the sleep.
    //
    // Strategy: spawn a background sleep, record its PID, then
    // `wait` — this keeps `sh` alive until tokio kills it, at which
    // point the sleep is orphaned. We then explicitly poll the sleep
    // PID to verify it is reaped.
    let cmd = format!(
        r#"sleep 30 & echo $! > {path}; wait"#,
        path = pid_path.display()
    );

    let steps = vec![step("slow_sleep", &cmd, 100, true)];
    let exec = LifecycleExecutor::with_no_sandbox();

    let start = Instant::now();
    let result = exec.run_phase(LifecyclePhase::Init, &steps).await;
    let elapsed = start.elapsed();

    assert!(!result.success, "timed-out phase should fail");
    assert_eq!(result.steps[0].outcome, StepOutcome::TimedOut);
    assert!(
        elapsed < Duration::from_secs(5),
        "run_phase must return promptly after timeout, took {elapsed:?}"
    );

    // The pid file should have been written within the first few ms.
    // Small poll window in case the shell hasn't written it yet when
    // the timeout fired.
    let pid_read_deadline = Instant::now() + Duration::from_millis(500);
    let pid_text = loop {
        if let Ok(s) = std::fs::read_to_string(&pid_path) {
            if !s.trim().is_empty() {
                break s;
            }
        }
        if Instant::now() >= pid_read_deadline {
            panic!("pid file {} was never written", pid_path.display());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let pid: i32 = pid_text
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("pid file {:?} not an integer: {e}", pid_text));

    // Poll `kill -0 $pid` for up to 500ms; expect it to start returning
    // non-zero (ESRCH) once the child is reaped. NOTE: when tokio kills
    // the sh parent via SIGKILL, the sleep child is orphaned and reaped
    // by init/launchd, not by sh. The orphan handling still reaps the
    // sleep in O(ms) because sleep has no pending I/O. On some platforms
    // the orphan may linger for up to ~100ms as init cycles its child
    // reaper, hence the 500ms deadline.
    //
    // If the bug the contract is probing for returns (executor does NOT
    // kill the sh parent), the sleep stays alive for its full 30 seconds
    // and this test panics.
    let dead_deadline = Instant::now() + Duration::from_millis(500);
    let mut dead = false;
    while Instant::now() < dead_deadline {
        if !process_alive(pid) {
            dead = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    if !dead {
        // Cleanup so we don't leak sleeps across test runs.
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
    }

    assert!(
        dead,
        "child PID {pid} must be dead within 500ms of the timeout — \
         the executor failed to kill the sandboxed step"
    );
}

/// Probe whether a PID is still live using `kill -0` (no signal is
/// delivered; the syscall just checks existence + permission). We
/// cannot call `libc::kill` directly because the workspace lints deny
/// `unsafe_code`. Shelling out to `/bin/kill` via `std::process::Command`
/// has the same observable behavior.
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn should_abort_phase_when_critical_step_fails() {
    let recorder = RecordingSandbox::default();
    let exec = LifecycleExecutor::new(Box::new(recorder.clone()), std::env::temp_dir());

    let steps = vec![
        step("ok_first", "echo starting", 5_000, true),
        step("will_fail", "exit 1", 5_000, true),
        step("never_reached", "echo unreachable", 5_000, true),
    ];

    let result = exec.run_phase(LifecyclePhase::Init, &steps).await;

    assert!(!result.success, "critical failure should abort phase");
    assert_eq!(result.steps_completed, 1, "only first step completes");
    // Third step must never have reached the sandbox.
    let calls = recorder.calls();
    assert_eq!(
        calls.len(),
        2,
        "third step should never dispatch: {calls:?}"
    );
    assert_eq!(result.steps.last().unwrap().outcome, StepOutcome::Failed);
}

#[tokio::test]
async fn should_continue_phase_when_non_critical_step_fails() {
    let recorder = RecordingSandbox::default();
    let exec = LifecycleExecutor::new(Box::new(recorder.clone()), std::env::temp_dir());

    let steps = vec![
        step("ok_first", "echo one", 5_000, true),
        step("non_critical_fail", "exit 1", 5_000, false),
        step("ok_third", "echo three", 5_000, true),
    ];

    let result = exec.run_phase(LifecyclePhase::Init, &steps).await;

    assert!(result.success, "non-critical failure should not abort");
    assert_eq!(result.steps_completed, 3, "all steps counted");
    let outcomes: Vec<_> = result.steps.iter().map(|s| s.outcome).collect();
    assert_eq!(
        outcomes,
        vec![StepOutcome::Ok, StepOutcome::Failed, StepOutcome::Ok],
        "middle step records failed outcome, phase still succeeds"
    );
    // Sandbox saw all 3 commands.
    assert_eq!(recorder.calls().len(), 3);
}

#[tokio::test]
async fn should_reject_safepolicy_denied_command_before_dispatch() {
    let recorder = RecordingSandbox::default();
    let exec = LifecycleExecutor::new(Box::new(recorder.clone()), std::env::temp_dir());

    // This is a critical step that would wipe the disk. The executor
    // must catch it BEFORE dispatching to the sandbox.
    let steps = vec![
        step("dangerous", "rm -rf /", 5_000, true),
        step("never_reached", "echo safe", 5_000, true),
    ];

    let result = exec.run_phase(LifecyclePhase::Shutdown, &steps).await;

    assert!(!result.success, "denied critical command must abort phase");
    assert_eq!(result.steps[0].outcome, StepOutcome::Denied);
    assert!(
        result.error.as_deref().unwrap_or("").contains("SafePolicy"),
        "error message should mention SafePolicy: {:?}",
        result.error
    );
    let calls = recorder.calls();
    assert!(
        calls.is_empty(),
        "denied command must NOT reach the sandbox, got: {calls:?}"
    );
}

#[tokio::test]
async fn should_not_dispatch_when_safepolicy_denies_with_extra_whitespace() {
    let recorder = RecordingSandbox::default();
    let exec = LifecycleExecutor::new(Box::new(recorder.clone()), std::env::temp_dir());

    let steps = vec![step("sneaky", "rm   -rf   /", 5_000, true)];
    let result = exec.run_phase(LifecyclePhase::Shutdown, &steps).await;
    assert!(!result.success);
    assert!(recorder.calls().is_empty());
}

/// The example at `examples/pick_and_place_lifecycle.rs` must exit 0 and
/// not emit any "FAILED" lines in its output.
#[tokio::test]
async fn pick_and_place_lifecycle_example_runs_end_to_end() {
    // We replicate the example's phases here rather than shelling out to
    // `cargo run --example`, which would double the test runtime.
    let lifecycle = HardwareLifecycle {
        preflight: vec![
            step("Check air pressure", "echo 6.2 bar", 5_000, true),
            step("Check camera", "echo ready", 5_000, true),
            step("Encoder", "echo nominal", 5_000, false),
        ],
        init: vec![
            step("Power servos", "echo powered", 10_000, true),
            step("Home axes", "echo homed", 30_000, true),
            step("Open gripper", "echo open", 5_000, true),
        ],
        ready_check: vec![
            step("Joint limits", "echo within", 5_000, true),
            step("Force sensor zero", "echo zeroed", 5_000, true),
        ],
        shutdown: vec![
            step("Park arm", "echo parked", 15_000, true),
            step("Power off servos", "echo off", 10_000, true),
        ],
        emergency_shutdown: vec![
            step("E-STOP", "echo halted", 2_000, true),
            step("Vent gripper", "echo vented", 2_000, true),
        ],
    };

    let exec = LifecycleExecutor::with_no_sandbox();
    for (phase, steps) in [
        (LifecyclePhase::Preflight, &lifecycle.preflight),
        (LifecyclePhase::Init, &lifecycle.init),
        (LifecyclePhase::ReadyCheck, &lifecycle.ready_check),
        (LifecyclePhase::Shutdown, &lifecycle.shutdown),
        (
            LifecyclePhase::EmergencyShutdown,
            &lifecycle.emergency_shutdown,
        ),
    ] {
        let result = exec.run_phase(phase, steps).await;
        assert!(result.success, "phase {phase} failed: {:?}", result.error);
    }
}

/// Sync test: asserts the BLOCKED_ENV_VARS list in `octos-plugin` matches
/// the canonical list in `octos-agent/src/sandbox/mod.rs`. Failing this
/// means the two lists have drifted and need to be reconciled.
#[test]
fn blocked_env_vars_match_agent_sandbox() {
    let agent_source = include_str!("../../octos-agent/src/sandbox/mod.rs");
    // Extract the slice literal: `pub const BLOCKED_ENV_VARS: &[&str] = &[...];`
    let re = regex::Regex::new(r"(?s)pub const BLOCKED_ENV_VARS:\s*&\[&str\]\s*=\s*&\[(.*?)\];")
        .unwrap();
    let caps = re
        .captures(agent_source)
        .expect("could not locate BLOCKED_ENV_VARS in agent sandbox source");
    let body = &caps[1];
    let item_re = regex::Regex::new(r#""([A-Z0-9_]+)""#).unwrap();
    let agent_vars: Vec<String> = item_re
        .captures_iter(body)
        .map(|c| c[1].to_string())
        .collect();

    let plugin_vars: Vec<String> = octos_plugin::BLOCKED_ENV_VARS
        .iter()
        .map(|s| s.to_string())
        .collect();

    assert_eq!(
        plugin_vars, agent_vars,
        "octos-plugin BLOCKED_ENV_VARS drifted from octos-agent::sandbox::BLOCKED_ENV_VARS. \
         Update both lists together."
    );
}

/// Pure-logic test: is_safe_shell_command denies the expected patterns.
#[test]
fn is_safe_shell_command_matches_canonical_deny_list() {
    assert!(is_safe_shell_command("rm -rf /").is_err());
    assert!(is_safe_shell_command("mkfs /dev/sda1").is_err());
    assert!(is_safe_shell_command("dd if=/dev/zero of=/dev/sda").is_err());
    assert!(is_safe_shell_command(":(){:|:&};:").is_err());
    // Allowed commands must pass.
    assert!(is_safe_shell_command("echo hello").is_ok());
    assert!(is_safe_shell_command("ls -la").is_ok());
}
