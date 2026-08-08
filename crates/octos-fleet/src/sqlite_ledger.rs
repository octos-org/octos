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
    /// Use this as the cursor for level-triggered queries instead of created_at_ms,
    /// which is not monotonic across processes and has same-millisecond races.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rowid: Option<i64>,
    pub finding_id: String,
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

        // CRITICAL: Enable foreign key constraints (OFF by default per-connection)
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // CRITICAL: Set busy timeout to handle SQLITE_BUSY under concurrent writes
        // Without this, two processes writing simultaneously cause immediate errors
        // instead of waiting for the lock to release.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS goals (
                goal_id TEXT PRIMARY KEY,
                objective TEXT NOT NULL,
                status TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                token_budget INTEGER NOT NULL,
                continuations_used INTEGER NOT NULL DEFAULT 0,
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
                created_at_ms INTEGER NOT NULL,
                created_by TEXT NOT NULL,
                FOREIGN KEY (goal_id) REFERENCES goals(goal_id),
                FOREIGN KEY (task_id) REFERENCES tasks(task_id)
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
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                goal.goal_id,
                goal.objective,
                goal.status,
                goal.tokens_used,
                goal.token_budget,
                goal.continuations_used,
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
            "SELECT goal_id, objective, status, tokens_used, token_budget, continuations_used, created_at_ms, updated_at_ms
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
                created_at_ms: row.get(6)?,
                updated_at_ms: row.get(7)?,
            })
        })?;
        Ok(rows.next().transpose()?)
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

    /// List all decisions for a goal.
    pub fn list_decisions(&self, goal_id: &str) -> Result<Vec<Decision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT decision_id, goal_id, task_id, question, options_considered, choice, rationale, based_on_findings, based_on_rev, decided_at_ms, decided_by
             FROM decisions WHERE goal_id = ?1 ORDER BY decided_at_ms ASC"
        )?;
        let decisions = stmt
            .query_map(params![goal_id], |row| {
                Ok(Decision {
                    decision_id: row.get(0)?,
                    goal_id: row.get(1)?,
                    task_id: row.get(2)?,
                    question: row.get(3)?,
                    options_considered: row.get(4)?,
                    choice: row.get(5)?,
                    rationale: row.get(6)?,
                    based_on_findings: row.get(7)?,
                    based_on_rev: row.get(8)?,
                    decided_at_ms: row.get(9)?,
                    decided_by: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(decisions)
    }

    /// Update goal status.
    pub fn update_goal_status(
        &self,
        goal_id: &str,
        status: &str,
        updated_at_ms: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE goals SET status = ?1, updated_at_ms = ?2 WHERE goal_id = ?3",
            params![status, updated_at_ms, goal_id],
        )?;
        Ok(())
    }

    /// Append a finding (with transaction for consistency).
    pub fn append_finding(&self, finding: &Finding) -> Result<i64> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO findings (finding_id, task_id, goal_id, kind, lifecycle, confidence, review_state, assertion, evidence, config_version, derived_from, created_at_ms, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                finding.finding_id,
                finding.task_id,
                finding.goal_id,
                finding.kind,
                finding.lifecycle,
                finding.confidence,
                finding.review_state,
                finding.assertion,
                finding.evidence,
                finding.config_version,
                finding.derived_from,
                finding.created_at_ms,
                finding.created_by,
            ],
        )?;

        let rowid = tx.last_insert_rowid();
        tx.commit()?;
        Ok(rowid)
    }

    /// List findings for a goal (level-triggered: only changes since `since_ms`).
    /// List findings for a goal (level-triggered: only changes since `since_rowid`).
    ///
    /// Uses SQLite's rowid (monotonic per insert) as the cursor, NOT created_at_ms.
    /// This avoids same-millisecond races and non-monotonic timestamps across processes.
    pub fn list_findings_since(&self, goal_id: &str, since_rowid: i64) -> Result<Vec<Finding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rowid, finding_id, task_id, goal_id, kind, lifecycle, confidence, review_state, assertion, evidence, config_version, derived_from, created_at_ms, created_by
             FROM findings WHERE goal_id = ?1 AND rowid > ?2 ORDER BY rowid ASC"
        )?;
        let findings = stmt
            .query_map(params![goal_id, since_rowid], |row| {
                Ok(Finding {
                    rowid: Some(row.get(0)?),
                    finding_id: row.get(1)?,
                    task_id: row.get(2)?,
                    goal_id: row.get(3)?,
                    kind: row.get(4)?,
                    lifecycle: row.get(5)?,
                    confidence: row.get(6)?,
                    review_state: row.get(7)?,
                    assertion: row.get(8)?,
                    evidence: row.get(9)?,
                    config_version: row.get(10)?,
                    derived_from: row.get(11)?,
                    created_at_ms: row.get(12)?,
                    created_by: row.get(13)?,
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
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        let finding = Finding {
            rowid: None, // Assigned by SQLite on insert
            finding_id: "f1".to_string(),
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
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&goal).unwrap();

        // Add findings at different times
        for i in 1..=5 {
            let finding = Finding {
                rowid: None, // Assigned by SQLite on insert
                finding_id: format!("f{}", i),
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
        // Verify rowid is monotonic
        assert_eq!(findings[0].rowid, Some(4));
        assert_eq!(findings[1].rowid, Some(5));
    }
}
