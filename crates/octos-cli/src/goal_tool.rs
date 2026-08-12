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
use octos_agent::tools::{ConcurrencyClass, Tool, ToolContext, ToolResult};
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
///    "fs": "workspace"|"host"
///        | { "write": ["exemplar.card", "cards/*.card"], "create_only": true } }`
///
/// `fs` is the coarse binary scope as a STRING, or (#1976) the per-path WRITE
/// fence as an OBJECT: a workspace-relative allowlist of writable paths
/// (`*`/`?` globs) with optional `create_only` (allowlisted paths may be
/// created but never overwritten/edited). The object form always implies the
/// workspace scope — reads stay workspace-wide, writes narrow to the list.
/// See [`parse_fs`].
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
        None | Some(Value::Null) => ParsedFs::default(),
        Some(fs) => parse_fs(fs, task_id)?,
    };

    let grant = WorkerGrant {
        network,
        tools,
        fs: fs.scope,
        write_paths: fs.write_paths,
        create_only: fs.create_only,
    };
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

/// The parsed `fs` lane of a grant: the coarse scope plus (#1976) the
/// optional per-path write fence. Default = workspace scope, no fence —
/// exactly the pre-#1976 behaviour for an absent `fs`.
#[derive(Default)]
struct ParsedFs {
    scope: FsGrant,
    write_paths: Option<Vec<String>>,
    create_only: bool,
}

