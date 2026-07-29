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
use octos_fleet::{AcceptanceCriterion, TaskSpec, Verifier};
use serde_json::{Value, json};

use crate::api::agent_orchestrator::default_agent_orchestrator;

/// PR 5a — wall-clock milliseconds for a fleet op (create / dispatch). Matches
/// the pool's own clock (`chrono::Utc::now().timestamp_millis()`), clamped
/// non-negative for the store's `u64` time fields.
fn fleet_now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// PR 5a — parse the `goal_plan` `tasks` array into fleet [`TaskSpec`]s. Each
/// task needs a `task_id` + `title`; `detail`/`deps` are optional. `acceptance`
/// is an optional array of shell command strings, each mapped to a
/// `CommandExit(0)` criterion (run as split argv, NO shell, by the worker's
/// acceptance gate) — a task with no acceptance is vacuously accepted once its
/// attempt ends.
fn parse_task_specs(args: &Value) -> Result<Vec<TaskSpec>, String> {
    let tasks = args
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| "`tasks` must be an array".to_owned())?;
    if tasks.is_empty() {
        return Err("`tasks` must contain at least one task".to_owned());
    }
    let mut out = Vec::with_capacity(tasks.len());
    for (i, task) in tasks.iter().enumerate() {
        let task_id = task
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("task[{i}]: `task_id` is required"))?
            .to_owned();
        let title = task
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if title.is_empty() {
            return Err(format!("task `{task_id}`: `title` is required"));
        }
        let detail = task
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let deps = task
            .get("deps")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let acceptance = task
            .get("acceptance")
            .and_then(Value::as_array)
            .map(|criteria| {
                criteria
                    .iter()
                    .enumerate()
                    .filter_map(|(j, criterion)| {
                        let cmd = criterion.as_str()?.trim();
                        (!cmd.is_empty()).then(|| AcceptanceCriterion {
                            id: format!("acc_{j}"),
                            description: format!("`{cmd}` exits 0"),
                            verifier: Verifier::CommandExit {
                                cmd: cmd.to_owned(),
                                code: 0,
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(TaskSpec {
            task_id,
            title,
            detail,
            deps,
            acceptance,
        });
    }
    Ok(out)
}

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
         (remaining budget), and continuation count. When the goal drives a fleet (see \
         goal_plan), also returns a `fleet` object with the objective, per-task \
         title/status/verdict, the ready set, and status counts — call this after a \
         fleet-completion wake to see progress and, when every task is accepted, the goal \
         auto-transitions to complete. Returns status=none when no goal is set."
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
        let orchestrator = default_agent_orchestrator();
        // PR 5a — resolve the fleet plan view FIRST: it self-detects completion
        // (`Fleet::is_complete` → mark the goal `complete`), which the budget
        // snapshot below then reflects. `Ok(None)` when this goal drives no
        // fleet, so a non-fleet goal_get is byte-for-byte the pre-5a shape.
        // `Err` (H3) when `goal.fleet_id` doesn't belong to this goal — surface
        // it instead of reading/completing a foreign fleet.
        let fleet = match orchestrator
            .model_fleet_snapshot(&session_id, &self.profile_id)
            .await
        {
            Ok(fleet) => fleet,
            Err(message) => {
                return Ok(ToolResult {
                    output: format!("goal_get: {message}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        let mut snapshot = orchestrator.model_goal_snapshot(&session_id, &self.profile_id);
        if let (Some(fleet), Value::Object(map)) = (fleet, &mut snapshot) {
            map.insert("fleet".to_owned(), fleet);
        }
        Ok(ToolResult {
            output: serde_json::to_string_pretty(&snapshot)
                .unwrap_or_else(|_| snapshot.to_string()),
            success: true,
            ..Default::default()
        })
    }
}

/// PR 5a — `goal_plan`: decompose the goal's objective onto a durable fleet.
pub struct GoalPlanTool {
    profile_id: String,
}

impl GoalPlanTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalPlanTool {
    fn name(&self) -> &str {
        "goal_plan"
    }

    fn description(&self) -> &str {
        "Decompose THIS session's goal into a durable fleet of tasks that background workers \
         execute. Call once, early in a goal, after you understand the objective: pass a list \
         of tasks, each with a stable `task_id`, a short `title`, a `detail` brief the worker \
         acts on, optional `deps` (task_ids that must finish first), and optional `acceptance` \
         (shell commands that must exit 0 for the task to count as done). Idempotent — if a \
         fleet already exists this returns its id unchanged. After planning, call goal_dispatch \
         to launch ready tasks. Requires a live session (the workspace root is captured then). \
         Each task runs in its OWN isolated scratch directory (replay-safe), NOT your repo \
         checkout: v1 fleet tasks do self-contained local work, not in-repo edits to the \
         controller's files — in-repo/remote-mutating goals are out of v1 scope."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The task graph to execute. Dependency-free tasks start ready.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task_id": {
                                "type": "string",
                                "description": "Stable unique id (referenced by other tasks' deps)."
                            },
                            "title": {
                                "type": "string",
                                "description": "One-line summary of the task."
                            },
                            "detail": {
                                "type": "string",
                                "description": "The brief the worker acts on."
                            },
                            "deps": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "task_ids that must succeed before this task is launchable."
                            },
                            "acceptance": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Shell commands (split argv, no shell) that must each exit 0 for the task to be accepted. Omit for a task with no mechanical check."
                            }
                        },
                        "required": ["task_id", "title"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["tasks"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let Some(session_id) = session_from_ctx(ctx) else {
            return Ok(ToolResult {
                output: "goal_plan: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        let tasks = match parse_task_specs(args) {
            Ok(tasks) => tasks,
            Err(message) => {
                return Ok(ToolResult {
                    output: format!("goal_plan: {message}"),
                    success: false,
                    ..Default::default()
                });
            }
        };
        match default_agent_orchestrator()
            .model_create_fleet_plan(&session_id, &self.profile_id, tasks, fleet_now_ms())
            .await
        {
            Ok(outcome) => Ok(ToolResult {
                output: format!(
                    "goal_plan:\n{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| outcome.to_string())
                ),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: format!("goal_plan: {message}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

/// PR 5a — `goal_dispatch`: launch every ready task of the goal's fleet.
pub struct GoalDispatchTool {
    profile_id: String,
}

impl GoalDispatchTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalDispatchTool {
    fn name(&self) -> &str {
        "goal_dispatch"
    }

    fn description(&self) -> &str {
        "Launch every currently-ready task of this goal's fleet onto background workers (call \
         goal_plan first to create the fleet). Ready = dependency-free or all deps succeeded. \
         Each launched task runs to completion in the background and wakes this goal when it \
         finishes, so the loop is: goal_dispatch → (workers run) → wake → goal_get to see \
         progress → goal_dispatch again for the newly-ready tasks, until goal_get reports all \
         tasks accepted. Safe to call repeatedly — already-running tasks are not relaunched."
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
                output: "goal_dispatch: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        match default_agent_orchestrator()
            .model_dispatch_fleet(&session_id, &self.profile_id, fleet_now_ms())
            .await
        {
            Ok(outcome) => Ok(ToolResult {
                output: format!(
                    "goal_dispatch:\n{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| outcome.to_string())
                ),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: format!("goal_dispatch: {message}"),
                success: false,
                ..Default::default()
            }),
        }
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
        "Transition this session's goal — use ONLY to mark it genuinely achieved or blocked. \
         Set status=\"complete\" ONLY when the objective has actually been achieved and no \
         required work remains (verify against evidence, not intent). Do NOT mark complete \
         merely because the token budget is nearly exhausted or because you are stopping work. \
         Set status=\"blocked\" ONLY when the same blocking condition has persisted across \
         multiple consecutive goal turns and you cannot make meaningful progress without user \
         input or an external state change — not because the work is merely hard, slow, \
         uncertain, or incomplete. Include a short evidence-based reason. Pause/resume/budget \
         changes are user-owned and will be rejected."
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
                    "goal_update: status `{status}` is not a model-allowed transition — only \
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

/// `goal_create` — model-owned goal creation (codex parity). Gated by its
/// description to "only when explicitly requested"; the orchestrator rejects the
/// call if this session already has an unfinished goal.
pub struct GoalCreateTool {
    profile_id: String,
}

impl GoalCreateTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalCreateTool {
    fn name(&self) -> &str {
        "goal_create"
    }

    fn description(&self) -> &str {
        "Create a persistent goal for this session — ONLY when the user or system/developer \
         instructions explicitly ask for a goal; do NOT infer one from an ordinary task. Starts \
         a new active goal when none exists, or replaces the current goal only when it is \
         already complete. Fails if an unfinished goal exists (complete or clear it first). Set \
         token_budget only when an explicit token budget is requested."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The concrete objective to start pursuing."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Optional positive token budget. Omit unless explicitly requested."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let Some(session_id) = session_from_ctx(ctx) else {
            return Ok(ToolResult {
                output: "goal_create: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        let objective = args
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if objective.is_empty() {
            return Ok(ToolResult {
                output: "goal_create: `objective` is required".into(),
                success: false,
                ..Default::default()
            });
        }
        let token_budget = match args.get("token_budget") {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_u64() {
                Some(budget) if budget > 0 => Some(budget),
                _ => {
                    return Ok(ToolResult {
                        output: "goal_create: token_budget must be a positive integer".into(),
                        success: false,
                        ..Default::default()
                    });
                }
            },
        };
        match default_agent_orchestrator().model_create_goal(
            &session_id,
            &self.profile_id,
            objective,
            token_budget,
        ) {
            Ok(goal) => Ok(ToolResult {
                output: format!(
                    "goal created:\n{}",
                    serde_json::to_string_pretty(&goal).unwrap_or_else(|_| goal.to_string())
                ),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: format!("goal_create: {message}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}
