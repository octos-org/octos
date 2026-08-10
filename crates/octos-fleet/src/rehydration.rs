// Peer agent rehydration: recover peer memory from ledger after crash

use crate::sqlite_ledger::{Decision, Finding, Goal, GoalLedger, Task};
use eyre::Result;

/// Recovered peer memory from the durable ledger.
#[derive(Debug, Clone)]
pub struct PeerMemory {
    pub goal: Goal,
    pub task: Option<Task>,
    pub findings: Vec<Finding>,
    pub decisions: Vec<Decision>,
}

/// Rehydrate a peer agent from the ledger after a crash.
///
/// Reads the goal, task, findings, and decisions to reconstruct the peer's
/// working memory, allowing it to resume without losing progress.
pub fn rehydrate_peer(
    ledger: &GoalLedger,
    goal_id: &str,
    task_id: Option<&str>,
) -> Result<PeerMemory> {
    // Load goal
    let goal = ledger
        .get_goal(goal_id)?
        .ok_or_else(|| eyre::eyre!("goal {} not found", goal_id))?;

    // Load task (if assigned)
    let task = if let Some(tid) = task_id {
        ledger.get_task(tid)?
    } else {
        None
    };

    // Load findings for this goal/task
    let findings = ledger.list_findings_since(goal_id, 0)?;

    // Load decisions for this goal
    let decisions = ledger.list_decisions(goal_id)?;

    Ok(PeerMemory {
        goal,
        task,
        findings,
        decisions,
    })
}
