//! Observable tool contracts, independent of any model or PPTX workflow.
use std::sync::Arc;

use base64::Engine;
use octos_agent::ReadFileTool;
use octos_agent::policy::AllowAllPolicy;
use octos_agent::sandbox::NoSandbox;
use octos_agent::tools::Tool;
use octos_agent::tools::coding_tools::{BashTool, ExecCommandTool, WriteStdinTool};
use serde_json::{Value, json};

#[cfg(unix)]
struct EnvironmentSandbox;
#[cfg(unix)]
impl octos_agent::sandbox::Sandbox for EnvironmentSandbox {
    fn wrap_command(&self, command: &str, cwd: &std::path::Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(cwd);
        cmd.env("OCTOS_PTY_TEST_ALLOWED", "wrapped");
        cmd.env("NODE_OPTIONS", "must-be-removed");
        cmd
    }
}

#[cfg(unix)]
#[tokio::test]
async fn should_preserve_sandbox_wrapping_and_environment_filtering_in_pty() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ExecCommandTool::new(dir.path(), Arc::new(EnvironmentSandbox))
        .with_policy(Arc::new(AllowAllPolicy));
    let result=tool.execute(&json!({"cmd":"printf '%s/%s' \"$OCTOS_PTY_TEST_ALLOWED\" \"${NODE_OPTIONS-unset}\"","tty":true,"yield_time_ms":1000})).await.unwrap();
    let value: Value = serde_json::from_str(&result.output).unwrap();
    assert!(
        value["output"].as_str().unwrap().contains("wrapped/unset"),
        "{value}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn should_keep_mutation_policy_on_the_yielded_command_path() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ExecCommandTool::new(dir.path(), Arc::new(NoSandbox))
        .with_policy(Arc::new(AllowAllPolicy))
        .with_bash_file_writes(octos_agent::tools::policy::BashFileWrites::Deny);
    for tty in [false, true] {
        let result = tool
            .execute(&json!({"cmd":"touch forbidden","tty":tty,"yield_time_ms":1}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(!dir.path().join("forbidden").exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn should_accept_terminal_input_and_deliver_new_output_once() {
    let dir = tempfile::tempdir().unwrap();
    let tool =
        ExecCommandTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let started=tool.execute(&json!({"cmd":"stty -echo; printf READY; read value; printf 'GOT:%s' \"$value\"","tty":true,"yield_time_ms":300})).await.unwrap();
    let started: Value = serde_json::from_str(&started.output).unwrap();
    assert!(started["output"].as_str().unwrap().contains("READY"));
    let result = WriteStdinTool
        .execute(&json!({"session_id":started["session_id"],"chars":"hello\n","yield_time_ms":500}))
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&result.output).unwrap();
    assert!(result["output"].as_str().unwrap().contains("GOT:hello"));
    assert!(!result["output"].as_str().unwrap().contains("READY"));
    assert_eq!(result["exit_code"], 0);
}

#[tokio::test]
async fn should_keep_original_pixels_when_source_is_replaced_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let original = b"\x89PNG\r\n\x1a\noriginal pixels";
    let source = dir.path().join("screen.png");
    std::fs::write(&source, original).unwrap();
    let result = ReadFileTool::new(dir.path())
        .execute(&json!({"path":"screen.png"}))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let saved = result.model_media[0].to_string_lossy().into_owned();
    std::fs::write(&source, b"\x89PNG\r\n\x1a\nnew pixels").unwrap();
    let (_, actual) = octos_llm::vision::encode_image(&saved, Some(dir.path())).unwrap();
    assert_eq!(
        actual,
        base64::engine::general_purpose::STANDARD.encode(original)
    );
    std::fs::remove_file(&source).unwrap();
    // Persisted references must work after a JSON round trip without a process-local cache.
    let saved: String = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
    assert_eq!(
        octos_llm::vision::encode_image(&saved, Some(dir.path()))
            .unwrap()
            .1,
        actual
    );
}

#[cfg(unix)]
#[tokio::test]
async fn should_save_complete_bash_log_when_visible_output_is_limited() {
    let dir = tempfile::tempdir().unwrap();
    let tool = BashTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let result = tool.execute(&json!({"cmd":"printf 'BEGIN\\n'; i=0; while [ $i -lt 12000 ]; do printf 'middle-line\\n'; i=$((i+1)); done; printf 'END\\n'"})).await.unwrap();
    assert!(result.success, "{}", result.output);
    let meta = result.structured_metadata.as_ref().unwrap();
    let path = meta["full_output_path"]
        .as_str()
        .expect("complete log path");
    let all = std::fs::read_to_string(path).unwrap();
    assert!(all.starts_with("BEGIN\n"));
    assert!(all.ends_with("END\n"));
    assert_eq!(all.matches("middle-line\n").count(), 12000);
    assert!(result.output.contains(path));
    assert!(
        result.output.contains("END"),
        "recent output should survive truncation"
    );
    assert!(result.output.len() < 52_000);
    let relative = std::path::Path::new(path).strip_prefix(dir.path()).unwrap();
    let read = ReadFileTool::new(dir.path())
        .execute(&json!({"path":relative,"limit":2}))
        .await
        .unwrap();
    assert!(
        read.success,
        "log must be readable through the same scoped read tool: {}",
        read.output
    );
}

#[cfg(unix)]
#[tokio::test]
async fn should_keep_log_recovery_within_the_shell_history_budget() {
    let dir = tempfile::tempdir().unwrap();
    let tool = octos_agent::ShellTool::new(dir.path()).with_policy(Arc::new(AllowAllPolicy));
    let result = tool
        .execute(&json!({"command":
            "i=0; while [ $i -lt 8000 ]; do printf 'line\\n'; i=$((i+1)); done; printf END"
        }))
        .await
        .unwrap();
    assert!(result.success, "{}", result.output);
    let path = result.structured_metadata.as_ref().unwrap()["full_output_path"]
        .as_str()
        .unwrap();
    let visible = octos_core::truncate_head_tail_report(
        &result.output,
        octos_core::tool_output_limit("shell"),
        0.7,
    )
    .content;
    assert!(
        visible.contains(path),
        "recovery path must reach the main model"
    );
    assert!(result.output.len() <= octos_core::tool_output_limit("shell"));
    assert_eq!(
        std::fs::read_to_string(path)
            .unwrap()
            .matches("line\n")
            .count(),
        8000
    );
}

#[cfg(unix)]
#[tokio::test]
async fn should_not_repeat_output_when_polling_a_completed_session() {
    let dir = tempfile::tempdir().unwrap();
    let tool =
        ExecCommandTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let first = tool
        .execute(&json!({"cmd":"printf UNIQUE_OUTPUT","yield_time_ms":1000}))
        .await
        .unwrap();
    assert!(first.success, "{}", first.output);
    let first: Value = serde_json::from_str(&first.output).unwrap();
    assert!(first["output"].as_str().unwrap().contains("UNIQUE_OUTPUT"));
    let second = WriteStdinTool
        .execute(&json!({"session_id":first["session_id"],"yield_time_ms":0}))
        .await
        .unwrap();
    let second: Value = serde_json::from_str(&second.output).unwrap();
    assert!(!second["output"].as_str().unwrap().contains("UNIQUE_OUTPUT"));
    assert_eq!(second["running"], false);
    assert_eq!(second["exit_code"], 0);
}

#[cfg(unix)]
#[tokio::test]
async fn should_allocate_a_real_terminal_when_tty_is_requested() {
    let dir = tempfile::tempdir().unwrap();
    let tool =
        ExecCommandTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let result = tool.execute(&json!({"cmd":"test -t 0 && test -t 1 && test -t 2 && printf REAL_TERMINAL","tty":true,"yield_time_ms":1000})).await.unwrap();
    assert!(result.success, "{}", result.output);
    let result: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(result["exit_code"], 0, "{result}");
    assert!(result["output"].as_str().unwrap().contains("REAL_TERMINAL"));
}

#[cfg(unix)]
#[tokio::test]
async fn should_kill_terminal_descendants_after_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let tool =
        ExecCommandTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let result = tool
        .execute(&json!({
            "cmd":"printf BEFORE_TIMEOUT; (sleep 3; touch late-write) & wait",
            "tty":true,"timeout_secs":1,"yield_time_ms":2200
        }))
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(result["running"], false, "{result}");
    assert_eq!(result["exit_code"], -1);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("BEFORE_TIMEOUT")
    );
    assert!(result["output"].as_str().unwrap().contains("timed out"));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(!dir.path().join("late-write").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn should_kill_terminal_when_cancelled_before_first_yield() {
    let dir = tempfile::tempdir().unwrap();
    let tool =
        ExecCommandTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let task = tokio::spawn(async move {
        tool.execute(&json!({"cmd":"touch started; sleep 2; touch late-write",
            "tty":true,"yield_time_ms":30000}))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !dir.path().join("started").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(!dir.path().join("late-write").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn should_budget_output_in_estimated_tokens_instead_of_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let tool =
        ExecCommandTool::new(dir.path(), Arc::new(NoSandbox)).with_policy(Arc::new(AllowAllPolicy));
    let result = tool.execute(&json!({"cmd":"i=0; while [ $i -lt 3000 ]; do printf x; i=$((i+1)); done","max_output_tokens":400})).await.unwrap();
    assert!(result.success, "{}", result.output);
    assert!(
        result.output.len() > 800,
        "400 tokens should not be a 400-byte cut: {}",
        result.output.len()
    );
    assert!(octos_llm::context::estimate_tokens(&result.output) <= 400);
    assert!(
        result.output.contains("Full output:"),
        "token truncation must retain a recovery path"
    );
}
