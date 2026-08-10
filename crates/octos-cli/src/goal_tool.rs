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
use octos_fleet::{
    AcceptanceCriterion, BASE_TOOLS, FsGrant, NetworkGrant, TaskSpec, Verifier, WorkerGrant,
};
use serde_json::{Value, json};

use crate::api::agent_orchestrator::default_agent_orchestrator;

/// PR 5a — wall-clock milliseconds for a fleet op (create / dispatch). Matches
/// the pool's own clock (`chrono::Utc::now().timestamp_millis()`), clamped
/// non-negative for the store's `u64` time fields.
fn fleet_now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Returns `true` when this tool call is running inside a peer session
/// (topic starts with `peer-`). Used to fence keeper-only operations
/// (`goal_create` / `goal_plan` / `goal_dispatch` / `goal_grant` /
/// `goal_deny`) so NEITHER a goal-scoped peer NOR a goal-less peer can
/// invoke them — both are peers, both must defer to the master. This is
/// the broader check codex flagged: `ctx.goal_id.is_some()` only catches
/// peers the master staged with a goal; a goal-less peer (peer/prepare
/// path, or a missing/malformed `goal` file) would slip through.
fn is_peer_session(session_id: &SessionKey) -> bool {
    session_id
        .topic()
        .map(|t| t.starts_with("peer-"))
        .unwrap_or(false)
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
        let grant = parse_grant(task.get("grant"), &task_id)?;
        out.push(TaskSpec {
            task_id,
            title,
            detail,
            deps,
            acceptance,
            grant,
        });
    }
    Ok(out)
}

/// PR A — parse a task's optional operator `grant` object into a
/// [`WorkerGrant`]. The master PROVISIONS each worker like an operator: it
/// grants exactly the network hosts / tools / paths a task needs, default-deny.
/// An absent (or null) grant is [`WorkerGrant::minimal`] — today's closed
/// worker (least privilege). Validated against the grantable catalog before it
/// can reach the store, so an unknown tool (or a web tool with no network) is
/// rejected at plan time.
///
/// Wire shape:
/// `{ "network": { "mode": "none"|"hosts"|"full", "hosts": ["example.com"] },
///    "tools": ["read_file", ..., "web_fetch"],
///    "fs": "workspace"|"host" }`
///
/// `fs` is the coarse binary scope (v1 has no per-path allowlist — see
/// [`parse_fs`]), NOT a `{read, write}` path object.
fn parse_grant(value: Option<&Value>, task_id: &str) -> Result<WorkerGrant, String> {
    let value = match value {
        None | Some(Value::Null) => return Ok(WorkerGrant::minimal()),
        Some(value) => value,
    };
    let obj = value
        .as_object()
        .ok_or_else(|| format!("task `{task_id}`: `grant` must be an object"))?;

    let network = match obj.get("network") {
        None | Some(Value::Null) => NetworkGrant::None,
        Some(network) => parse_network(network, task_id)?,
    };

    let tools = match obj.get("tools") {
        None | Some(Value::Null) => BASE_TOOLS.iter().map(|s| (*s).to_string()).collect(),
        Some(Value::Array(items)) => {
            let mut tools = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        format!("task `{task_id}`: each `grant.tools` entry must be a tool name")
                    })?;
                tools.push(name.to_owned());
            }
            tools
        }
        Some(_) => return Err(format!("task `{task_id}`: `grant.tools` must be an array")),
    };

    let fs = match obj.get("fs") {
        None | Some(Value::Null) => FsGrant::Workspace,
        Some(fs) => parse_fs(fs, task_id)?,
    };

    let grant = WorkerGrant { network, tools, fs };
    grant
        .validate()
        .map_err(|e| format!("task `{task_id}`: {e}"))?;
    Ok(grant)
}

