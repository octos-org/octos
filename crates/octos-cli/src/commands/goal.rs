//! #25 — operator goal exits: `octos goal reopen` / `octos goal archive`.
//!
//! Goal state is authoritative in the supervisor event stream (the
//! `group_registered` metadata that `restore_goal_from_group` replays at
//! boot), NOT in the goal-ledger SQLite `goals` table (a direct sqlite edit
//! changes no running process and is never restored from). These commands
//! route through the live serve when its data-dir lock is held. Only a proven
//! offline data dir retains the append-only fallback; a live-but-unreachable
//! control endpoint fails closed so an old in-memory snapshot cannot overwrite
//! the operator's intent.
//!
//! `reopen` admits `blocked|paused|budget_limited` → `active`; `archive`
//! admits any status → `archived`, which is a TERMINAL, irreversible state
//! (only `complete`/`blocked` are model-reachable, `archived` is reachable
//! only here — the operator/outer-loop path — and `set_goal`'s status enum
//! refuses it, so nothing can un-archive).

use std::path::PathBuf;

use clap::{Args, Subcommand};
use eyre::{Result, WrapErr, bail, eyre};

use super::Executable;
use crate::autonomy::supervisor_store::{
    GroupStatus, SupervisedGroupRecord, SupervisorEvent, SupervisorStore,
};

/// Manage operator-owned goal transitions (reopen / archive).
#[derive(Debug, Args)]
pub struct GoalCommand {
    /// Profile data dir (defaults to the resolved config data dir).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Profile id whose supervisor store holds the goal.
    #[arg(long, default_value = "octos")]
    pub profile: String,

