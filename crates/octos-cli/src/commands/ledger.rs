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
        // 整改要求 2: never silently empty — report the resolved path.
        return Err(eyre::eyre!(
            "no goal ledger at {} (resolved data root: {})",
            db_path.display(),
            data_dir.display()
        ));
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
                // 整改: shared per-instance profile data root (see goal.rs).
                let data_dir = super::obs::resolve_profile_data_root(
                    &super::resolve_data_dir(None)?,
                    &std::env::current_dir()?,
                    super::obs::DEFAULT_PROFILE_ID,
                );
                let data_dir = args.data_dir.clone().unwrap_or(data_dir);
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
