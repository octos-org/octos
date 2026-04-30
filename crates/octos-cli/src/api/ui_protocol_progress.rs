//! Mapping from legacy progress JSON frames to UI protocol progress shapes.
//!
//! This module is deliberately independent from the WebSocket loop so the
//! protocol mapping can be tested before the live transport adopts it.

#![allow(dead_code)]

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    ApprovalId, ApprovalRequestedEvent, MessageDeltaEvent, ToolCompletedEvent, ToolProgressEvent,
    ToolStartedEvent, TurnId, UiFileMutationNotice, UiNotification, UiProgressEvent,
    UiProgressMetadata, UiRetryBackoff, UiTokenCostUpdate, WarningEvent, file_mutation_operations,
    progress_kinds,
};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProgressMappingContext {
    pub session_id: SessionKey,
    pub turn_id: TurnId,
}

impl ProgressMappingContext {
    pub(crate) fn new(session_id: SessionKey, turn_id: TurnId) -> Self {
        Self {
            session_id,
            turn_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UiProgressStatus {
    pub event: UiProgressEvent,
}

impl UiProgressStatus {
    fn new(context: &ProgressMappingContext, metadata: UiProgressMetadata) -> Self {
        Self {
            event: UiProgressEvent::new(
                context.session_id.clone(),
                Some(context.turn_id.clone()),
                metadata,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct UiProgressMapping {
    pub notifications: Vec<UiNotification>,
    pub status: Option<UiProgressStatus>,
    pub warning: Option<WarningEvent>,
}

impl UiProgressMapping {
    fn notifications(notifications: Vec<UiNotification>) -> Self {
        Self {
            notifications,
            status: None,
            warning: None,
        }
    }

    fn status(context: &ProgressMappingContext, metadata: UiProgressMetadata) -> Self {
        Self {
            notifications: Vec::new(),
            status: Some(UiProgressStatus::new(context, metadata)),
            warning: None,
        }
    }

    fn warning(context: &ProgressMappingContext, code: impl Into<String>, message: String) -> Self {
        Self {
            notifications: Vec::new(),
            status: None,
            warning: Some(WarningEvent {
                session_id: context.session_id.clone(),
                turn_id: Some(context.turn_id.clone()),
                code: code.into(),
                message,
            }),
        }
    }
}

pub(crate) fn map_progress_json(
    context: &ProgressMappingContext,
    event: &Value,
) -> UiProgressMapping {
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            "progress event is missing string field `type`".to_string(),
        );
    };

    match event_type {
        "token" => map_token(context, event),
        "tool_start" => map_tool_start(context, event),
        "tool_progress" => map_tool_progress(context, event),
        "tool_end" => map_tool_end(context, event),
        "task_started" => map_task_started(context, event),
        "thinking" => map_simple_status(context, event, progress_kinds::THINKING),
        "response" => map_simple_status(context, event, progress_kinds::RESPONSE),
        "cost_update" => map_cost_update(context, event),
        "stream_end" => map_simple_status(context, event, progress_kinds::STREAM_END),
        "retry" | "retry_backoff" => map_retry_backoff(context, event),
        "approval_requested" | "approval_request" => map_approval_requested(context, event),
        "file_modified" | "file_written" | "file_mutation" => {
            map_file_mutation(context, event_type, event)
        }
        other => UiProgressMapping::warning(
            context,
            "unmapped_progress",
            format!("unmapped progress event: {other}"),
        ),
    }
}

fn map_approval_requested(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let title = string_field(event, &["title"]).unwrap_or_else(|| "Approval requested".into());
    let body = string_field(event, &["body", "message", "reason"]).unwrap_or_default();
    let approval_id = event
        .get("approval_id")
        .cloned()
        .and_then(|value| serde_json::from_value::<ApprovalId>(value).ok())
        .unwrap_or_default();

    UiProgressMapping::notifications(vec![UiNotification::ApprovalRequested(
        ApprovalRequestedEvent::generic(
            context.session_id.clone(),
            approval_id,
            context.turn_id.clone(),
            tool_name,
            title,
            body,
        ),
    )])
}

fn map_task_started(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut metadata = UiProgressMetadata::new(progress_kinds::STATUS);
    metadata.message = Some("task started".into());
    if let Some(task_id) = string_field(event, &["task_id"]) {
        metadata.extra.insert("task_id".into(), json!(task_id));
    }
    UiProgressMapping::status(context, metadata)
}

fn map_token(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let Some(text) = string_field(event, &["text"]) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            "token progress event is missing string field `text`".to_string(),
        );
    };

    UiProgressMapping::notifications(vec![UiNotification::MessageDelta(MessageDeltaEvent {
        session_id: context.session_id.clone(),
        turn_id: context.turn_id.clone(),
        text,
    })])
}

fn map_tool_start(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let tool_call_id =
        string_field(event, &["tool_call_id", "id"]).unwrap_or_else(|| tool_name.clone());

    UiProgressMapping::notifications(vec![UiNotification::ToolStarted(ToolStartedEvent {
        session_id: context.session_id.clone(),
        turn_id: context.turn_id.clone(),
        tool_call_id,
        tool_name,
        arguments: event.get("arguments").cloned(),
    })])
}

fn map_tool_progress(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let tool_call_id =
        string_field(event, &["tool_call_id", "id"]).unwrap_or_else(|| tool_name.clone());

    UiProgressMapping::notifications(vec![UiNotification::ToolProgress(ToolProgressEvent {
        session_id: context.session_id.clone(),
        turn_id: context.turn_id.clone(),
        tool_call_id,
        message: string_field(event, &["message", "status"]),
        progress_pct: f32_field(event, &["progress_pct", "progress"]),
    })])
}

fn map_tool_end(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let tool_name = string_field(event, &["tool", "tool_name"]).unwrap_or_else(|| "tool".into());
    let tool_call_id =
        string_field(event, &["tool_call_id", "id"]).unwrap_or_else(|| tool_name.clone());

    let mut metadata = UiProgressMetadata::new(progress_kinds::TOOL_COMPLETED);
    metadata
        .extra
        .insert("tool".into(), json!(tool_name.clone()));
    metadata
        .extra
        .insert("tool_call_id".into(), json!(tool_call_id.clone()));
    if let Some(success) = bool_field(event, &["success"]) {
        metadata.extra.insert("success".into(), json!(success));
    }
    if let Some(duration_ms) = u64_field(event, &["duration_ms", "elapsed_ms"]) {
        metadata
            .extra
            .insert("duration_ms".into(), json!(duration_ms));
    }

    UiProgressMapping {
        notifications: vec![UiNotification::ToolCompleted(ToolCompletedEvent {
            session_id: context.session_id.clone(),
            turn_id: context.turn_id.clone(),
            tool_call_id,
            tool_name,
            success: bool_field(event, &["success"]),
            output_preview: string_field(event, &["output_preview"]),
            duration_ms: u64_field(event, &["duration_ms", "elapsed_ms"]),
        })],
        status: Some(UiProgressStatus::new(context, metadata)),
        warning: None,
    }
}

fn map_simple_status(
    context: &ProgressMappingContext,
    event: &Value,
    kind: &'static str,
) -> UiProgressMapping {
    let mut metadata = UiProgressMetadata::new(kind);
    metadata.message = string_field(event, &["message", "status"]);
    metadata.iteration = u32_field(event, &["iteration"]);
    UiProgressMapping::status(context, metadata)
}

fn map_cost_update(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut update = UiTokenCostUpdate::new();
    update.input_tokens = u64_field(event, &["input_tokens", "tokens_in"]);
    update.output_tokens = u64_field(event, &["output_tokens", "tokens_out"]);
    update.reasoning_tokens = u64_field(event, &["reasoning_tokens"]);
    update.cache_read_tokens = u64_field(event, &["cache_read_tokens"]);
    update.cache_write_tokens = u64_field(event, &["cache_write_tokens"]);
    update.total_tokens = u64_field(event, &["total_tokens"]);
    update.response_cost = f64_field(event, &["response_cost"]);
    update.session_cost = f64_field(event, &["session_cost"]);
    update.currency = string_field(event, &["currency"]);

    let mut metadata = UiProgressMetadata::token_cost(update);
    metadata.message = string_field(event, &["message", "status"]);
    UiProgressMapping::status(context, metadata)
}

fn map_retry_backoff(context: &ProgressMappingContext, event: &Value) -> UiProgressMapping {
    let mut retry = UiRetryBackoff::new();
    retry.attempt = u32_field(event, &["attempt", "retry_round"]);
    retry.max_attempts = u32_field(event, &["max_attempts", "limit"]);
    retry.backoff_ms = u64_field(event, &["backoff_ms", "delay_ms", "retry_after_ms"]);
    retry.reason = string_field(event, &["reason", "message"]);
    retry.provider = string_field(event, &["provider"]);
    retry.next_provider = string_field(event, &["next_provider"]);

    let mut metadata = UiProgressMetadata::retry_backoff(retry);
    metadata.message = string_field(event, &["message", "status"]);
    UiProgressMapping::status(context, metadata)
}

fn map_file_mutation(
    context: &ProgressMappingContext,
    event_type: &str,
    event: &Value,
) -> UiProgressMapping {
    let Some(path) = string_field(event, &["path", "file"]) else {
        return UiProgressMapping::warning(
            context,
            "invalid_progress",
            format!("{event_type} progress event is missing string field `path`"),
        );
    };
    let operation = string_field(event, &["operation", "op"]).unwrap_or_else(|| match event_type {
        "file_written" => file_mutation_operations::WRITE.to_string(),
        "file_modified" => file_mutation_operations::MODIFY.to_string(),
        _ => file_mutation_operations::MODIFY.to_string(),
    });
    let mut notice = UiFileMutationNotice::new(path, operation);
    notice.tool_call_id = string_field(event, &["tool_call_id", "id"]);
    notice.bytes_written = u64_field(event, &["bytes_written", "bytes"]);

    let mut metadata = UiProgressMetadata::file_mutation(notice);
    metadata.message = string_field(event, &["message", "status"]);
    UiProgressMapping::status(context, metadata)
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        value.as_u64().or_else(|| {
            value
                .as_i64()
                .and_then(|number| u64::try_from(number).ok())
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    })
}

fn u32_field(value: &Value, keys: &[&str]) -> Option<u32> {
    u64_field(value, keys).and_then(|number| u32::try_from(number).ok())
}

fn f64_field(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn f32_field(value: &Value, keys: &[&str]) -> Option<f32> {
    f64_field(value, keys).map(|number| number as f32)
}

fn bool_field(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{TurnId, UiNotification};
    use uuid::Uuid;

    fn context() -> ProgressMappingContext {
        ProgressMappingContext::new(SessionKey("local:demo".into()), TurnId(Uuid::from_u128(7)))
    }

    #[test]
    fn ui_protocol_progress_maps_token_to_message_delta() {
        let mapping = map_progress_json(&context(), &json!({ "type": "token", "text": "hi" }));

        assert_eq!(mapping.status, None);
        assert_eq!(mapping.warning, None);
        let [UiNotification::MessageDelta(delta)] = mapping.notifications.as_slice() else {
            panic!("expected message delta notification");
        };
        assert_eq!(delta.text, "hi");
    }

    #[test]
    fn ui_protocol_progress_preserves_tool_progress_call_id() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_progress",
                "tool": "shell",
                "tool_call_id": "call-42",
                "message": "running",
                "progress_pct": 37.5
            }),
        );

        let [UiNotification::ToolProgress(progress)] = mapping.notifications.as_slice() else {
            panic!("expected tool progress notification");
        };
        assert_eq!(progress.tool_call_id, "call-42");
        assert_eq!(progress.message.as_deref(), Some("running"));
        assert_eq!(progress.progress_pct, Some(37.5));
    }