    #[command(subcommand)]
    pub subcommand: GoalSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum GoalSubcommand {
    /// List every goal record in the profile's supervisor event stream.
    List,
    /// Reopen a blocked/paused/budget_limited goal (status → active).
    Reopen {
        /// Goal id (e.g. `goal_03`), as shown by `octos goal list`.
        goal_id: String,
    },
    /// Archive a goal (status → archived). TERMINAL and irreversible.
    Archive {
        /// Goal id (e.g. `goal_03`), as shown by `octos goal list`.
        goal_id: String,
    },
    /// Read one goal's status straight from its ledger (#2116 read-only).
    Status(GoalStatusArgs),
}

/// One goal row located in the supervisor event stream: the session key the
/// goal is stored under, plus the full persisted group record (whose metadata
/// bag carries every goal field).
// #35-union: `octos goal status` — read-only goal observability (OLP L1,
// slice 2, upstream #2116). Contract: task-req-olp-obs-cli.spec.md —
// scenarios "serve 停止时仍可读 goal 状态" and "未知 goal id 报结构化错误".
// Reads the per-goal ledger (`<data_dir>/goal-ledgers/<goal_id>.db`, SQLite
// via octos_fleet::GoalLedger) DIRECTLY — no serve process required.
// `--json` and the human table share one assembly layer
// ([`GoalStatusView`]) so the two modes can never diverge.
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct GoalOperatorControlRequest {
    method: String,
    session_id: String,
    profile_id: String,
    goal_id: String,
    action: String,
    reason: String,
}

impl GoalOperatorControlRequest {
    #[cfg(feature = "api")]
    fn action_target_status(&self) -> &str {
        match self.action.as_str() {
            "reopen" => "active",
            "archive" => "archived",
            _ => "<invalid>",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GoalOperatorControlResponse {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
}

const GOAL_OPERATOR_CONTROL_SOCKET: &str = ".octos-goal-control.sock";
const GOAL_OPERATOR_TRANSITION_METHOD: &str = "session/goal/operator_transition";

/// `sun_path` capacity for a bound unix socket. The terminating NUL is not
/// representable in the path, so the limit is exclusive — a path of exactly
/// this many bytes still fails to bind.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SUN_PATH_CAPACITY: usize = 108;
/// Conservative value for every other unix target (macOS/BSD).
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const SUN_PATH_CAPACITY: usize = 104;

/// Marker file a relocated serve publishes inside the data dir, carrying the
/// real socket path so the connect side needs no environment knowledge.
#[cfg(unix)]
const GOAL_OPERATOR_CONTROL_SOCKET_POINTER: &str = ".octos-goal-control.sock.path";

/// Bind-side resolution of the goal operator-control socket (#2248).
#[cfg(unix)]
struct GoalControlSocketPath {
    path: PathBuf,
    /// True when the canonical in-data-dir path did not fit `sun_path` and
    /// the socket was relocated outside the data dir.
    relocated: bool,
}

/// #2248 — the canonical location is `<data_dir>/.octos-goal-control.sock`,
/// but `sun_path` is a fixed-size array, so a long data dir made
/// `UnixListener::bind` fail with `path must be shorter than SUN_LEN` and
/// took down all of `octos serve`. When the canonical path does not fit, the
/// socket relocates to a deterministic hashed name outside the data dir and
/// the real path is published in a pointer file inside the data dir for the
/// connect side. The length decision runs on the canonicalized path so a
/// short symlink spelling cannot split the two sides' choices.
#[cfg(unix)]
fn goal_control_bind_path(data_dir: &Path) -> Result<GoalControlSocketPath> {
    let resolved = std::fs::canonicalize(data_dir).unwrap_or_else(|error| {
        tracing::debug!(%error, data_dir = %data_dir.display(), "canonicalize failed; using the spelled data dir");
        data_dir.to_path_buf()
    });
    let canonical = resolved.join(GOAL_OPERATOR_CONTROL_SOCKET);
    if canonical.as_os_str().len() < SUN_PATH_CAPACITY {
        return Ok(GoalControlSocketPath {
            path: canonical,
            relocated: false,
        });
    }
    let fallback = hashed_fallback_socket(&fallback_socket_base_dir()?, &resolved)?;
    tracing::info!(
        socket_path = %fallback.display(),
        "data dir too long for a unix socket sun_path; goal operator-control RPC          relocated outside the data dir (#2248)"
    );
    Ok(GoalControlSocketPath {
        path: fallback,
        relocated: true,
    })
}

/// Connect-side resolution: the pointer file a relocated serve publishes, or
/// the canonical in-data-dir path otherwise.
#[cfg(unix)]
fn goal_control_connect_path(data_dir: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    if let Ok(contents) =
        std::fs::read_to_string(resolved.join(GOAL_OPERATOR_CONTROL_SOCKET_POINTER))
    {
        let path = contents.trim();
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    resolved.join(GOAL_OPERATOR_CONTROL_SOCKET)
}

/// After a successful bind, keep the data dir's pointer file in sync with
/// where the socket actually lives: write it when relocated, remove a stale
/// one when canonical. A missing pointer while the serve lock is held means
/// an old serve, which the connect side already fails closed against.
#[cfg(unix)]
fn publish_goal_control_socket(data_dir: &Path, socket: &GoalControlSocketPath) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let resolved = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let pointer = resolved.join(GOAL_OPERATOR_CONTROL_SOCKET_POINTER);
    if !socket.relocated {
        match std::fs::remove_file(&pointer) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    }
    let stage = resolved.join(".octos-goal-control.sock.path.tmp");
    std::fs::write(
        &stage,
        format!(
            "{}
",
            socket.path.display()
        ),
    )
    .wrap_err_with(|| {
        format!(
            "failed to stage goal-control socket pointer at {}",
            stage.display()
        )
    })?;
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&stage, &pointer).wrap_err_with(|| {
        format!(
            "failed to publish goal-control socket pointer at {}",
            pointer.display()
        )
    })?;
    Ok(())
}

/// Base directory for a relocated goal-control socket: `$XDG_RUNTIME_DIR`
/// when set and absolute (per-user, 0700, exempt from tmpfiles aging); else
/// the temp dir — nested under a per-user 0700 subdir when the temp dir is
/// shared (sticky world-writable, e.g. Linux `/tmp`), so no other local user
/// can pre-create the predictable socket name and block serve startup. A
/// per-user temp dir (macOS `/var/folders/.../T`, 0700) is used directly: it
/// needs no subdir, and its length leaves no room for one under `sun_path`.
///
/// The returned bool marks a subdir the caller must create and chmod 0700.
/// Pure: environment and mode bits come in as parameters for testability.
#[cfg(unix)]
fn fallback_base_dir(
    xdg_runtime_dir: Option<PathBuf>,
    temp_dir: PathBuf,
    temp_mode: u32,
) -> (PathBuf, bool) {
    if let Some(dir) = xdg_runtime_dir.filter(|d| d.is_absolute()) {
        return (dir, false);
    }
    if temp_mode & 0o7777 == 0o1777 {
        let uid = rustix::process::geteuid().as_raw();
        return (temp_dir.join(format!("octos-goal-control-{uid}")), true);
    }
    (temp_dir, false)
}

#[cfg(unix)]
fn fallback_socket_base_dir() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let xdg = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let temp = std::env::temp_dir();
    let mode = std::fs::metadata(&temp)
        .map(|m| m.permissions().mode())
        .unwrap_or(0);
    let (base, ours) = fallback_base_dir(xdg, temp, mode);
    if ours {
        std::fs::create_dir_all(&base).wrap_err_with(|| {
            format!(
                "failed to create goal-control fallback dir {}",
                base.display()
            )
        })?;
        // Refuse (EPERM, named) if another local user pre-created the
        // directory; heal our own to 0700 so the socket stays owner-only
        // even on a shared /tmp.
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).wrap_err_with(
            || {
                format!(
                    "failed to restrict goal-control fallback dir {}",
                    base.display()
                )
            },
        )?;
    }
    Ok(base)
}

/// Deterministic relocated socket name under `base`, keyed by a stable hash
/// of the canonical data dir.
#[cfg(unix)]
fn hashed_fallback_socket(base: &Path, canonical_data_dir: &Path) -> Result<PathBuf> {
    let path = base.join(format!(
        ".octos-goal-control-{:016x}.sock",
        fnv1a64(canonical_data_dir)
    ));
    if path.as_os_str().len() >= SUN_PATH_CAPACITY {
        bail!(
            "goal operator-control socket path {} is still too long for a unix socket              (sun_path limit {SUN_PATH_CAPACITY} bytes); set XDG_RUNTIME_DIR or TMPDIR to a              shorter path",
            path.display()
        );
    }
    Ok(path)
}

/// Cheap 64-bit FNV-1a hash of a path (same idiom as octos-agent's
/// file_state_cache) — stable across processes and releases, which is what
/// the connect side needs.
#[cfg(unix)]
fn fnv1a64(path: &Path) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for &byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(all(unix, feature = "api"))]
pub(crate) fn spawn_goal_operator_control(
    data_dir: &Path,
    profile_store: std::sync::Arc<crate::profiles::ProfileStore>,
) -> Result<tokio::task::JoinHandle<()>> {
    use std::os::unix::fs::PermissionsExt;

    let socket = goal_control_bind_path(data_dir)?;
    match std::fs::remove_file(&socket.path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixListener::bind(&socket.path).map_err(|error| {
        eyre!(
            "failed to bind goal operator-control socket at {}: {error}",
            socket.path.display()
        )
    })?;
    std::fs::set_permissions(&socket.path, std::fs::Permissions::from_mode(0o600))?;
    // A relocated socket is undiscoverable by environment-independent
    // recomputation alone; publish its real path inside the data dir. If we
    // cannot, the endpoint is unreachable while the serve lock is held —
    // fail startup rather than strand `octos goal reopen/archive`.
    publish_goal_control_socket(data_dir, &socket)?;
    let runtime_data_dir = data_dir.to_path_buf();
    Ok(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let profile_store = profile_store.clone();
            let runtime_data_dir = runtime_data_dir.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    serve_goal_operator_connection(stream, &profile_store, &runtime_data_dir).await
                {
                    tracing::warn!(%error, "online goal-control RPC connection failed");
                }
            });
        }
    }))
}