/// Parse `{ "mode": "none"|"hosts"|"full", "hosts": [...] }` into a
/// [`NetworkGrant`]. `hosts` mode requires a non-empty allowlist.
fn parse_network(value: &Value, task_id: &str) -> Result<NetworkGrant, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("task `{task_id}`: `grant.network` must be an object"))?;
    let mode = obj
        .get("mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("none");
    match mode {
        "none" => Ok(NetworkGrant::None),
        "full" => Ok(NetworkGrant::Full),
        "hosts" => {
            let hosts: Vec<String> = obj
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
            if hosts.is_empty() {
                return Err(format!(
                    "task `{task_id}`: network mode `hosts` requires a non-empty `hosts` allowlist"
                ));
            }
            Ok(NetworkGrant::Hosts(hosts))
        }
        other => Err(format!(
            "task `{task_id}`: unknown network mode `{other}` (use `none`, `hosts`, or `full`)"
        )),
    }
}

/// Parse the coarse `fs` scope — the string `"workspace"` (cwd-only, the
/// default) or `"host"` (full daemon-user read+write). v1 is deliberately
/// binary: the native tools have no per-path allowlist, so a narrow-paths grant
/// is not offered here (it would falsely promise narrow but deliver host-wide).
fn parse_fs(value: &Value, task_id: &str) -> Result<FsGrant, String> {
    let mode = value.as_str().map(str::trim).ok_or_else(|| {
        format!("task `{task_id}`: `grant.fs` must be the string \"workspace\" or \"host\"")
    })?;
    match mode.to_ascii_lowercase().as_str() {
        "workspace" => Ok(FsGrant::Workspace),
        "host" => Ok(FsGrant::Host),
        other => Err(format!(
            "task `{task_id}`: unknown fs scope `{other}` (use \"workspace\" or \"host\")"
        )),
    }
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
    /// Peer-agent-based goal: the profile's persistent `data_dir`. When set,
    /// `goal_get` aggregates the latest `result.md` from every staged peer
    /// whose `goal` file points at the queried goal (under
    /// `<data_dir>/peers/<slug>/goal`), AND the durable findings from the
    /// goal's sqlite ledger (under `<data_dir>/goal-ledgers/<goal_id>.db`),
    /// surfacing them as `peer_findings` and `ledger_findings` in the
    /// snapshot. `None` preserves pre-peer-goal behaviour (no aggregation).
    data_dir: Option<std::path::PathBuf>,
}

