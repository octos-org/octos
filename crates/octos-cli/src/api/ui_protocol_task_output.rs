//! `task/output/read` support for the UI protocol.
//!
//! The current runtime persists task snapshots, not disk-routed stdout/stderr
//! streams. This module exposes a typed, cursorable projection of that snapshot
//! data and reports whether this read source itself can be live-tailed.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use octos_core::ui_protocol::{
    OutputCursor, RpcError, TaskOutputReadLimitation, TaskOutputReadParams, TaskOutputReadResult,
    TaskOutputReadSource, methods,
};
use octos_core::{SessionKey, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;

const DEFAULT_LIMIT_BYTES: usize = 64 * 1024;
const MAX_LIMIT_BYTES: usize = 1024 * 1024;
const TASK_LEDGER_SCHEMA_MAX: u32 = 1;

#[derive(Debug, Deserialize)]
struct PersistedTaskRecord {
    #[serde(default)]
    schema_version: u32,
    task: octos_agent::BackgroundTask,
}

pub(crate) async fn read_task_output(
    state: &AppState,
    params: TaskOutputReadParams,
) -> Result<TaskOutputReadResult, RpcError> {
    let Some(sessions) = &state.sessions else {
        return Err(RpcError::internal_error("Sessions not available"));
    };

    let data_dir = {
        let sessions = sessions.lock().await;
        sessions.data_dir()
    };

    read_task_output_from_data_dir(&data_dir, params)
}

fn read_task_output_from_data_dir(
    data_dir: &Path,
    params: TaskOutputReadParams,
) -> Result<TaskOutputReadResult, RpcError> {
    if !octos_bus::SessionHandle::session_exists(data_dir, &params.session_id) {
        return Err(session_not_found_error(&params.session_id));
    }

    let ledger_path = task_state_path(data_dir, &params.session_id);
    let task = read_latest_task_snapshot(&ledger_path, &params.session_id, &params.task_id)?
        .ok_or_else(|| task_not_found_error(&params.session_id, &params.task_id))?;

    Ok(project_task_output(data_dir, params, &task))
}

pub(crate) fn task_state_path(data_dir: &Path, session_id: &SessionKey) -> PathBuf {
    let encoded_base = octos_bus::session::encode_path_component(session_id.base_key());
    let topic = session_id
        .topic()
        .filter(|topic| !topic.is_empty())
        .unwrap_or("default");
    let encoded_topic = octos_bus::session::encode_path_component(topic);

    data_dir
        .join("users")
        .join(encoded_base)
        .join("sessions")
        .join(format!("{encoded_topic}.tasks.jsonl"))
}

fn read_latest_task_snapshot(
    ledger_path: &Path,
    session_id: &SessionKey,
    task_id: &TaskId,
) -> Result<Option<octos_agent::BackgroundTask>, RpcError> {
    let file = match File::open(ledger_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(
                RpcError::internal_error(format!("failed to read task ledger: {error}"))
                    .with_data(json!({ "path": ledger_path.display().to_string() })),
            );
        }
    };

    let task_id = task_id.to_string();
    let mut latest = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<PersistedTaskRecord>(&line) else {
            continue;
        };
        if record.schema_version > TASK_LEDGER_SCHEMA_MAX {
            continue;
        }
        if record.task.id != task_id {
            continue;
        }
        if record
            .task
            .session_key
            .as_deref()
            .is_some_and(|task_session| task_session != session_id.0)
        {
            continue;
        }
        latest = Some(record.task);
    }
    Ok(latest)
}

fn project_task_output(
    data_dir: &Path,
    params: TaskOutputReadParams,
    task: &octos_agent::BackgroundTask,
) -> TaskOutputReadResult {
    let task_status = task.status.as_str().to_owned();
    let runtime_state = serde_label(&task.runtime_state);
    let lifecycle_state = serde_label(&task.lifecycle_state());
    let runtime_detail = task.runtime_detail.as_deref().map(runtime_detail_value);
    let output_files = task
        .output_files
        .iter()
        .map(|path| task_response_path(data_dir, path))
        .collect::<Vec<_>>();
    let text = projection_text(
        task,
        &task_status,
        &runtime_state,
        &lifecycle_state,
        runtime_detail.as_ref(),
        &output_files,
    );
    let window = read_window(&text, params.cursor, params.limit_bytes);

    TaskOutputReadResult {
        session_id: params.session_id,
        task_id: params.task_id,
        source: TaskOutputReadSource::RuntimeProjection,
        cursor: OutputCursor {
            offset: window.start_offset as u64,
        },
        next_cursor: OutputCursor {
            offset: window.end_offset as u64,
        },
        text: window.text,
        bytes_read: window.bytes_read as u64,
        total_bytes: text.len() as u64,
        truncated: window.end_offset < text.len(),
        complete: window.end_offset >= text.len(),
        live_tail_supported: false,
        task_status,
        runtime_state,
        lifecycle_state,
        runtime_detail,
        output_files,
        limitations: runtime_projection_limitations(),
    }
}

