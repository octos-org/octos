//! `octos ledger tail` — read-only goal-ledger tail (OLP L1, slice 4).
//!
//! Contract: task-req-olp-obs-cli.spec.md — "findings/escalations/
//! decisions 尾读" + scenario "ledger tail 对空账本输出空数组".
//! Reads `<data_dir>/goal-ledgers/<goal_id>.db` directly (no serve).
//! `--json` and the human table share one assembly layer.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use eyre::Result;
use serde::Serialize;

use super::Executable;

#[derive(Debug, Args)]
pub struct LedgerCommand {
    #[command(subcommand)]
    pub action: LedgerAction,
}

#[derive(Debug, Subcommand)]
pub enum LedgerAction {
    /// Tail a goal ledger's findings/escalations/decisions.
    Tail(LedgerTailArgs),
}

#[derive(Debug, Args)]
pub struct LedgerTailArgs {
    /// Goal id whose ledger to tail.
    #[arg(value_name = "GOAL_ID")]
    pub goal_id: String,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Max entries per kind (newest last). Default 20.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Data-dir override (defaults to the standard resolution).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

/// One assembled tail. Field names are part of the machine contract.
#[derive(Debug, Serialize)]
pub(crate) struct LedgerTailView {
    pub goal_id: String,
    pub findings: Vec<octos_fleet::Finding>,
    pub escalations: Vec<octos_fleet::Escalation>,
    pub decisions: Vec<octos_fleet::Decision>,
}

fn goal_ledger_db_path(data_dir: &Path, goal_id: &str) -> PathBuf {
    // Same layout as `commands::goal` (goal-ledgers/<sanitized>.db).
    let sanitized: String = goal_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir
        .join("goal-ledgers")
        .join(format!("{sanitized}.db"))
}

fn tail_vec<T>(mut items: Vec<T>, limit: usize) -> Vec<T> {
    if items.len() > limit {
        items.drain(..items.len() - limit);
    }
    items
}

/// Assemble the tail straight from the ledger file. An absent ledger file
/// yields EMPTY collections (contract: 空账本输出空数组, exit 0) — the
/// goal may simply have no findings yet.
pub(crate) fn load_ledger_tail(
    data_dir: &Path,
    goal_id: &str,
    limit: usize,
) -> Result<LedgerTailView> {
    let db_path = goal_ledger_db_path(data_dir, goal_id);
    if !db_path.exists() {
        return Ok(LedgerTailView {
            goal_id: goal_id.to_owned(),
            findings: Vec::new(),
            escalations: Vec::new(),
            decisions: Vec::new(),
        });
    }
    let ledger = octos_fleet::GoalLedger::open(&db_path)?;
    let findings = tail_vec(ledger.list_findings_since(goal_id, 0)?, limit);
    let escalations = tail_vec(ledger.list_open_escalations(goal_id)?, limit);
    let decisions = tail_vec(ledger.list_decisions(goal_id)?, limit);
    Ok(LedgerTailView {
        goal_id: goal_id.to_owned(),
        findings,
        escalations,
        decisions,
    })
}

fn print_table(view: &LedgerTailView) {
    println!("goal: {}", view.goal_id);
    println!("findings ({}):", view.findings.len());
    for f in &view.findings {
        println!(
            "  [{}] {} ({}/{})",
            f.finding_id, f.assertion, f.kind, f.lifecycle
        );
    }
    println!("escalations ({}):", view.escalations.len());
    for e in &view.escalations {
        println!("  [{}] {} [{}]", e.escalation_id, e.question, e.status);
    }
    println!("decisions ({}):", view.decisions.len());
    for d in &view.decisions {
        println!("  [{}] {}", d.decision_id, d.question);
    }
}

impl Executable for LedgerCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            LedgerAction::Tail(args) => {
                let data_dir = super::resolve_data_dir(args.data_dir.clone())?;
                let view = load_ledger_tail(&data_dir, &args.goal_id, args.limit)?;
                if args.json {
                    println!("{}", serde_json::to_string(&view).expect("tail json"));
                } else {
                    print_table(&view);
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_ledger(data_dir: &Path, goal_id: &str, with_finding: bool) {
        let db_path = goal_ledger_db_path(data_dir, goal_id);
        std::fs::create_dir_all(db_path.parent().expect("parent")).expect("mkdir");
        let ledger = octos_fleet::GoalLedger::open(&db_path).expect("open");
        ledger
            .upsert_goal(&octos_fleet::Goal {
                goal_id: goal_id.to_owned(),
                objective: "fixture".to_owned(),
                status: "active".to_owned(),
                tokens_used: 0,
                token_budget: 0,
                time_used_seconds: 0,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .expect("seed goal");
        if with_finding {
            ledger
                .append_finding(&octos_fleet::Finding {
                    rowid: None,
                    finding_id: "f-1".to_owned(),
                    seq: 1,
                    task_id: None,
                    goal_id: goal_id.to_owned(),
                    kind: "observation".to_owned(),
                    lifecycle: "observed".to_owned(),
                    confidence: "high".to_owned(),
                    review_state: "unreviewed".to_owned(),
                    assertion: "the writer is innocent".to_owned(),
                    evidence: None,
                    config_version: None,
                    derived_from: None,
                    supersedes: Vec::new(),
                    cost_tokens: 0,
                    created_at_ms: 2,
                    created_by: "test".to_owned(),
                })
                .expect("append finding");
        }
    }

    /// Contract scenario "ledger tail 对空账本输出空数组": a goal whose
    /// ledger has no findings must produce a valid JSON payload whose
    /// collections are empty, exit-0 shape (no error path).
    #[test]
    fn olp_obs_ledger_tail_empty_goal() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_ledger(temp.path(), "goal_empty", false);
        let view = load_ledger_tail(temp.path(), "goal_empty", 20).expect("tail");
        assert!(view.findings.is_empty());
        assert!(view.escalations.is_empty());
        assert!(view.decisions.is_empty());
        let json = serde_json::to_value(&view).expect("json");
        assert_eq!(json["findings"], serde_json::json!([]));
        assert_eq!(json["escalations"], serde_json::json!([]));
        assert_eq!(json["decisions"], serde_json::json!([]));
    }

    /// Absent ledger FILE (goal never seen) is also an empty tail, not an
    /// error — the contract's 空数组 shape covers both.
    #[test]
    fn olp_obs_ledger_tail_absent_file_is_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let view = load_ledger_tail(temp.path(), "goal_never", 20).expect("tail");
        assert!(view.findings.is_empty());
    }

    /// Findings recorded in the ledger show up in the tail.
    #[test]
    fn olp_obs_ledger_tail_reads_findings() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_ledger(temp.path(), "goal_f", true);
        let view = load_ledger_tail(temp.path(), "goal_f", 20).expect("tail");
        assert_eq!(view.findings.len(), 1);
        assert_eq!(view.findings[0].assertion, "the writer is innocent");
    }
}