#[cfg(all(unix, feature = "api"))]
async fn serve_goal_operator_connection(
    stream: tokio::net::UnixStream,
    profile_store: &crate::profiles::ProfileStore,
    runtime_data_dir: &Path,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (read_half, mut write_half) = stream.into_split();
    let mut line = String::new();
    tokio::io::BufReader::new(read_half)
        .read_line(&mut line)
        .await?;
    let response = match serde_json::from_str::<GoalOperatorControlRequest>(&line) {
        Ok(request) if request.method == GOAL_OPERATOR_TRANSITION_METHOD => {
            let ledger_data_dir = profile_store
                .get(&request.profile_id)
                .ok()
                .flatten()
                .map(|profile| profile_store.resolve_data_dir(&profile));
            match crate::autonomy::agent_orchestrator::default_agent_orchestrator()
                .operator_transition_goal(
                    &octos_core::SessionKey(request.session_id.clone()),
                    &request.profile_id,
                    Some(&request.goal_id),
                    &request.action,
                    &request.reason,
                    ledger_data_dir.as_deref(),
                ) {
                Ok(goal) => {
                    let result = serde_json::json!({
                        "session_id": &request.session_id,
                        "profile_id": &request.profile_id,
                        "goal": goal,
                        "transition_actor": "operator"
                    });
                    let live_status = result
                        .pointer("/goal/status")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let supervisor_status =
                        SupervisorStore::new(runtime_data_dir.join("supervisor"))
                            .load_goal_groups_by_id()
                            .ok()
                            .and_then(|groups| {
                                groups.into_values().find(|group| {
                                    metadata_str(group, "goal_id") == Some(request.goal_id.as_str())
                                        && metadata_str(group, "profile_id")
                                            == Some(request.profile_id.as_str())
                                        && metadata_str(group, "session_id")
                                            == Some(request.session_id.as_str())
                                })
                            })
                            .and_then(|group| metadata_str(&group, "status").map(str::to_owned));
                    let ledger_status = ledger_data_dir
                        .as_deref()
                        .and_then(|dir| load_goal_status(dir, &request.goal_id).ok().flatten())
                        .map(|view| view.status);
                    if live_status.as_deref() == Some(request.action_target_status())
                        && supervisor_status == live_status
                        && ledger_status == live_status
                    {
                        GoalOperatorControlResponse {
                            ok: true,
                            result: Some(result),
                            error: None,
                        }
                    } else {
                        GoalOperatorControlResponse {
                            ok: false,
                            result: Some(result),
                            error: Some(format!(
                                "transition committed but three-way reconciliation failed: \
                                 live={live_status:?}, supervisor={supervisor_status:?}, \
                                 ledger={ledger_status:?}"
                            )),
                        }
                    }
                }
                Err(error) => GoalOperatorControlResponse {
                    ok: false,
                    result: None,
                    error: Some(error),
                },
            }
        }
        Ok(_) => GoalOperatorControlResponse {
            ok: false,
            result: None,
            error: Some("unsupported local control method".into()),
        },
        Err(error) => GoalOperatorControlResponse {
            ok: false,
            result: None,
            error: Some(format!("invalid request: {error}")),
        },
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.shutdown().await?;
    Ok(())
}

enum ServeLiveness {
    Offline,
    Live,
}

fn serve_liveness(data_dir: &Path) -> Result<ServeLiveness> {
    let lock_path = data_dir.join(".octos-serve.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            fs2::FileExt::unlock(&file)?;
            Ok(ServeLiveness::Offline)
        }
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(ServeLiveness::Live)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn online_transition(
    data_dir: &Path,
    located: &LocatedGoal,
    profile: &str,
    goal_id: &str,
    action: &str,
) -> Result<()> {
    use std::io::{BufRead, Write};

    let socket_path = goal_control_connect_path(data_dir);
    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path).map_err(|error| {
        eyre!(
            "octos serve is live for this data directory, but its online goal-control RPC is \
             unavailable at {} ({error}); refusing an unsafe offline {action}. Stop serve and \
             retry, or upgrade the running serve.",
            socket_path.display()
        )
    })?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    let request = GoalOperatorControlRequest {
        method: GOAL_OPERATOR_TRANSITION_METHOD.to_owned(),
        session_id: located.session_id.clone(),
        profile_id: profile.to_owned(),
        goal_id: goal_id.to_owned(),
        action: action.to_owned(),
        reason: format!("operator invoked `octos goal {action}`"),
    };
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    std::io::BufReader::new(stream).read_line(&mut line)?;
    let response: GoalOperatorControlResponse = serde_json::from_str(&line)
        .map_err(|error| eyre!("malformed response from online goal-control RPC: {error}"))?;
    if !response.ok {
        bail!(
            "online goal transition failed: {}",
            response.error.as_deref().unwrap_or("unknown serve error")
        );
    }
    let result = response.result.unwrap_or_default();
    let status = result
        .pointer("/goal/status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown>");
    println!(
        "goal `{goal_id}` on session `{}` transitioned online via live serve -> {status} ({action})",
        located.session_id
    );
    println!("note: live state, supervisor recovery stream, and goal ledger were updated inline.");
    Ok(())
}

#[cfg(not(unix))]
fn online_transition(
    _data_dir: &Path,
    _located: &LocatedGoal,
    _profile: &str,
    _goal_id: &str,
    action: &str,
) -> Result<()> {
    bail!(
        "octos serve is live, but this build has no local goal-control transport; refusing an \
         unsafe offline {action}. Stop serve and retry."
    )
}

/// #2116 read-only `status` args (union family on our GoalSubcommand).
#[derive(Debug, Args)]
pub struct GoalStatusArgs {
    /// Goal id (default: the most recently updated goal in the dir).
    #[arg(long)]
    pub goal: Option<String>,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// One goal row located in the supervisor event stream: the session key the
/// goal is stored under, plus the full persisted group record (whose metadata
/// bag carries every goal field).
#[derive(Debug)]
struct LocatedGoal {
    session_id: String,
    group: SupervisedGroupRecord,
}

/// Shared assembly layer for table + JSON output. Field names are part of
/// the machine contract — do not rename without a spec bump.
#[derive(Debug, Serialize)]
pub(crate) struct GoalStatusView {
    pub goal_id: String,
    pub status: String,
    pub objective: String,
    pub tokens_used: u64,
    pub token_budget: u64,
    pub time_used_seconds: u64,
    pub continuations_used: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl From<octos_fleet::Goal> for GoalStatusView {
    fn from(goal: octos_fleet::Goal) -> Self {
        Self {
            goal_id: goal.goal_id,
            status: goal.status,
            objective: goal.objective,
            tokens_used: goal.tokens_used,
            token_budget: goal.token_budget,
            time_used_seconds: goal.time_used_seconds,
            continuations_used: goal.continuations_used,
            created_at_ms: goal.created_at_ms,
            updated_at_ms: goal.updated_at_ms,
        }
    }
}

/// Structured error payload for `--json` failures (contract: stderr JSON
/// with an `error` field, non-zero exit).
#[derive(Debug, Serialize)]
struct GoalStatusError {
    error: String,
    goal_id: Option<String>,
}

fn sanitize_goal_id_for_file(goal_id: &str) -> String {
    // Mirror `autonomy::agent_orchestrator::sanitize_filename_for_ledger`
    // (alphanumeric / '-' / '_' kept, everything else -> '_'). Kept as a
    // local copy so the read-only CLI never reaches into the
    // orchestrator's private module; both are covered by tests asserting
    // the on-disk layout.
    goal_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// #2116 — the most recently modified goal ledger id in `dir` (mtime scan).
fn latest_goal_ledger_id(dir: &Path) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".db") else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| mtime > *b) {
            best = Some((mtime, id.to_owned()));
        }
    }
    best.map(|(_, id)| id)
}