fn projection_text(
    task: &octos_agent::BackgroundTask,
    task_status: &str,
    runtime_state: &str,
    lifecycle_state: &str,
    runtime_detail: Option<&Value>,
    output_files: &[String],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("task_id: {}\n", task.id));
    out.push_str(&format!("tool_name: {}\n", task.tool_name));
    out.push_str(&format!("tool_call_id: {}\n", task.tool_call_id));
    out.push_str(&format!("status: {task_status}\n"));
    out.push_str(&format!("runtime_state: {runtime_state}\n"));
    out.push_str(&format!("lifecycle_state: {lifecycle_state}\n"));

    if let Some(detail) = runtime_detail {
        out.push_str("runtime_detail:\n");
        match serde_json::to_string_pretty(detail) {
            Ok(pretty) => {
                out.push_str(&pretty);
                out.push('\n');
            }
            Err(_) => {
                out.push_str(&detail.to_string());
                out.push('\n');
            }
        }
    }

    if !output_files.is_empty() {
        out.push_str("output_files:\n");
        for path in output_files {
            out.push_str("- ");
            out.push_str(path);
            out.push('\n');
        }
    }

    if let Some(error) = task.error.as_deref().filter(|error| !error.is_empty()) {
        out.push_str("error:\n");
        out.push_str(error);
        if !error.ends_with('\n') {
            out.push('\n');
        }
    }

    out.push_str("limitations:\n");
    out.push_str("- disk-routed task stdout/stderr is not wired in this runtime\n");
    out.push_str(
        "- this task/output/read source is a snapshot projection, not a live-tail stream\n",
    );
    out
}

struct OutputWindow {
    start_offset: usize,
    end_offset: usize,
    bytes_read: usize,
    text: String,
}

fn read_window(text: &str, cursor: Option<OutputCursor>, limit_bytes: Option<u64>) -> OutputWindow {
    let start_offset = align_start_offset(text, cursor.map(|cursor| cursor.offset).unwrap_or(0));
    let limit = limit_bytes
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(DEFAULT_LIMIT_BYTES)
        .min(MAX_LIMIT_BYTES);
    let mut end_offset = start_offset.saturating_add(limit).min(text.len());
    while end_offset > start_offset && !text.is_char_boundary(end_offset) {
        end_offset -= 1;
    }

    let slice = &text[start_offset..end_offset];
    OutputWindow {
        start_offset,
        end_offset,
        bytes_read: slice.len(),
        text: slice.to_owned(),
    }
}

