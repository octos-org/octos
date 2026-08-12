//! The always-on `escalate` safety valve (PR B).
//!
//! A closed fleet worker is provisioned with a least-privilege
//! [`octos_fleet::WorkerGrant`]. When a task genuinely needs a capability it was
//! NOT granted — a host, a tool, host filesystem — the worker cannot self-widen
//! (only the keeper's `goal_grant` mutates the grant). Instead it calls
//! `escalate`: a fire-and-**return** tool that RECORDS a
//! [`octos_fleet::EscalationRequest`] into a shared slot and returns
//! immediately. It does NOT block on input (the closed worker has no human to
//! ask and can never park) — the contrast with the FORBIDDEN
//! `ask_user_question`/`peer_handoff`. After the turn, [`crate::run_attempt`]
//! reads the slot and, if set, settles the attempt NON-terminally
//! (`record_escalation` → child `Blocked`), yielding it to the keeper.
//!
//! The tool is **always registered** (never grant-gated): the safety valve must
//! exist even for a minimal-grant worker, so a task that hits the edge of ANY
//! grant can ask. The requested grant is **advisory** — the tool records what
//! the model asked for; the keeper decides the actual grant.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::Result;
use octos_agent::tools::{Tool, ToolResult};
use octos_fleet::{EscalationRequest, FsGrant, NetworkGrant, WorkerGrant};
use serde_json::{Value, json};

/// The shared one-shot signal the [`EscalateTool`] writes into. `run_attempt`
/// creates one per attempt, threads a clone into the tool at build time, and
/// reads it AFTER the agent turn returns — so the recorded escalation wins
/// deterministically regardless of how the turn ended (EndTurn / deadline /
/// budget). `std::sync::Mutex` (not tokio) because the tool's `execute` writes
/// it synchronously and `run_attempt` reads it outside any await.
pub type EscalationSlot = Arc<Mutex<Option<EscalationRequest>>>;

/// The `escalate` tool: record a mid-task request for a wider grant + yield.
pub struct EscalateTool {
    slot: EscalationSlot,
}

impl EscalateTool {
    pub fn new(slot: EscalationSlot) -> Self {
        Self { slot }
    }
}

#[async_trait]
impl Tool for EscalateTool {
    fn name(&self) -> &str {
        "escalate"
    }