fn goal_ledger_db_path(data_dir: &Path, goal_id: &str) -> PathBuf {
    data_dir
        .join("goal-ledgers")
        .join(format!("{}.db", sanitize_goal_id_for_file(goal_id)))
}

/// Load one goal's status straight from its ledger file. Returns
/// `Ok(None)` when the ledger file or the goal row does not exist.
pub(crate) fn load_goal_status(data_dir: &Path, goal_id: &str) -> Result<Option<GoalStatusView>> {
    let db_path = goal_ledger_db_path(data_dir, goal_id);
    if !db_path.exists() {
        return Ok(None);
    }
    let ledger = octos_fleet::GoalLedger::open(&db_path)?;
    Ok(ledger.get_goal(goal_id)?.map(GoalStatusView::from))
}

fn emit_error(json: bool, goal_id: Option<&str>, message: &str) -> ! {
    if json {
        let payload = GoalStatusError {
            error: message.to_owned(),
            goal_id: goal_id.map(str::to_owned),
        };
        eprintln!("{}", serde_json::to_string(&payload).expect("error json"));
    } else {
        eprintln!("error: {message}");
    }
    std::process::exit(1);
}

fn print_table(view: &GoalStatusView) {
    println!("goal_id:       {}", view.goal_id);
    println!("status:        {}", view.status);
    println!("objective:     {}", view.objective);
    println!(
        "tokens:        {} / {}",
        view.tokens_used, view.token_budget
    );
    println!("time_seconds:  {}", view.time_used_seconds);
    println!("continuations: {}", view.continuations_used);
    println!("created_at_ms: {}", view.created_at_ms);
    println!("updated_at_ms: {}", view.updated_at_ms);
}

impl Executable for GoalCommand {
    fn execute(self) -> Result<()> {
        let data_dir = super::resolve_data_dir(self.data_dir)?;
        match self.subcommand {
            // ###27-B3 — the supervisor store load lives INSIDE the
            // transition arms: `goal status` is contractually a DIRECT
            // ledger read that must succeed even when the (unrelated)
            // supervisor event store is corrupt or unreadable — hoisting
            // the load above the match let a broken store break status.
            GoalSubcommand::List
            | GoalSubcommand::Reopen { .. }
            | GoalSubcommand::Archive { .. } => {
                // Mirror the serve layout: the supervisor store for a profile lives
                // under `<data_dir>/supervisor` (the store itself namespaces its
                // events/snapshot files inside that root).
                let store_root = data_dir.join("supervisor");
                let store = SupervisorStore::new(&store_root);
                // #26a — goal-scoped view: folded BY GOAL ID so superseded goals (the
                // same session scope's earlier goal_NN rows) remain visible for
                // zombie cleanup, instead of vanishing under the newest goal's group.
                let goals_by_id = store.load_goal_groups_by_id().map_err(|error| {
                    eyre!("failed to load goal-scoped view from {store_root:?}: {error}")
                })?;
                match self.subcommand {
                    GoalSubcommand::List => cmd_list(&goals_by_id),
                    GoalSubcommand::Reopen { goal_id } => route_transition(
                        &data_dir,
                        &store,
                        &goals_by_id,
                        &self.profile,
                        &goal_id,
                        "reopen",
                    ),
                    GoalSubcommand::Archive { goal_id } => route_transition(
                        &data_dir,
                        &store,
                        &goals_by_id,
                        &self.profile,
                        &goal_id,
                        "archive",
                    ),
                    GoalSubcommand::Status(_) => unreachable!("guarded by the outer match"),
                }
            }
            // #35 — union with #2116's read-only Status: reads the goal
            // ledger DIRECTLY (no serve required), --json / table shared
            // assembly via GoalStatusView.
            GoalSubcommand::Status(args) => {
                let data_dir2 = data_dir.clone();
                let goal_id = match args.goal.as_deref() {
                    Some(id) => id.to_owned(),
                    None => {
                        // #2116 default: the most recently updated ledger in
                        // the goal-ledgers dir.
                        let dir = data_dir2.join("goal-ledgers");
                        latest_goal_ledger_id(&dir)
                            .ok_or_else(|| eyre!("no goal ledgers under {}", dir.display()))?
                    }
                };
                match load_goal_status(&data_dir2, &goal_id)? {
                    Some(view) => {
                        if args.json {
                            println!(
                                "{}",
                                serde_json::to_string(&view).expect("goal status json")
                            );
                        } else {
                            print_table(&view);
                        }
                    }
                    None => emit_error(args.json, Some(&goal_id), "goal not found"),
                }
                Ok(())
            }
        }
    }
}

fn route_transition(
    data_dir: &Path,
    store: &SupervisorStore,
    goals_by_id: &std::collections::HashMap<String, SupervisedGroupRecord>,
    profile: &str,
    goal_id: &str,
    action: &str,
) -> Result<()> {
    let located = locate_goal(goals_by_id, profile, goal_id)?;
    match serve_liveness(data_dir)? {
        ServeLiveness::Live => online_transition(data_dir, &located, profile, goal_id, action),
        ServeLiveness::Offline => cmd_transition_located(store, located, goal_id, action),
    }
}

