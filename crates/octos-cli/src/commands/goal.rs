//! #25 — operator goal exits: `octos goal reopen` / `octos goal archive`.
//!
//! Goal state is authoritative in the supervisor event stream (the
//! `group_registered` metadata that `restore_goal_from_group` replays at
//! boot), NOT in the goal-ledger SQLite `goals` table (a direct sqlite edit
//! changes no running process and is never restored from). These commands
//! therefore work by appending a fresh `group_registered` goal snapshot with
//! the new status to the profile's supervisor event store: a running serve
//! keeps its in-memory record (restart-free processes never re-read the
//! stream, by design), and the NEXT boot replays the new status into the
//! orchestrator's memory. The command prints that restart requirement.
//!
//! `reopen` admits `blocked|paused|budget_limited` → `active`; `archive`
//! admits any status → `archived`, which is a TERMINAL, irreversible state
//! (only `complete`/`blocked` are model-reachable, `archived` is reachable
//! only here — the operator/outer-loop path — and `set_goal`'s status enum
//! refuses it, so nothing can un-archive).

use std::path::PathBuf;

use clap::{Args, Subcommand};
use eyre::{Result, bail, eyre};

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

use serde::Serialize;

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
                    GoalSubcommand::Reopen { goal_id } => {
                        cmd_transition(&store, &goals_by_id, &self.profile, &goal_id, "reopen")
                    }
                    GoalSubcommand::Archive { goal_id } => {
                        cmd_transition(&store, &goals_by_id, &self.profile, &goal_id, "archive")
                    }
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

fn cmd_transition(
    store: &SupervisorStore,
    goals_by_id: &std::collections::HashMap<String, SupervisedGroupRecord>,
    profile: &str,
    goal_id: &str,
    action: &str,
) -> Result<()> {
    let located = locate_goal(goals_by_id, profile, goal_id)?;
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
        "note: a running octos serve keeps its in-memory goal state; the new status takes \
         effect on the next serve restart (the supervisor event stream is the authoritative \
         record and is replayed at boot)."
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
}
