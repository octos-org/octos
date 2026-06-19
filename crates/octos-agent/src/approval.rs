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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRule {
    pub tools: Vec<String>,
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
        let tool_args_digest = digest_tool_args(&tool_args);
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

pub fn digest_tool_args(args: &serde_json::Value) -> String {
    let encoded = serde_json::to_vec(args).unwrap_or_default();
    let digest = Sha256::digest(encoded);
    format!("sha256:{:x}", digest)
}

fn approval_title_for_tool(tool_name: &str) -> String {
    match tool_name {
        "shell" => "Approve shell command".to_string(),
        "write_file" => "Approve file write".to_string(),
        "read_file" => "Approve file read".to_string(),
        _ => format!("Approve {tool_name}"),
    }
}

fn approval_summary_for_tool(tool_name: &str, tool_args: &serde_json::Value) -> String {
    match tool_name {
        "shell" => string_arg(tool_args, "command").unwrap_or_else(|| compact_tool_args(tool_args)),
        "write_file" | "read_file" => {
            string_arg(tool_args, "path").unwrap_or_else(|| compact_tool_args(tool_args))
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn valid_spec() -> ApprovalRequestSpec {
        ApprovalRequestSpec {
            title: "Execute shell".to_string(),
            summary: "rm -rf tmp".to_string(),
            risk_level: ApprovalRiskLevel::Critical,
            authorized_approvers: vec!["@alice:example.org".to_string()],
            expires_at: Utc::now() + Duration::minutes(10),
            on_timeout: ApprovalTimeoutBehavior::Notify,
        }
    }

    #[test]
    fn test_create_pending_approval_with_digest_and_expiry() {
        let pending = PendingApprovalDraft::from_spec(
            "shell",
            "tc1",
            serde_json::json!({"command": "rm -rf tmp"}),
            valid_spec(),
        )
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");

        assert_eq!(pending.request.tool_name, "shell");
        assert!(pending.request.tool_args_digest.starts_with("sha256:"));
        assert_eq!(pending.room_id, "!room:example.org");
        assert!(!pending.is_expired(Utc::now()));
    }

    #[test]
    fn test_pending_approval_rejects_empty_authorized_approvers() {
        let mut spec = valid_spec();
        spec.authorized_approvers.clear();
        let err = PendingApprovalDraft::from_spec("shell", "tc1", serde_json::json!({}), spec)
            .unwrap_err();

        assert!(err.contains("authorized_approvers"));
    }

    #[test]
    fn test_pending_approval_rejects_expired_request() {
        let mut store = PendingApprovalStore::default();
        let mut spec = valid_spec();
        spec.expires_at = Utc::now() - Duration::minutes(1);
        let pending = PendingApprovalDraft::from_spec(
            "shell",
            "tc1",
            serde_json::json!({"command": "echo hi"}),
            spec,
        )
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");
        let response = ApprovalResponsePayload {
            request_id: pending.request.request_id.clone(),
            decision: ApprovalDecision::Approve,
            source_event_id: "$source".to_string(),
            tool_args_digest: pending.request.tool_args_digest.clone(),
        };
        store.insert(pending);

        assert_eq!(
            store
                .validate_response(
                    "!room:example.org",
                    "@alice:example.org",
                    &response,
                    Utc::now(),
                )
                .unwrap_err(),
            ApprovalValidationError::Expired
        );
    }

    #[test]
    fn test_pending_approval_rejects_wrong_room() {
        let mut store = PendingApprovalStore::default();
        let pending = PendingApprovalDraft::from_spec(
            "shell",
            "tc1",
            serde_json::json!({"command": "echo hi"}),
            valid_spec(),
        )
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");
        let response = ApprovalResponsePayload {
            request_id: pending.request.request_id.clone(),
            decision: ApprovalDecision::Approve,
            source_event_id: "$source".to_string(),
            tool_args_digest: pending.request.tool_args_digest.clone(),
        };
        store.insert(pending);

        assert_eq!(
            store
                .validate_response(
                    "!other:example.org",
                    "@alice:example.org",
                    &response,
                    Utc::now(),
                )
                .unwrap_err(),
            ApprovalValidationError::WrongRoom
        );
    }

    #[test]
    fn test_pending_approval_rejects_duplicate_consume() {
        let mut store = PendingApprovalStore::default();
        let pending = PendingApprovalDraft::from_spec(
            "shell",
            "tc1",
            serde_json::json!({"command": "echo hi"}),
            valid_spec(),
        )
        .unwrap()
        .into_pending("!room:example.org", "@requester:example.org");
        let response = ApprovalResponsePayload {
            request_id: pending.request.request_id.clone(),
            decision: ApprovalDecision::Approve,
            source_event_id: "$source".to_string(),
            tool_args_digest: pending.request.tool_args_digest.clone(),
        };
        store.insert(pending);

        let _ = store
            .consume(
                "!room:example.org",
                "@alice:example.org",
                &response,
                Utc::now(),
            )
            .unwrap();

        assert_eq!(
            store
                .validate_response(
                    "!room:example.org",
                    "@alice:example.org",
                    &response,
                    Utc::now(),
                )
                .unwrap_err(),
            ApprovalValidationError::AlreadyConsumed
        );
    }

    #[test]
    fn test_approval_policy_first_match_wins() {
        let policy = HumanApprovalRules::new(vec![
            ApprovalRule {
                tools: vec!["shell".to_string()],
                risk_level: ApprovalRiskLevel::Critical,
                authorized_approvers: vec!["@alice:example.org".to_string()],
                expires_in_secs: 300,
                on_timeout: ApprovalTimeoutBehavior::Notify,
            },
            ApprovalRule {
                tools: vec!["shell".to_string()],
                risk_level: ApprovalRiskLevel::Normal,
                authorized_approvers: vec!["@bob:example.org".to_string()],
                expires_in_secs: 60,
                on_timeout: ApprovalTimeoutBehavior::Notify,
            },
        ]);

        let draft = policy
            .draft_for_tool_call(
                "shell",
                "tc-shell",
                serde_json::json!({"command": "echo hi"}),
                Utc::now(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(
            draft.request.authorized_approvers,
            vec!["@alice:example.org".to_string()]
        );
        assert_eq!(draft.request.risk_level, ApprovalRiskLevel::Critical);
    }

    #[test]
    fn test_approval_policy_non_matching_tool_returns_none() {
        let policy = HumanApprovalRules::new(vec![ApprovalRule {
            tools: vec!["shell".to_string()],
            risk_level: ApprovalRiskLevel::Critical,
            authorized_approvers: vec!["@alice:example.org".to_string()],
            expires_in_secs: 300,
            on_timeout: ApprovalTimeoutBehavior::Notify,
        }]);

        assert!(
            policy
                .draft_for_tool_call(
                    "read_file",
                    "tc-read",
                    serde_json::json!({"path": "README.md"}),
                    Utc::now(),
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_approval_policy_generates_relative_expiry() {
        let policy = HumanApprovalRules::new(vec![ApprovalRule {
            tools: vec!["shell".to_string()],
            risk_level: ApprovalRiskLevel::Critical,
            authorized_approvers: vec!["@alice:example.org".to_string()],
            expires_in_secs: 300,
            on_timeout: ApprovalTimeoutBehavior::Notify,
        }]);
        let created_at = chrono::DateTime::parse_from_rfc3339("2026-04-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let draft = policy
            .draft_for_tool_call(
                "shell",
                "tc-shell",
                serde_json::json!({"command": "echo hi"}),
                created_at,
            )
            .unwrap()
            .unwrap();

        assert_eq!(
            draft.request.expires_at,
            chrono::DateTime::parse_from_rfc3339("2026-04-14T12:05:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn test_approval_policy_shell_call_emits_pending_approval() {
        let policy = HumanApprovalRules::new(vec![ApprovalRule {
            tools: vec!["shell".to_string()],
            risk_level: ApprovalRiskLevel::Critical,
            authorized_approvers: vec!["@alice:example.org".to_string()],
            expires_in_secs: 300,
            on_timeout: ApprovalTimeoutBehavior::Notify,
        }]);

        let draft = policy
            .draft_for_tool_call(
                "shell",
                "tc-shell",
                serde_json::json!({"command": "ls"}),
                Utc::now(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(draft.request.tool_name, "shell");
        assert_eq!(
            draft.request.authorized_approvers,
            vec!["@alice:example.org".to_string()]
        );
        assert_eq!(draft.request.on_timeout, ApprovalTimeoutBehavior::Notify);
        assert_eq!(draft.request.risk_level, ApprovalRiskLevel::Critical);
    }
}