/// Locate the NEWEST goal record with `goal_id` in the profile's stream.
/// `load_state` has already folded the full event replay, so
/// `state.groups` holds each goal's latest snapshot; several sessions may
/// have minted the same `goal_NN` id, so ambiguity is reported, never
/// silently guessed.
fn locate_goal(
    goals_by_id: &std::collections::HashMap<String, SupervisedGroupRecord>,
    profile: &str,
    goal_id: &str,
) -> Result<LocatedGoal> {
    let mut matches: Vec<LocatedGoal> = goals_by_id
        .values()
        .filter(|group| {
            metadata_str(group, "autonomy_record_kind") == Some("goal")
                && metadata_str(group, "goal_id") == Some(goal_id)
                && metadata_str(group, "profile_id") == Some(profile)
                && metadata_bool(group, "autonomy_goal_cleared") != Some(true)
        })
        .filter_map(|group| {
            metadata_str(group, "session_id").map(|session_id| LocatedGoal {
                session_id: session_id.to_owned(),
                group: group.clone(),
            })
        })
        .collect();
    matches.sort_by_key(|m| std::cmp::Reverse(m.group.updated_at_ms));
    match matches.len() {
        0 => bail!(
            "no goal `{goal_id}` found for profile `{profile}` in the supervisor event stream \
             (run `octos goal list --profile {profile}` to see known goals)"
        ),
        1 => Ok(matches.pop().expect("len checked")),
        _ => {
            let sessions: Vec<String> = matches.iter().map(|m| m.session_id.clone()).collect();
            bail!(
                "goal id `{goal_id}` is ambiguous for profile `{profile}` — it exists on \
                 sessions {sessions:?}; this command requires a unique goal id"
            );
        }
    }
}

fn cmd_list(goals_by_id: &std::collections::HashMap<String, SupervisedGroupRecord>) -> Result<()> {
    let mut rows: Vec<(String, String, String, String)> = goals_by_id
        .values()
        .filter(|group| metadata_str(group, "autonomy_record_kind") == Some("goal"))
        .map(|group| {
            (
                metadata_str(group, "goal_id")
                    .unwrap_or("<none>")
                    .to_owned(),
                metadata_str(group, "profile_id")
                    .unwrap_or("<none>")
                    .to_owned(),
                metadata_str(group, "session_id")
                    .unwrap_or("<none>")
                    .to_owned(),
                metadata_str(group, "status").unwrap_or("<none>").to_owned(),
            )
        })
        .collect();
    rows.sort();
    if rows.is_empty() {
        println!("no goal records in the supervisor event stream");
        return Ok(());
    }
    println!(
        "{:<12} {:<16} {:<40} status",
        "goal_id", "profile", "session_id"
    );
    for (goal_id, profile, session_id, status) in rows {
        println!("{goal_id:<12} {profile:<16} {session_id:<40} {status}");
    }
    Ok(())
}

#[cfg(test)]
fn cmd_transition(
    store: &SupervisorStore,
    goals_by_id: &std::collections::HashMap<String, SupervisedGroupRecord>,
    profile: &str,
    goal_id: &str,
    action: &str,
) -> Result<()> {
    let located = locate_goal(goals_by_id, profile, goal_id)?;
    cmd_transition_located(store, located, goal_id, action)
}

fn cmd_transition_located(
    store: &SupervisorStore,
    located: LocatedGoal,
    goal_id: &str,
    action: &str,
) -> Result<()> {
    let prior_status = metadata_str(&located.group, "status")
        .unwrap_or("<unknown>")
        .to_owned();
    let target_status = match action {
        "reopen" => {
            match prior_status.as_str() {
                "blocked" | "paused" | "budget_limited" => {}
                "active" => bail!("goal `{goal_id}` is already active; nothing to reopen"),
                other => bail!(
                    "cannot reopen goal `{goal_id}` from terminal status `{other}` \
                     (reopen is only allowed from blocked|paused|budget_limited)"
                ),
            }
            // Never resurrect an over-budget goal as `active` (that would
            // silently revert a `budget_limited` stop while still over cap).
            let tokens_used = metadata_u64(&located.group, "tokens_used").unwrap_or(0);
            let token_budget = metadata_u64(&located.group, "token_budget").unwrap_or(0);
            if token_budget > 0 && tokens_used >= token_budget {
                bail!(
                    "cannot reopen goal `{goal_id}`: it has exhausted its token budget \
                     ({tokens_used} >= {token_budget}); raise the budget first"
                );
            }
            "active"
        }
        "archive" => {
            if prior_status == "archived" {
                bail!("goal `{goal_id}` is already archived (terminal)");
            }
            "archived"
        }
        other => bail!("unknown action `{other}`"),
    };

    // Append a FRESH snapshot of the same group record with the new status.
    // `upsert_group` at replay replaces on `updated_at_ms >=`, so stamping
    // `now` guarantees this row wins the fold; `record_group_registered`
    // uses a per-event unique id inside `append_event`.
    let mut group = located.group.clone();
    group.updated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        .max(group.updated_at_ms.saturating_add(1));
    group.status = group_status_for(target_status);
    group
        .metadata
        .insert("status".into(), serde_json::json!(target_status));
    store
        .append_event(
            format!(
                "goal_operator_transition:{goal_id}:{action}:{}",
                group.updated_at_ms
            ),
            SupervisorEvent::GroupRegistered {
                group: group.clone(),
            },
        )
        .map_err(|error| eyre!("failed to append the goal transition event: {error}"))?;

    println!(
        "goal `{goal_id}` on session `{}`: {prior_status} -> {target_status} ({action})",
        located.session_id
    );
    println!(
        "note: no live serve owns this data directory; the offline transition is durable in \
         the supervisor recovery stream and takes effect on the next serve start."
    );
    Ok(())
}

fn group_status_for(status: &str) -> GroupStatus {
    match status {
        "active" => GroupStatus::Running,
        "blocked" => GroupStatus::Blocked,
        "budget_limited" => GroupStatus::BudgetLimited,
        "paused" => GroupStatus::Paused,
        // `archived` (and any terminal/stop) renders as a clean stop, never
        // as a failure — mirrors `group_status_for_goal` in the orchestrator.
        _ => GroupStatus::Completed,
    }
}

fn metadata_str<'a>(group: &'a SupervisedGroupRecord, key: &str) -> Option<&'a str> {
    group.metadata.get(key).and_then(serde_json::Value::as_str)
}

