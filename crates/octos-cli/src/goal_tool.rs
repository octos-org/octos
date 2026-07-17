//! Structured goal tools for the model (#1696) — replaces the fragile
//! `<goal:complete>` text sentinel with first-class, executor-enforced tool
//! calls, mirroring codex's `get_goal`/`update_goal` ownership matrix.
//!
//! - `goal_get` — read the CURRENT session's goal: objective, status, token
//!   spend vs budget (so the model can see remaining budget), continuation
//!   count. Read-only.
//! - `goal_update` — transition the goal. The executor accepts ONLY
//!   `complete` (success criteria demonstrably met) and `blocked` (the same
//!   blocker persists and the model cannot advance). `pause`, `resume`,
//!   `active`, and budget changes are user/system-owned and are REJECTED here
//!   regardless of what the model asks for.
//!
//! Both tools resolve the session from [`ToolContext::parent_session_key`],
//! which every session-actor and AppUI turn threads into tool execution. The
//! goal record itself lives in the process-wide
//! [`default_agent_orchestrator`], so the tools carry no state of their own
//! beyond the owning profile id (enforced against `goal.profile_id` exactly
//! like the accountants).

use async_trait::async_trait;
use eyre::Result;
use octos_agent::tools::{Tool, ToolContext, ToolResult};
use octos_core::SessionKey;
use serde_json::{Value, json};

use crate::api::agent_orchestrator::default_agent_orchestrator;

/// Statuses the MODEL may set via `goal_update`. Everything else is
/// user/system-owned (see the module docs). Keep in sync with the
/// `input_schema` enum below.
const MODEL_ALLOWED_TRANSITIONS: [&str; 2] = ["complete", "blocked"];

fn session_from_ctx(ctx: &ToolContext) -> Option<SessionKey> {
    ctx.parent_session_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .map(|key| SessionKey(key.to_owned()))
}

/// `goal_get` — read-only goal snapshot for the current session.
pub struct GoalGetTool {
    profile_id: String,
}

impl GoalGetTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalGetTool {
    fn name(&self) -> &str {
        "goal_get"
    }

    fn description(&self) -> &str {
        "Read this session's persistent goal: objective, status, token spend vs budget \
         (remaining budget), and continuation count. Returns status=none when no goal is set."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, _args: &Value) -> Result<ToolResult> {
        let Some(session_id) = session_from_ctx(ctx) else {
            return Ok(ToolResult {
                output: "goal_get: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        let snapshot =
            default_agent_orchestrator().model_goal_snapshot(&session_id, &self.profile_id);
        Ok(ToolResult {
            output: serde_json::to_string_pretty(&snapshot)
                .unwrap_or_else(|_| snapshot.to_string()),
            success: true,
            ..Default::default()
        })
    }
}

/// `goal_update` — model-owned terminal transitions ONLY.
pub struct GoalUpdateTool {
    profile_id: String,
}

impl GoalUpdateTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalUpdateTool {
    fn name(&self) -> &str {
        "goal_update"
    }

    fn description(&self) -> &str {
        "Transition this session's goal. Allowed ONLY when justified: status=\"complete\" once \
         the goal's success criteria are demonstrably met (verify against evidence first), or \
         status=\"blocked\" when the same blocker has persisted across turns and you cannot \
         advance. Include a short evidence-based reason. Pause/resume/budget changes are \
         user-owned and will be rejected."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": MODEL_ALLOWED_TRANSITIONS,
                    "description": "complete = success criteria met; blocked = cannot advance"
                },
                "reason": {
                    "type": "string",
                    "description": "One-line evidence-based justification for the transition"
                }
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let Some(session_id) = session_from_ctx(ctx) else {
            return Ok(ToolResult {
                output: "goal_update: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        let status = args.get("status").and_then(Value::as_str).unwrap_or("");
        // Executor-enforced ownership: reject anything outside the model's
        // allowed matrix EVEN IF the schema was bypassed.
        if !MODEL_ALLOWED_TRANSITIONS.contains(&status) {
            return Ok(ToolResult {
                output: format!(
                    "goal_update: status `{status}` is not model-transitionable — only \
                     `complete` and `blocked` are. Pause/resume/budget changes belong to the user."
                ),
                success: false,
                ..Default::default()
            });
        }
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match default_agent_orchestrator().model_transition_goal(
            &session_id,
            &self.profile_id,
            status,
            reason,
        ) {
            Ok(goal) => Ok(ToolResult {
                output: format!(
                    "goal transitioned to `{status}`:\n{}",
                    serde_json::to_string_pretty(&goal).unwrap_or_else(|_| goal.to_string())
                ),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: format!("goal_update: {message}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}
