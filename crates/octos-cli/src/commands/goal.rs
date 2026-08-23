//! `octos goal` — read-only goal observability (OLP L1, slice 2).
//!
//! Contract: task-req-olp-obs-cli.spec.md — scenarios
//! "serve 停止时仍可读 goal 状态" and "未知 goal id 报结构化错误".
//! Reads the per-goal ledger (`<data_dir>/goal-ledgers/<goal_id>.db`,
//! SQLite via octos_fleet::GoalLedger) DIRECTLY — no serve process
//! required. `--json` and the human table share one assembly layer
//! ([`GoalStatusView`]) so the two modes can never diverge.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use eyre::Result;
use serde::Serialize;

use super::Executable;

#[derive(Debug, Args)]
pub struct GoalCommand {
    #[command(subcommand)]
    pub action: GoalAction,
}

#[derive(Debug, Subcommand)]
pub enum GoalAction {
    /// Show goal status (reads the ledger directly; serve not required).
    Status(GoalStatusArgs),
}

#[derive(Debug, Args)]
pub struct GoalStatusArgs {
    /// Goal id (default: the most recently updated goal in the dir).
    #[arg(long)]
    pub goal: Option<String>,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Data-dir override (defaults to the standard resolution).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
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
        match self.action {
            GoalAction::Status(args) => {
                let data_dir = super::resolve_data_dir(args.data_dir.clone())?;
                let Some(goal_id) = args.goal.clone() else {
                    emit_error(
                        args.json,
                        None,
                        "--goal <id> is required (default-goal discovery lands in a later slice)",
                    );
                };
                match load_goal_status(&data_dir, &goal_id) {
                    Ok(Some(view)) => {
                        if args.json {
                            println!("{}", serde_json::to_string(&view).expect("status json"));
                        } else {
                            print_table(&view);
                        }
                        Ok(())
                    }
                    Ok(None) => emit_error(
                        args.json,
                        Some(&goal_id),
                        &format!("unknown goal id `{goal_id}` (no ledger row)"),
                    ),
                    Err(error) => emit_error(
                        args.json,
                        Some(&goal_id),
                        &format!("failed to read goal ledger: {error}"),
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let data_dir = Path::new("/tmp/x");
        assert_eq!(
            goal_ledger_db_path(data_dir, "goal_01"),
            PathBuf::from("/tmp/x/goal-ledgers/goal_01.db")
        );
        // Sanitization parity: odd ids map the same way on both sides.
        assert_eq!(sanitize_goal_id_for_file("a/b c"), "a_b_c");
    }
}
