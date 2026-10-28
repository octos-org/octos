//! Bridge durable external-job receipts into the existing supervised background
//! task lifecycle. This observes an already submitted job; it never submits,
//! retries, cancels, or validates the output of the external job itself.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{Tool, ToolContext, ToolInputError, ToolResult};
use crate::policy::FilesystemScope;

const MAX_RECEIPT_BYTES: u64 = 256 * 1024;
const MAX_WAIT_SECS: u64 = 86_400;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) const START_MESSAGE: &str = "Watching the existing external job. Continue independent work; \
    completion or failure will arrive through background-task notifications. Do not submit the job \
    again or use fixed shell sleeps to check it. A successful job still needs artifact validation.";

pub struct WatchExternalJobTool {
    base_dir: PathBuf,
    filesystem_scope: FilesystemScope,
}

impl WatchExternalJobTool {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            filesystem_scope: FilesystemScope::Workspace,
        }
    }

    pub fn with_filesystem_scope(mut self, scope: FilesystemScope) -> Self {
        self.filesystem_scope = scope;
        self
    }

    fn parse_input(&self, args: &Value) -> Result<Input> {
        let input: Input = super::args::parse_tool_args(self.name(), &self.input_schema(), args)?;
        if input.receipt_path.trim().is_empty()
            || input.receipt_path.len() > 4096
            || input.job_id.trim().is_empty()
            || input.job_id.len() > 256
            || !(1..=MAX_WAIT_SECS).contains(&input.timeout_secs)
            || input
                .request_sha256
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 128)
        {
            return Err(ToolInputError::new("Provide a nonempty receipt_path (max 4096 bytes), job_id \
                (max 256 bytes), optional nonempty request_sha256 (max 128 bytes), and timeout_secs \
                between 1 and 86400.").into());
        }
        Ok(input)
    }

    fn resolve(&self, ctx: &ToolContext, path: &str) -> Result<PathBuf> {
        match ctx.session_scope.as_ref() {
            Some(scope) => super::resolve_path_for_session_scope_read(scope, path)
                .map_err(|e| eyre::eyre!("{e}")),
            None if !self.filesystem_scope.is_host() => {
                // The legacy lexical resolver alone accepts symlinked parents.
                // Use the scoped reader's canonical containment check even for
                // direct/CLI callers that have not supplied a SessionScope.
                let scope = octos_core::SessionScope::solo(self.base_dir.clone(), vec![])?;
                super::resolve_path_for_session_scope_read(&scope, path)
                    .map_err(|e| eyre::eyre!("{e}"))
            }
            None => super::resolve_path_with_scope(&self.base_dir, path, self.filesystem_scope),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    receipt_path: String,
    job_id: String,
    #[serde(default)]
    request_sha256: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    1800
}

fn report(
    input: &Input,
    start: Instant,
    watch_status: &str,
    job_status: Option<&str>,
    terminal: bool,
    detail: &str,
) -> ToolResult {
    let value = json!({
        "job_id": input.job_id,
        "receipt_path": input.receipt_path,
        "watch_status": watch_status,
        "job_status": job_status.map(|s| octos_core::truncated_utf8(s, 128, "…")),
        "job_terminal": terminal,
        "waited_ms": start.elapsed().as_millis(),
        "detail": octos_core::truncated_utf8(detail, 2048, "…"),
    });
    ToolResult {
        success: watch_status == "finished" && job_status == Some("completed"),
        output: value.to_string(),
        structured_metadata: Some(value),
        ..Default::default()
    }
}

// Bounded read from one descriptor. NONBLOCK prevents a FIFO masquerading as a
// receipt from parking a blocking-pool thread forever; only regular files pass.
// Resolve scope afresh on every poll, then reject leaf symlinks at open time.
async fn read_receipt(path: &Path) -> std::io::Result<Vec<u8>> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(not(unix))]
        if path.symlink_metadata()?.is_symlink() {
            return Err(std::io::Error::other("symlink receipt rejected"));
        }
        let file = options.open(&path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > MAX_RECEIPT_BYTES {
            return Err(std::io::Error::other(
                "receipt must be a regular file of at most 256 KiB",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECEIPT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(std::io::Error::other("receipt exceeds 256 KiB"));
        }
        Ok(bytes)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[async_trait]
impl Tool for WatchExternalJobTool {
    fn name(&self) -> &str {
        "watch_external_job"
    }

    fn description(&self) -> &str {
        "Watch an ALREADY SUBMITTED external job's local JSON receipt until it finishes. Use this \
        instead of sleep-and-poll loops for renders, image generation, builds or remote jobs that \
        publish receipts. Runs as a supervised background task in the agent; continue independent \
        work and receive a terminal notification. Receipt contract: id equals job_id; status is \
        queued/pending/running, then completed/failed/cancelled/canceled/interrupted. Producers \
        should atomically replace the receipt. Optionally pin request_sha256. Missing/partial JSON \
        is retried until timeout. Timeout only ends observation, never cancels or resubmits the job. \
        Completion is not artifact QA. Read results from receipt_path after notification. Register \
        once per job; after a runtime restart re-watch the same receipt, never re-submit the job."
    }

    fn tags(&self) -> &[&str] {
        &["fs", "code"]
    }

    fn input_schema(&self) -> Value {
        json!({
            "type":"object", "additionalProperties":false,
            "properties": {
                "receipt_path":{"type":"string", "description":"Local JSON job receipt path, relative to the session workspace or an allowed absolute path."},
                "job_id":{"type":"string", "description":"Expected immutable job id; must match the receipt's id."},
                "request_sha256":{"type":"string", "description":"Optional expected request hash, to reject a stale/replaced job receipt."},
                "timeout_secs":{"type":"integer", "minimum":1, "maximum":MAX_WAIT_SECS, "default":1800,
                    "description":"Maximum observation time. Returns immediately when terminal; this is not a fixed sleep."}
            },
            "required":["receipt_path", "job_id"]
        })
    }

    fn execution_timeout_secs(&self) -> Option<u64> {
        Some(MAX_WAIT_SECS + 5)
    }

    fn preserve_background_registration(&self) -> bool {
        false
    }

    fn retry_background_on_transient_error(&self) -> bool {
        false
    }

    async fn pre_flight_validate(&self, args: &Value) -> Result<(), String> {
        self.parse_input(args)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let input = self.parse_input(args)?;
        let start = Instant::now();
        if !ctx.permissions.is_tool_allowed(self.name()) {
            return Ok(report(
                &input,
                start,
                "error",
                None,
                false,
                "watch_external_job is not permitted in this context",
            ));
        }
        let deadline = start + Duration::from_secs(input.timeout_secs);
        let mut last_status: Option<String> = None;
        let mut last_issue = String::new();
        loop {
            let path = match self.resolve(ctx, &input.receipt_path) {
                Ok(path) => path,
                Err(e) => {
                    return Ok(report(
                        &input,
                        start,
                        "error",
                        last_status.as_deref(),
                        false,
                        &e.to_string(),
                    ));
                }
            };
            match tokio::time::timeout_at(deadline, read_receipt(&path)).await {
                Ok(Ok(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(receipt) => {
                        if receipt.get("id").and_then(Value::as_str) != Some(input.job_id.as_str())
                        {
                            return Ok(report(
                                &input,
                                start,
                                "error",
                                None,
                                false,
                                "Receipt id does not match job_id; refusing stale or unrelated completion.",
                            ));
                        }
                        if let Some(expected) = &input.request_sha256
                            && receipt.get("request_sha256").and_then(Value::as_str)
                                != Some(expected.as_str())
                        {
                            return Ok(report(
                                &input,
                                start,
                                "error",
                                None,
                                false,
                                "Receipt request_sha256 mismatch; refusing completion for a different request.",
                            ));
                        }
                        let status = receipt.get("status").and_then(Value::as_str).unwrap_or("");
                        match status {
                            "completed" => return Ok(report(&input, start, "finished", Some(status), true,
                                "External job completed. Read the receipt for result paths and validate artifacts before claiming delivery.")),
                            "failed" | "cancelled" | "canceled" | "interrupted" => return Ok(report(&input, start, "finished", Some(status), true,
                                receipt.get("error").and_then(Value::as_str).unwrap_or("External job ended without success. Inspect the receipt before deciding whether to retry."))),
                            "queued" | "pending" | "running" => {
                                last_status = Some(status.to_string());
                                last_issue.clear();
                            },
                            _ => return Ok(report(&input, start, "error", Some(status), false, "Unsupported or missing receipt status; completion is unverified.")),
                        }
                    }
                    Err(_) => last_issue = "Receipt is not yet valid JSON.".into(),
                },
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    last_issue = "Receipt not found yet.".into()
                }
                Ok(Err(e)) => {
                    return Ok(report(
                        &input,
                        start,
                        "error",
                        last_status.as_deref(),
                        false,
                        &format!("Cannot read receipt: {e}"),
                    ));
                }
                Err(_) => {
                    last_issue =
                        "Receipt read did not finish before the observation deadline.".into()
                }
            }
            if Instant::now() >= deadline {
                return Ok(report(
                    &input,
                    start,
                    "timed_out",
                    last_status.as_deref(),
                    false,
                    &format!(
                        "Observation timed out; external job completion remains unverified. {last_issue} Re-watch this receipt if needed; do not resubmit the job merely because the watch timed out."
                    ),
                ));
            }
            tokio::time::sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_reject_invalid_watch_arguments_before_backgrounding() {
        let tool = WatchExternalJobTool::new(".");
        for args in [
            json!({"receipt_path":"job.json", "job_id":""}),
            json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":0}),
            json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":86401}),
            json!({"receipt_path":"job.json", "job_id":"job", "submit":true}),
        ] {
            assert!(tool.pre_flight_validate(&args).await.is_err());
        }
    }

    #[tokio::test]
    async fn should_use_session_scope_and_respect_typed_permissions() {
        let legacy = tempfile::tempdir().unwrap();
        let scoped = tempfile::tempdir().unwrap();
        std::fs::write(
            scoped.path().join("job.json"),
            r#"{"id":"job","status":"completed"}"#,
        )
        .unwrap();
        let tool = WatchExternalJobTool::new(legacy.path());
        let args = json!({"receipt_path":"job.json", "job_id":"job", "timeout_secs":1});
        let mut ctx = ToolContext::zero();
        ctx.session_scope = Some(std::sync::Arc::new(
            octos_core::SessionScope::solo(scoped.path().to_owned(), vec![]).unwrap(),
        ));
        assert!(
            tool.execute_with_context(&ctx, &args)
                .await
                .unwrap()
                .success
        );
        ctx.permissions =
            super::super::ToolPermissions::from_profile(&crate::profile::ProfileDefinition {
                tools: crate::profile::ProfileTools::DenyList {
                    tools: vec!["group:fs".into()],
                },
                ..Default::default()
            });
        let refused = tool.execute_with_context(&ctx, &args).await.unwrap();
        assert!(!refused.success);
        assert!(refused.output.contains("not permitted"));
    }

    #[tokio::test]
    async fn should_reject_oversized_receipt_without_reading_it_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("job.json"),
            vec![b' '; MAX_RECEIPT_BYTES as usize + 1],
        )
        .unwrap();
        let result = WatchExternalJobTool::new(dir.path())
            .execute(&json!({
                "receipt_path":"job.json", "job_id":"job", "timeout_secs":1
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("256 KiB"));
    }
}
