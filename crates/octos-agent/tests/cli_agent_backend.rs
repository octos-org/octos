//! Integration tests for the CLI agent backend — the non-MCP dispatch
//! lane that invokes one-shot headless agents (`claude -p`,
//! `codex exec`) and maps process results onto [`DispatchOutcome`].

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use octos_agent::tools::mcp_agent::{
    CliAgentBackend, DispatchOutcome, DispatchRequest, McpAgentBackend, McpAgentBackendConfig,
    build_backend_from_config,
};

fn write_script(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write script");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).expect("perms").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

fn cli_config(cmd: &str) -> McpAgentBackendConfig {
    McpAgentBackendConfig::Cli {
        cmd: cmd.to_string(),
        args: Vec::new(),
        env: HashMap::new(),
        dispatch_timeout_secs: Some(10),
        prompt_via_stdin: false,
    }
}

fn prompt_request(prompt: &str) -> DispatchRequest {
    DispatchRequest::new("ignored_for_cli", serde_json::json!({ "prompt": prompt }))
}

#[tokio::test]
async fn should_pass_prompt_as_final_arg_and_return_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        &dir,
        "echo-arg.sh",
        "#!/bin/sh\nprintf 'cli-out:%s' \"$1\"\n",
    );
    let backend = CliAgentBackend::from_config(&cli_config(&script.display().to_string()))
        .expect("build cli backend");

    let response = backend.dispatch(prompt_request("hello world")).await;

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert_eq!(response.output, "cli-out:hello world");
    assert!(response.error.is_none());
}

#[tokio::test]
async fn should_append_prompt_after_configured_args() {
    let dir = tempfile::tempdir().unwrap();
    // Mirrors `claude -p <prompt>`: configured args come first, the
    // prompt is always the final argv entry.
    let script = write_script(&dir, "argv.sh", "#!/bin/sh\nprintf '%s|%s' \"$1\" \"$2\"\n");
    let config = McpAgentBackendConfig::Cli {
        cmd: script.display().to_string(),
        args: vec!["-p".to_string()],
        env: HashMap::new(),
        dispatch_timeout_secs: Some(10),
        prompt_via_stdin: false,
    };
    let backend = CliAgentBackend::from_config(&config).unwrap();

    let response = backend.dispatch(prompt_request("task text")).await;

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert_eq!(response.output, "-p|task text");
}

#[tokio::test]
async fn should_pipe_prompt_via_stdin_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        &dir,
        "stdin.sh",
        "#!/bin/sh\nread line\nprintf 'stdin:%s' \"$line\"\n",
    );
    let config = McpAgentBackendConfig::Cli {
        cmd: script.display().to_string(),
        args: Vec::new(),
        env: HashMap::new(),
        dispatch_timeout_secs: Some(10),
        prompt_via_stdin: true,
    };
    let backend = CliAgentBackend::from_config(&config).unwrap();

    let response = backend.dispatch(prompt_request("piped prompt")).await;

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert_eq!(response.output, "stdin:piped prompt");
}

#[tokio::test]
async fn should_serialize_whole_task_when_no_prompt_field() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "raw.sh", "#!/bin/sh\nprintf '%s' \"$1\"\n");
    let backend = CliAgentBackend::from_config(&cli_config(&script.display().to_string())).unwrap();

    let request = DispatchRequest::new("ignored", serde_json::json!({ "objective": "x", "n": 1 }));
    let response = backend.dispatch(request).await;

    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert!(
        response.output.contains("\"objective\":\"x\""),
        "whole task JSON should reach the CLI: {}",
        response.output
    );
}

#[tokio::test]
async fn should_map_nonzero_exit_to_retryable_remote_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        &dir,
        "fail.sh",
        "#!/bin/sh\nprintf 'partial' \necho 'boom happened' >&2\nexit 3\n",
    );
    let backend = CliAgentBackend::from_config(&cli_config(&script.display().to_string())).unwrap();

    let response = backend.dispatch(prompt_request("x")).await;

    assert_eq!(response.outcome, DispatchOutcome::RemoteError);
    let error = response.error.expect("error populated");
    assert!(
        error.contains('3') && error.contains("boom happened"),
        "error should carry exit code and stderr: {error}"
    );
}

#[tokio::test]
async fn should_map_missing_binary_to_transport_error() {
    let backend =
        CliAgentBackend::from_config(&cli_config("/definitely/not/a/real/binary")).unwrap();
    let response = backend.dispatch(prompt_request("x")).await;
    assert_eq!(response.outcome, DispatchOutcome::TransportError);
}

#[tokio::test]
async fn should_time_out_and_kill_hung_cli() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "hang.sh", "#!/bin/sh\nsleep 30\n");
    let config = McpAgentBackendConfig::Cli {
        cmd: script.display().to_string(),
        args: Vec::new(),
        env: HashMap::new(),
        dispatch_timeout_secs: Some(1),
        prompt_via_stdin: false,
    };
    let backend = CliAgentBackend::from_config(&config).unwrap();

    let started = Instant::now();
    let response = backend.dispatch(prompt_request("x")).await;

    assert_eq!(response.outcome, DispatchOutcome::Timeout);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout must not wait for the hung child: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn should_scrub_blocked_env_vars_from_cli_child() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    let script = write_script(
        &dir,
        "env.sh",
        &format!(
            "#!/bin/sh\nprintf 'LD=%s NODE=%s OK=%s' \"${{LD_PRELOAD:-}}\" \"${{NODE_OPTIONS:-}}\" \"${{CLI_OK:-}}\" > {}\n",
            out.display()
        ),
    );
    let mut env = HashMap::new();
    env.insert("LD_PRELOAD".to_string(), "evil.so".to_string());
    env.insert("NODE_OPTIONS".to_string(), "--evil".to_string());
    env.insert("CLI_OK".to_string(), "yes".to_string());
    let config = McpAgentBackendConfig::Cli {
        cmd: script.display().to_string(),
        args: Vec::new(),
        env,
        dispatch_timeout_secs: Some(10),
        prompt_via_stdin: false,
    };
    let backend = CliAgentBackend::from_config(&config).unwrap();

    let response = backend.dispatch(prompt_request("x")).await;
    assert_eq!(response.outcome, DispatchOutcome::Success);

    let seen = std::fs::read_to_string(&out).expect("env capture written");
    assert_eq!(seen, "LD= NODE= OK=yes", "blocked vars must be scrubbed");
}

#[tokio::test]
async fn should_build_cli_backend_via_generic_constructor() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "ok.sh", "#!/bin/sh\nprintf 'done'\n");
    let backend =
        build_backend_from_config(&cli_config(&script.display().to_string()), Some(dir.path()))
            .expect("generic constructor accepts Cli variant");

    assert_eq!(backend.backend_label(), "cli");
    let response = backend.dispatch(prompt_request("x")).await;
    assert_eq!(response.outcome, DispatchOutcome::Success);
    assert_eq!(response.output, "done");
}