/// Parse the `fs` lane — either the coarse binary scope as a STRING
/// (`"workspace"` cwd-only, the default; `"host"` full daemon-user
/// read+write) or (#1976) the per-path WRITE fence as an OBJECT:
/// `{ "write": ["exemplar.card", "cards/*.card"], "create_only": true }`.
/// The object form always implies the workspace scope (a fence under `host`
/// is incoherent — `WorkerGrant::validate` also rejects the programmatic
/// combination); pattern syntax is validated there too (`*`/`?` globs,
/// relative, no `..`). `write` is REQUIRED in the object form so `fs: {}`
/// cannot silently mean "no fence".
fn parse_fs(value: &Value, task_id: &str) -> Result<ParsedFs, String> {
    if let Some(obj) = value.as_object() {
        for key in obj.keys() {
            if key != "write" && key != "create_only" {
                return Err(format!(
                    "task `{task_id}`: unknown `grant.fs` key `{key}` (use `write` and \
                     `create_only`)"
                ));
            }
        }
        let write_paths = match obj.get("write") {
            Some(Value::Array(items)) => {
                let mut paths = Vec::with_capacity(items.len());
                for item in items {
                    let pattern = item.as_str().map(str::trim).ok_or_else(|| {
                        format!(
                            "task `{task_id}`: each `grant.fs.write` entry must be a \
                             workspace-relative path pattern"
                        )
                    })?;
                    paths.push(pattern.to_owned());
                }
                paths
            }
            _ => {
                return Err(format!(
                    "task `{task_id}`: `grant.fs.write` (array of workspace-relative path \
                     patterns) is required in the object form of `grant.fs`"
                ));
            }
        };
        let create_only = match obj.get("create_only") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                return Err(format!(
                    "task `{task_id}`: `grant.fs.create_only` must be a boolean"
                ));
            }
        };
        return Ok(ParsedFs {
            scope: FsGrant::Workspace,
            write_paths: Some(write_paths),
            create_only,
        });
    }
    let mode = value.as_str().map(str::trim).ok_or_else(|| {
        format!(
            "task `{task_id}`: `grant.fs` must be the string \"workspace\" or \"host\", or \
             the per-path object {{\"write\": [...], \"create_only\": bool}}"
        )
    })?;
    match mode.to_ascii_lowercase().as_str() {
        "workspace" => Ok(ParsedFs::default()),
        "host" => Ok(ParsedFs {
            scope: FsGrant::Host,
            ..ParsedFs::default()
        }),
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
    /// `<data_dir>/peers/<slug>/goal`), AND the durable findings + open
    /// escalations from the goal's sqlite ledger (under
    /// `<data_dir>/goal-ledgers/<goal_id>.db`), surfacing them as
    /// `peer_findings`, `ledger_findings` and `open_escalations` (#1967) in
    /// the snapshot. `None` preserves pre-peer-goal behaviour (no
    /// aggregation).
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
                // #1964 — pass the profile data dir so a fleet terminal the
                // snapshot backstop detects syncs the per-goal ledger (#1957).
                .model_fleet_snapshot(&session_id, &self.profile_id, self.data_dir.as_deref())
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
            // #1967 — fold in the goal's OPEN escalations (rows written when a
            // goal-scoped peer parks on an approval/question). Until this read
            // the escalations table was write-only: a master that missed the
            // park-time wake could never rediscover a blocked peer from
            // goal_get. Open rows only, compact shape (question ≤300 chars),
            // omitted entirely when none are open.
            let open_escalations =
                orchestrator.model_goal_ledger_open_escalations(data_dir, &goal_id);
            if !open_escalations.is_empty() {
                if let Value::Object(map) = &mut snapshot {
                    map.insert(
                        "open_escalations".to_owned(),
                        Value::Array(open_escalations),
                    );
                }
            }
            // #1945 — and the bounded ledger DIGEST, what a master
            // re-orienting after a restart reads: fixed-size however large
            // the goal grew, absent when the goal has no ledger file. Keys:
            // `tasks` = counts by status over the ledger's FK stub rows —
            // production only ever writes `running` stubs today and nothing
            // updates them, so real counts read {"running": N} until a task-
            // status writer lands; `findings.total`/`by_lifecycle`/`by_kind`
            // = SQL aggregates over ALL findings, task-less rows included;
            // `cost_tokens` sums the real per-finding charges (#1965) —
            // Completed peer turns carry real usage; errored/interrupted
            // turns under-charge 0 until #1969 lands.
            if let Some(ledger_digest) = orchestrator.model_goal_ledger_digest(data_dir, &goal_id) {
                if let Value::Object(map) = &mut snapshot {
                    map.insert("ledger_digest".to_owned(), ledger_digest);
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

    /// #1935 codex round 6 — goal_plan mutates the goal's fleet/binding state
    /// through snapshot→await→commit sequences; two calls in one parallel
    /// batch would interleave those windows (the goal_plan double-bind was
    /// the observed instance). Serialize the whole family; each call is a
    /// short control-plane operation, so exclusivity costs nothing.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
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
                                        "description": "Filesystem reach. Omit = workspace (the worker's own scratch dir only, read+write). String \"host\" = FULL daemon-user filesystem read+write (broad — grant only when a task genuinely needs host access). OBJECT = per-path WRITE fence (#1976): {\"write\": [\"exemplar.card\", \"cards/*.card\"], \"create_only\": true} — the worker may WRITE only the listed workspace-relative paths (globs: * and ? within one path segment; no **), everything else is read-only, kernel-enforced (file tools + shell sandbox). create_only additionally means listed paths may be CREATED but never overwritten/edited.",
                                        "oneOf": [
                                            { "type": "string", "enum": ["workspace", "host"] },
                                            {
                                                "type": "object",
                                                "properties": {
                                                    "write": {
                                                        "type": "array",
                                                        "items": { "type": "string" },
                                                        "description": "Workspace-relative writable path patterns (* and ? globs). Everything else is read-only."
                                                    },
                                                    "create_only": {
                                                        "type": "boolean",
                                                        "description": "Listed paths may be created but never overwritten, edited, or deleted."
                                                    }
                                                },
                                                "required": ["write"],
                                                "additionalProperties": false
                                            }
                                        ]
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

    /// #1935 codex round 6 — goal_dispatch mutates the goal's fleet/binding state
    /// through snapshot→await→commit sequences; two calls in one parallel
    /// batch would interleave those windows (the goal_plan double-bind was
    /// the observed instance). Serialize the whole family; each call is a
    /// short control-plane operation, so exclusivity costs nothing.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
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

    /// #1935 codex round 6 — goal_grant mutates the goal's fleet/binding state
    /// through snapshot→await→commit sequences; two calls in one parallel
    /// batch would interleave those windows (the goal_plan double-bind was
    /// the observed instance). Serialize the whole family; each call is a
    /// short control-plane operation, so exclusivity costs nothing.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
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
                        "fs": {
                            "description": "\"workspace\" | \"host\", or the #1976 per-path write fence object {\"write\": [globs], \"create_only\": bool} (same shape as goal_plan's task grant).",
                            "oneOf": [
                                { "type": "string", "enum": ["workspace", "host"] },
                                {
                                    "type": "object",
                                    "properties": {
                                        "write": { "type": "array", "items": { "type": "string" } },
                                        "create_only": { "type": "boolean" }
                                    },
                                    "required": ["write"],
                                    "additionalProperties": false
                                }
                            ]
                        }
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
    /// #1964 — profile's persistent data dir, so a deny that renders the fleet
    /// un-completable syncs the goal-row status + a `decisions` row into
    /// `<data_dir>/goal-ledgers/` (#1957). `None` on paths that never wired it
    /// (the sync is then skipped).
    data_dir: Option<std::path::PathBuf>,
}

impl GoalDenyTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            data_dir: None,
        }
    }

    /// #1964 — mirrors `GoalGetTool::with_data_dir` / `GoalUpdateTool::with_data_dir`.
    pub fn with_data_dir(mut self, data_dir: std::path::PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }
}

