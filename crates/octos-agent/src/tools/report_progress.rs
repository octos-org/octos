//! UPCR-2026-R3DB2ECCA: an agent's own progress report to the user of its
//! session. It does not finish the turn, wake another agent, or imply that
//! anyone verified or approved the work. The host stamps session, origin and
//! turn; the model supplies only the kind and the texts.
use super::{Tool, ToolResult};
use async_trait::async_trait;
use eyre::Result;
use octos_core::ui_protocol::AgentReportKind;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Longest `summary` the tool accepts, in bytes.
pub const PROGRESS_REPORT_SUMMARY_MAX: usize = 2_000;
/// Longest `next_step` or `evidence` the tool accepts, in bytes.
pub const PROGRESS_REPORT_DETAIL_MAX: usize = 500;

/// The model-authored fields of one report. Nothing here names a session, an
/// agent or a turn, so a report cannot claim to come from someone else.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressReport {
    pub kind: AgentReportKind,
    pub summary: String,
    #[serde(default)]
    pub next_step: Option<String>,
    #[serde(default)]
    pub evidence: Option<String>,
}

pub type ReportProgressCallback =
    Arc<dyn Fn(ProgressReport) -> Result<String, String> + Send + Sync>;
pub struct ReportProgressTool {
    publish: ReportProgressCallback,
}
impl ReportProgressTool {
    pub fn new(publish: ReportProgressCallback) -> Self {
        Self { publish }
    }
}

fn refuse(output: String) -> ToolResult {
    ToolResult {
        output,
        success: false,
        ..Default::default()
    }
}

#[async_trait]
impl Tool for ReportProgressTool {
    fn name(&self) -> &str {
        "report_progress"
    }
    fn description(&self) -> &str {
        "Tell the user how a multi-stage task is going while you keep working: a finished stage or a found cause (kind milestone), something you cannot get past (blocker), a correction of something you reported earlier (correction), or the result of a check you ran yourself (verification). Write one to three sentences and say what is still unverified. The user sees it attributed to you. It is your own statement, not an approval or proof; it does not end the turn and nobody answers it. Do not report single routine steps. Never include secrets or raw tool output."
    }
    fn tags(&self) -> &[&str] {
        &["gateway"]
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","required":["kind","summary"],"additionalProperties":false,"properties":{
            "kind":{"type":"string","enum":["milestone","blocker","correction","verification"]},
            "summary":{"type":"string","minLength":1,"maxLength":PROGRESS_REPORT_SUMMARY_MAX},
            "next_step":{"type":"string","maxLength":PROGRESS_REPORT_DETAIL_MAX,"description":"What you will do next, when you know."},
            "evidence":{"type":"string","maxLength":PROGRESS_REPORT_DETAIL_MAX,"description":"Path or command the user can check. Its existence is not acceptance."}
        }})
    }
    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let report = match serde_json::from_value::<ProgressReport>(args.clone()) {
            Ok(report) => report,
            Err(e) => return Ok(refuse(format!("invalid report_progress arguments: {e}"))),
        };
        let detail_too_long = [&report.next_step, &report.evidence]
            .into_iter()
            .flatten()
            .any(|detail| detail.len() > PROGRESS_REPORT_DETAIL_MAX);
        if report.summary.trim().is_empty()
            || report.summary.len() > PROGRESS_REPORT_SUMMARY_MAX
            || detail_too_long
        {
            return Ok(refuse(format!(
                "summary must contain 1..{PROGRESS_REPORT_SUMMARY_MAX} UTF-8 bytes; next_step and evidence at most {PROGRESS_REPORT_DETAIL_MAX} each"
            )));
        }
        Ok(match (self.publish)(report) {
            Ok(output) => ToolResult {
                output,
                success: true,
                ..Default::default()
            },
            Err(output) => refuse(output),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn should_refuse_caller_chosen_identity_when_a_report_is_published() {
        let tool =
            ReportProgressTool::new(Arc::new(|r| Ok(format!("{:?}: {}", r.kind, r.summary))));
        let sent = tool
            .execute(&json!({"kind":"blocker","summary":"tests need a key","next_step":"ask"}))
            .await
            .unwrap();
        assert!(sent.success);
        assert_eq!(sent.output, "Blocker: tests need a key");
        for args in [
            json!({"kind":"milestone","summary":" "}),
            json!({"kind":"approved","summary":"done"}),
            json!({"kind":"milestone","summary":"x".repeat(PROGRESS_REPORT_SUMMARY_MAX + 1)}),
            json!({"kind":"milestone","summary":"ok","evidence":"x".repeat(PROGRESS_REPORT_DETAIL_MAX + 1)}),
            json!({"kind":"milestone","summary":"done","session_id":"other","origin_id":"reviewer"}),
        ] {
            assert!(!tool.execute(&args).await.unwrap().success, "{args}");
        }
    }
}