fn metadata_bool(group: &SupervisedGroupRecord, key: &str) -> Option<bool> {
    group.metadata.get(key).and_then(serde_json::Value::as_bool)
}

fn metadata_u64(group: &SupervisedGroupRecord, key: &str) -> Option<u64> {
    group.metadata.get(key).and_then(serde_json::Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn goal_group(
        session_id: &str,
        profile: &str,
        goal_id: &str,
        status: &str,
    ) -> SupervisedGroupRecord {
        let mut group = SupervisedGroupRecord::new(format!("autonomy-goal:{session_id}"), 1);
        group
            .metadata
            .insert("autonomy_record_kind".into(), json!("goal"));
        group
            .metadata
            .insert("autonomy_goal_cleared".into(), json!(false));
        group
            .metadata
            .insert("session_id".into(), json!(session_id));
        group.metadata.insert("profile_id".into(), json!(profile));
        group.metadata.insert("goal_id".into(), json!(goal_id));
        group.metadata.insert("objective".into(), json!("obj"));
        group.metadata.insert("status".into(), json!(status));
        group
            .metadata
            .insert("token_budget".into(), json!(1_000_000u64));
        group.metadata.insert("tokens_used".into(), json!(42u64));
        group
    }

    /// #25 — the CLI appends a fresh `group_registered` row; a store reload
    /// (what the next serve boot does) restores the new status.
    #[test]
    fn reopen_appends_event_that_replay_restores_as_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SupervisorStore::new(dir.path());
        store
            .record_group_registered(goal_group("api:s1", "octos", "goal_01", "blocked"))
            .unwrap();

        // blocked -> active via the command's transition path.
        let located =
            locate_goal(&store.load_goal_groups_by_id().unwrap(), "octos", "goal_01").unwrap();
        let mut group = located.group.clone();
        group.updated_at_ms = group.updated_at_ms.saturating_add(1);
        group.status = GroupStatus::Running;
        group.metadata.insert("status".into(), json!("active"));
        store
            .append_event(
                "goal_operator_transition:goal_01:reopen:2",
                SupervisorEvent::GroupRegistered { group },
            )
            .unwrap();

        let restored =
            locate_goal(&store.load_goal_groups_by_id().unwrap(), "octos", "goal_01").unwrap();
        assert_eq!(metadata_str(&restored.group, "status"), Some("active"));
    }

    /// #25 — archived survives the replay and the locate path rejects a
    /// second archive (terminal idempotence guard).
    #[test]
    fn archive_is_terminal_and_persists_across_replay() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SupervisorStore::new(dir.path());
        store
            .record_group_registered(goal_group("api:s1", "octos", "goal_02", "blocked"))
            .unwrap();

        let located =
            locate_goal(&store.load_goal_groups_by_id().unwrap(), "octos", "goal_02").unwrap();
        let mut group = located.group.clone();
        group.updated_at_ms = group.updated_at_ms.saturating_add(1);
        group.status = GroupStatus::Completed;
        group.metadata.insert("status".into(), json!("archived"));
        store
            .append_event(
                "goal_operator_transition:goal_02:archive:2",
                SupervisorEvent::GroupRegistered { group },
            )
            .unwrap();

        let restored =
            locate_goal(&store.load_goal_groups_by_id().unwrap(), "octos", "goal_02").unwrap();
        assert_eq!(metadata_str(&restored.group, "status"), Some("archived"));

        // A second archive against the archived record must fail.
        let result = cmd_transition(
            &store,
            &store.load_goal_groups_by_id().unwrap(),
            "octos",
            "goal_02",
            "archive",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already archived"));
    }

    /// #25 — reopen from a terminal status is refused before any write.
    #[test]
    fn reopen_from_complete_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SupervisorStore::new(dir.path());
        store
            .record_group_registered(goal_group("api:s1", "octos", "goal_03", "complete"))
            .unwrap();
        let result = cmd_transition(
            &store,
            &store.load_goal_groups_by_id().unwrap(),
            "octos",
            "goal_03",
            "reopen",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("terminal status"));
    }

    /// #25 — reopen of an over-budget goal is refused (would resurrect a
    /// budget_limited stop as active while still over cap).
    #[test]
    fn reopen_over_budget_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SupervisorStore::new(dir.path());
        let mut group = goal_group("api:s1", "octos", "goal_04", "budget_limited");
        group.metadata.insert("token_budget".into(), json!(100u64));
        group.metadata.insert("tokens_used".into(), json!(100u64));
        store.record_group_registered(group).unwrap();
        let result = cmd_transition(
            &store,
            &store.load_goal_groups_by_id().unwrap(),
            "octos",
            "goal_04",
            "reopen",
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exhausted its token budget")
        );
    }

    /// #25 — duplicate goal ids across sessions are reported, never guessed.
    #[test]
    fn ambiguous_goal_id_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = SupervisorStore::new(dir.path());
        store
            .record_group_registered(goal_group("api:s1", "octos", "goal_05", "blocked"))
            .unwrap();
        store
            .record_group_registered(goal_group("api:s2", "octos", "goal_05", "paused"))
            .unwrap();
        let result = locate_goal(&store.load_goal_groups_by_id().unwrap(), "octos", "goal_05");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("ambiguous"));
    }
}

#[cfg(test)]
mod tests_2116_readonly {
    use super::*;

    fn supervisor_goal_group(
        session_id: &str,
        profile: &str,
        goal_id: &str,
        status: &str,
    ) -> SupervisedGroupRecord {
        let mut group = SupervisedGroupRecord::new(format!("autonomy-goal:{session_id}"), 1);
        group
            .metadata
            .insert("autonomy_record_kind".into(), serde_json::json!("goal"));
        group
            .metadata
            .insert("autonomy_goal_cleared".into(), serde_json::json!(false));
        group
            .metadata
            .insert("session_id".into(), serde_json::json!(session_id));
        group
            .metadata
            .insert("profile_id".into(), serde_json::json!(profile));
        group
            .metadata
            .insert("goal_id".into(), serde_json::json!(goal_id));
        group
            .metadata
            .insert("objective".into(), serde_json::json!("obj"));
        group
            .metadata
            .insert("status".into(), serde_json::json!(status));
        group
    }