#[async_trait]
impl Tool for GoalDenyTool {
    fn name(&self) -> &str {
        "goal_deny"
    }

    /// #1935 codex round 6 — goal_deny mutates the goal's fleet/binding state
    /// through snapshot→await→commit sequences; two calls in one parallel
    /// batch would interleave those windows (the goal_plan double-bind was
    /// the observed instance). Serialize the whole family; each call is a
    /// short control-plane operation, so exclusivity costs nothing.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
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
                // #1964 — deny-driven goal terminals sync the per-goal ledger.
                self.data_dir.as_deref(),
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
    /// #1957 — profile's persistent data dir, so a transition can sync the
    /// goal-row status + a `decisions` row into `<data_dir>/goal-ledgers/`.
    /// `None` on paths that never wired it (the sync is then skipped).
    data_dir: Option<std::path::PathBuf>,
    /// #1935 — the INDEPENDENT verifier lane (profile `sub_providers` key
    /// `goal_verifier`, resolved at profile build). When set, completion
    /// claims are graded on THIS provider instead of the grading turn's own
    /// `ctx.llm_provider`; `None` falls back to the turn provider — the
    /// pre-#1935 behavior, kept as the back-compat default.
    verifier_llm: Option<std::sync::Arc<dyn octos_llm::LlmProvider>>,
}

