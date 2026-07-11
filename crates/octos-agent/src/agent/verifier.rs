//! Optional inference-time verifier and compact structured turn ledger.
//!
//! The verifier is deliberately opt-in. When no [`AgentVerifierConfig`] is
//! attached to an [`Agent`](super::Agent), the hot loop keeps the legacy
//! EndTurn/tool-use behaviour and does not write a sidecar ledger.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use octos_core::{Message, MessageRole};
use octos_llm::{ChatConfig, Lane, LaneContext, LlmProvider, ResponseFormat, ToolChoice};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use super::Agent;
use super::turn_state::LoopTurnState;
use crate::TokenTracker;

pub const TURN_LEDGER_SCHEMA_VERSION: u32 = 1;
const MAX_LEDGER_ENTRIES_IN_MEMORY: usize = 64;
const VERIFIER_MAX_TOKENS: u32 = 512;

/// Opt-in verifier runtime configuration.
///
/// Production callers should pass a cheap/fast provider here. Tests use this
/// to prove verifier calls are separated from the primary planner provider.
#[derive(Clone)]
pub struct AgentVerifierConfig {
    pub enabled: bool,
    pub provider: Arc<dyn LlmProvider>,
    pub model_label: String,
    pub lane_context: LaneContext,
    pub ledger_path: Option<PathBuf>,
    pub max_quiet_turns: u32,
}

impl AgentVerifierConfig {
    pub fn with_provider(provider: Arc<dyn LlmProvider>, model_label: impl Into<String>) -> Self {
        Self {
            enabled: true,
            provider,
            model_label: model_label.into(),
            lane_context: LaneContext {
                lane: Some(Lane::FastChat),
                config: None,
            },
            ledger_path: None,
            max_quiet_turns: 0,
        }
    }

    pub fn with_ledger_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.ledger_path = Some(path.into());
        self
    }

    pub fn with_max_quiet_turns(mut self, turns: u32) -> Self {
        self.max_quiet_turns = turns;
        self
    }

    pub fn with_lane_context(mut self, lane_context: LaneContext) -> Self {
        self.lane_context = lane_context;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Ok,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    ToolError,
    ContractFail,
    Timeout,
    BadArgs,
    PolicyDenied,
    HookDenied,
    SessionLimit,
    Panic,
    Unknown,
}