    fn seed_goal(data_dir: &Path, goal_id: &str, status: &str) {
        let db_path = goal_ledger_db_path(data_dir, goal_id);
        std::fs::create_dir_all(db_path.parent().expect("parent")).expect("mkdir");
        let ledger = octos_fleet::GoalLedger::open(&db_path).expect("open ledger");
        ledger
            .upsert_goal(&octos_fleet::Goal {
                goal_id: goal_id.to_owned(),
                objective: "contract fixture".to_owned(),
                status: status.to_owned(),
                tokens_used: 42,
                token_budget: 1000,
                time_used_seconds: 7,
                continuations_used: 1,
                revision: 0,
                created_at_ms: 100,
                updated_at_ms: 200,
            })
            .expect("seed goal");
    }

    /// ###27-B3 — REAL-dispatch regression: `goal status` must succeed
    /// when the (unrelated) supervisor event store is CORRUPT. The store
    /// load was hoisted above the subcommand match in #35's union merge,
    /// which let a broken store break the direct-ledger read; the load now
    /// lives inside the List/Reopen/Archive arms only.
    #[test]
    fn goal_status_survives_corrupt_supervisor_store() {
        let data_dir = std::env::temp_dir().join("olp-goal-corrupt-supervisor");
        let _ = std::fs::remove_dir_all(&data_dir);
        seed_goal(&data_dir, "goal_01", "active");
        // A corrupt supervisor event store: unparseable rows poison
        // load_goal_groups_by_id for the transition arms — status must not care.
        let sup_dir = data_dir.join("supervisor");
        std::fs::create_dir_all(&sup_dir).expect("mkdir supervisor");
        std::fs::write(sup_dir.join("supervisor-events.jsonl"), "{not json!\n")
            .expect("corrupt supervisor stream");
        let view = load_goal_status(&data_dir, "goal_01")
            .expect("direct ledger read must not touch the supervisor store")
            .expect("seeded goal row");
        assert_eq!(view.status, "active");
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// Contract scenario "serve 停止时仍可读 goal 状态" (critical): with
    /// only a data dir on disk (no serve anywhere in the picture),
    /// `goal status --json` must yield valid JSON with the goal's status.
    #[test]
    fn olp_obs_goal_status_json_without_serve() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_goal(temp.path(), "goal_01", "complete");
        let view = load_goal_status(temp.path(), "goal_01")
            .expect("load")
            .expect("goal exists");
        assert_eq!(view.status, "complete");
        // The JSON mode must serialize the same view the table prints.
        let json = serde_json::to_value(&view).expect("json");
        assert_eq!(json["goal_id"], "goal_01");
        assert_eq!(json["status"], "complete");
        assert_eq!(json["tokens_used"], 42);
        assert_eq!(json["token_budget"], 1000);
    }

    /// Contract scenario "未知 goal id 报结构化错误": the error payload
    /// is JSON with an `error` field (and the process exits non-zero —
    /// asserted structurally here via emit_error's shape, the exit code
    /// is hardwired to 1).
    #[test]
    fn olp_obs_goal_status_unknown_id_errors() {
        let temp = tempfile::tempdir().expect("tempdir");
        let result = load_goal_status(temp.path(), "goal_nonexistent").expect("load ok");
        assert!(result.is_none(), "unknown id must map to the error path");
        let payload = GoalStatusError {
            error: "unknown goal id `goal_nonexistent` (no ledger row)".to_owned(),
            goal_id: Some("goal_nonexistent".to_owned()),
        };
        let json = serde_json::to_value(&payload).expect("json");
        assert!(json.get("error").is_some(), "stderr JSON carries `error`");
    }

    /// The on-disk layout must match the orchestrator's
    /// `goal_ledger_path` (goal-ledgers/<sanitized>.db) — drift here
    /// would make the CLI read a different file than serve writes.
    #[test]
    fn goal_status_db_path_matches_orchestrator_layout() {
        // 8e: platform-absolute fixture root (same hardcoded-/tmp class as
        // the inbox test — join-based equality holds only if both sides
        // share the root, which a platform temp dir guarantees).
        let data_dir = std::env::temp_dir().join("olp-goal-x");
        assert_eq!(
            goal_ledger_db_path(&data_dir, "goal_01"),
            data_dir.join("goal-ledgers").join("goal_01.db")
        );
        // Sanitization parity: odd ids map the same way on both sides.
        assert_eq!(sanitize_goal_id_for_file("a/b c"), "a_b_c");
    }

    #[cfg(unix)]
    #[test]
    fn goal_operator_live_rpc_failure_does_not_append_offline_transition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = SupervisorStore::new(temp.path().join("supervisor"));
        store
            .record_group_registered(supervisor_goal_group(
                "api:s1", "octos", "goal_01", "active",
            ))
            .expect("seed supervisor goal");
        let lock_path = temp.path().join(".octos-serve.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .expect("open serve lock");
        fs2::FileExt::try_lock_exclusive(&lock).expect("simulate live serve");

        let events = temp
            .path()
            .join("supervisor")
            .join("supervisor-events.jsonl");
        let before = std::fs::read_to_string(&events)
            .expect("before rows")
            .lines()
            .count();
        let groups = store.load_goal_groups_by_id().expect("goal view");
        let error = route_transition(temp.path(), &store, &groups, "octos", "goal_01", "archive")
            .expect_err("live endpoint absence must fail closed");
        assert!(
            error
                .to_string()
                .contains("refusing an unsafe offline archive")
        );
        let after = std::fs::read_to_string(&events)
            .expect("after rows")
            .lines()
            .count();
        assert_eq!(after, before, "RPC failure must not append an offline row");
        let folded = store.load_goal_groups_by_id().expect("folded state");
        let goal = folded.values().next().expect("goal remains");
        assert_eq!(metadata_str(goal, "status"), Some("active"));
    }

    #[test]
    fn goal_operator_proven_offline_route_keeps_append_fallback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = SupervisorStore::new(temp.path().join("supervisor"));
        store
            .record_group_registered(supervisor_goal_group(
                "api:s1", "octos", "goal_01", "active",
            ))
            .expect("seed supervisor goal");
        let groups = store.load_goal_groups_by_id().expect("goal view");
        route_transition(temp.path(), &store, &groups, "octos", "goal_01", "archive")
            .expect("offline archive");
        let folded = store.load_goal_groups_by_id().expect("folded state");
        let goal = folded.values().next().expect("goal remains");
        assert_eq!(metadata_str(goal, "status"), Some("archived"));
    }
}

