// SQLite-backed durable ledger for goals, tasks, findings, and escalations.
//
// Unlike redb (single-writer-single-process), SQLite supports multi-process
// access via WAL mode, making it suitable for the peer agent architecture
// where master/PM/peers run as independent processes sharing the same ledger.

use eyre::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct GoalLedger {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub goal_id: String,
    pub objective: String,
    pub status: String, // active | complete | blocked | budget_limited | paused
    pub tokens_used: u64,
    pub token_budget: u64,
    pub continuations_used: u32,
    /// Optimistic concurrency control: incremented on every update.
    /// Used for CAS (compare-and-swap) in update_goal_status.
    #[serde(default)]
    pub revision: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub goal_id: String,
    pub title: String,
    pub detail: String,
    pub status: String, // pending | running | complete | failed
    pub assigned_peer: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Monotonic row ID (SQLite rowid, assigned on insert, never changes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rowid: Option<i64>,
    /// Finding ID: "f-{seq}" format (assigned by store).
    pub finding_id: String,
    /// Monotonic sequence number (per-goal, assigned by store).
    pub seq: u64,
    pub task_id: Option<String>,
    pub goal_id: String,
    pub kind: String, // observation | hypothesis | diagnosis | constraint | experiment_result
    pub lifecycle: String, // proposed | observed | reproduced | verified | refuted | superseded
    pub confidence: String, // high | medium | low
    pub review_state: String, // unreviewed | peer_reviewed | independently_reproduced
    pub assertion: String,
    pub evidence: Option<String>, // JSON
    pub config_version: Option<String>,
    pub derived_from: Option<String>, // JSON array of finding_ids
    /// Findings this one overturns (by finding_id).
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// What it cost to learn (tokens). Feeds cost-against-yield per path.
    #[serde(default)]
    pub cost_tokens: u64,
    pub created_at_ms: u64,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub escalation_id: String,
    pub goal_id: String,
    pub task_id: Option<String>,
    pub peer_id: String,
    pub question: String,
    pub context: Option<String>, // JSON
    pub status: String,          // open | resolved
    pub default_action: Option<String>,
    pub default_after_secs: Option<i64>,
    pub created_at_ms: u64,
    pub resolved_at_ms: Option<u64>,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub decision_id: String,
    pub goal_id: String,
    pub task_id: Option<String>,
    pub question: String,
    pub options_considered: Option<String>, // JSON array
    pub choice: String,
    pub rationale: String,
    pub based_on_findings: Option<String>, // JSON array of finding_ids
    pub based_on_rev: i64,
    pub decided_at_ms: u64,
    pub decided_by: String,
}

impl GoalLedger {
    /// Open (or create) the SQLite ledger at `path`, creating all tables.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;

        // Enable WAL mode for multi-process access
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS goals (
                goal_id TEXT PRIMARY KEY,
                objective TEXT NOT NULL,
                status TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                token_budget INTEGER NOT NULL,
                continuations_used INTEGER NOT NULL DEFAULT 0,
                revision INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
                task_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                title TEXT NOT NULL,
                detail TEXT NOT NULL,
                status TEXT NOT NULL,
                assigned_peer TEXT,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL,
                FOREIGN KEY (goal_id) REFERENCES goals(goal_id)
            );

            CREATE TABLE IF NOT EXISTS findings (
                finding_id TEXT PRIMARY KEY,
                seq INTEGER NOT NULL,
                task_id TEXT,
                goal_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                lifecycle TEXT NOT NULL,
                confidence TEXT NOT NULL,
                review_state TEXT NOT NULL,
                assertion TEXT NOT NULL,
                evidence TEXT,
                config_version TEXT,
                derived_from TEXT,
                supersedes TEXT, -- JSON array
                cost_tokens INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL,
                created_by TEXT NOT NULL,
                FOREIGN KEY (goal_id) REFERENCES goals(goal_id),
                FOREIGN KEY (task_id) REFERENCES tasks(task_id),
                UNIQUE(goal_id, seq)
            );

            CREATE TABLE IF NOT EXISTS escalations (
                escalation_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                task_id TEXT,
                peer_id TEXT NOT NULL,
                question TEXT NOT NULL,
                context TEXT,
                status TEXT NOT NULL,
                default_action TEXT,
                default_after_secs INTEGER,
                created_at_ms INTEGER NOT NULL,
                resolved_at_ms INTEGER,
                resolved_by TEXT,
                resolution TEXT,
                FOREIGN KEY (goal_id) REFERENCES goals(goal_id),
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE TABLE IF NOT EXISTS decisions (
                decision_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                task_id TEXT,
                question TEXT NOT NULL,
                options_considered TEXT,
                choice TEXT NOT NULL,
                rationale TEXT NOT NULL,
                based_on_findings TEXT,
                based_on_rev INTEGER NOT NULL,
                decided_at_ms INTEGER NOT NULL,
                decided_by TEXT NOT NULL,
                FOREIGN KEY (goal_id) REFERENCES goals(goal_id),
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
            );

            CREATE INDEX IF NOT EXISTS idx_tasks_goal ON tasks(goal_id, status);
            CREATE INDEX IF NOT EXISTS idx_findings_goal ON findings(goal_id, created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_findings_task ON findings(task_id, created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_escalations_status ON escalations(status, created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_decisions_goal ON decisions(goal_id, decided_at_ms);
            ",
        )?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create a new goal.
    pub fn create_goal(&self, goal: &Goal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                goal.goal_id,
                goal.objective,
                goal.status,
                goal.tokens_used,
                goal.token_budget,
                goal.continuations_used,
                goal.revision,
                goal.created_at_ms,
                goal.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Get a goal by ID.
    pub fn get_goal(&self, goal_id: &str) -> Result<Option<Goal>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms
             FROM goals WHERE goal_id = ?1"
        )?;
        let mut rows = stmt.query_map(params![goal_id], |row| {
            Ok(Goal {
                goal_id: row.get(0)?,
                objective: row.get(1)?,
                status: row.get(2)?,
                tokens_used: row.get(3)?,
                token_budget: row.get(4)?,
                continuations_used: row.get(5)?,
                revision: row.get(6)?,
                created_at_ms: row.get(7)?,
                updated_at_ms: row.get(8)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Create a new task.
    pub fn create_task(&self, task: &Task) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                task.task_id,
                task.goal_id,
                task.title,
                task.detail,
                task.status,
                task.assigned_peer,
                task.created_at_ms,
                task.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Get a task by ID.
    pub fn get_task(&self, task_id: &str) -> Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms
             FROM tasks WHERE task_id = ?1"
        )?;
        let mut rows = stmt.query_map(params![task_id], |row| {
            Ok(Task {
                task_id: row.get(0)?,
                goal_id: row.get(1)?,
                title: row.get(2)?,
                detail: row.get(3)?,
                status: row.get(4)?,
                assigned_peer: row.get(5)?,
                created_at_ms: row.get(6)?,
                updated_at_ms: row.get(7)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Update goal status.
    /// Update goal status with optimistic concurrency control (CAS).
    ///
    /// Uses revision for compare-and-swap: only updates if the current revision
    /// matches expected_revision. Returns error if goal not found or revision mismatch
    /// (stale writer).
    pub fn update_goal_status(
        &self,
        goal_id: &str,
        status: &str,
        expected_revision: u64,
        updated_at_ms: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rows_affected = conn.execute(
            "UPDATE goals SET status = ?1, revision = revision + 1, updated_at_ms = ?2 WHERE goal_id = ?3 AND revision = ?4",
            params![status, updated_at_ms, goal_id, expected_revision],
        )?;

        if rows_affected == 0 {
            return Err(eyre::eyre!(
                "update_goal_status failed: goal {} not found or revision mismatch (expected {})",
                goal_id,
                expected_revision
            ));
        }

        Ok(())
    }

    /// Append a finding (with transaction for consistency).
    /// Append a finding to the ledger (with seq assignment and supersedes validation).
    ///
    /// This is a TRUE cross-table transaction:
    /// 1. Read max seq for this goal (from findings table)
    /// 2. Validate supersedes edges (from findings table)
    /// 3. Insert new finding (into findings table)
    /// All in one atomic transaction.
    /// Insert a finding with validation (shared by append_finding and commit_state_with_audit).
    ///
    /// This is the SINGLE validated insert path — all finding writes go through here
    /// to ensure cross-goal FK and supersedes validation are always enforced.
    fn insert_finding_validated(
        tx: &rusqlite::Transaction,
        finding: &Finding,
        goal_id: &str,
    ) -> Result<()> {
        // Step 1: Get next seq for this goal
        let max_seq: Option<u64> = tx
            .query_row(
                "SELECT MAX(seq) FROM findings WHERE goal_id = ?1",
                params![goal_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let next_seq = max_seq.unwrap_or(0) + 1;

        // Step 1.5: Validate task belongs to this goal (prevent cross-goal FK confusion)
        if let Some(ref task_id) = finding.task_id {
            let task_goal: Option<String> = tx
                .query_row(
                    "SELECT goal_id FROM tasks WHERE task_id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .ok();

            match task_goal {
                None => {
                    return Err(eyre::eyre!("task {} not found", task_id));
                }
                Some(tg) if tg != goal_id => {
                    return Err(eyre::eyre!(
                        "cross-goal FK violation: task {} belongs to goal {}, not goal {}",
                        task_id,
                        tg,
                        goal_id
                    ));
                }
                _ => {}
            }
        }

        // Step 2: Validate supersedes edges
        if !finding.supersedes.is_empty() {
            let mut stmt = tx.prepare(
                "SELECT finding_id FROM findings WHERE goal_id = ?1 AND finding_id = ?2",
            )?;
            for superseded_id in &finding.supersedes {
                let exists: Option<String> = stmt
                    .query_row(params![goal_id, superseded_id], |row| row.get(0))
                    .ok();
                if exists.is_none() {
                    return Err(eyre::eyre!(
                        "supersedes references unknown finding {} in goal {}",
                        superseded_id,
                        goal_id
                    ));
                }
            }
        }

        // Step 3: Insert new finding
        let supersedes_json = serde_json::to_string(&finding.supersedes)?;
        tx.execute(
            "INSERT INTO findings (finding_id, seq, task_id, goal_id, kind, lifecycle, confidence, review_state, assertion, evidence, config_version, derived_from, supersedes, cost_tokens, created_at_ms, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                finding.finding_id,
                next_seq,
                finding.task_id,
                goal_id,
                finding.kind,
                finding.lifecycle,
                finding.confidence,
                finding.review_state,
                finding.assertion,
                finding.evidence,
                finding.config_version,
                finding.derived_from,
                supersedes_json,
                finding.cost_tokens,
                finding.created_at_ms,
                finding.created_by,
            ],
        )?;

        Ok(())
    }

    pub fn append_finding(&self, finding: &Finding) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        Self::insert_finding_validated(&tx, finding, &finding.goal_id)?;

        let rowid = tx.last_insert_rowid();
        tx.commit()?;
        Ok(rowid)
    }

    /// Append an escalation to the ledger.
    pub fn append_escalation(&self, escalation: &Escalation) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Validate task belongs to this goal (prevent cross-goal FK confusion)
        if let Some(ref task_id) = escalation.task_id {
            let task_goal: Option<String> = tx
                .query_row(
                    "SELECT goal_id FROM tasks WHERE task_id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .ok();

            match task_goal {
                None => {
                    return Err(eyre::eyre!("task {} not found", task_id));
                }
                Some(tg) if tg != escalation.goal_id => {
                    return Err(eyre::eyre!(
                        "cross-goal FK violation: task {} belongs to goal {}, not goal {}",
                        task_id,
                        tg,
                        escalation.goal_id
                    ));
                }
                _ => {}
            }
        }

        tx.execute(
            "INSERT INTO escalations (escalation_id, goal_id, task_id, peer_id, question, context, status, default_action, default_after_secs, created_at_ms, resolved_at_ms, resolved_by, resolution)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                escalation.escalation_id,
                escalation.goal_id,
                escalation.task_id,
                escalation.peer_id,
                escalation.question,
                escalation.context,
                escalation.status,
                escalation.default_action,
                escalation.default_after_secs,
                escalation.created_at_ms,
                escalation.resolved_at_ms,
                escalation.resolved_by,
                escalation.resolution,
            ],
        )?;

        let rowid = tx.last_insert_rowid();
        tx.commit()?;
        Ok(rowid)
    }

    /// Append a decision to the ledger.
    pub fn append_decision(&self, decision: &Decision) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Validate task belongs to this goal (prevent cross-goal FK confusion)
        if let Some(ref task_id) = decision.task_id {
            let task_goal: Option<String> = tx
                .query_row(
                    "SELECT goal_id FROM tasks WHERE task_id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .ok();

            match task_goal {
                None => {
                    return Err(eyre::eyre!("task {} not found", task_id));
                }
                Some(tg) if tg != decision.goal_id => {
                    return Err(eyre::eyre!(
                        "cross-goal FK violation: task {} belongs to goal {}, not goal {}",
                        task_id,
                        tg,
                        decision.goal_id
                    ));
                }
                _ => {}
            }
        }

        tx.execute(
            "INSERT INTO decisions (decision_id, goal_id, task_id, question, options_considered, choice, rationale, based_on_findings, based_on_rev, decided_at_ms, decided_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                decision.decision_id,
                decision.goal_id,
                decision.task_id,
                decision.question,
                decision.options_considered,
                decision.choice,
                decision.rationale,
                decision.based_on_findings,
                decision.based_on_rev,
                decision.decided_at_ms,
                decision.decided_by,
            ],
        )?;

        let rowid = tx.last_insert_rowid();
        tx.commit()?;
        Ok(rowid)
    }

    /// Atomically commit state change + audit record (finding + decision).
    ///
    /// This is the TRUE cross-table transaction: state transition and audit log
    /// are committed together, or both roll back.
    pub fn commit_state_with_audit(
        &self,
        goal_id: &str,
        new_status: &str,
        expected_revision: u64,
        updated_at_ms: u64,
        finding: Option<&Finding>,
        decision: Option<&Decision>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Step 1: Update goal state (CAS)
        let rows_affected = tx.execute(
            "UPDATE goals SET status = ?1, revision = revision + 1, updated_at_ms = ?2 WHERE goal_id = ?3 AND revision = ?4",
            params![new_status, updated_at_ms, goal_id, expected_revision],
        )?;

        if rows_affected == 0 {
            return Err(eyre::eyre!(
                "commit_state_with_audit failed: goal {} not found or revision mismatch",
                goal_id
            ));
        }

        // Step 2: Append finding (if provided) — uses SHARED validated insert
        if let Some(f) = finding {
            Self::insert_finding_validated(&tx, f, goal_id)?;
        }

        // Step 3: Append decision (if provided)
        if let Some(d) = decision {
            tx.execute(
                "INSERT INTO decisions (decision_id, goal_id, task_id, question, options_considered, choice, rationale, based_on_findings, based_on_rev, decided_at_ms, decided_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    d.decision_id,
                    goal_id,
                    d.task_id,
                    d.question,
                    d.options_considered,
                    d.choice,
                    d.rationale,
                    d.based_on_findings,
                    d.based_on_rev,
                    d.decided_at_ms,
                    d.decided_by,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// List findings for a goal (level-triggered: only changes since `since_rowid`).
    ///
    /// Uses SQLite's rowid (monotonic per insert) as the cursor, NOT created_at_ms.
    pub fn list_findings_since(&self, goal_id: &str, since_rowid: i64) -> Result<Vec<Finding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rowid, finding_id, seq, task_id, goal_id, kind, lifecycle, confidence, review_state, assertion, evidence, config_version, derived_from, supersedes, cost_tokens, created_at_ms, created_by
             FROM findings WHERE goal_id = ?1 AND rowid > ?2 ORDER BY rowid ASC"
        )?;
        let findings = stmt
            .query_map(params![goal_id, since_rowid], |row| {
                let supersedes_json: Option<String> = row.get(13)?;
                let supersedes: Vec<String> = if let Some(json) = supersedes_json {
                    serde_json::from_str(&json).unwrap_or_default()
                } else {
                    Vec::new()
                };
                Ok(Finding {
                    rowid: Some(row.get(0)?),
                    finding_id: row.get(1)?,
                    seq: row.get(2)?,
                    task_id: row.get(3)?,
                    goal_id: row.get(4)?,
                    kind: row.get(5)?,
                    lifecycle: row.get(6)?,
                    confidence: row.get(7)?,
                    review_state: row.get(8)?,
                    assertion: row.get(9)?,
                    evidence: row.get(10)?,
                    config_version: row.get(11)?,
                    derived_from: row.get(12)?,
                    supersedes,
                    cost_tokens: row.get(14)?,
                    created_at_ms: row.get(15)?,
                    created_by: row.get(16)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_ledger_multi_process_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");

        // Process 1: create and write
        let ledger1 = GoalLedger::open(&path).unwrap();
        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test goal".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger1.create_goal(&goal).unwrap();

        // Process 2: open the same file (WAL mode allows this)
        let ledger2 = GoalLedger::open(&path).unwrap();
        let retrieved = ledger2.get_goal("g1").unwrap().unwrap();
        assert_eq!(retrieved.objective, "test goal");
        assert_eq!(retrieved.status, "active");
    }

    #[test]
    fn append_finding_with_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        let finding = Finding {
            rowid: None,
            finding_id: "f1".to_string(),
            seq: 1, // Will be overwritten by store
            task_id: None,
            goal_id: "g1".to_string(),
            kind: "observation".to_string(),
            lifecycle: "verified".to_string(),
            confidence: "high".to_string(),
            review_state: "peer_reviewed".to_string(),
            assertion: "test assertion".to_string(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };
        ledger.append_finding(&finding).unwrap();

        let findings = ledger.list_findings_since("g1", 0).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].assertion, "test assertion");
    }

    #[test]
    fn level_triggered_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        // Add findings at different times
        for i in 1..=5 {
            let finding = Finding {
                rowid: None,
                finding_id: format!("f{}", i),
                seq: i as u64, // Will be overwritten by store
                task_id: None,
                goal_id: "g1".to_string(),
                kind: "observation".to_string(),
                lifecycle: "verified".to_string(),
                confidence: "high".to_string(),
                review_state: "peer_reviewed".to_string(),
                assertion: format!("assertion {}", i),
                evidence: None,
                config_version: None,
                derived_from: None,
                supersedes: Vec::new(),
                cost_tokens: 0,
                created_at_ms: 1000 + i * 100,
                created_by: "peer-a".to_string(),
            };
            ledger.append_finding(&finding).unwrap();
        }

        // Level-triggered: only get findings since rowid=3 (f1-f3 have rowid 1-3)
        let findings = ledger.list_findings_since("g1", 3).unwrap();
        assert_eq!(findings.len(), 2); // f4 and f5
        assert_eq!(findings[0].finding_id, "f4");
        assert_eq!(findings[1].finding_id, "f5");
    }

    #[test]
    fn update_goal_status_with_cas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        // CAS success: correct revision
        ledger
            .update_goal_status("g1", "complete", 0, 2000)
            .unwrap();
        let updated = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(updated.status, "complete");
        assert_eq!(updated.revision, 1);

        // CAS failure: stale revision
        let result = ledger.update_goal_status("g1", "blocked", 0, 3000);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("revision mismatch")
        );

        // CAS failure: nonexistent goal
        let result = ledger.update_goal_status("g999", "complete", 0, 3000);
        assert!(result.is_err());
    }

    #[test]
    fn append_finding_rejects_cross_goal_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        // Create two goals
        for gid in &["g1", "g2"] {
            let goal = Goal {
                goal_id: gid.to_string(),
                objective: "test".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 10000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            };
            ledger.create_goal(&goal).unwrap();
        }

        // Create a task for g1
        let task = Task {
            task_id: "t1".to_string(),
            goal_id: "g1".to_string(),
            title: "test task".to_string(),
            detail: "test".to_string(),
            status: "pending".to_string(),
            assigned_peer: None,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_task(&task).unwrap();

        // Try to append a finding for g2 that references g1's task
        let finding = Finding {
            rowid: None,
            finding_id: "f1".to_string(),
            seq: 1,
            task_id: Some("t1".to_string()), // t1 belongs to g1, not g2
            goal_id: "g2".to_string(),
            kind: "observation".to_string(),
            lifecycle: "verified".to_string(),
            confidence: "high".to_string(),
            review_state: "peer_reviewed".to_string(),
            assertion: "test".to_string(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };
        let result = ledger.append_finding(&finding);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cross-goal FK violation")
        );
    }

    #[test]
    fn commit_state_with_audit_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        let finding = Finding {
            rowid: None,
            finding_id: "f1".to_string(),
            seq: 1,
            task_id: None,
            goal_id: "g1".to_string(),
            kind: "observation".to_string(),
            lifecycle: "verified".to_string(),
            confidence: "high".to_string(),
            review_state: "peer_reviewed".to_string(),
            assertion: "all tests pass".to_string(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };

        // Atomically: complete goal + append finding
        ledger
            .commit_state_with_audit("g1", "complete", 0, 3000, Some(&finding), None)
            .unwrap();

        // Verify goal is complete
        let goal = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(goal.status, "complete");
        assert_eq!(goal.revision, 1);

        // Verify finding was committed
        let findings = ledger.list_findings_since("g1", 0).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].assertion, "all tests pass");
    }

    #[test]
    fn commit_state_with_audit_rolls_back_on_cas_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 5, // Start at revision 5
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        let finding = Finding {
            rowid: None,
            finding_id: "f1".to_string(),
            seq: 1,
            task_id: None,
            goal_id: "g1".to_string(),
            kind: "observation".to_string(),
            lifecycle: "verified".to_string(),
            confidence: "high".to_string(),
            review_state: "peer_reviewed".to_string(),
            assertion: "test".to_string(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };

        // CAS failure: expected revision 0 but actual is 5
        let result = ledger.commit_state_with_audit(
            "g1",
            "complete",
            0, // Wrong revision
            3000,
            Some(&finding),
            None,
        );
        assert!(result.is_err());

        // Verify NOTHING was committed (atomic rollback)
        let goal = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(goal.status, "active"); // Not changed
        assert_eq!(goal.revision, 5); // Not incremented

        let findings = ledger.list_findings_since("g1", 0).unwrap();
        assert_eq!(findings.len(), 0); // Finding not committed
    }

    #[test]
    fn commit_state_with_audit_rolls_back_on_mid_transaction_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        // Create a task for a DIFFERENT goal (will cause cross-goal FK violation)
        let other_goal = Goal {
            goal_id: "g2".to_string(),
            objective: "other".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&other_goal).unwrap();

        let task = Task {
            task_id: "t1".to_string(),
            goal_id: "g2".to_string(), // Task belongs to g2, not g1
            title: "test".to_string(),
            detail: "test".to_string(),
            status: "pending".to_string(),
            assigned_peer: None,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_task(&task).unwrap();

        // Finding references g2's task, but we're committing to g1 (cross-goal FK violation)
        let finding = Finding {
            rowid: None,
            finding_id: "f1".to_string(),
            seq: 1,
            task_id: Some("t1".to_string()), // t1 belongs to g2, not g1
            goal_id: "g1".to_string(),
            kind: "observation".to_string(),
            lifecycle: "verified".to_string(),
            confidence: "high".to_string(),
            review_state: "peer_reviewed".to_string(),
            assertion: "test".to_string(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };

        // Goal update succeeds (CAS passes), but finding insert fails (cross-goal FK)
        let result = ledger.commit_state_with_audit(
            "g1",
            "complete",
            0, // Correct revision
            3000,
            Some(&finding),
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cross-goal FK violation")
        );

        // Verify atomic rollback: goal NOT updated, finding NOT committed
        let goal = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(goal.status, "active"); // NOT changed to complete
        assert_eq!(goal.revision, 0); // NOT incremented

        let findings = ledger.list_findings_since("g1", 0).unwrap();
        assert_eq!(findings.len(), 0); // Finding NOT committed
    }

    #[test]
    fn finding_converts_to_records_finding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        let finding = Finding {
            rowid: None,
            finding_id: "f1".to_string(),
            seq: 1,
            task_id: None,
            goal_id: "g1".to_string(),
            kind: "observation".to_string(),
            lifecycle: "verified".to_string(),
            confidence: "high".to_string(),
            review_state: "peer_reviewed".to_string(),
            assertion: "test claim".to_string(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };
        ledger.append_finding(&finding).unwrap();

        let findings = ledger.list_findings_since("g1", 0).unwrap();
        assert_eq!(findings.len(), 1);

        // Convert to records::Finding (the type digest uses)
        let records_finding: crate::records::Finding = (&findings[0]).into();
        assert_eq!(records_finding.id, "f1");
        assert_eq!(records_finding.claim, "test claim");
        assert_eq!(records_finding.fleet_id, "g1");
        assert_eq!(records_finding.seq, 1);
        assert_eq!(
            records_finding.status,
            crate::records::FindingStatus::Confirmed
        );
    }
}

/// Convenience helper: read findings from the ledger and compute a digest.
///
/// This wires the digest to the SQLite ledger end-to-end:
/// 1. Read findings from ledger (sqlite_ledger::Finding)
/// 2. Convert to records::Finding (the type digest consumes)
/// 3. Compute digest
pub fn digest_from_ledger(
    ledger: &GoalLedger,
    goal_id: &str,
    opts: &crate::digest::DigestOptions,
) -> Result<crate::digest::Digest> {
    let findings = ledger.list_findings_since(goal_id, 0)?;
    let records_findings: Vec<crate::records::Finding> = findings.iter().map(Into::into).collect();
    Ok(crate::digest::digest(&records_findings, opts))
}

// Conversion from sqlite_ledger::Finding to records::Finding (the canonical type digest uses).
// This allows digest() to consume findings from the SQLite ledger.
impl From<&Finding> for crate::records::Finding {
    fn from(f: &Finding) -> Self {
        Self {
            schema_version: crate::records::SCHEMA_VERSION,
            id: f.finding_id.clone(),
            seq: f.seq,
            fleet_id: f.goal_id.clone(),
            task_id: f.task_id.clone(),
            claim: f.assertion.clone(),
            status: match f.lifecycle.as_str() {
                "verified" => crate::records::FindingStatus::Confirmed,
                "proposed" | "observed" => crate::records::FindingStatus::Predicted,
                "refuted" => crate::records::FindingStatus::RuledOut,
                _ => crate::records::FindingStatus::Predicted,
            },
            component: f.kind.clone(),
            evidence: f
                .evidence
                .as_ref()
                .and_then(|e| serde_json::from_str(e).ok())
                .unwrap_or_default(),
            config: f
                .config_version
                .as_ref()
                .and_then(|c| serde_json::from_str(c).ok())
                .unwrap_or_default(),
            supersedes: f.supersedes.clone(),
            cost_tokens: f.cost_tokens,
            by: f.created_by.clone(),
            at_ms: f.created_at_ms,
            kind: Some(f.kind.clone()),
            lifecycle: Some(f.lifecycle.clone()),
            confidence: Some(f.confidence.clone()),
            review_state: Some(f.review_state.clone()),
            rowid: f.rowid,
            derived_from: f.derived_from.clone(),
        }
    }
}

#[cfg(test)]
mod digest_integration_tests {
    use super::*;

    #[test]
    fn digest_from_ledger_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();

        // Create goal
        let goal = Goal {
            goal_id: "g1".to_string(),
            objective: "test digest".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        // Add findings
        for i in 1..=5 {
            // Create task first (required by FK validation)
            let task = Task {
                task_id: format!("task-{}", i),
                goal_id: "g1".to_string(),
                title: format!("task {}", i),
                detail: "test".to_string(),
                status: "pending".to_string(),
                assigned_peer: None,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            };
            ledger.create_task(&task).unwrap();

            let finding = Finding {
                rowid: None,
                finding_id: format!("f{}", i),
                seq: i as u64,
                task_id: Some(format!("task-{}", i)),
                goal_id: "g1".to_string(),
                kind: "observation".to_string(),
                lifecycle: "verified".to_string(),
                confidence: "high".to_string(),
                review_state: "peer_reviewed".to_string(),
                assertion: format!("claim {}", i),
                evidence: None,
                config_version: None,
                derived_from: None,
                supersedes: Vec::new(),
                cost_tokens: 100 * i as u64,
                created_at_ms: 1000 + i * 100,
                created_by: "peer-a".to_string(),
            };
            ledger.append_finding(&finding).unwrap();
        }

        // Compute digest from ledger (end-to-end test)
        let digest = digest_from_ledger(
            &ledger,
            "g1",
            &crate::digest::DigestOptions {
                max_chars: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();

        // Verify digest contains all findings
        assert_eq!(digest.new_findings.len(), 5);
        assert_eq!(digest.new_findings[0].seq, 1);
        assert_eq!(digest.new_findings[4].seq, 5);

        // Verify cost tracking works (cost_tokens is not zero)
        let total_cost: u64 = digest.cost_by_path.iter().map(|p| p.tokens).sum();
        assert!(
            total_cost > 0,
            "cost_tokens must be tracked, not hardcoded to 0"
        );
    }
}