impl GoalUpdateTool {
    pub fn new(profile_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            data_dir: None,
            verifier_llm: None,
        }
    }

    /// #1957 — set the profile data dir used to sync a transition to the ledger.
    pub fn with_data_dir(mut self, data_dir: std::path::PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    /// #1935 — route completion verification through the profile's dedicated
    /// `goal_verifier` sub-provider lane instead of the turn's own provider.
    pub fn with_verifier_provider(
        mut self,
        provider: std::sync::Arc<dyn octos_llm::LlmProvider>,
    ) -> Self {
        self.verifier_llm = Some(provider);
        self
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
        //
        // #1935 codex round 4 (A1) — this used to be the FOURTH verifier path
        // still reading the objective and the goal id in SEPARATE lock
        // acquisitions, re-reading the id after the await, and then
        // transitioning unconditionally. It now takes the same one-lock
        // `goal_verification_snapshot` as the three sentinel paths, grades
        // `snapshot.objective`, and threads the snapshot INTO the guarded
        // transition, which re-checks (id, objective, revision) under the
        // mutation lock itself.
        //
        // #1935 codex round 5 — the verifier's spend is charged DIRECTLY to
        // the goal here (the `allow_budget_limited` verifier-charge path),
        // immediately after the verifier returns and BEFORE the transition,
        // exactly once for BOTH verdicts. The former #1958 route — stamping
        // `ToolResult.tokens_used` so the agent loop folds it into turn
        // totals and the post-turn accountant charges the goal — was lost
        // precisely when it succeeded: the tool completes the goal, and the
        // post-turn charge is active-only, so it refused the now-complete
        // goal and the stamped usage never landed. The stamp is REMOVED so
        // no path double-charges (consumer trace: ToolResult.tokens_used →
        // execute_tools aggregation → loop_runner `turn.record_usage` → turn
        // totals → the post-turn goal charge; the sentinel paths' verifier
        // calls never rode turn totals either, so goal accounting is now
        // uniform across all four verifier sites — the only trade-off is the
        // TURN cost display no longer counts this one out-of-band call).
        let mut verified_snapshot: Option<
            crate::api::agent_orchestrator::GoalVerificationSnapshot,
        > = None;
        if status == "complete" {
            let orchestrator = default_agent_orchestrator();
            let Some(snapshot) =
                orchestrator.goal_verification_snapshot(&session_id, &self.profile_id)
            else {
                return Ok(ToolResult {
                    output: "goal_update: no goal objective found for verification".into(),
                    success: false,
                    ..Default::default()
                });
            };
            // #1935 — grade on the INDEPENDENT verifier lane when the profile
            // configures one; otherwise the turn's own provider (unchanged
            // pre-#1935 behavior). The evidence is the model's reason for
            // claiming completion.
            let verifier_provider = self
                .verifier_llm
                .clone()
                .unwrap_or_else(|| ctx.llm_provider.clone());
            let (verdict, usage) =
                crate::api::agent_orchestrator::run_goal_completion_verifier_with_usage(
                    verifier_provider,
                    &snapshot.objective,
                    reason,
                )
                .await;
            // #1935 round 5 — exactly-once direct charge, while the goal is
            // still active/budget_limited (a `complete` goal cannot be
            // charged). Covers Done AND NotDone outcomes.
            let _ = orchestrator.charge_goal_verifier_usage(
                &session_id,
                &self.profile_id,
                Some(&snapshot.goal_id),
                &usage,
            );
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
            // The stale-verdict recheck happens INSIDE the guarded transition
            // below, under the same lock that mutates — no racy re-read here.
            verified_snapshot = Some(snapshot);
        }

        // #1957 — pass the profile data dir so the transition syncs into the
        // ledger (goals-row status + a decision) using the snapshot it just
        // transitioned. Done INSIDE the transition (not here) so it uses the
        // correct goal without a racy re-fetch (codex #3).
        match default_agent_orchestrator()
            .model_transition_goal_guarded(
                &session_id,
                &self.profile_id,
                status,
                reason,
                verified_snapshot.as_ref(),
                self.data_dir.as_deref(),
            )
            .await
        {
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

    /// #1935 codex round 7 — goal_create's admission ("no unfinished goal
    /// exists") spans a check in `model_create_goal` and the create inside
    /// `set_goal`, re-locking between them; two creates in one parallel batch
    /// both passed the check and the loser overwrote the winner's objective.
    /// The admission is now ALSO enforced atomically inside `set_goal`
    /// (actor == "model" refuses the update branch for an unfinished goal),
    /// and this override is the batch-level defense-in-depth, matching the
    /// fleet-mutating family below.
    fn concurrency_class(&self) -> ConcurrencyClass {
        ConcurrencyClass::Exclusive
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

    /// #1935 — a call-counting scripted provider for verifier-lane routing
    /// assertions: replies with a fixed verdict line and fixed token usage.
    struct CountingVerifierProvider {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        reply: &'static str,
        usage: octos_llm::TokenUsage,
    }

    #[async_trait]
    impl octos_llm::LlmProvider for CountingVerifierProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(octos_llm::ChatResponse {
                content: Some(self.reply.to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: self.usage.clone(),
                provider_index: Some(0),
            })
        }
        fn model_id(&self) -> &str {
            "counting-verifier"
        }
        fn provider_name(&self) -> &str {
            "counting-verifier"
        }
    }

    /// #1935 — when the profile configures a `goal_verifier` sub-provider
    /// lane, `goal_update(status="complete")` must run the INDEPENDENT
    /// completion verifier on THAT lane, never on the grading turn's own
    /// provider (`ctx.llm_provider`), and the verifier's token usage must
    /// still be stamped on the ToolResult (#1958).
    #[tokio::test]
    async fn goal_update_routes_verifier_through_configured_lane() {
        use crate::api::agent_orchestrator::{AgentOrchestrator as _, GoalSetRequest};
        let orchestrator = default_agent_orchestrator();
        // Process-global orchestrator: unique key, never cleared (same idiom
        // as the sibling goal_get tests).
        let session = SessionKey("verifier-lane-prof:api:goal-update-lane".to_owned());
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session.clone(),
                profile_id: "verifier-lane-prof".to_owned(),
                objective: "route the verifier through the lane".to_owned(),
                status: Some("active".to_owned()),
                token_budget: Some(10_000),
                transition_actor: None,
            })
            .expect("set goal");

        let lane_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let turn_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let lane_provider = std::sync::Arc::new(CountingVerifierProvider {
            calls: lane_calls.clone(),
            reply: "DONE",
            usage: octos_llm::TokenUsage {
                input_tokens: 40,
                output_tokens: 2,
                ..Default::default()
            },
        });
        let turn_provider = std::sync::Arc::new(CountingVerifierProvider {
            calls: turn_calls.clone(),
            // If the tool wrongly grades on the turn provider, the verdict
            // flips NotDone and the assertions below fail loudly.
            reply: "NOT_DONE: wrong lane",
            usage: octos_llm::TokenUsage::default(),
        });

        let tool = GoalUpdateTool::new("verifier-lane-prof").with_verifier_provider(lane_provider);
        let mut ctx = ToolContext::zero();
        ctx.parent_session_key = Some(session.0.clone());
        ctx.llm_provider = turn_provider;

        let result = tool
            .execute_with_context(&ctx, &json!({"status": "complete", "reason": "did it"}))
            .await
            .expect("goal_update runs");
        assert!(result.success, "verified completion: {}", result.output);
        assert_eq!(
            lane_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the configured verifier lane grades the completion",
        );
        assert_eq!(
            turn_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the grading turn's own provider must NOT be consulted",
        );
        // #1935 round 5 — the verifier spend lands DIRECTLY on the GOAL
        // (charged before the completion flip), not on the ToolResult stamp:
        // the stamped route was rejected by the active-only post-turn charge
        // exactly when the tool succeeded (the goal is complete by then).
        let (tokens_used, _, _) = orchestrator
            .goal_counters_for_test(&session)
            .expect("goal exists");
        assert_eq!(
            tokens_used, 42,
            "verifier input+output charged to the goal before the flip",
        );
        assert!(
            result.tokens_used.is_none(),
            "the #1958 stamp is removed — the direct charge is the only route",
        );
    }

    /// #1935 back-compat — without a configured lane the tool falls back to
    /// the turn's own provider (`ctx.llm_provider`), the pre-#1935 behavior.
    #[tokio::test]
    async fn goal_update_verifier_falls_back_to_turn_provider_when_lane_unconfigured() {
        use crate::api::agent_orchestrator::{AgentOrchestrator as _, GoalSetRequest};
        let orchestrator = default_agent_orchestrator();
        let session = SessionKey("verifier-fallback-prof:api:goal-update-fallback".to_owned());
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session.clone(),
                profile_id: "verifier-fallback-prof".to_owned(),
                objective: "fall back to the turn provider".to_owned(),
                status: Some("active".to_owned()),
                token_budget: Some(10_000),
                transition_actor: None,
            })
            .expect("set goal");

        let turn_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let turn_provider = std::sync::Arc::new(CountingVerifierProvider {
            calls: turn_calls.clone(),
            reply: "DONE",
            usage: octos_llm::TokenUsage {
                input_tokens: 7,
                output_tokens: 1,
                ..Default::default()
            },
        });

        let tool = GoalUpdateTool::new("verifier-fallback-prof");
        let mut ctx = ToolContext::zero();
        ctx.parent_session_key = Some(session.0.clone());
        ctx.llm_provider = turn_provider;

        let result = tool
            .execute_with_context(&ctx, &json!({"status": "complete", "reason": "done"}))
            .await
            .expect("goal_update runs");
        assert!(result.success, "verified completion: {}", result.output);
        assert_eq!(
            turn_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "unconfigured lane ⇒ the turn's provider grades (unchanged behavior)",
        );
        let (tokens_used, _, _) = orchestrator
            .goal_counters_for_test(&session)
            .expect("goal exists");
        assert_eq!(tokens_used, 8, "verifier spend (7+1) charged to the goal");
    }

    /// #1935 codex round 5 — a NotDone refusal also charges the verifier's
    /// spend to the goal exactly ONCE, directly (the removed ToolResult stamp
    /// would have ridden the turn totals into a SECOND charge on the still-
    /// active goal). Asserts the GOAL counter, not the ToolResult.
    #[tokio::test]
    async fn goal_update_notdone_refusal_charges_verifier_usage_once() {
        use crate::api::agent_orchestrator::{AgentOrchestrator as _, GoalSetRequest};
        let orchestrator = default_agent_orchestrator();
        let session = SessionKey("verifier-notdone-prof:api:goal-update-notdone".to_owned());
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session.clone(),
                profile_id: "verifier-notdone-prof".to_owned(),
                objective: "refuse but charge once".to_owned(),
                status: Some("active".to_owned()),
                token_budget: Some(10_000),
                transition_actor: None,
            })
            .expect("set goal");

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier = std::sync::Arc::new(CountingVerifierProvider {
            calls: calls.clone(),
            reply: "NOT_DONE: evidence missing",
            usage: octos_llm::TokenUsage {
                input_tokens: 20,
                output_tokens: 8,
                ..Default::default()
            },
        });
        let tool = GoalUpdateTool::new("verifier-notdone-prof").with_verifier_provider(verifier);
        let mut ctx = ToolContext::zero();
        ctx.parent_session_key = Some(session.0.clone());

        let result = tool
            .execute_with_context(&ctx, &json!({"status": "complete", "reason": "not yet"}))
            .await
            .expect("goal_update runs");
        assert!(!result.success, "NotDone verdict refuses the transition");
        assert!(result.tokens_used.is_none(), "no stamp on refusal either");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (tokens_used, _, _) = orchestrator
            .goal_counters_for_test(&session)
            .expect("goal exists");
        assert_eq!(
            tokens_used, 28,
            "the refused verifier call is still real spend, charged exactly once",
        );
        assert_eq!(
            orchestrator.goal_status_for_test(&session).as_deref(),
            Some("active"),
            "the goal stays active after the refusal",
        );
    }

    /// #1935 codex round 6 — the fleet-mutating goal tools must not run in a
    /// parallel batch: their snapshot→await→commit windows interleave (the
    /// goal_plan double-bind was the observed instance). `goal_update` stays
    /// Safe deliberately — its transition is CAS-guarded by the revision
    /// snapshot, so a concurrent loser gets a clean stale-verdict error.
    #[test]
    fn goal_fleet_tools_are_concurrency_exclusive() {
        assert_eq!(
            GoalPlanTool::new("p").concurrency_class(),
            ConcurrencyClass::Exclusive
        );
        assert_eq!(
            GoalDispatchTool::new("p").concurrency_class(),
            ConcurrencyClass::Exclusive
        );
        assert_eq!(
            GoalGrantTool::new("p").concurrency_class(),
            ConcurrencyClass::Exclusive
        );
        assert_eq!(
            GoalDenyTool::new("p").concurrency_class(),
            ConcurrencyClass::Exclusive
        );
        // #1935 round 7 — goal_create's admission spans two lock scopes; the
        // batch must serialize it (plus the atomic set_goal guard).
        assert_eq!(
            GoalCreateTool::new("p").concurrency_class(),
            ConcurrencyClass::Exclusive
        );
        // goal_get stays Safe NOT because it is read-only — its fleet
        // snapshot carries the lazy backstop terminalization — but because
        // that backstop drives `drive_goal_terminal_transition` with the
        // in-hand fleet id, and `model_transition_goal_at_key` re-verifies
        // the binding (`expected_fleet_id`) UNDER the state lock immediately
        // before the flip (#1865 review FIX 2), so concurrent goal_get calls
        // cannot terminalize a re-planned goal on stale evidence.
        assert_eq!(
            GoalGetTool::new("p").concurrency_class(),
            ConcurrencyClass::Safe
        );
        // goal_update stays Safe: its transition is CAS-guarded by the
        // revision snapshot, so a concurrent loser fails cleanly.
        assert_eq!(
            GoalUpdateTool::new("p").concurrency_class(),
            ConcurrencyClass::Safe
        );
    }

    /// #1967 — `goal_get` must SURFACE open escalations: the rows are written
    /// when a goal-scoped peer parks (`model_goal_record_peer_escalation`) but
    /// until this fold no production read existed, so the master model could
    /// never see them. Same data_dir gate as `ledger_findings`.
    #[tokio::test]
    async fn goal_get_includes_open_escalations_when_data_dir_set() {
        use crate::api::agent_orchestrator::{AgentOrchestrator as _, GoalSetRequest};
        // The tool reads the PROCESS-GLOBAL orchestrator: use a unique
        // session/profile and never clear the shared state (sibling tests own
        // their own keys — same idiom as the ui_protocol continuation tests).
        let orchestrator = default_agent_orchestrator();
        let session = SessionKey("esc-tenant:api:goal-get-open-escalations".to_owned());
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session.clone(),
                profile_id: "esc-tenant".to_owned(),
                objective: "surface open escalations".to_owned(),
                status: Some("active".to_owned()),
                token_budget: Some(1_000),
                transition_actor: None,
            })
            .expect("set goal");
        let goal_id = orchestrator
            .goal_id_for_session(&session)
            .expect("goal id minted");

        // Seed the goal's ledger with ONE open escalation (goal ids are
        // `goal_NN` — already filename-safe).
        let data_dir = tempfile::tempdir().unwrap();
        let ledger_dir = data_dir.path().join("goal-ledgers");
        std::fs::create_dir_all(&ledger_dir).unwrap();
        let ledger =
            octos_fleet::GoalLedger::open(ledger_dir.join(format!("{goal_id}.db"))).unwrap();
        ledger
            .upsert_goal(&octos_fleet::Goal {
                goal_id: goal_id.clone(),
                objective: "surface open escalations".to_owned(),
                status: "active".to_owned(),
                tokens_used: 0,
                token_budget: 1_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        ledger
            .append_escalation(&octos_fleet::Escalation {
                escalation_id: "esc-helper-1".to_owned(),
                goal_id: goal_id.clone(),
                task_id: None,
                peer_id: "helper".to_owned(),
                question: "[question] which port?".to_owned(),
                context: None,
                status: "open".to_owned(),
                default_action: None,
                default_after_secs: None,
                created_at_ms: 1_000,
                resolved_at_ms: None,
                resolved_by: None,
                resolution: None,
            })
            .unwrap();

        let tool = GoalGetTool::new("esc-tenant").with_data_dir(data_dir.path().to_path_buf());
        let mut ctx = ToolContext::zero();
        ctx.parent_session_key = Some(session.0.clone());
        let result = tool
            .execute_with_context(&ctx, &json!({}))
            .await
            .expect("goal_get runs");
        assert!(result.success, "goal_get succeeds: {}", result.output);
        let snapshot: Value = serde_json::from_str(&result.output).expect("json snapshot");
        let escalations = snapshot["open_escalations"]
            .as_array()
            .unwrap_or_else(|| panic!("open_escalations folded in, got: {snapshot}"));
        assert_eq!(escalations.len(), 1);
        assert_eq!(escalations[0]["escalation_id"], json!("esc-helper-1"));
        assert_eq!(escalations[0]["peer_id"], json!("helper"));
        assert_eq!(escalations[0]["question"], json!("[question] which port?"));
        assert!(
            escalations[0]["age_seconds"].as_u64().is_some(),
            "age_seconds present"
        );
    }

    /// #1945 — `goal_get` folds the compact `ledger_digest` (next to the
    /// `ledger_findings` row dump) when constructed with a data dir, so a
    /// master re-orienting after a restart reads bounded counts, never a
    /// view that grows with the goal. codex round — the ledger is seeded
    /// through the REAL production writer (`model_goal_record_peer_finding`),
    /// so the assertions state what production actually produces: a `running`
    /// task stub (nothing updates task status today) and — now that #1965
    /// charges real tokens — a cost total that includes the task-less row
    /// the old path-digest roll-up dropped.
    #[tokio::test]
    async fn goal_get_folds_ledger_digest_when_constructed_with_data_dir() {
        use crate::api::agent_orchestrator::{AgentOrchestrator, GoalSetRequest};

        let orchestrator = default_agent_orchestrator();
        // Unique wire id: the default orchestrator is process-global.
        let wire = "digest-prof:local:goal-digest-tool";
        let session = SessionKey(wire.to_owned());
        orchestrator
            .set_goal(GoalSetRequest {
                session_id: session.clone(),
                profile_id: "digest-prof".into(),
                objective: "prove the ledger_digest fold".into(),
                status: Some("active".into()),
                token_budget: Some(10_000),
                transition_actor: None,
            })
            .expect("set goal");
        let goal_id = orchestrator
            .goal_id_for_session(&session)
            .expect("goal id for session");

        // Seed the ledger through the production write path: one finding
        // staged with a fleet task (creates the `running` FK stub row) that
        // charges 700 tokens (#1965), one WITHOUT a task_id (the common
        // peer_handoff case — the row the old path-digest cost roll-up
        // dropped) charging 0.
        let data_dir = tempfile::TempDir::new().unwrap();
        orchestrator
            .model_goal_record_peer_finding(
                data_dir.path(),
                &goal_id,
                "digest-prof",
                wire,
                "peer-alpha",
                Some("t1"),
                "it works",
                700,
            )
            .expect("task-scoped finding recorded");
        orchestrator
            .model_goal_record_peer_finding(
                data_dir.path(),
                &goal_id,
                "digest-prof",
                wire,
                "peer-beta",
                None,
                "task-less, still counted",
                0,
            )
            .expect("task-less finding recorded");

        let tool = GoalGetTool::new("digest-prof").with_data_dir(data_dir.path().to_path_buf());
        let mut ctx = ToolContext::zero();
        ctx.parent_session_key = Some(wire.to_owned());
        let result = tool
            .execute_with_context(&ctx, &json!({}))
            .await
            .expect("goal_get executes");
        assert!(result.success, "goal_get failed: {}", result.output);
        let snapshot: Value = serde_json::from_str(&result.output).expect("json snapshot");
        let digest = snapshot
            .get("ledger_digest")
            .expect("ledger_digest folded into the goal snapshot");
        assert_eq!(digest["tasks"], json!({"running": 1}));
        assert_eq!(digest["findings"]["total"], json!(2));
        assert_eq!(digest["findings"]["by_lifecycle"], json!({"observed": 2}));
        assert_eq!(digest["findings"]["by_kind"], json!({"observation": 2}));
        assert_eq!(
            digest["cost_tokens"],
            json!(700),
            "digest sums the real #1965 per-finding charges"
        );
        // The row-dump sibling still rides along — digest summarizes, it does
        // not replace.
        assert!(
            snapshot.get("ledger_findings").is_some(),
            "ledger_findings must remain next to ledger_digest"
        );
    }

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
    fn goal_plan_parses_per_path_write_grant() {
        // #1976 — the object form of `fs` expresses a per-path WRITE fence:
        // `{ "write": [globs], "create_only": bool }`. The fs scope stays
        // Workspace (a fence is only coherent there) and the allowlist +
        // create_only land on the grant verbatim.
        let args = json!({
            "tasks": [ {
                "task_id": "refine",
                "title": "refine the exemplar",
                "grant": {
                    "fs": { "write": ["exemplar.card", "cards/*.card"], "create_only": true }
                }
            } ]
        });
        let specs = parse_task_specs(&args).expect("parses");
        let grant = &specs[0].grant;
        assert_eq!(grant.fs, FsGrant::Workspace);
        assert_eq!(
            grant.write_paths,
            Some(vec![
                "exemplar.card".to_string(),
                "cards/*.card".to_string()
            ]),
        );
        assert!(grant.create_only);
        // Omitted create_only defaults false; entries are trimmed.
        let plain = json!({
            "tasks": [ {
                "task_id": "t",
                "title": "t",
                "grant": { "fs": { "write": ["  out.txt  "] } }
            } ]
        });
        let specs = parse_task_specs(&plain).expect("parses");
        assert_eq!(
            specs[0].grant.write_paths,
            Some(vec!["out.txt".to_string()])
        );
        assert!(!specs[0].grant.create_only);
    }

    #[test]
    fn goal_plan_rejects_malformed_per_path_write_grant() {
        // #1976 — parse/validation failures surface as plan-time errors, so
        // an inexpressible fence can never reach the store: traversal,
        // absolute paths, create_only-with-nothing, unknown keys, and
        // non-boolean create_only are all named in the error.
        let cases: [(Value, &str); 6] = [
            (json!({ "write": ["../escape"] }), "fs.write"),
            (json!({ "write": ["/etc/passwd"] }), "fs.write"),
            (json!({ "write": [], "create_only": true }), "create_only"),
            (json!({ "create_only": true }), "write"),
            (json!({ "write": ["ok.txt"], "surprise": 1 }), "surprise"),
            (
                json!({ "write": ["ok.txt"], "create_only": "yes" }),
                "create_only",
            ),
        ];
        for (fs, needle) in cases {
            let args = json!({
                "tasks": [ {
                    "task_id": "t1",
                    "title": "do it",
                    "grant": { "fs": fs }
                } ]
            });
            let err = parse_task_specs(&args).expect_err("malformed fence rejected");
            assert!(
                err.contains(needle),
                "error for fs={} must mention `{needle}`: {err}",
                args["tasks"][0]["grant"]["fs"],
            );
        }
    }

    #[test]
    fn goal_plan_rejects_per_path_write_grant_with_host_fs() {
        // #1976 — the object form always implies workspace scope; a fence
        // cannot be combined with `host` (there is no syntax for it: `fs` is
        // ONE field), and the fleet-side validate() also rejects the
        // programmatic combination. Assert the parse-level story: `host`
        // still parses as the binary grant with NO fence.
        let args = json!({
            "tasks": [ {
                "task_id": "t1",
                "title": "do it",
                "grant": { "fs": "host" }
            } ]
        });
        let specs = parse_task_specs(&args).expect("binary host grant parses");
        assert_eq!(specs[0].grant.fs, FsGrant::Host);
        assert_eq!(specs[0].grant.write_paths, None);
        assert!(!specs[0].grant.create_only);
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