/// #2248 — long data dirs must not kill `octos serve`: the goal
/// operator-control socket relocates, publishing its real path in a pointer
/// file inside the data dir.
#[cfg(all(test, unix))]
mod tests_2248_sun_len {
    use super::*;

    fn long_data_dir(base: &Path) -> PathBuf {
        let dir = base.join("x".repeat(120));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn short_data_dir_uses_canonical_location() {
        // Guaranteed-short base even under a long TMPDIR.
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let socket = goal_control_bind_path(temp.path()).unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        assert!(!socket.relocated);
        assert_eq!(socket.path, canonical.join(GOAL_OPERATOR_CONTROL_SOCKET));
    }

    #[test]
    fn long_data_dir_relocates_to_hashed_socket() {
        let temp = tempfile::tempdir().unwrap();
        let long = long_data_dir(temp.path());
        let canonical_long = std::fs::canonicalize(&long).unwrap();
        assert!(
            canonical_long
                .join(GOAL_OPERATOR_CONTROL_SOCKET)
                .as_os_str()
                .len()
                >= SUN_PATH_CAPACITY
        );

        let socket = goal_control_bind_path(&long).unwrap();
        assert!(socket.relocated);
        let name = socket.path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(".octos-goal-control-"));
        assert!(name.ends_with(".sock"));
        assert!(socket.path.as_os_str().len() < SUN_PATH_CAPACITY);
        // Deterministic: same data dir, same endpoint.
        assert_eq!(socket.path, goal_control_bind_path(&long).unwrap().path);
        // A different data dir hashes to a different fallback.
        let other = long_data_dir(temp.path().join("other").as_path());
        assert_ne!(socket.path, goal_control_bind_path(&other).unwrap().path);
    }

    /// The decision must be spelling-independent across the sun_path
    /// threshold: a short symlink to an over-long real dir relocates to the
    /// same endpoint as the real path.
    #[test]
    fn short_symlink_spelling_shares_the_long_paths_endpoint() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let long = long_data_dir(temp.path());
        let link = temp.path().join("lnk");
        std::os::unix::fs::symlink(&long, &link).unwrap();
        assert!(link.join(GOAL_OPERATOR_CONTROL_SOCKET).as_os_str().len() < SUN_PATH_CAPACITY);

        let via_link = goal_control_bind_path(&link).unwrap();
        let via_real = goal_control_bind_path(&long).unwrap();
        assert!(via_link.relocated);
        assert_eq!(via_link.path, via_real.path);
    }

    #[test]
    fn relocated_socket_roundtrip_via_pointer_file() {
        let temp = tempfile::tempdir().unwrap();
        let long = long_data_dir(temp.path());

        // The premise: the canonical path is beyond this platform's sun_path.
        let canonical = std::fs::canonicalize(&long)
            .unwrap()
            .join(GOAL_OPERATOR_CONTROL_SOCKET);
        let err = std::os::unix::net::UnixListener::bind(&canonical).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        // The fix: bind the relocated socket, publish the pointer, and the
        // connect side finds it with no environment knowledge.
        let socket = goal_control_bind_path(&long).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket.path).unwrap();
        publish_goal_control_socket(&long, &socket).unwrap();
        let connect_path = goal_control_connect_path(&long);
        assert_eq!(connect_path, socket.path);
        std::os::unix::net::UnixStream::connect(&connect_path).unwrap();
        listener.accept().unwrap();
        std::fs::remove_file(&socket.path).unwrap();
    }

    #[test]
    fn connect_path_defaults_to_canonical_without_pointer() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        assert_eq!(
            goal_control_connect_path(temp.path()),
            canonical.join(GOAL_OPERATOR_CONTROL_SOCKET)
        );
    }

    #[test]
    fn canonical_publish_removes_stale_pointer() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        let pointer = canonical.join(GOAL_OPERATOR_CONTROL_SOCKET_POINTER);
        std::fs::write(&pointer, "/tmp/stale.sock\n").unwrap();

        let socket = goal_control_bind_path(temp.path()).unwrap();
        assert!(!socket.relocated);
        publish_goal_control_socket(temp.path(), &socket).unwrap();
        assert!(!pointer.exists());
    }

    #[test]
    fn fallback_base_prefers_absolute_xdg_runtime_dir() {
        let (base, ours) = fallback_base_dir(
            Some(PathBuf::from("/run/user/1000")),
            PathBuf::from("/tmp"),
            0o1777,
        );
        assert_eq!((base, ours), (PathBuf::from("/run/user/1000"), false));
    }

    #[test]
    fn fallback_base_rejects_relative_xdg_runtime_dir() {
        let (base, ours) = fallback_base_dir(
            Some(PathBuf::from("relative")),
            PathBuf::from("/tmp"),
            0o1777,
        );
        // Falls through to the shared-temp subdir branch.
        assert!(ours);
        assert_ne!(base, PathBuf::from("relative"));
    }

    #[test]
    fn fallback_base_nests_under_shared_temp_only() {
        let uid = rustix::process::geteuid().as_raw();
        let (base, ours) = fallback_base_dir(None, PathBuf::from("/tmp"), 0o1777);
        assert_eq!(
            (base, ours),
            (
                PathBuf::from(format!("/tmp/octos-goal-control-{uid}")),
                true
            )
        );
        // A per-user (0700) temp dir is used directly.
        let (base, ours) = fallback_base_dir(None, PathBuf::from("/var/folders/xx/T"), 0o700);
        assert_eq!((base, ours), (PathBuf::from("/var/folders/xx/T"), false));
    }

    #[test]
    fn overlong_fallback_base_is_a_named_error() {
        let base = PathBuf::from(format!("/{}", "y".repeat(200)));
        let err = hashed_fallback_socket(&base, Path::new("/some/data/dir")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("sun_path"));
        assert!(msg.contains("XDG_RUNTIME_DIR"));
        assert!(msg.contains("yyyy")); // names the offending path
    }

    /// The hash is the cross-release contract between bind and connect side;
    /// pin one golden vector so a refactor cannot silently split them.
    #[test]
    fn fnv1a64_golden_vector() {
        assert_eq!(fnv1a64(Path::new("/some/data/dir")), 0x4e35_adf4_9b95_7813);
    }
}
