//! Human-approval model for suspend-and-resume approval flows.
//!
//! Backs the gateway-channel approval flow decided in
//! `docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md`: when a tool call matches a
//! configured [`HumanApprovalRules`] rule, the agent loop returns early with
//! a [`PendingApprovalDraft`] instead of executing the tool. The host
//! (session actor) projects the request to the channel, stores the
//! [`PendingApproval`], and resumes via [`PendingApprovalStore::consume`]
//! when an authorized human answers.
//!
//! Named `HumanApprovalRules` (not `ApprovalPolicy`) to avoid colliding with
//! the interactive-prompt gate [`crate::policy::ApprovalPolicy`]. The two are
//! complementary: `policy::ApprovalPolicy` controls whether a live-socket
//! client may be prompted mid-turn (`ToolApprovalRequester`); this module
//! drives the asynchronous flow for store-and-forward channels.
//!
//! Security invariants (do not weaken):
//! - `tool_args_digest` (SHA-256) binds an approval to the exact arguments
//!   drafted — a response with a different digest is rejected.
//! - the consumed-set rejects replayed responses after the first decision.
//! - responses are bound to the originating room and validated against the
//!   rule's `authorized_approvers` snapshot.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use octos_llm::canonical_object_keys;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalRiskLevel {
    Normal,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalTimeoutBehavior {
    Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequestSpec {
    pub title: String,
    pub summary: String,
    pub risk_level: ApprovalRiskLevel,
    pub authorized_approvers: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub on_timeout: ApprovalTimeoutBehavior,
}

impl ApprovalRequestSpec {
    pub fn validate(&self) -> Result<(), String> {
        if self.authorized_approvers.is_empty() {
            return Err("authorized_approvers must not be empty".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequestEnvelope {
    pub request_id: String,
    pub tool_name: String,
    pub tool_args_digest: String,
    pub title: String,
    pub summary: String,
    pub risk_level: ApprovalRiskLevel,
    pub authorized_approvers: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub on_timeout: ApprovalTimeoutBehavior,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalResponsePayload {
    pub request_id: String,
    pub decision: ApprovalDecision,
    pub source_event_id: String,
    pub tool_args_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRuleAction {
    /// Suspend this exact tool call until a human approves it once. The
    /// canonical-argument digest and consumed-set prevent replay.
    #[default]
    AllowOnce,
    /// Refuse the call before tool execution.
    Deny,
    /// Explicitly execute without prompting, overriding later rules for the
    /// same tool. This preserves the historical unmatched-tool default.
    DefaultAllow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRule {
    pub tools: Vec<String>,
    pub action: ApprovalRuleAction,
    pub risk_level: ApprovalRiskLevel,
    pub authorized_approvers: Vec<String>,
    pub expires_in_secs: u64,
    pub on_timeout: ApprovalTimeoutBehavior,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HumanApprovalRules {
    rules: Vec<ApprovalRule>,
}

impl HumanApprovalRules {
    pub fn new(rules: Vec<ApprovalRule>) -> Self {
        Self { rules }
    }

    pub fn rules(&self) -> &[ApprovalRule] {
        &self.rules
    }

    pub fn matching_rule(&self, tool_name: &str) -> Option<&ApprovalRule> {
        self.rules
            .iter()
            .find(|rule| rule.tools.iter().any(|tool| tool == tool_name))
    }

    pub fn draft_for_tool_call(
        &self,
        tool_name: &str,
        tool_id: &str,
        tool_args: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<Option<PendingApprovalDraft>, String> {
        let Some(rule) = self.matching_rule(tool_name) else {
            return Ok(None);
        };
        match rule.action {
            ApprovalRuleAction::AllowOnce => {}
            ApprovalRuleAction::DefaultAllow => return Ok(None),
            ApprovalRuleAction::Deny => {
                return Err(format!("tool '{tool_name}' is denied by approval policy"));
            }
        }

        let spec = ApprovalRequestSpec {
            title: approval_title_for_tool(tool_name),
            summary: approval_summary_for_tool(tool_name, &tool_args),
            risk_level: rule.risk_level,
            authorized_approvers: rule.authorized_approvers.clone(),
            expires_at: now + chrono::Duration::seconds(rule.expires_in_secs as i64),
            on_timeout: rule.on_timeout,
        };

        PendingApprovalDraft::from_spec(tool_name, tool_id, tool_args, spec).map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalDraft {
    pub request: ApprovalRequestEnvelope,
    pub tool_id: String,
    pub tool_args: serde_json::Value,
}

impl PendingApprovalDraft {
    pub fn from_spec(
        tool_name: &str,
        tool_id: &str,
        tool_args: serde_json::Value,
        spec: ApprovalRequestSpec,
    ) -> Result<Self, String> {
        spec.validate()?;
        let tool_args_digest = digest_tool_call(tool_name, &tool_args);
        Ok(Self {
            request: ApprovalRequestEnvelope {
                request_id: next_request_id(),
                tool_name: tool_name.to_string(),
                tool_args_digest,
                title: spec.title,
                summary: spec.summary,
                risk_level: spec.risk_level,
                authorized_approvers: spec.authorized_approvers,
                expires_at: spec.expires_at,
                on_timeout: spec.on_timeout,
            },
            tool_id: tool_id.to_string(),
            tool_args,
        })
    }

    pub fn into_pending(
        self,
        room_id: impl Into<String>,
        requester: impl Into<String>,
    ) -> PendingApproval {
        PendingApproval {
            request: self.request,
            room_id: room_id.into(),
            requester: requester.into(),
            tool_id: self.tool_id,
            tool_args: self.tool_args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub request: ApprovalRequestEnvelope,
    pub room_id: String,
    pub requester: String,
    pub tool_id: String,
    pub tool_args: serde_json::Value,
}

impl PendingApproval {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.request.expires_at <= now
    }

    pub fn revalidate_response(
        &self,
        room_id: &str,
        sender_user_id: &str,
        response: &ApprovalResponsePayload,
        now: DateTime<Utc>,
    ) -> Result<(), ApprovalValidationError> {
        if room_id != self.room_id {
            return Err(ApprovalValidationError::WrongRoom);
        }
        if self.is_expired(now) {
            return Err(ApprovalValidationError::Expired);
        }
        if response.tool_args_digest != self.request.tool_args_digest {
            return Err(ApprovalValidationError::DigestMismatch);
        }
        if !self
            .request
            .authorized_approvers
            .iter()
            .any(|approver| approver == sender_user_id)
        {
            return Err(ApprovalValidationError::Unauthorized);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalValidationError {
    UnknownRequest,
    AlreadyConsumed,
    WrongRoom,
    Expired,
    Unauthorized,
    DigestMismatch,
}

#[derive(Debug, Default)]
pub struct PendingApprovalStore {
    pending: HashMap<String, PendingApproval>,
    consumed: HashSet<String>,
}

impl PendingApprovalStore {
    pub fn insert(&mut self, pending: PendingApproval) {
        self.pending
            .insert(pending.request.request_id.clone(), pending);
    }

    pub fn get(&self, request_id: &str) -> Option<&PendingApproval> {
        self.pending.get(request_id)
    }

    pub fn remove(&mut self, request_id: &str) -> Option<PendingApproval> {
        self.pending.remove(request_id)
    }

    pub fn mark_consumed(&mut self, request_id: &str) {
        self.consumed.insert(request_id.to_string());
    }

    pub fn is_consumed(&self, request_id: &str) -> bool {
        self.consumed.contains(request_id)
    }

    pub fn validate_response(
        &self,
        room_id: &str,
        sender_user_id: &str,
        response: &ApprovalResponsePayload,
        now: DateTime<Utc>,
    ) -> Result<(), ApprovalValidationError> {
        if self.is_consumed(&response.request_id) {
            return Err(ApprovalValidationError::AlreadyConsumed);
        }
        let pending = self
            .pending
            .get(&response.request_id)
            .ok_or(ApprovalValidationError::UnknownRequest)?;
        pending.revalidate_response(room_id, sender_user_id, response, now)
    }

    pub fn consume(
        &mut self,
        room_id: &str,
        sender_user_id: &str,
        response: &ApprovalResponsePayload,
        now: DateTime<Utc>,
    ) -> Result<PendingApproval, ApprovalValidationError> {
        self.validate_response(room_id, sender_user_id, response, now)?;
        let pending = self
            .pending
            .remove(&response.request_id)
            .ok_or(ApprovalValidationError::UnknownRequest)?;
        self.mark_consumed(&response.request_id);
        Ok(pending)
    }
}

/// Digest tool arguments into a stable identity string. Object keys are
/// canonicalized to sorted order first, so arguments hash the same no matter
/// which key order the provider serialized them with.
pub fn digest_tool_args(args: &serde_json::Value) -> String {
    let encoded = serde_json::to_vec(&canonical_object_keys(args.clone())).unwrap_or_default();
    let digest = Sha256::digest(encoded);
    format!("sha256:{digest:x}")
}

/// Digest the normalized call, not just its wire spelling. File-tool aliases
/// are canonicalized before hashing so `filePath`/`oldString`/`newString`
/// cannot create a second approval identity for the same accepted call.
/// Different paths, patch text, or replacement content still produce a
/// different digest and can never replay the original approval.
pub fn digest_tool_call(tool_name: &str, args: &serde_json::Value) -> String {
    let mut normalized = args.clone();
    if let Some(object) = normalized.as_object_mut() {
        let aliases: &[(&str, &str)] = match tool_name {
            "write_file" => &[("filePath", "path")],
            "edit_file" => &[
                ("filePath", "path"),
                ("oldString", "old_string"),
                ("newString", "new_string"),
            ],
            _ => &[],
        };
        for (alias, canonical) in aliases {
            if !object.contains_key(*canonical)
                && let Some(value) = object.remove(*alias)
            {
                object.insert((*canonical).to_string(), value);
            }
        }
    }
    digest_tool_args(&normalized)
}

fn approval_title_for_tool(tool_name: &str) -> String {
    match tool_name {
        "shell" => "Approve shell command".to_string(),
        "write_file" => "Approve file write".to_string(),
        "edit_file" => "Approve file edit".to_string(),
        "diff_edit" => "Approve unified diff".to_string(),
        "apply_patch" => "Approve patch".to_string(),
        "read_file" => "Approve file read".to_string(),
        _ => format!("Approve {tool_name}"),
    }
}

fn approval_summary_for_tool(tool_name: &str, tool_args: &serde_json::Value) -> String {
    match tool_name {
        "shell" => string_arg(tool_args, "command").unwrap_or_else(|| compact_tool_args(tool_args)),
        "write_file" | "read_file" | "edit_file" | "diff_edit" => string_arg(tool_args, "path")
            .or_else(|| string_arg(tool_args, "filePath"))
            .unwrap_or_else(|| compact_tool_args(tool_args)),
        "apply_patch" => {
            if let Some(path) = string_arg(tool_args, "path") {
                path
            } else if let Some(patch) = string_arg(tool_args, "patch") {
                let paths: Vec<&str> = patch
                    .lines()
                    .filter_map(|line| {
                        [
                            "*** Add File: ",
                            "*** Update File: ",
                            "*** Delete File: ",
                            "*** Move to: ",
                        ]
                        .iter()
                        .find_map(|prefix| line.strip_prefix(prefix))
                    })
                    .collect();
                if paths.is_empty() {
                    compact_tool_args(tool_args)
                } else {
                    format!("paths: {}", paths.join(", "))
                }
            } else {
                compact_tool_args(tool_args)
            }
        }
        _ => compact_tool_args(tool_args),
    }
}

fn string_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn compact_tool_args(args: &serde_json::Value) -> String {
    let serialized = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    octos_core::truncated_utf8(&serialized, 180, "...")
}

fn next_request_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("req_{}_{}", Utc::now().timestamp_millis(), seq)
}