impl ErrorClass {
    fn as_verifier_label(self) -> &'static str {
        match self {
            Self::ToolError => "ToolError",
            Self::ContractFail => "ContractFail",
            Self::Timeout => "Timeout",
            Self::BadArgs => "BadArgs",
            Self::PolicyDenied => "PolicyDenied",
            Self::HookDenied => "HookDenied",
            Self::SessionLimit => "SessionLimit",
            Self::Panic => "Panic",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLedgerEntry {
    pub schema_version: u32,
    pub turn: u32,
    pub stated_intent: String,
    pub tool: String,
    pub args_fingerprint: u64,
    pub outcome: TurnOutcome,
    pub error_class: Option<ErrorClass>,
    pub result_digest: String,
    pub repeating: bool,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum VerifierVerdict {
    Progressing,
    Insufficient { reason: String },
    Repeating { error_class: ErrorClass },
    Blocked { reason: String },
    ReadyToAnswer,
}

impl VerifierVerdict {
    fn label(&self) -> &'static str {
        match self {
            Self::Progressing => "Progressing",
            Self::Insufficient { .. } => "Insufficient",
            Self::Repeating { .. } => "Repeating",
            Self::Blocked { .. } => "Blocked",
            Self::ReadyToAnswer => "ReadyToAnswer",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            Self::Insufficient { reason } | Self::Blocked { reason } => Some(reason.clone()),
            Self::Repeating { error_class } => {
                Some(format!("error_class={}", error_class.as_verifier_label()))
            }
            _ => None,
        }
    }

    pub(crate) fn ready_to_answer(&self) -> bool {
        matches!(self, Self::ReadyToAnswer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifierRecord {
    pub schema_version: u32,
    pub turn: u32,
    pub verdict: VerifierVerdict,
    pub model: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub(crate) struct TurnLedger {
    entries: VecDeque<TurnLedgerEntry>,
    latest_verdict: Option<VerifierVerdict>,
    last_verified_turn: Option<u32>,
    path: Option<PathBuf>,
    write_error_logged: bool,
}

impl TurnLedger {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_LEDGER_ENTRIES_IN_MEMORY),
            latest_verdict: None,
            last_verified_turn: None,
            path,
            write_error_logged: false,
        }
    }

    pub(crate) fn push_entry(&mut self, entry: TurnLedgerEntry) {
        self.append_jsonl(&entry);
        self.entries.push_back(entry);
        while self.entries.len() > MAX_LEDGER_ENTRIES_IN_MEMORY {
            self.entries.pop_front();
        }
    }

    pub(crate) fn record_verdict(&mut self, turn: u32, verdict: VerifierVerdict, model: &str) {
        let record = VerifierRecord {
            schema_version: TURN_LEDGER_SCHEMA_VERSION,
            turn,
            verdict: verdict.clone(),
            model: model.to_string(),
            timestamp: chrono::Utc::now(),
        };
        self.append_jsonl(&record);
        self.latest_verdict = Some(verdict);
        self.last_verified_turn = Some(turn);
    }

    pub(crate) fn latest_verdict(&self) -> Option<&VerifierVerdict> {
        self.latest_verdict.as_ref()
    }

    pub(crate) fn should_verify_after_tool_batch(&self, max_quiet_turns: u32) -> bool {
        let Some(latest) = self.entries.back() else {
            return false;
        };
        // codex pre-merge P2: a single LLM response can execute MULTIPLE tools,
        // appending several entries. Checking only `entries.back()` misses a
        // failure/repeat in an EARLIER result of the batch when a later result
        // succeeds — so with the default `max_quiet_turns == 0` the verifier
        // never classifies the failure and a premature EndTurn bypasses it.
        // Scan every UNVERIFIED entry (turn beyond `last_verified_turn`) for a
        // problem signal.
        let problem_in_unverified = self
            .entries
            .iter()
            .filter(|e| match self.last_verified_turn {
                Some(last) => e.turn > last,
                None => true,
            })
            .any(|e| e.repeating || e.outcome == TurnOutcome::Err);
        if problem_in_unverified {
            return true;
        }
        if max_quiet_turns == 0 {
            return false;
        }
        match self.last_verified_turn {
            Some(last) => latest.turn.saturating_sub(last) >= max_quiet_turns,
            None => latest.turn >= max_quiet_turns,
        }
    }

    pub(crate) fn ready_gate_active(&self) -> bool {
        self.latest_verdict
            .as_ref()
            .is_some_and(|verdict| !verdict.ready_to_answer())
    }

    pub(crate) fn recent_view(&self, limit: usize) -> String {
        let start = self.entries.len().saturating_sub(limit);
        self.entries
            .iter()
            .skip(start)
            .map(|entry| {
                let error = entry
                    .error_class
                    .map(|class| class.as_verifier_label())
                    .unwrap_or("-");
                format!(
                    "- turn={} tool={} outcome={:?} error={} repeating={} args={} result={}",
                    entry.turn,
                    entry.tool,
                    entry.outcome,
                    error,
                    entry.repeating,
                    entry.args_fingerprint,
                    entry.result_digest
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn append_jsonl<T: Serialize>(&mut self, row: &T) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                self.log_write_error(&path, error);
                return;
            }
        }
        let line = match serde_json::to_string(row) {
            Ok(line) => line,
            Err(error) => {
                self.log_write_error(&path, error);
                return;
            }
        };
        let mut line = line;
        line.push('\n');
        if let Err(error) = append_line(&path, &line) {
            self.log_write_error(&path, error);
        }
    }

    fn log_write_error(&mut self, path: &std::path::Path, error: impl std::fmt::Display) {
        if !self.write_error_logged {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to append turn ledger sidecar; continuing without durable verifier ledger"
            );
            self.write_error_logged = true;
        }
    }
}

fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

pub(crate) fn ledger_entry_from_tool_result(
    turn: u32,
    stated_intent: Option<&str>,
    tool: &str,
    args: &serde_json::Value,
    success: Option<bool>,
    result: &str,
    repeating: bool,
) -> TurnLedgerEntry {
    let error_class = classify_tool_result(success, result);
    TurnLedgerEntry {
        schema_version: TURN_LEDGER_SCHEMA_VERSION,
        turn,
        stated_intent: compact_intent(stated_intent.unwrap_or_default()),
        tool: tool.to_string(),
        args_fingerprint: fingerprint_args(args),
        outcome: if error_class.is_some() {
            TurnOutcome::Err
        } else {
            TurnOutcome::Ok
        },
        error_class,
        result_digest: digest_result(result),
        repeating,
        timestamp: chrono::Utc::now(),
    }
}

fn compact_intent(intent: &str) -> String {
    let trimmed = intent.trim();
    if trimmed.is_empty() {
        return "(not stated)".to_string();
    }
    octos_core::truncated_utf8(trimmed, 160, "...")
}

fn classify_tool_result(success: Option<bool>, result: &str) -> Option<ErrorClass> {
    if success == Some(true) {
        return None;
    }
    let trimmed = result.trim_start();
    if trimmed.is_empty() && success != Some(false) {
        return None;
    }
    if trimmed.starts_with("[VALIDATION FAILED]") || trimmed.contains("workspace contract") {
        return Some(ErrorClass::ContractFail);
    }
    if trimmed.starts_with("[POLICY DENIED]") {
        return Some(ErrorClass::PolicyDenied);
    }
    if trimmed.starts_with("[HOOK DENIED]") {
        return Some(ErrorClass::HookDenied);
    }
    if trimmed.starts_with("[SESSION LIMIT]") {
        return Some(ErrorClass::SessionLimit);
    }
    if trimmed.contains("timed out") || trimmed.contains("timeout") {
        return Some(ErrorClass::Timeout);
    }
    if trimmed.contains("malformed")
        || trimmed.contains("invalid")
        || trimmed.contains("bad argument")
        || trimmed.contains("BadArgs")
    {
        return Some(ErrorClass::BadArgs);
    }
    if trimmed.starts_with("Tool '") && trimmed.contains("panicked") {
        return Some(ErrorClass::Panic);
    }
    if success == Some(false) || trimmed.starts_with("Error:") {
        return Some(ErrorClass::ToolError);
    }
    None
}

fn fingerprint_args(args: &serde_json::Value) -> u64 {
    let digest = Sha256::digest(normalized_json(args).as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

fn digest_result(result: &str) -> String {
    let digest = Sha256::digest(result.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalized_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

impl Agent {
    pub fn with_verifier_config(mut self, config: AgentVerifierConfig) -> Self {
        self.verifier_config = config.enabled.then_some(config);
        self
    }

    pub(super) fn new_turn_ledger(&self) -> Option<TurnLedger> {
        self.verifier_config
            .as_ref()
            .filter(|config| config.enabled)
            .map(|config| TurnLedger::new(config.ledger_path.clone()))
    }

    pub(super) async fn maybe_run_verifier_after_tool_batch(
        &self,
        messages: &mut Vec<Message>,
        ledger: Option<&mut TurnLedger>,
        iteration: u32,
        turn: &mut LoopTurnState,
        tracker: Option<&TokenTracker>,
    ) -> Result<()> {
        let Some(config) = self
            .verifier_config
            .as_ref()
            .filter(|config| config.enabled)
        else {
            return Ok(());
        };
        let Some(ledger) = ledger else {
            return Ok(());
        };
        if !ledger.should_verify_after_tool_batch(config.max_quiet_turns) {
            return Ok(());
        }
        let verdict = self
            .run_verifier(config, ledger, None, iteration, turn, tracker)
            .await?;
        inject_verifier_note(messages, ledger, &verdict);
        Ok(())
    }

    pub(super) async fn verifier_allows_termination(
        &self,
        messages: &mut Vec<Message>,
        ledger: Option<&mut TurnLedger>,
        proposed_answer: &str,
        iteration: u32,
        turn: &mut LoopTurnState,
        tracker: Option<&TokenTracker>,
    ) -> Result<bool> {
        let Some(config) = self
            .verifier_config
            .as_ref()
            .filter(|config| config.enabled)
        else {
            return Ok(true);
        };
        let Some(ledger) = ledger else {
            return Ok(true);
        };
        if ledger
            .latest_verdict()
            .is_some_and(VerifierVerdict::ready_to_answer)
        {
            return Ok(true);
        }
        if !ledger.ready_gate_active() {
            return Ok(true);
        }
        let verdict = self
            .run_verifier(
                config,
                ledger,
                Some(proposed_answer),
                iteration,
                turn,
                tracker,
            )
            .await?;
        let ready = verdict.ready_to_answer();
        if !ready {
            inject_verifier_note(messages, ledger, &verdict);
        }
        Ok(ready)
    }

    async fn run_verifier(
        &self,
        config: &AgentVerifierConfig,
        ledger: &mut TurnLedger,
        proposed_answer: Option<&str>,
        iteration: u32,
        turn: &mut LoopTurnState,
        tracker: Option<&TokenTracker>,
    ) -> Result<VerifierVerdict> {
        let prompt = verifier_prompt(ledger, proposed_answer);
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You are the Octos verifier. Return only the requested JSON verdict."
                    .to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::User,
                content: prompt,
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        let verifier_config = ChatConfig {
            max_tokens: Some(VERIFIER_MAX_TOKENS),
            temperature: Some(0.0),
            tool_choice: ToolChoice::None,
            response_format: Some(ResponseFormat::JsonSchema {
                name: "octos_verifier_verdict".to_string(),
                schema: verifier_schema(),
                strict: true,
            }),
            ..Default::default()
        };
        let response = octos_llm::with_lane_context(
            config.lane_context.clone(),
            config.provider.chat(&messages, &[], &verifier_config),
        )
        .await?;
        turn.record_usage(
            response.usage.input_tokens,
            response.usage.output_tokens,
            tracker,
            // The verifier runs on its own provider — price its usage at
            // the verifier's model, not the conversation's active slot.
            octos_llm::pricing::model_pricing(&config.model_label)
                .map(|p| p.cost(response.usage.input_tokens, response.usage.output_tokens)),
        );
        let content = response.content.unwrap_or_default();
        let verdict = parse_verifier_verdict(&content).unwrap_or_else(|| {
            warn!(
                model = %config.model_label,
                response = %content,
                "verifier returned unparsable verdict; treating as insufficient"
            );
            VerifierVerdict::Insufficient {
                reason: "verifier returned an unparsable verdict".to_string(),
            }
        });
        ledger.record_verdict(iteration, verdict.clone(), &config.model_label);
        Ok(verdict)
    }
}

fn verifier_prompt(ledger: &TurnLedger, proposed_answer: Option<&str>) -> String {
    let answer = proposed_answer
        .filter(|answer| !answer.trim().is_empty())
        .map(|answer| {
            format!(
                "\n\nProposed answer:\n{}",
                octos_core::truncated_utf8(answer, 1200, "...")
            )
        })
        .unwrap_or_default();
    format!(
        "Classify the agent state using the recent TurnLedger. Prefer Repeating \
         when the ledger marks repeated failing identical tool results. Prefer \
         ReadyToAnswer only when the proposed answer is sufficient or the ledger \
         shows enough progress to answer.\n\nRecent TurnLedger:\n{}\n{}\n\n\
         Return JSON only, one of:\n\
         {{\"verdict\":\"Progressing\"}}\n\
         {{\"verdict\":\"Insufficient\",\"reason\":\"...\"}}\n\
         {{\"verdict\":\"Repeating\",\"error_class\":\"ContractFail\"}}\n\
         {{\"verdict\":\"Blocked\",\"reason\":\"...\"}}\n\
         {{\"verdict\":\"ReadyToAnswer\"}}",
        ledger.recent_view(5),
        answer
    )
}

fn inject_verifier_note(
    messages: &mut Vec<Message>,
    ledger: &TurnLedger,
    verdict: &VerifierVerdict,
) {
    let mut content = format!(
        "[verifier]\nverdict: {}\nrecent_turn_ledger:\n{}",
        verdict.label(),
        ledger.recent_view(5)
    );
    if let Some(detail) = verdict.detail() {
        content.push_str("\nreason: ");
        content.push_str(&detail);
    }
    content.push_str(
        "\nplanner_instruction: condition the next action on this structured verdict; \
         if Repeating, change tool, arguments, or strategy instead of replaying the same call.",
    );
    messages.push(Message {
        role: MessageRole::System,
        content,
        media: vec![],
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        client_message_id: None,
        thread_id: None,
        timestamp: chrono::Utc::now(),
    });
}

fn parse_verifier_verdict(content: &str) -> Option<VerifierVerdict> {
    let value: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    let verdict = value.get("verdict")?.as_str()?;
    match normalize_label(verdict).as_str() {
        "progressing" => Some(VerifierVerdict::Progressing),
        "insufficient" => Some(VerifierVerdict::Insufficient {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("insufficient progress")
                .to_string(),
        }),
        "repeating" => Some(VerifierVerdict::Repeating {
            error_class: parse_error_class(
                value
                    .get("error_class")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown"),
            ),
        }),
        "blocked" => Some(VerifierVerdict::Blocked {
            reason: value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("blocked")
                .to_string(),
        }),
        "readytoanswer" | "ready_to_answer" => Some(VerifierVerdict::ReadyToAnswer),
        _ => None,
    }
}

fn parse_error_class(raw: &str) -> ErrorClass {
    match normalize_label(raw).as_str() {
        "toolerror" | "tool_error" => ErrorClass::ToolError,
        "contractfail" | "contract_fail" => ErrorClass::ContractFail,
        "timeout" => ErrorClass::Timeout,
        "badargs" | "bad_args" => ErrorClass::BadArgs,
        "policydenied" | "policy_denied" => ErrorClass::PolicyDenied,
        "hookdenied" | "hook_denied" => ErrorClass::HookDenied,
        "sessionlimit" | "session_limit" => ErrorClass::SessionLimit,
        "panic" => ErrorClass::Panic,
        _ => ErrorClass::Unknown,
    }
}

fn normalize_label(raw: &str) -> String {
    raw.chars()
        .filter(|ch| *ch != '-' && !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn verifier_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "verdict": {
                "type": "string",
                "enum": ["Progressing", "Insufficient", "Repeating", "Blocked", "ReadyToAnswer"]
            },
            "reason": { "type": "string" },
            "error_class": {
                "type": "string",
                "enum": [
                    "ToolError",
                    "ContractFail",
                    "Timeout",
                    "BadArgs",
                    "PolicyDenied",
                    "HookDenied",
                    "SessionLimit",
                    "Panic",
                    "Unknown"
                ]
            }
        },
        "required": ["verdict"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ledger_marks_repeating_only_when_caller_reports_repeating() {
        let first = ledger_entry_from_tool_result(
            1,
            Some("check"),
            "check_workspace_contract",
            &json!({"path": "."}),
            Some(true),
            "running",
            false,
        );
        let second = ledger_entry_from_tool_result(
            2,
            Some("check"),
            "check_workspace_contract",
            &json!({"path": "."}),
            Some(true),
            "completed",
            false,
        );

        assert!(!first.repeating);
        assert!(!second.repeating);
        assert_eq!(first.outcome, TurnOutcome::Ok);
        assert_eq!(second.outcome, TurnOutcome::Ok);
        assert_ne!(first.result_digest, second.result_digest);
    }

    #[test]
    fn should_verify_detects_failure_earlier_in_multi_tool_batch() {
        // codex pre-merge P2: a multi-tool batch can have an EARLIER failure
        // and a LATER success. With max_quiet_turns == 0, checking only the
        // last entry would return false and skip verification. Scanning all
        // unverified entries must catch the earlier failure.
        let mut ledger = TurnLedger::new(None);
        // turn 1: FAILED tool result
        ledger.push_entry(ledger_entry_from_tool_result(
            1,
            Some("do thing"),
            "check_workspace_contract",
            &json!({"path": "."}),
            Some(false),
            "Error: contract failed",
            false,
        ));
        // turn 1 (same batch): SUCCESSFUL tool result (the back() entry)
        ledger.push_entry(ledger_entry_from_tool_result(
            1,
            Some("do other thing"),
            "read_file",
            &json!({"path": "ok.txt"}),
            Some(true),
            "file contents",
            false,
        ));
        assert!(
            matches!(
                ledger.entries.back().map(|e| e.outcome),
                Some(TurnOutcome::Ok)
            ),
            "test setup: the LAST entry must be a success (the trap)"
        );
        assert!(
            ledger.should_verify_after_tool_batch(0),
            "an earlier failure in the batch must trigger verification even though the last entry succeeded",
        );
    }

    #[test]
    fn should_verify_false_when_all_unverified_entries_succeed() {
        let mut ledger = TurnLedger::new(None);
        ledger.push_entry(ledger_entry_from_tool_result(
            1,
            Some("a"),
            "read_file",
            &json!({"path": "a"}),
            Some(true),
            "ok",
            false,
        ));
        ledger.push_entry(ledger_entry_from_tool_result(
            1,
            Some("b"),
            "read_file",
            &json!({"path": "b"}),
            Some(true),
            "ok2",
            false,
        ));
        assert!(
            !ledger.should_verify_after_tool_batch(0),
            "all-success batch with max_quiet_turns=0 must NOT trigger verification",
        );
    }

    #[test]
    fn parser_accepts_ready_and_repeating_verdicts() {
        assert_eq!(
            parse_verifier_verdict(r#"{"verdict":"ReadyToAnswer"}"#),
            Some(VerifierVerdict::ReadyToAnswer)
        );
        assert_eq!(
            parse_verifier_verdict(r#"{"verdict":"Repeating","error_class":"ContractFail"}"#),
            Some(VerifierVerdict::Repeating {
                error_class: ErrorClass::ContractFail
            })
        );
    }
}
