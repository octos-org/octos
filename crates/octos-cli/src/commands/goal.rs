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
}

/// One goal row located in the supervisor event stream: the session key the
/// goal is stored under, plus the full persisted group record (whose metadata
/// bag carries every goal field).
#[derive(Debug)]
struct LocatedGoal {
    session_id: String,
    group: SupervisedGroupRecord,
}

impl Executable for GoalCommand {
    fn execute(self) -> Result<()> {
        let data_dir = super::resolve_data_dir(self.data_dir)?;
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