fn align_start_offset(text: &str, offset: u64) -> usize {
    let mut offset = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn runtime_detail_value(detail: &str) -> Value {
    serde_json::from_str(detail).unwrap_or_else(|_| Value::String(detail.to_owned()))
}

fn serde_label<T>(value: &T) -> String
where
    T: Serialize,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn task_response_path(data_dir: &Path, path: &str) -> String {
    octos_bus::file_handle::encode_profile_file_handle(data_dir, Path::new(path))
        .unwrap_or_else(|| path.to_owned())
}

fn runtime_projection_limitations() -> Vec<TaskOutputReadLimitation> {
    vec![
        TaskOutputReadLimitation {
            code: "disk_output_unavailable".to_owned(),
            message:
                "No M8.7 disk-routed task stdout/stderr stream is discoverable in this runtime"
                    .to_owned(),
        },
        TaskOutputReadLimitation {
            code: "live_tail_unavailable".to_owned(),
            message:
                "task/output/read is served from a snapshot projection; live tail is available only from task/output/delta notifications emitted during active turns"
                    .to_owned(),
        },
    ]
}

fn session_not_found_error(session_id: &SessionKey) -> RpcError {
    RpcError::invalid_params("session not found").with_data(json!({
        "kind": "session_not_found",
        "method": methods::TASK_OUTPUT_READ,
        "session_id": session_id,
        "reason": "missing_session",
    }))
}

fn task_not_found_error(session_id: &SessionKey, task_id: &TaskId) -> RpcError {
    RpcError::invalid_params("task not found").with_data(json!({
        "kind": "task_not_found",
        "method": methods::TASK_OUTPUT_READ,
        "session_id": session_id,
        "task_id": task_id,
        "reason": "missing_task",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_bus::SessionManager;
    use octos_core::Message;
    use octos_core::ui_protocol::TaskOutputReadParams;
    use octos_core::ui_protocol::rpc_error_codes;

    fn params(session_id: &SessionKey, task_id: &TaskId) -> TaskOutputReadParams {
        TaskOutputReadParams {
            session_id: session_id.clone(),
            task_id: task_id.clone(),
            cursor: None,
            limit_bytes: None,
        }
    }

    async fn persist_session(data_dir: &Path, session_id: &SessionKey) {
        let mut sessions = SessionManager::open(data_dir).expect("session manager");
        sessions
            .add_message(session_id, Message::user("hello"))
            .await
            .expect("persist session");
    }

    fn persisted_task(
        data_dir: &Path,
        session_id: &SessionKey,
    ) -> (TaskId, octos_agent::TaskSupervisor) {
        let supervisor = octos_agent::TaskSupervisor::new();
        supervisor
            .enable_persistence(task_state_path(data_dir, session_id))
            .expect("enable task persistence");
        let task_id = supervisor.register("shell", "call-1", Some(&session_id.0));
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::DeliveringOutputs,
            Some(
                json!({
                    "workflow_kind": "coding",
                    "current_phase": "collecting_output",
                    "progress_message": "Collecting stdout"
                })
                .to_string(),
            ),
        );
        supervisor.mark_failed(
            &task_id,
            "stderr: first line\nstderr: second line\n".to_owned(),
        );
        (task_id.parse().expect("task id"), supervisor)
    }

    #[tokio::test]
    async fn read_projection_supports_cursor_offsets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey::new("api", "session");
        persist_session(temp.path(), &session_id).await;
        let (task_id, _supervisor) = persisted_task(temp.path(), &session_id);

        let initial = read_task_output_from_data_dir(temp.path(), params(&session_id, &task_id))
            .expect("read output");
        let offset = initial.text.find("stderr: second").expect("substring");
        let result = read_task_output_from_data_dir(
            temp.path(),
            TaskOutputReadParams {
                session_id,
                task_id,
                cursor: Some(OutputCursor {
                    offset: offset as u64,
                }),
                limit_bytes: Some(14),
            },
        )
        .expect("read output at cursor");

        assert_eq!(result.cursor.offset, offset as u64);
        assert_eq!(result.text, "stderr: second");
        assert_eq!(result.next_cursor.offset, (offset + 14) as u64);
    }

    #[tokio::test]
    async fn read_projection_applies_limit_without_splitting_utf8() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey::new("api", "session");
        persist_session(temp.path(), &session_id).await;
        let (task_id, _supervisor) = persisted_task(temp.path(), &session_id);

        let result = read_task_output_from_data_dir(
            temp.path(),
            TaskOutputReadParams {
                session_id,
                task_id,
                cursor: Some(OutputCursor { offset: 0 }),
                limit_bytes: Some(13),
            },
        )
        .expect("read output");

        assert!(result.text.len() <= 13);
        assert_eq!(result.bytes_read, result.text.len() as u64);
        assert!(result.truncated);
        assert!(!result.complete);
    }

    #[test]
    fn missing_session_returns_invalid_params() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey::new("api", "missing");
        let task_id = TaskId::new();

        let error = read_task_output_from_data_dir(temp.path(), params(&session_id, &task_id))
            .expect_err("missing session");

        assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("reason")),
            Some(&json!("missing_session"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("session_not_found"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("method")),
            Some(&json!(methods::TASK_OUTPUT_READ))
        );
    }

    #[tokio::test]
    async fn missing_task_returns_invalid_params() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey::new("api", "session");
        persist_session(temp.path(), &session_id).await;
        let task_id = TaskId::new();

        let error = read_task_output_from_data_dir(temp.path(), params(&session_id, &task_id))
            .expect_err("missing task");

        assert_eq!(error.code, rpc_error_codes::INVALID_PARAMS);
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("reason")),
            Some(&json!("missing_task"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("kind")),
            Some(&json!("task_not_found"))
        );
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("method")),
            Some(&json!(methods::TASK_OUTPUT_READ))
        );
    }

    #[tokio::test]
    async fn runtime_projection_reports_snapshot_read_has_no_live_tail_support() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey::new("api", "session");
        persist_session(temp.path(), &session_id).await;
        let (task_id, _supervisor) = persisted_task(temp.path(), &session_id);

        let result = read_task_output_from_data_dir(temp.path(), params(&session_id, &task_id))
            .expect("read output");

        assert_eq!(result.source, TaskOutputReadSource::RuntimeProjection);
        assert!(!result.live_tail_supported);
        assert!(
            result
                .limitations
                .iter()
                .any(|limitation| { limitation.code == "live_tail_unavailable" })
        );
        assert!(
            result
                .text
                .contains("this task/output/read source is a snapshot projection")
        );
    }

    #[tokio::test]
    async fn runtime_projection_serializes_stable_live_tail_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionKey::new("api", "session");
        persist_session(temp.path(), &session_id).await;
        let (task_id, _supervisor) = persisted_task(temp.path(), &session_id);

        let result = read_task_output_from_data_dir(temp.path(), params(&session_id, &task_id))
            .expect("read output");
        let value = serde_json::to_value(&result).expect("serialize task output result");

        assert_eq!(value["source"], json!("runtime_projection"));
        assert_eq!(value["live_tail_supported"], json!(false));
        assert_eq!(value["complete"], json!(true));
        assert!(value["total_bytes"].as_u64().unwrap() >= value["bytes_read"].as_u64().unwrap());
        assert_eq!(
            value["limitations"]
                .as_array()
                .expect("limitations array")
                .iter()
                .map(|limitation| limitation["code"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["disk_output_unavailable", "live_tail_unavailable"]
        );
        assert!(
            value["text"]
                .as_str()
                .unwrap()
                .contains("snapshot projection, not a live-tail stream")
        );
    }
}