    #[test]
    fn ui_protocol_progress_maps_tool_start() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_start",
                "tool": "shell",
                "tool_call_id": "call-42",
                "arguments": {"command": "cargo test"}
            }),
        );

        let [UiNotification::ToolStarted(started)] = mapping.notifications.as_slice() else {
            panic!("expected tool started notification");
        };
        assert_eq!(started.tool_name, "shell");
        assert_eq!(started.tool_call_id, "call-42");
        assert_eq!(
            started
                .arguments
                .as_ref()
                .and_then(|args| args.get("command")),
            Some(&json!("cargo test"))
        );
        assert_eq!(mapping.status, None);
        assert_eq!(mapping.warning, None);
    }

    #[test]
    fn ui_protocol_progress_maps_task_started_to_status() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "task_started",
                "task_id": "01900000-0000-7000-8000-000000000001"
            }),
        );

        let status = mapping.status.expect("status mapping");
        assert_eq!(status.event.metadata.kind, progress_kinds::STATUS);
        assert_eq!(
            status.event.metadata.message.as_deref(),
            Some("task started")
        );
        assert_eq!(
            status.event.metadata.extra.get("task_id"),
            Some(&json!("01900000-0000-7000-8000-000000000001"))
        );
        assert!(mapping.notifications.is_empty());
        assert_eq!(mapping.warning, None);
    }

    #[test]
    fn ui_protocol_progress_maps_silent_status_events() {
        for (event_type, expected_kind) in [
            ("thinking", progress_kinds::THINKING),
            ("response", progress_kinds::RESPONSE),
            ("stream_end", progress_kinds::STREAM_END),
        ] {
            let mapping =
                map_progress_json(&context(), &json!({ "type": event_type, "iteration": 2 }));

            let status = mapping.status.expect("status mapping");
            assert_eq!(status.event.metadata.kind, expected_kind);
            assert_eq!(status.event.metadata.iteration, Some(2));
            assert!(mapping.notifications.is_empty());
            assert_eq!(mapping.warning, None);
        }
    }

    #[test]
    fn ui_protocol_progress_maps_cost_update_to_token_cost_status() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "cost_update",
                "input_tokens": 10,
                "output_tokens": 4,
                "session_cost": 0.0012
            }),
        );

        let status = mapping.status.expect("cost status");
        let cost = status
            .event
            .metadata
            .token_cost
            .expect("token cost metadata");
        assert_eq!(
            status.event.metadata.kind,
            progress_kinds::TOKEN_COST_UPDATE
        );
        assert_eq!(cost.input_tokens, Some(10));
        assert_eq!(cost.output_tokens, Some(4));
        assert_eq!(cost.session_cost, Some(0.0012));
    }

    #[test]
    fn ui_protocol_progress_preserves_tool_end_success_metadata() {
        let mapping = map_progress_json(
            &context(),
            &json!({
                "type": "tool_end",
                "tool": "shell",
                "tool_call_id": "call-42",
                "success": false,
                "output_preview": "permission denied",
                "duration_ms": 1250
            }),
        );

        let [UiNotification::ToolCompleted(completed)] = mapping.notifications.as_slice() else {
            panic!("expected tool completed notification");
        };
        assert_eq!(completed.tool_call_id, "call-42");
        assert_eq!(completed.success, Some(false));
        assert_eq!(
            completed.output_preview.as_deref(),
            Some("permission denied")
        );
        assert_eq!(completed.duration_ms, Some(1250));

        let status = mapping.status.expect("tool completion status");
        assert_eq!(status.event.metadata.kind, progress_kinds::TOOL_COMPLETED);
        assert_eq!(
            status.event.metadata.extra.get("success"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            status.event.metadata.extra.get("duration_ms"),
            Some(&json!(1250))
        );
    }

    #[test]
    fn ui_protocol_progress_maps_retry_and_file_mutation_status() {
        let retry = map_progress_json(
            &context(),
            &json!({
                "type": "retry_backoff",
                "attempt": 2,
                "max_attempts": 4,
                "backoff_ms": 750,
                "reason": "rate limit"
            }),
        );
        let retry = retry
            .status
            .expect("retry status")
            .event
            .metadata
            .retry
            .expect("retry metadata");
        assert_eq!(retry.attempt, Some(2));
        assert_eq!(retry.backoff_ms, Some(750));

        let file = map_progress_json(
            &context(),
            &json!({
                "type": "file_written",
                "path": "src/lib.rs",
                "bytes_written": 128
            }),
        );
        let notice = file
            .status
            .expect("file status")
            .event
            .metadata
            .file_mutation
            .expect("file metadata");
        assert_eq!(notice.path, "src/lib.rs");
        assert_eq!(notice.operation, file_mutation_operations::WRITE);
        assert_eq!(notice.bytes_written, Some(128));
    }

    #[test]
    fn ui_protocol_progress_maps_unknown_to_warning() {
        let mapping = map_progress_json(&context(), &json!({ "type": "surprise" }));

        let warning = mapping.warning.expect("warning");
        assert_eq!(warning.code, "unmapped_progress");
        assert!(warning.message.contains("surprise"));
        assert!(mapping.notifications.is_empty());
        assert_eq!(mapping.status, None);
    }
}