impl GoalGetTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            data_dir: None,
        }
    }

    /// Builder: set the profile's persistent data dir so the tool can
    /// aggregate peer findings (live + ledger) into the goal snapshot.
    pub fn with_data_dir(mut self, data_dir: std::path::PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    /// Back-compat alias for callers that already computed the peers root.
    /// Strips the trailing `peers` component ONLY when it is the final
    /// component; otherwise treats the input as the data dir directly. This
    /// avoids the bug where `with_peers_root("/x/data")` would silently
    /// re-root to `/x`.
    pub fn with_peers_root(mut self, peers_root: std::path::PathBuf) -> Self {
        let data_dir = if peers_root
            .file_name()
            .map(|n| n == "peers")
            .unwrap_or(false)
        {
            peers_root
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(peers_root.clone())
        } else {
            peers_root
        };
        self.data_dir = Some(data_dir);
        self
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
         auto-transitions to complete. A task with status `Blocked` and a `pending_escalation` \
         (a worker's `reason` + advisory `requested_grant`) is waiting on YOUR operator decision: \
         call goal_grant (widen its grant + resume it) or goal_deny (fail it). Returns status=none \
         when no goal is set."
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
        // Peer-agent-based goal: when the Agent was populated with a goal_id
        // (peer session whose staged dir carried a `goal` file), resolve by
        // goal_id DIRECTLY — the peer's session key does not own the goal.
        // The originator session is read from `ctx.originator_session`,
        // which was captured ONCE at peer boot from `peers/<slug>/originator`
        // (codex v9 High #2 fix: no per-call disk re-read, no symlink
        // vulnerability, no mid-turn rebinding).
        let mut snapshot = if let Some(goal_id) = ctx.goal_id.as_deref() {
            let Some(originator) = ctx.originator_session.as_deref() else {
                return Ok(ToolResult {
                    output: "goal_get: peer has goal context but no originator in tool \
                             context — the peer boot should have injected it; refusing \
                             to fall back to a per-call disk read (codex v9 High #2)"
                        .into(),
                    success: false,
                    ..Default::default()
                });
            };
            orchestrator.model_goal_snapshot_by_id(goal_id, &self.profile_id, originator)
        } else {
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
            snapshot
        };
        // Peer-agent-based goal (ledger association): when the snapshot
        // carries a goal_id AND this tool was constructed with a data_dir,
        // fold in BOTH the live `result.md` findings (under `<data_dir>/peers`)
        // AND the durable ledger findings (under `<data_dir>/goal-ledgers`).
        // This is what makes peer results visible to the keeper on `goal_get`.
        //
        // The `goal_id` is extracted BEFORE the mutable borrow so we never
        // hold `&snapshot` and `&mut snapshot` simultaneously.
        let goal_id_for_findings = snapshot
            .get("goal_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if let (Some(data_dir), Some(goal_id)) = (self.data_dir.as_ref(), goal_id_for_findings) {
            let peers_root = data_dir.join("peers");
            let live_findings =
                orchestrator.model_goal_peer_findings(&peers_root, &goal_id, &self.profile_id);
            if !live_findings.is_empty() {
                if let Value::Object(map) = &mut snapshot {
                    map.insert("peer_findings".to_owned(), Value::Array(live_findings));
                }
            }
            // Also fold in the DURABLE ledger findings (persisted when each
            // goal-scoped peer completed a turn). These survive restarts and
            // result.md overwrites — the authoritative history.
            let ledger_findings = orchestrator.model_goal_ledger_findings(data_dir, &goal_id);
            if !ledger_findings.is_empty() {
                if let Value::Object(map) = &mut snapshot {
                    map.insert("ledger_findings".to_owned(), Value::Array(ledger_findings));
                }
            }
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
         controller's files — in-repo/remote-mutating goals are out of v1 scope. \
         \
         YOU are the operator: provision each worker's capabilities with an optional per-task \
         `grant` — least privilege by default. Omit `grant` and the worker gets exactly today's \
         closed set (no network, the base file tools read/write/edit/glob/grep/list_dir/shell, \
         its own scratch dir). Grant MORE only where a task needs it: `network.mode`=`hosts` \
         with a `hosts` allowlist lets its web_fetch reach ONLY those hosts (the shell still has \
         no raw network); `network.mode`=`full` gives raw egress (git/npm); add `web_fetch`/\
         `web_search` to `tools` (they require a network grant); set `fs`=`host` ONLY when a task \
         needs the full host filesystem (it is broad — the default scratch-dir scope covers most \
         work). Grant each task the minimum it needs."
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
                            },
                            "grant": {
                                "type": "object",
                                "description": "Optional operator capability grant for THIS task's worker. Omit for least privilege (today's closed worker: no network, base file tools, own scratch dir). Grant more only where needed.",
                                "properties": {
                                    "network": {
                                        "type": "object",
                                        "description": "Network egress for the worker. Omit = none.",
                                        "properties": {
                                            "mode": {
                                                "type": "string",
                                                "enum": ["none", "hosts", "full"],
                                                "description": "none = no network; hosts = only the listed hosts, via web_fetch only (the shell still has no raw network); full = raw egress (git/npm)."
                                            },
                                            "hosts": {
                                                "type": "array",
                                                "items": { "type": "string" },
                                                "description": "Allowlisted hosts (required, non-empty, when mode=hosts). web_fetch may reach only these hosts and their subdomains."
                                            }
                                        },
                                        "additionalProperties": false
                                    },
                                    "tools": {
                                        "type": "array",
                                        "items": { "type": "string" },
                                        "description": "The tools the worker may hold. Omit = the base file tools (read_file/write_file/edit_file/glob/grep/list_dir/shell). Add web_fetch/web_search (each REQUIRES a network grant)."
                                    },
                                    "fs": {
                                        "type": "string",
                                        "enum": ["workspace", "host"],
                                        "description": "Filesystem reach. Omit = workspace (the worker's own scratch dir only, read+write). host = FULL daemon-user filesystem read+write (broad — grant only when a task genuinely needs host access). v1 is binary: narrow per-path grants are not yet supported."
                                    }
                                },
                                "additionalProperties": false
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
        // Peer-agent-based goal: a peer MUST NOT decompose its assigned goal
        // into a fleet — that is the keeper's job. A peer that wants to fan
        // out sub-work should either do it directly in its own session, or
        // escalate to the master asking for a sub-fleet. Refuse explicitly
        // so the model knows why.
        if is_peer_session(&session_id) {
            return Ok(ToolResult {
                output: "goal_plan: peers cannot create fleet plans — only the master \
                         (keeper) can decompose a goal onto a fleet. Do the work \
                         directly in this peer session, or escalate to the master \
                         if you need a sub-fleet."
                    .into(),
                success: false,
                ..Default::default()
            });
        }
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
         tasks accepted. Safe to call repeatedly — already-running tasks are not relaunched. If a \
         woken task is `Blocked` on a `pending_escalation`, resolve it with goal_grant or goal_deny \
         (not goal_dispatch) before it can run again."
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
        // Peer-agent-based goal: a peer MUST NOT dispatch its goal's fleet —
        // dispatching launches workers that drive the keeper's plan, and a
        // peer doing so would race with (or double-launch) the keeper's own
        // dispatch loop. Escalate to the master instead.
        if is_peer_session(&session_id) {
            return Ok(ToolResult {
                output: "goal_dispatch: peers cannot dispatch fleets — only the master \
                         (keeper) can launch a goal's tasks. Escalate to the master \
                         if you believe work is stalled."
                    .into(),
                success: false,
                ..Default::default()
            });
        }
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

/// PR B — `goal_grant`: APPROVE a worker's mid-task escalation, widen the task's
/// grant, and resume it.
pub struct GoalGrantTool {
    profile_id: String,
}

impl GoalGrantTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalGrantTool {
    fn name(&self) -> &str {
        "goal_grant"
    }

    fn description(&self) -> &str {
        "APPROVE a fleet worker's mid-task escalation: widen the blocked task's operator grant \
         and resume it (a fresh attempt re-runs with the new capability; its scratch dir \
         persists). Use when goal_get shows a task with status `Blocked` and a `pending_escalation` \
         — read the worker's `reason` and its advisory `requested_grant`, then decide. Pass the \
         `task_id`. Omit `grant` to approve the worker's requested grant as-is, OR pass a `grant` \
         (same shape as goal_plan's task grant) to grant LESS — you are the operator and may narrow \
         what it asked for (e.g. a tighter host allowlist). The grant is validated exactly as at \
         plan time. To refuse instead, use goal_deny."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The Blocked task whose escalation you are approving."
                },
                "grant": {
                    "type": "object",
                    "description": "Optional operator grant to apply (same shape as goal_plan's task grant: network/tools/fs). Omit to approve the worker's requested grant as-is; provide to grant LESS.",
                    "properties": {
                        "network": {
                            "type": "object",
                            "properties": {
                                "mode": { "type": "string", "enum": ["none", "hosts", "full"] },
                                "hosts": { "type": "array", "items": { "type": "string" } }
                            },
                            "additionalProperties": false
                        },
                        "tools": { "type": "array", "items": { "type": "string" } },
                        "fs": { "type": "string", "enum": ["workspace", "host"] }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let Some(session_id) = session_from_ctx(ctx) else {
            return Ok(ToolResult {
                output: "goal_grant: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        // Peer-agent-based goal: only the master (keeper) can approve a
        // worker's escalation — a peer approving its own escalation would
        // defeat the point of the gate.
        if is_peer_session(&session_id) {
            return Ok(ToolResult {
                output: "goal_grant: peers cannot approve escalations — only the master \
                         (keeper) can widen a task's grant. Your escalation is already \
                         visible to the master; wait for its decision."
                    .into(),
                success: false,
                ..Default::default()
            });
        }
        let Some(task_id) = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult {
                output: "goal_grant: `task_id` is required".into(),
                success: false,
                ..Default::default()
            });
        };
        // Absent/null grant → approve as-requested (None); a supplied grant is
        // parsed + validated exactly like a plan-time grant.
        let grant = match args.get("grant") {
            None | Some(Value::Null) => None,
            Some(value) => match parse_grant(Some(value), task_id) {
                Ok(grant) => Some(grant),
                Err(message) => {
                    return Ok(ToolResult {
                        output: format!("goal_grant: {message}"),
                        success: false,
                        ..Default::default()
                    });
                }
            },
        };
        match default_agent_orchestrator()
            .model_grant_escalation(
                &session_id,
                &self.profile_id,
                task_id,
                grant,
                fleet_now_ms(),
            )
            .await
        {
            Ok(outcome) => Ok(ToolResult {
                output: format!(
                    "goal_grant:\n{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| outcome.to_string())
                ),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: format!("goal_grant: {message}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

/// PR B — `goal_deny`: REFUSE a worker's mid-task escalation, failing the task.
pub struct GoalDenyTool {
    profile_id: String,
}

impl GoalDenyTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
        }
    }
}

#[async_trait]
impl Tool for GoalDenyTool {
    fn name(&self) -> &str {
        "goal_deny"
    }

    fn description(&self) -> &str {
        "REFUSE a fleet worker's mid-task escalation: the blocked task cannot proceed without a \
         capability you are not willing to grant, so FAIL it (terminal). Use when goal_get shows a \
         Blocked task with a `pending_escalation` you decide not to approve. Pass the `task_id` and \
         a short `reason`. This is terminal — the task will not re-run; the fleet then completes \
         around it (a Blocked task left undecided would wedge the goal forever, so decide every \
         escalation with either goal_grant or goal_deny). To approve instead, use goal_grant."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The Blocked task whose escalation you are refusing."
                },
                "reason": {
                    "type": "string",
                    "description": "One-line reason for refusing (recorded on the failed task)."
                }
            },
            "required": ["task_id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        let Some(session_id) = session_from_ctx(ctx) else {
            return Ok(ToolResult {
                output: "goal_deny: no session context for this turn".into(),
                success: false,
                ..Default::default()
            });
        };
        // Peer-agent-based goal: only the master (keeper) can refuse a
        // worker's escalation — a peer denying its own escalation would
        // silently kill its own task without the keeper's knowledge.
        if is_peer_session(&session_id) {
            return Ok(ToolResult {
                output: "goal_deny: peers cannot refuse escalations — only the master \
                         (keeper) can terminate a blocked task. Your escalation is \
                         already visible to the master; wait for its decision."
                    .into(),
                success: false,
                ..Default::default()
            });
        }
        let Some(task_id) = args
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult {
                output: "goal_deny: `task_id` is required".into(),
                success: false,
                ..Default::default()
            });
        };
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("escalation refused by the operator");
        match default_agent_orchestrator()
            .model_deny_escalation(
                &session_id,
                &self.profile_id,
                task_id,
                reason,
                fleet_now_ms(),
            )
            .await
        {
            Ok(outcome) => Ok(ToolResult {
                output: format!(
                    "goal_deny:\n{}",
                    serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| outcome.to_string())
                ),
                success: true,
                ..Default::default()
            }),
            Err(message) => Ok(ToolResult {
                output: format!("goal_deny: {message}"),
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

        // Peer-agent-based goal: when the Agent carries a `goal_id` (peer
        // session boot populated it from `peers/<slug>/goal`), the peer is
        // NOT allowed to transition the goal itself — a peer declaring its
        // sub-task done is not the same as the goal being done, and a peer
        // calling `goal_update(status="complete")` would prematurely close
        // the goal out from under the keeper. Refuse the by-id dispatch
        // outright and instruct the peer to record its contribution as a
        // finding instead (the completion half of `goal_get` aggregation).
        // NOTE: this checks ctx.goal_id (peer has goal context), NOT
        // is_peer_session — a goal-less peer calling goal_update falls
        // through to the session-key path and fails there with "no goal is
        // set", which is the correct error for that case.
        if ctx.goal_id.is_some() {
            return Ok(ToolResult {
                output: "goal_update: peers cannot transition the goal directly — \
                         a peer's job is to advance its assigned sub-task and let \
                         the master (keeper) judge overall completion. Record your \
                         contribution by writing your result (your `result.md` is \
                         automatically folded into the goal's ledger on turn \
                         completion), or escalate to the master if the goal itself \
                         needs a status change."
                    .into(),
                success: false,
                ..Default::default()
            });
        }

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

        // CRITICAL: Gate `complete` transitions on the INDEPENDENT verifier.
        // The goal_update tool is the FIRST-CLASS path (replacing the <goal:complete>
        // sentinel), so it must NOT bypass the verifier. When the model claims
        // completion, we call run_goal_completion_verifier to independently check
        // the objective against the model's evidence. Blocked transitions skip
        // verification (they're failure declarations, not success claims).
        if status == "complete" {
            let orchestrator = default_agent_orchestrator();
            let Some(objective) = orchestrator.goal_objective_for_test(&session_id) else {
                return Ok(ToolResult {
                    output: "goal_update: no goal objective found for verification".into(),
                    success: false,
                    ..Default::default()
                });
            };
            // Snapshot goal_id BEFORE the async verifier call to prevent
            // completing the wrong goal if it changes during the await.
            let expected_goal_id = orchestrator.goal_id_for_session(&session_id);
            // The evidence is the model's reason for claiming completion.
            let verdict = crate::api::agent_orchestrator::run_goal_completion_verifier(
                ctx.llm_provider.clone(),
                &objective,
                reason,
            )
            .await;
            if !verdict.is_done() {
                return Ok(ToolResult {
                    output: format!(
                        "goal_update: completion NOT verified — independent verifier returned: {}",
                        match verdict {
                            crate::api::goal_loop_runtime::GoalCompletionVerdict::NotDone {
                                reason,
                            } => reason,
                            _ => "unknown".to_string(),
                        }
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            // Verifier confirmed Done, but we must still revalidate goal identity
            // to prevent stale verdicts (goal changed during the await).
            let current_goal_id = orchestrator.goal_id_for_session(&session_id);
            if expected_goal_id != current_goal_id {
                return Ok(ToolResult {
                    output: format!(
                        "goal_update: goal changed during verification (was {:?}, now {:?}) — stale verdict rejected",
                        expected_goal_id, current_goal_id
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        }

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
        // Peer-agent-based goal: a peer (with `ctx.goal_id` set from its
        // staged `goal` file) MUST NOT create new goals. A peer's job is to
        // advance ITS assigned goal, not fork new ones — that would silently
        // orphan the master's fleet plan. Refuse explicitly with a clear
        // message so the model knows why (and what to do instead).
        if is_peer_session(&session_id) {
            return Ok(ToolResult {
                output: "goal_create: peers cannot create new goals — only the master \
                         session can. To contribute to your assigned goal, advance your \
                         sub-task and let the master judge overall completion; to fork \
                         work, escalate to the master instead."
                    .into(),
                success: false,
                ..Default::default()
            });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_plan_absent_grant_is_minimal() {
        // A task with no `grant` is today's closed worker (least privilege).
        let args = json!({
            "tasks": [ { "task_id": "t1", "title": "do it" } ]
        });
        let specs = parse_task_specs(&args).expect("parses");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].grant, WorkerGrant::minimal());
    }

    #[test]
    fn goal_plan_parses_per_task_grant() {
        // The master provisions a worker: hosts-restricted network + web_fetch +
        // host filesystem. The parsed grant matches exactly.
        let args = json!({
            "tasks": [ {
                "task_id": "fetch",
                "title": "grab the report",
                "grant": {
                    "network": { "mode": "hosts", "hosts": ["example.com", "docs.example.com"] },
                    "tools": ["read_file", "write_file", "web_fetch"],
                    "fs": "host"
                }
            } ]
        });
        let specs = parse_task_specs(&args).expect("parses");
        let grant = &specs[0].grant;
        assert_eq!(
            grant.network,
            NetworkGrant::Hosts(vec!["example.com".into(), "docs.example.com".into()]),
        );
        assert_eq!(grant.tools, vec!["read_file", "write_file", "web_fetch"]);
        assert_eq!(grant.fs, FsGrant::Host);
        // A `full` grant parses to raw egress; omitted fs = Workspace.
        let full = json!({
            "tasks": [ {
                "task_id": "build",
                "title": "npm install",
                "grant": { "network": { "mode": "full" }, "tools": ["shell", "web_fetch"] }
            } ]
        });
        let specs = parse_task_specs(&full).expect("parses");
        assert_eq!(specs[0].grant.network, NetworkGrant::Full);
        assert_eq!(specs[0].grant.fs, FsGrant::Workspace);
    }

    #[test]
    fn goal_plan_rejects_empty_hosts_allowlist() {
        // Fail-closed: `hosts` with no hosts is rejected at parse (the operator
        // meant `none`; an empty allowlist must never read as "unrestricted").
        let args = json!({
            "tasks": [ {
                "task_id": "t1",
                "title": "do it",
                "grant": { "network": { "mode": "hosts", "hosts": [] }, "tools": ["web_fetch"] }
            } ]
        });
        let err = parse_task_specs(&args).expect_err("empty hosts rejected");
        assert!(
            err.contains("hosts"),
            "error names the empty allowlist: {err}"
        );
    }

    #[test]
    fn goal_plan_rejects_unknown_granted_tool() {
        let args = json!({
            "tasks": [ {
                "task_id": "t1",
                "title": "do it",
                "grant": { "tools": ["read_file", "not_a_real_tool"] }
            } ]
        });
        let err = parse_task_specs(&args).expect_err("unknown tool rejected");
        assert!(
            err.contains("not_a_real_tool"),
            "error names the tool: {err}"
        );
    }

    #[test]
    fn goal_plan_rejects_web_tool_without_network() {
        // web_fetch under the default (none) network is incoherent.
        let args = json!({
            "tasks": [ {
                "task_id": "t1",
                "title": "do it",
                "grant": { "tools": ["read_file", "web_fetch"] }
            } ]
        });
        let err = parse_task_specs(&args).expect_err("web tool without network rejected");
        assert!(err.contains("web_fetch"), "error names the tool: {err}");
        assert!(err.contains("network"), "error explains the fix: {err}");
    }

    #[test]
    fn goal_plan_rejects_hosts_mode_without_hosts() {
        let args = json!({
            "tasks": [ {
                "task_id": "t1",
                "title": "do it",
                "grant": { "network": { "mode": "hosts" }, "tools": ["read_file", "web_fetch"] }
            } ]
        });
        let err = parse_task_specs(&args).expect_err("empty hosts rejected");
        assert!(
            err.contains("hosts"),
            "error names the missing allowlist: {err}"
        );
    }
}