    fn description(&self) -> &str {
        "Request MORE capability from the master (operator) when THIS task cannot proceed with \
         the capabilities you were granted — a host you cannot reach, a tool you do not hold, or \
         filesystem access you lack. This RECORDS your request and YIELDS: your attempt stops, \
         the master reviews it, and if approved the task RE-RUNS from scratch with the widened \
         grant (your scratch dir persists). You canNOT widen your own grant — only the master \
         can. Give a specific `reason` (what you tried, exactly what capability you need and \
         why). Optionally describe the `requested_grant` you'd need (advisory — the master \
         decides the actual grant and may grant less). Use this ONLY when you are genuinely \
         blocked on a missing capability, not for ordinary difficulty."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Why you are blocked: what you attempted and exactly which capability you need."
                },
                "requested_grant": {
                    "type": "object",
                    "description": "Optional advisory grant you believe you need (same shape as goal_plan's task grant). The master decides the actual grant and may grant less.",
                    "properties": {
                        "network": {
                            "type": "object",
                            "properties": {
                                "mode": {
                                    "type": "string",
                                    "enum": ["none", "hosts", "full"]
                                },
                                "hosts": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            },
                            "additionalProperties": false
                        },
                        "tools": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "fs": {
                            "type": "string",
                            "enum": ["workspace", "host"]
                        }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["reason"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if reason.is_empty() {
            return Ok(ToolResult {
                output: "escalate: `reason` is required — describe what you tried and exactly \
                         which capability you need."
                    .to_string(),
                success: false,
                ..Default::default()
            });
        }
        let requested_grant = parse_requested_grant(args.get("requested_grant"));
        // Record into the shared slot; last write wins. `run_attempt` reads this
        // AFTER the turn and settles the attempt escalated (record_escalation),
        // so the escalation wins no matter how the turn ends (determinism).
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(EscalationRequest {
            requested_grant,
            reason,
        });
        Ok(ToolResult {
            output: "escalation recorded. Your attempt will yield to the master (operator) for a \
                     grant decision; if approved, this task re-runs with the widened capability. \
                     You do not need to do anything else."
                .to_string(),
            success: true,
            ..Default::default()
        })
    }
}

/// Parse the ADVISORY `requested_grant` wire object (same shape as goal_plan's
/// task grant) into a [`WorkerGrant`], leniently. Unlike the plan-time parser
/// this NEVER errors — the request only informs the keeper's decision, which
/// re-validates the grant IT chooses. Missing / malformed fields fall back to
/// least privilege so the recorded request is always coherent.
fn parse_requested_grant(value: Option<&Value>) -> WorkerGrant {
    let Some(obj) = value.and_then(Value::as_object) else {
        return WorkerGrant::minimal();
    };

    let network = obj
        .get("network")
        .and_then(Value::as_object)
        .map(|net| {
            let mode = net
                .get("mode")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("none");
            match mode {
                "full" => NetworkGrant::Full,
                "hosts" => {
                    let hosts: Vec<String> = net
                        .get("hosts")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    // An empty allowlist is meaningless as a REQUEST; record it
                    // as None (the keeper picks the real grant anyway).
                    if hosts.is_empty() {
                        NetworkGrant::None
                    } else {
                        NetworkGrant::Hosts(hosts)
                    }
                }
                _ => NetworkGrant::None,
            }
        })
        .unwrap_or_default();

    let tools: Vec<String> = obj
        .get("tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .filter(|t: &Vec<String>| !t.is_empty())
        .unwrap_or_else(|| WorkerGrant::minimal().tools);

    let fs = match obj.get("fs").and_then(Value::as_str).map(str::trim) {
        Some("host") => FsGrant::Host,
        _ => FsGrant::Workspace,
    };

    WorkerGrant {
        network,
        tools,
        fs,
        // #1976 — the escalate valve's REQUESTED grant stays binary in v1: a
        // fenced worker asking for more asks for the coarse scopes; only the
        // keeper's `goal_grant` (which parses the full object form) can set a
        // per-path fence. The request is advisory anyway — the keeper decides.
        write_paths: None,
        create_only: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> EscalationSlot {
        Arc::new(Mutex::new(None))
    }

    #[tokio::test]
    async fn escalate_records_request_into_the_slot_and_does_not_park() {
        let slot = slot();
        let tool = EscalateTool::new(slot.clone());
        // The tool must NOT block on human input — a closed worker can never park.
        assert!(!tool.blocks_on_human_input());

        let out = tool
            .execute(&json!({
                "reason": "cannot reach example.com to fetch the report",
                "requested_grant": {
                    "network": { "mode": "hosts", "hosts": ["example.com"] },
                    "tools": ["read_file", "web_fetch"]
                }
            }))
            .await
            .unwrap();
        assert!(out.success, "escalate returns success (fire-and-return)");

        let recorded = slot.lock().unwrap().clone().expect("slot set");
        assert_eq!(
            recorded.reason,
            "cannot reach example.com to fetch the report"
        );
        assert_eq!(
            recorded.requested_grant.network,
            NetworkGrant::Hosts(vec!["example.com".into()]),
        );
        assert!(recorded.requested_grant.tools.contains(&"web_fetch".into()));
    }

    #[tokio::test]
    async fn escalate_requires_a_reason() {
        let slot = slot();
        let tool = EscalateTool::new(slot.clone());
        let out = tool.execute(&json!({})).await.unwrap();
        assert!(!out.success, "a reason is required");
        assert!(slot.lock().unwrap().is_none(), "no request recorded");
    }

    #[test]
    fn parse_requested_grant_defaults_to_minimal_when_absent() {
        assert_eq!(parse_requested_grant(None), WorkerGrant::minimal());
        // A malformed grant never errors — it falls back to least privilege.
        assert_eq!(
            parse_requested_grant(Some(&json!("garbage"))),
            WorkerGrant::minimal(),
        );
    }

    #[test]
    fn parse_requested_grant_reads_full_and_host() {
        let g = parse_requested_grant(Some(&json!({
            "network": { "mode": "full" },
            "tools": ["shell", "web_search"],
            "fs": "host"
        })));
        assert_eq!(g.network, NetworkGrant::Full);
        assert_eq!(g.fs, FsGrant::Host);
        assert_eq!(g.tools, vec!["shell".to_string(), "web_search".to_string()]);
    }
}
