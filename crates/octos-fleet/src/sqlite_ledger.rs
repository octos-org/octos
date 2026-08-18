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
    // `cleared` (#1973 fix B) is the terminal a user `goal_clear` stamps; the
    // upsert guard still refuses to downgrade a `complete` row to it.
    pub status: String, // active | complete | blocked | budget_limited | paused | cleared
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

/// #1967 — the one column list every escalation SELECT shares; order is the
/// contract [`GoalLedger::escalation_from_row`] maps by index.
const ESCALATION_SELECT_COLUMNS: &str = "escalation_id, goal_id, task_id, peer_id, question, \
     context, status, default_action, default_after_secs, created_at_ms, resolved_at_ms, \
     resolved_by, resolution";

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

/// #1865 review FIX 1 — whether an error from a ledger op is SQLITE_BUSY /
/// SQLITE_LOCKED-class lock contention (worth a brief retry) as opposed to a
/// structural failure (missing parent dir, corrupt file, permissions) that
/// retrying can never fix. Lives here — not in callers — because only this
/// crate sees `rusqlite` and can classify by the REAL error code instead of
/// string-matching messages.
pub fn error_is_lock_contention(err: &eyre::Report) -> bool {
    let Some(sqlite_err) = err.downcast_ref::<rusqlite::Error>() else {
        return false;
    };
    matches!(
        sqlite_err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

impl GoalLedger {
    /// Open (or create) the SQLite ledger at `path`, creating all tables.
    ///
    /// DEFAULT (pre-#1865) profile — review round 3: NO explicit
    /// `busy_timeout`, which means rusqlite's own default applies (rusqlite
    /// installs `sqlite3_busy_timeout(db, 5000)` on every connection — see
    /// rusqlite `inner_connection.rs`). That is BYTE-EQUIVALENT to what every
    /// pre-existing inline caller (the finding / escalation writers and
    /// `goal_get`'s serial ledger reads, all on tokio worker tasks) has
    /// always run with: handler-covered contention waits up to ~5s, while the
    /// fresh-db concurrent-init race fails instantly through a
    /// handler-bypassing path (the historically observed `database is
    /// locked`). Do NOT add an explicit timeout here in either direction —
    /// shorter would newly skip best-effort writes that today wait and
    /// succeed; the transition sync that must SURVIVE the init race uses
    /// [`Self::open_with_busy_retry`] from a blocking thread instead.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(path.as_ref(), None)
    }

    /// #1865 review FIX 1 — BOUNDED-RETRY profile of [`Self::open`], for the
    /// goal-transition ledger sync ONLY (octos-cli runs it inside
    /// `spawn_blocking`, never on an executor worker). Two connections
    /// initializing the SAME fresh WAL db can fail `database is locked`
    /// through a path the busy handler does NOT cover (observed empirically
    /// on concurrent FIRST initialization — the wal-index/shm recovery lock;
    /// the journal-mode switch itself IS handler-covered, see the contention
    /// test), so a one-shot open can lose a millisecond init race outright —
    /// and that loss silently drops the audit row.
    ///
    /// Retries at most 3 attempts, ONLY when [`error_is_lock_contention`]
    /// classifies the failure as BUSY/LOCKED (structural errors return
    /// immediately), with 50ms between attempts — and THIS profile's
    /// connection overrides the busy_timeout DOWN to 1s. Honest bound math:
    /// busy_timeout is PER lock acquisition, not per call — one attempt runs
    /// a handful of locking ops (journal-mode pragma, schema batch), so a
    /// pathological attempt can block a small multiple of 1s, and the 3-try
    /// cap keeps the PRACTICAL worst case in single-digit seconds. That is a
    /// blocking-pool budget, not a wall-clock guarantee.
    pub fn open_with_busy_retry(path: impl AsRef<Path>) -> Result<Self> {
        const ATTEMPTS: usize = 3;
        let path = path.as_ref();
        let mut last_err: Option<eyre::Report> = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            match Self::open_inner(path, Some(std::time::Duration::from_secs(1))) {
                Ok(ledger) => return Ok(ledger),
                Err(err) if error_is_lock_contention(&err) && attempt + 1 < ATTEMPTS => {
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_err.unwrap_or_else(|| eyre::eyre!("ledger open retry exhausted")))
    }

    /// The shared open core: connection, optional busy_timeout OVERRIDE (the
    /// ONLY difference between the two public profiles above; `None` keeps
    /// rusqlite's 5s default), WAL + synchronous pragmas, schema. An override
    /// persists for the connection's lifetime, so a retry-profile ledger caps
    /// each later row-write wait at 1s too — fine, because that profile never
    /// runs on an executor worker.
    fn open_inner(path: &Path, busy_timeout: Option<std::time::Duration>) -> Result<Self> {
        let conn = Connection::open(path)?;

        if let Some(timeout) = busy_timeout {
            conn.busy_timeout(timeout)?;
        }

        // Enable WAL mode for multi-process access
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        Self::create_tables(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// The shared `CREATE TABLE IF NOT EXISTS` schema batch (split out of
    /// [`Self::open`] so the retry wrapper stays a thin loop over it).
    fn create_tables(conn: &Connection) -> Result<()> {
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
                -- #2055 review round 3: provenance mark for the ONE admitted
                -- terminal-to-terminal transition. 1 = this `failed` row is an
                -- OBSERVER-provisional verdict the owner may still correct to
                -- `complete` (task_supervisor.rs:2527); 0 = final (owner
                -- failure, cancellation, or any non-failed status).
                correctable INTEGER NOT NULL DEFAULT 0,
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
        // #2055 review round 3 — best-effort migration for a tasks table
        // created before `correctable` existed (`CREATE TABLE IF NOT EXISTS`
        // never alters an existing table). The duplicate-column error on an
        // already-migrated database is expected and swallowed; any other
        // failure is also non-fatal here because the first statement that
        // actually touches the column surfaces it loudly.
        let _ = conn.execute(
            "ALTER TABLE tasks ADD COLUMN correctable INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(())
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

    /// #1957 — upsert the goal row with its REAL fields. `create_goal` was the
    /// only writer and callers used a placeholder (objective empty, status
    /// stale, tokens 0) purely to satisfy the findings/escalations FK, so an
    /// audit of the ledger saw no real goal state. This writes the authoritative
    /// objective/status/tokens/budget on first use AND keeps them fresh on every
    /// later call. `created_at_ms` and `revision` are preserved on conflict.
    ///
    /// #1957 codex #2 — the UPDATE is GUARDED so a stale snapshot cannot undo a
    /// newer one. Three clauses:
    ///  1. `updated_at_ms >=`: only overwrite when the incoming snapshot is at
    ///     least as new as the stored row. Both producers (the transition sync
    ///     and the finding/escalation path) capture `status` and `updated_at_ms`
    ///     together under the orchestrator state lock, so a stale status always
    ///     carries a stale timestamp and this clause rejects it. `>=` (not `>`)
    ///     keeps same-instant re-writes of the SAME state idempotent.
    ///  2. `tokens_used >=` (#1965 codex round): the counter is MONOTONIC per
    ///     `goal_id` — every in-memory writer only ever `saturating_add`s, a
    ///     replacement goal mints a FRESH goal_id (different ledger file), and
    ///     re-activation never resets counters. Two peers finishing in the
    ///     SAME millisecond tie on clause 1, so without this clause the
    ///     smaller charge upserting second would roll the durable counter
    ///     backwards while memory holds the higher total.
    ///  3. never downgrade a `complete` row to a non-`complete` status. This is
    ///     defence-in-depth against the millisecond-resolution tie the `>=`
    ///     clause alone cannot break: `complete` is terminal for a given
    ///     `goal_id` (re-activation mints a FRESH goal_id), so no legitimate
    ///     writer ever moves an existing `complete` row back to active/blocked.
    ///     `blocked` is deliberately NOT protected — a blocked goal is
    ///     user-resumable to `active` under the same id.
    ///
    /// #1973 fix-round — returns whether the write was ADMITTED (`true`: the
    /// row was inserted, or the guarded update fired), via SQLite's
    /// rows-changed count. A guarded rejection returns `false`, so an
    /// administrative STATUS sync (park/clear) whose snapshot carries stale
    /// lower counters can detect the loss and retry once with max'd monotonic
    /// fields — instead of silently leaving the row on its old status while a
    /// decision row claims the transition happened.
    pub fn upsert_goal(&self, goal: &Goal) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let admitted = conn.execute(
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(goal_id) DO UPDATE SET
                 objective = excluded.objective,
                 status = excluded.status,
                 tokens_used = excluded.tokens_used,
                 token_budget = excluded.token_budget,
                 continuations_used = excluded.continuations_used,
                 updated_at_ms = excluded.updated_at_ms
             WHERE excluded.updated_at_ms >= goals.updated_at_ms
               AND excluded.tokens_used >= goals.tokens_used
               AND NOT (goals.status = 'complete' AND excluded.status <> 'complete')",
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
        Ok(admitted > 0)
    }

    /// #2055 review round 2 — create a goals row only when none exists yet
    /// (`INSERT … ON CONFLICT(goal_id) DO NOTHING`), the goals-side twin of
    /// [`Self::create_task_if_absent`].
    ///
    /// For FK-parent seeding by the task-row registration recorder, which
    /// must NEVER update a goals row: [`Self::upsert_goal`]'s monotonic
    /// guard admits equal `updated_at_ms` (millisecond resolution), so a
    /// delayed stale `active` snapshot could overwrite a same-millisecond
    /// `paused`/`blocked`/`budget_limited`/`cleared` row and regress its
    /// counters. This can't: an existing row — whatever its state — is
    /// preserved byte-for-byte. Returns `Ok(true)` when a row was inserted,
    /// `Ok(false)` for the preserve-existing no-op.
    pub fn create_goal_if_absent(&self, goal: &Goal) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(goal_id) DO NOTHING",
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
        Ok(inserted > 0)
    }

    /// #1973 fix-round 3/4 — targeted STATUS compare-and-swap for an
    /// administrative transition (park/clear) whose guarded [`Self::upsert_goal`]
    /// was rejected by the monotonic-token clause. Flips ONLY the status (+
    /// `updated_at_ms`); the row's counters are authoritative and untouched.
    ///
    /// A TRUE CAS (round-4 codex): the predicate requires the row to still
    /// carry the EXACT `(expected_status, expected_updated_at_ms)` pair the
    /// caller just read — so ANY interleaved write, including a same-status
    /// counters refresh with a newer timestamp, changes the pair and defeats
    /// the CAS (a status-only predicate admitted that interleave and could
    /// stamp an older `updated_at_ms` over a newer row: a clock REGRESSION
    /// that then let a delayed older transition pass the caller's ordering
    /// gate). `status <> 'complete'` stays as belt-and-suspenders (complete
    /// is terminal per goal_id; the caller never reads `complete` as its
    /// expectation anyway). Returns whether a row changed — the caller's
    /// decision-append gate; a defeated CAS is NOT re-attempted (one shot —
    /// the newer state wins).
    ///
    /// Clock invariant: the caller gates the CAS on
    /// `snapshot.updated_at_ms >= expected_updated_at_ms` (the row value it
    /// read), and the CAS fires only while the row STILL carries exactly that
    /// value — so the stamp written here is always `>=` the timestamp it
    /// replaces. `updated_at_ms` never regresses. The one interleave the pair
    /// cannot detect — an equal-timestamp, same-status write (counters-only,
    /// same millisecond) — is safe by construction: the CAS writes only
    /// status + timestamp, so the interleaved counters survive untouched.
    pub fn cas_goal_status(
        &self,
        goal_id: &str,
        new_status: &str,
        expected_status: &str,
        expected_updated_at_ms: u64,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE goals SET status = ?1, updated_at_ms = ?2
             WHERE goal_id = ?3 AND status = ?4 AND updated_at_ms = ?5
               AND status <> 'complete'",
            params![
                new_status,
                updated_at_ms,
                goal_id,
                expected_status,
                expected_updated_at_ms
            ],
        )?;
        Ok(changed > 0)
    }

    /// #1973 fix-round — number of decision rows recorded for `goal_id`. The
    /// audit-side counterpart to [`Self::append_decision`]: the transition
    /// sync appends a decision ONLY when the goals-row actually reflects the
    /// transition, and this reader lets callers (and tests) verify the two
    /// never diverge.
    pub fn count_decisions(&self, goal_id: &str) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM decisions WHERE goal_id = ?1",
            params![goal_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
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

    /// #2055 — create a task row only when none exists yet
    /// (`INSERT … ON CONFLICT(task_id) DO NOTHING`).
    ///
    /// Registration is the caller: the same task can be registered,
    /// relaunched, and re-registered across a restart, and findings /
    /// denials / escalations still write FK stubs for tasks that may already
    /// have a row. All of those must PRESERVE an existing row — its status
    /// (including a status that already went terminal via
    /// [`Self::update_task_status`]), title, and timestamps — rather than
    /// error or overwrite. Returns `Ok(true)` when a row was inserted,
    /// `Ok(false)` for the preserve-existing no-op.
    pub fn create_task_if_absent(&self, task: &Task) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let inserted = conn.execute(
            "INSERT INTO tasks (task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(task_id) DO NOTHING",
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
        Ok(inserted > 0)
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

    /// #2054 — settle a task's status. This is the writer whose absence left
    /// every row frozen on the `"running"` FK stub it was inserted with, so
    /// `task_status_counts` could only ever answer `{"running": N}` and a goal
    /// could never observe its own work finishing.
    ///
    /// Returns `Ok(true)` when the row actually moved, `Ok(false)` for an
    /// accepted no-op. A no-op is NOT an error: the caller is
    /// `TaskSupervisor`'s terminal sink, which legitimately fires for tasks
    /// that have no ledger row (never bound to a goal) and can fire more than
    /// once for one task, because the strangler wiring keeps the legacy
    /// `on_change` / `on_failure` callbacks alive alongside `on_terminal`.
    ///
    /// The guard is deliberately a first-terminal-wins rule rather than a
    /// caller-supplied CAS pair like [`Self::cas_goal_status`]. The terminal
    /// sink does not know what the ledger currently holds — it knows only what
    /// just happened to the task — so demanding an `expected_status` would
    /// force a read-then-write race at exactly the layer that must stay
    /// fire-and-forget. Encoding the rule in the `WHERE` clause makes
    /// redelivery and out-of-order delivery both harmless in one statement:
    ///
    /// - absent row            → 0 rows → `false`
    /// - `running` → terminal  → 1 row  → `true`
    /// - terminal → same       → guard excludes → `false` (redelivery)
    /// - terminal → `running`  → guard excludes → `false` (late refresh)
    /// - correctable `failed` → `complete` → 1 row → `true` (correction)
    /// - any other terminal → terminal → guard excludes → `false`
    ///
    /// #2055 review round 3 — the ONE admitted terminal→terminal transition
    /// is PROVENANCE-GATED: `failed → complete` requires the row to carry
    /// `correctable = 1`, which only
    /// [`Self::update_task_status_with_provenance`] can stamp and only for a
    /// `failed` write. That mirrors the supervisor's authority model —
    /// `TaskSupervisor::mark_completed` overrides exactly an
    /// OBSERVER-derived provisional failure (task_supervisor.rs:2527) — and
    /// keeps every FINAL `failed` row (owner failure, cancellation) closed:
    /// blanket failed→complete admission would let a genuinely-cancelled
    /// row be flipped by a racing completion from another supervisor copy
    /// (#2060). The correction clears the mark; `complete` is immutable;
    /// `updated_at_ms` records the latest admitted write and cannot regress
    /// to a straggler.
    ///
    /// Fidelity note: cancellation is collapsed into failure upstream
    /// (`TerminalOutcome` has no `Cancelled`; the change-feed settle maps
    /// `TaskStatus::Cancelled` to `"failed"` too), so a cancelled task lands
    /// here as `"failed"` and the two are indistinguishable in the ledger —
    /// distinguishable only through the provenance mark.
    ///
    /// This plain form writes owner-final provenance (`correctable = 0`).
    pub fn update_task_status(
        &self,
        task_id: &str,
        new_status: &str,
        updated_at_ms: u64,
    ) -> Result<bool> {
        self.update_task_status_with_provenance(task_id, new_status, false, updated_at_ms)
    }

    /// #2055 review round 3 — [`Self::update_task_status`] with explicit
    /// correction provenance. `correctable = true` is meaningful only for a
    /// `"failed"` write (the observer-provisional verdict); it is clamped to
    /// `0` for every other status, so a completion can never re-open a row.
    ///
    /// Besides the correction itself, a correctable `failed` row also admits
    /// an owner-final `failed` DOWNGRADE (`correctable 1 → 0`, status
    /// unchanged): when the owner re-marks an observer-failed task, the
    /// supervisor clears the provisional stamp, and the row must follow —
    /// otherwise the correction window would stay open forever on a failure
    /// the owner already confirmed. A provisional redelivery (`1 → 1`) stays
    /// a no-op.
    pub fn update_task_status_with_provenance(
        &self,
        task_id: &str,
        new_status: &str,
        correctable: bool,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let stamp_correctable = i64::from(correctable && new_status == "failed");
        let changed = conn.execute(
            "UPDATE tasks SET status = ?1, updated_at_ms = ?2, correctable = ?4
             WHERE task_id = ?3
               AND (status NOT IN ('complete', 'failed')
                    OR (status = 'failed' AND correctable = 1 AND ?1 = 'complete')
                    OR (status = 'failed' AND correctable = 1 AND ?1 = 'failed' AND ?4 = 0))",
            params![new_status, updated_at_ms, task_id, stamp_correctable],
        )?;
        Ok(changed > 0)
    }

    /// #2055 review round 3 — every task row bound to `goal_id`, oldest
    /// first. Read-side companion to [`Self::task_status_counts`] for
    /// callers that need titles/status rather than aggregates.
    pub fn tasks_for_goal(&self, goal_id: &str) -> Result<Vec<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms
             FROM tasks WHERE goal_id = ?1 ORDER BY created_at_ms, task_id",
        )?;
        let rows = stmt.query_map(params![goal_id], |row| {
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
        let mut tasks = Vec::new();
        for task in rows {
            tasks.push(task?);
        }
        Ok(tasks)
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

    /// Insert a finding with validation — the SINGLE validated write path,
    /// shared by `append_finding` and `commit_state_with_audit` so the two can
    /// never drift. All steps run in the caller-provided transaction `tx`:
    ///
    /// 1. cross-goal FK — the finding's `task_id` must belong to `goal_id`
    /// 2. supersedes edges must reference existing findings in this goal
    /// 3. seq assignment (max+1 for the goal) + insert
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

    /// #1961 — mark this peer's OPEN escalation resolved. `append_escalation`
    /// only ever wrote the `open` row; without this the ledger's escalation
    /// history showed every answered escalation as perpetually open. A peer
    /// parks one prompt at a time (depth-1), so matching `peer_id` + the
    /// `open` status resolves exactly the escalation just answered. Returns the
    /// number of rows updated (0 when there was no open escalation — e.g. an
    /// approval on a goal-less peer, or a double-resolve).
    ///
    /// #1967 — this bulk-by-peer form stays the right call for the ANSWER and
    /// CLOSE paths ("everything open for this peer" IS the one escalation the
    /// depth-1 peer parked on / abandoned). The timeout sweep instead addresses
    /// individual rows via [`Self::resolve_escalation_by_id`].
    pub fn resolve_escalation(
        &self,
        peer_id: &str,
        resolution: &str,
        resolved_by: &str,
        resolved_at_ms: i64,
    ) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE escalations
             SET status = 'resolved', resolution = ?1, resolved_by = ?2, resolved_at_ms = ?3
             WHERE peer_id = ?4 AND status = 'open'",
            params![resolution, resolved_by, resolved_at_ms, peer_id],
        )?;
        Ok(updated)
    }

    /// #1967 — shared row → [`Escalation`] mapper for the SELECT paths below.
    /// Column order must match [`ESCALATION_SELECT_COLUMNS`].
    fn escalation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Escalation> {
        Ok(Escalation {
            escalation_id: row.get(0)?,
            goal_id: row.get(1)?,
            task_id: row.get(2)?,
            peer_id: row.get(3)?,
            question: row.get(4)?,
            context: row.get(5)?,
            status: row.get(6)?,
            default_action: row.get(7)?,
            default_after_secs: row.get(8)?,
            created_at_ms: row.get(9)?,
            resolved_at_ms: row.get(10)?,
            resolved_by: row.get(11)?,
            resolution: row.get(12)?,
        })
    }

    /// #1967 — the READ half of the escalation lifecycle. Producers wrote rows
    /// (`append_escalation`) and the answer path flipped them
    /// (`resolve_escalation`), but no production SELECT existed — an open
    /// escalation was invisible to the master model. `goal_get` folds these in
    /// via `model_goal_ledger_open_escalations`; oldest first so the master
    /// answers in park order.
    pub fn list_open_escalations(&self, goal_id: &str) -> Result<Vec<Escalation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ESCALATION_SELECT_COLUMNS} FROM escalations
             WHERE goal_id = ?1 AND status = 'open'
             ORDER BY created_at_ms ASC"
        ))?;
        let rows = stmt
            .query_map(params![goal_id], Self::escalation_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// #1967 — TARGETED resolve: flip ONE escalation by primary key, provided
    /// it is still `open`. Returns whether a row flipped (`false` = unknown id
    /// or already resolved — a recorded resolution is never clobbered). Used
    /// by the timeout sweep, which addresses each expired row individually;
    /// the answer/close paths use the peer_id-bulk
    /// [`Self::resolve_escalation`] instead (see its doc).
    pub fn resolve_escalation_by_id(
        &self,
        escalation_id: &str,
        resolution: &str,
        resolved_by: &str,
    ) -> Result<bool> {
        let resolved_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE escalations
             SET status = 'resolved', resolution = ?1, resolved_by = ?2, resolved_at_ms = ?3
             WHERE escalation_id = ?4 AND status = 'open'",
            params![resolution, resolved_by, resolved_at_ms, escalation_id],
        )?;
        Ok(updated > 0)
    }

    /// #1967 — timeout candidates for the escalation sweep: OPEN rows whose
    /// `default_after_secs` timer has ELAPSED (`created_at_ms +
    /// default_after_secs*1000 < now_ms`, strict). Deliberately NO goal
    /// filter: the sweep addresses one per-goal ledger FILE, and the filename
    /// is a lossy `sanitize_filename_for_ledger` mapping so the goal_id cannot
    /// be recovered from the path — the rows carry it. Rows without a default
    /// never qualify (the master must answer them via peer_respond, or
    /// peer_close resolves them).
    pub fn list_expired_open_escalations(&self, now_ms: i64) -> Result<Vec<Escalation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ESCALATION_SELECT_COLUMNS} FROM escalations
             WHERE status = 'open' AND default_after_secs IS NOT NULL
               AND created_at_ms + default_after_secs * 1000 < ?1
             ORDER BY created_at_ms ASC"
        ))?;
        let rows = stmt
            .query_map(params![now_ms], Self::escalation_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
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

    /// #1964 — all decisions recorded for `goal_id`, ordered by
    /// `decided_at_ms` (ties by insertion rowid). The read half of
    /// [`Self::append_decision`]: fleet-driven goal terminals now append an
    /// audit decision (#1865 eager convergence / deny), and their tests assert
    /// exactly-one-per-transition through this.
    pub fn list_decisions(&self, goal_id: &str) -> Result<Vec<Decision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT decision_id, goal_id, task_id, question, options_considered, choice, \
             rationale, based_on_findings, based_on_rev, decided_at_ms, decided_by \
             FROM decisions WHERE goal_id = ?1 ORDER BY decided_at_ms ASC, rowid ASC",
        )?;
        let rows = stmt
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
        Ok(rows)
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

    /// #1945 — count this goal's tasks per status (pending/running/complete/
    /// failed), sorted by status for a stable shape. The tasks half of the
    /// `goal_get` `ledger_digest`: the re-orienting master reads counts, never
    /// rows, so this stays fixed-size however large the plan grows. NOTE:
    /// today's only production task writer is the FK stub in
    /// `model_goal_record_peer_finding` (octos-cli), which inserts `running`
    /// rows and never updates them — so production counts read
    /// `{"running": N}` until a real task-status writer lands.
    pub fn task_status_counts(&self, goal_id: &str) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM tasks WHERE goal_id = ?1 GROUP BY status ORDER BY status",
        )?;
        let counts = stmt
            .query_map(params![goal_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(counts)
    }

    /// #1945 codex round — count this goal's findings per raw `lifecycle`
    /// (proposed/observed/…), sorted by lifecycle. A direct GROUP BY over ALL
    /// findings — INCLUDING `task_id IS NULL` rows, which the digest's
    /// per-path roll-up skips and which are exactly what ordinary peers write
    /// (peer_handoff stages no fleet task).
    pub fn findings_count_by_lifecycle(&self, goal_id: &str) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT lifecycle, COUNT(*) FROM findings WHERE goal_id = ?1 \
             GROUP BY lifecycle ORDER BY lifecycle",
        )?;
        let counts = stmt
            .query_map(params![goal_id], |row| {
                let n: i64 = row.get(1)?;
                Ok((row.get(0)?, u64::try_from(n).unwrap_or(0)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(counts)
    }

    /// #1945 codex round — count this goal's findings per `kind`
    /// (observation/hypothesis/…), sorted by kind. Same ALL-rows semantics as
    /// [`Self::findings_count_by_lifecycle`].
    pub fn findings_count_by_kind(&self, goal_id: &str) -> Result<Vec<(String, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT kind, COUNT(*) FROM findings WHERE goal_id = ?1 \
             GROUP BY kind ORDER BY kind",
        )?;
        let counts = stmt
            .query_map(params![goal_id], |row| {
                let n: i64 = row.get(1)?;
                Ok((row.get(0)?, u64::try_from(n).unwrap_or(0)))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(counts)
    }

    /// #1945 codex round — the goal's total learning cost:
    /// `COALESCE(SUM(cost_tokens), 0)` over ALL findings, task-scoped or not
    /// (the digest's `cost_by_path` drops `task_id IS NULL` rows — the ones
    /// the #1965 cost lane populates). Saturating `i64 → u64`, no unchecked
    /// casts; an empty goal sums to 0, not NULL.
    pub fn total_cost_tokens(&self, goal_id: &str) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_tokens), 0) FROM findings WHERE goal_id = ?1",
            params![goal_id],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(total).unwrap_or(0))
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
            // #1965 — a REAL cost, so the round-trip below proves the row
            // persists the caller's spend instead of a hardcoded 0.
            cost_tokens: 4_321,
            created_at_ms: 2000,
            created_by: "peer-a".to_string(),
        };
        ledger.append_finding(&finding).unwrap();

        let findings = ledger.list_findings_since("g1", 0).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].assertion, "test assertion");
        assert_eq!(
            findings[0].cost_tokens, 4_321,
            "Finding row must persist the real cost_tokens (#1965)"
        );
    }

    /// #1865 review round 3 — the TWO open profiles under handler-covered
    /// contention (a fresh, still-DELETE-mode db whose lock is held by a raw
    /// connection, so `open`'s `journal_mode=WAL` switch must wait): plain
    /// `open` keeps rusqlite's DEFAULT 5s busy handler — byte-equivalent to
    /// what every pre-existing inline caller has always run with (NOT our 1s
    /// override, NOT zero) — while `open_with_busy_retry` overrides each lock
    /// acquisition DOWN to 1s and retries lock-class failures a bounded
    /// number of times. NOTE an already-WAL, already-created ledger does not
    /// contend on open at all (pragma + `CREATE TABLE IF NOT EXISTS` resolve
    /// without the write lock), so the fresh-db path is the one pinned here.
    /// Deliberately slow (~8s of real lock-waiting): this is the regression
    /// pin for the blocker where an explicit timeout on `open` changed every
    /// inline caller's blocking profile.
    #[test]
    fn open_keeps_rusqlite_default_busy_handling_while_retry_profile_bounds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch("BEGIN EXCLUSIVE;").unwrap();

        // DEFAULT profile: waits rusqlite's built-in ~5s
        // (`sqlite3_busy_timeout(db, 5000)` in rusqlite inner_connection.rs)
        // and then surfaces a busy-class error. `>= 2s` pins that no explicit
        // SHORTER override (like the retry profile's 1s — or a fail-fast 0)
        // was reintroduced on this path.
        let started = std::time::Instant::now();
        let err = GoalLedger::open(&path)
            .err()
            .expect("write-locked db must fail a plain open once the default handler expires");
        assert!(
            error_is_lock_contention(&err),
            "the default-profile failure must be busy-class: {err}",
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(2)
                && elapsed < std::time::Duration::from_secs(15),
            "plain open must keep rusqlite's default (~5s) busy handling, \
             byte-equivalent to pre-#1865 inline callers: took {elapsed:?}",
        );

        // BOUNDED-RETRY profile: the 1s per-acquisition override makes each
        // attempt wait ~1s on the held journal-mode switch; 3 attempts + 50ms
        // sleeps ≈ 3.1s here, then the same busy-class error surfaces.
        let started = std::time::Instant::now();
        let err = GoalLedger::open_with_busy_retry(&path)
            .err()
            .expect("still locked: retries must exhaust");
        assert!(
            error_is_lock_contention(&err),
            "busy-class after retries: {err}",
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_secs(1)
                && elapsed < std::time::Duration::from_secs(10),
            "the retry profile must wait ~1s per acquisition, bounded by the \
             3-attempt cap: took {elapsed:?}",
        );
    }

    /// #1865 review FIX 1 — `open_with_busy_retry` (a) succeeds like `open` on
    /// a healthy path, and (b) classifies ONLY SQLITE_BUSY/LOCKED-class errors
    /// as retryable: a structural failure (opening a DIRECTORY) must return
    /// its error immediately, with no retry sleeps burned on it.
    #[test]
    fn open_with_busy_retry_round_trips_and_refuses_non_busy_errors() {
        // (a) healthy open behaves like `open` (WAL, tables created).
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open_with_busy_retry(dir.path().join("ledger.db")).unwrap();
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "retry open".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 1_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .unwrap();
        assert!(ledger.get_goal("g1").unwrap().is_some());

        // (b) a non-busy error is NOT retried: opening a directory fails
        // structurally; with 2 inter-attempt sleeps it would take >=100ms, so
        // a fast error proves the busy-only classification short-circuited.
        let started = std::time::Instant::now();
        assert!(GoalLedger::open_with_busy_retry(dir.path()).is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "a structural (non-busy) open error must fail fast, not retry: took {:?}",
            started.elapsed(),
        );

        // Classifier unit coverage: busy/locked codes retry, others do not.
        let busy = eyre::Report::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        ));
        let locked = eyre::Report::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            Some("database table is locked".into()),
        ));
        let structural = eyre::Report::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some("unable to open database file".into()),
        ));
        assert!(error_is_lock_contention(&busy));
        assert!(error_is_lock_contention(&locked));
        assert!(!error_is_lock_contention(&structural));
    }

    /// #1964 — `list_decisions` round-trips appended decisions for ONE goal in
    /// decided_at order. The fleet-convergence tests (octos-cli) use it to
    /// assert an eager fleet terminal appended exactly one audit decision.
    #[test]
    fn list_decisions_round_trips_per_goal_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        for gid in ["g1", "g2"] {
            ledger
                .create_goal(&Goal {
                    goal_id: gid.to_string(),
                    objective: "test".to_string(),
                    status: "active".to_string(),
                    tokens_used: 0,
                    token_budget: 10_000,
                    continuations_used: 0,
                    revision: 0,
                    created_at_ms: 1_000,
                    updated_at_ms: 1_000,
                })
                .unwrap();
        }
        let decision = |id: &str, goal: &str, at: u64| Decision {
            decision_id: id.to_string(),
            goal_id: goal.to_string(),
            task_id: None,
            question: format!("q-{id}"),
            options_considered: None,
            choice: "complete".to_string(),
            rationale: "all fleet tasks accepted".to_string(),
            based_on_findings: None,
            based_on_rev: 0,
            decided_at_ms: at,
            decided_by: "keeper".to_string(),
        };
        // Append out of decided_at order + one row on ANOTHER goal.
        ledger
            .append_decision(&decision("d2", "g1", 2_000))
            .unwrap();
        ledger
            .append_decision(&decision("d1", "g1", 1_500))
            .unwrap();
        ledger
            .append_decision(&decision("dx", "g2", 1_700))
            .unwrap();

        let rows = ledger.list_decisions("g1").unwrap();
        assert_eq!(
            rows.iter()
                .map(|d| d.decision_id.as_str())
                .collect::<Vec<_>>(),
            vec!["d1", "d2"],
            "g1's decisions only, ordered by decided_at_ms",
        );
        assert_eq!(rows[0].choice, "complete");
        assert_eq!(rows[0].rationale, "all fleet tasks accepted");
        assert_eq!(rows[0].decided_by, "keeper");
        assert!(ledger.list_decisions("missing").unwrap().is_empty());
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
                seq: i, // Will be overwritten by store
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
                // Verified states
                "verified" => crate::records::FindingStatus::Confirmed,
                "reproduced" => crate::records::FindingStatus::Confirmed, // Stronger than verified
                // Predicted states
                "proposed" | "observed" => crate::records::FindingStatus::Predicted,
                // Ruled out states
                "refuted" => crate::records::FindingStatus::RuledOut,
                "superseded" | "retracted" => crate::records::FindingStatus::RuledOut,
                // Unknown lifecycle → Predicted (conservative default)
                _ => {
                    tracing::warn!(
                        lifecycle = %f.lifecycle,
                        "unknown lifecycle in From conversion, defaulting to Predicted"
                    );
                    crate::records::FindingStatus::Predicted
                }
            },
            component: f.kind.clone(),
            evidence: {
                let parsed = f
                    .evidence
                    .as_ref()
                    .and_then(|e| serde_json::from_str(e).ok());
                if f.evidence.is_some() && parsed.is_none() {
                    tracing::warn!(
                        finding_id = %f.finding_id,
                        "evidence JSON parse failed in From conversion, using empty vec"
                    );
                }
                parsed.unwrap_or_default()
            },
            config: {
                let parsed = f
                    .config_version
                    .as_ref()
                    .and_then(|c| serde_json::from_str(c).ok());
                if f.config_version.is_some() && parsed.is_none() {
                    tracing::warn!(
                        finding_id = %f.finding_id,
                        "config_version JSON parse failed in From conversion, using empty map"
                    );
                }
                parsed.unwrap_or_default()
            },
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
                seq: i,
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
                cost_tokens: 100 * i,
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

    #[test]
    fn lifecycle_to_status_mapping_complete() {
        let test_cases = vec![
            ("verified", crate::records::FindingStatus::Confirmed),
            ("reproduced", crate::records::FindingStatus::Confirmed),
            ("proposed", crate::records::FindingStatus::Predicted),
            ("observed", crate::records::FindingStatus::Predicted),
            ("refuted", crate::records::FindingStatus::RuledOut),
            ("superseded", crate::records::FindingStatus::RuledOut),
            ("retracted", crate::records::FindingStatus::RuledOut),
            ("unknown_state", crate::records::FindingStatus::Predicted), // Fallback
        ];

        for (lifecycle, expected_status) in test_cases {
            let finding = Finding {
                rowid: None,
                finding_id: "f1".to_string(),
                seq: 1,
                task_id: None,
                goal_id: "g1".to_string(),
                kind: "observation".to_string(),
                lifecycle: lifecycle.to_string(),
                confidence: "high".to_string(),
                review_state: "peer_reviewed".to_string(),
                assertion: "test".to_string(),
                evidence: None,
                config_version: None,
                derived_from: None,
                supersedes: Vec::new(),
                cost_tokens: 0,
                created_at_ms: 1000,
                created_by: "peer-a".to_string(),
            };

            let records_finding: crate::records::Finding = (&finding).into();
            assert_eq!(
                records_finding.status, expected_status,
                "lifecycle '{}' should map to {:?}",
                lifecycle, expected_status
            );
        }
    }

    // #1945 — the goal_get `ledger_digest` read path reduces the digest to
    // COUNTS; this proves the counts it derives are right over a ledger with
    // mixed lifecycles, a supersession, and per-path cost. The lifecycle →
    // FindingStatus mapping is the `From<&Finding>` conversion above:
    // verified/reproduced → Confirmed, proposed/observed → Predicted,
    // refuted/superseded/retracted → RuledOut.
    #[test]
    fn digest_from_ledger_counts_mixed_lifecycles_and_supersession() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "count me".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 10000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        for (task_id, status) in [("t-run", "running"), ("t-done", "complete")] {
            ledger
                .create_task(&Task {
                    task_id: task_id.to_string(),
                    goal_id: "g1".to_string(),
                    title: task_id.to_string(),
                    detail: "test".to_string(),
                    status: status.to_string(),
                    assigned_peer: None,
                    created_at_ms: 1000,
                    updated_at_ms: 1000,
                })
                .unwrap();
        }
        // (finding_id, task, kind, lifecycle, cost, supersedes)
        type SeedRow = (
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            u64,
            Vec<&'static str>,
        );
        let rows: [SeedRow; 4] = [
            ("f1", "t-run", "hypothesis", "observed", 100, vec![]),
            ("f2", "t-run", "observation", "verified", 200, vec![]),
            ("f3", "t-done", "observation", "refuted", 300, vec![]),
            // f4 overturns f1: f1 leaves the live frontier, f4 joins it.
            ("f4", "t-done", "diagnosis", "verified", 400, vec!["f1"]),
        ];
        for (finding_id, task_id, kind, lifecycle, cost, supersedes) in rows {
            ledger
                .append_finding(&Finding {
                    rowid: None,
                    finding_id: finding_id.to_string(),
                    seq: 0, // assigned by store
                    task_id: Some(task_id.to_string()),
                    goal_id: "g1".to_string(),
                    kind: kind.to_string(),
                    lifecycle: lifecycle.to_string(),
                    confidence: "medium".to_string(),
                    review_state: "unreviewed".to_string(),
                    assertion: format!("claim {finding_id}"),
                    evidence: None,
                    config_version: None,
                    derived_from: None,
                    supersedes: supersedes.into_iter().map(str::to_string).collect(),
                    cost_tokens: cost,
                    created_at_ms: 2000,
                    created_by: "peer-a".to_string(),
                })
                .unwrap();
        }

        let digest = digest_from_ledger(
            &ledger,
            "g1",
            &crate::digest::DigestOptions {
                max_chars: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();

        // f1 was overturned → 3 live findings, 1 overturn edge, watermark = 4.
        assert_eq!(digest.new_findings.len(), 3, "superseded f1 is not live");
        assert!(digest.new_findings.iter().all(|f| f.id != "f1"));
        assert_eq!(digest.overturns.len(), 1);
        assert_eq!(digest.overturns[0].overturned, "f1");
        assert_eq!(digest.watermark, 4);
        let confirmed = digest
            .new_findings
            .iter()
            .filter(|f| f.status == crate::records::FindingStatus::Confirmed)
            .count();
        let ruled_out = digest
            .new_findings
            .iter()
            .filter(|f| f.status == crate::records::FindingStatus::RuledOut)
            .count();
        assert_eq!((confirmed, ruled_out), (2, 1), "verified×2 + refuted×1");
        // `component` carries the ledger `kind` (see `From<&Finding>`), so the
        // read path can count findings per kind straight off the digest.
        let observations = digest
            .new_findings
            .iter()
            .filter(|f| f.component == "observation")
            .count();
        assert_eq!(observations, 2);
        // Cost rolls up per path; the total is what `ledger_digest` reports.
        let total: u64 = digest.cost_by_path.iter().map(|p| p.tokens).sum();
        assert_eq!(total, 1000, "all four findings' cost, live or not");
    }

    // #1945 — task counts by status: the tasks half of the goal_get
    // `ledger_digest`. A read this small must not require listing rows.
    #[test]
    fn task_status_counts_groups_this_goals_tasks_only() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        for gid in ["g1", "g2"] {
            ledger
                .create_goal(&Goal {
                    goal_id: gid.to_string(),
                    objective: "test".to_string(),
                    status: "active".to_string(),
                    tokens_used: 0,
                    token_budget: 10000,
                    continuations_used: 0,
                    revision: 0,
                    created_at_ms: 1000,
                    updated_at_ms: 1000,
                })
                .unwrap();
        }
        for (task_id, goal_id, status) in [
            ("t1", "g1", "running"),
            ("t2", "g1", "running"),
            ("t3", "g1", "complete"),
            ("t4", "g2", "failed"), // foreign goal — must not be counted
        ] {
            ledger
                .create_task(&Task {
                    task_id: task_id.to_string(),
                    goal_id: goal_id.to_string(),
                    title: task_id.to_string(),
                    detail: "test".to_string(),
                    status: status.to_string(),
                    assigned_peer: None,
                    created_at_ms: 1000,
                    updated_at_ms: 1000,
                })
                .unwrap();
        }
        let counts = ledger.task_status_counts("g1").unwrap();
        assert_eq!(
            counts,
            vec![("complete".to_string(), 1), ("running".to_string(), 2)]
        );
        assert!(
            ledger.task_status_counts("g-none").unwrap().is_empty(),
            "an unknown goal has no task rows"
        );
    }

    // #1945 codex round — AGGREGATE UNIT TEST (direct-SQL seeding is fine
    // here; the integration-shaped tests in octos-cli drive the production
    // writer instead). The lifecycle/kind/cost aggregates must count EVERY
    // finding of the goal, including `task_id = NULL` rows — exactly the rows
    // ordinary peers write (peer_handoff stages no fleet task) and the rows
    // the digest's per-path cost roll-up drops on the floor.
    #[test]
    fn finding_aggregates_count_task_less_rows_too() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "aggregate me".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 10000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        ledger
            .create_task(&Task {
                task_id: "t1".to_string(),
                goal_id: "g1".to_string(),
                title: "t1".to_string(),
                detail: "test".to_string(),
                status: "running".to_string(),
                assigned_peer: None,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        // (id, task, kind, lifecycle, cost) — f2 and f3 have NO task_id.
        let rows: [(&str, Option<&str>, &str, &str, u64); 3] = [
            ("f1", Some("t1"), "observation", "observed", 100),
            ("f2", None, "observation", "verified", 200),
            ("f3", None, "hypothesis", "observed", 300),
        ];
        for (id, task, kind, lifecycle, cost) in rows {
            ledger
                .append_finding(&Finding {
                    rowid: None,
                    finding_id: id.to_string(),
                    seq: 0, // assigned by store
                    task_id: task.map(str::to_string),
                    goal_id: "g1".to_string(),
                    kind: kind.to_string(),
                    lifecycle: lifecycle.to_string(),
                    confidence: "medium".to_string(),
                    review_state: "unreviewed".to_string(),
                    assertion: format!("claim {id}"),
                    evidence: None,
                    config_version: None,
                    derived_from: None,
                    supersedes: Vec::new(),
                    cost_tokens: cost,
                    created_at_ms: 2000,
                    created_by: "peer-a".to_string(),
                })
                .unwrap();
        }

        assert_eq!(
            ledger.findings_count_by_lifecycle("g1").unwrap(),
            vec![("observed".to_string(), 2), ("verified".to_string(), 1)]
        );
        assert_eq!(
            ledger.findings_count_by_kind("g1").unwrap(),
            vec![
                ("hypothesis".to_string(), 1),
                ("observation".to_string(), 2)
            ]
        );
        assert_eq!(
            ledger.total_cost_tokens("g1").unwrap(),
            600,
            "task-less findings' cost counts too"
        );
        assert_eq!(
            ledger.total_cost_tokens("g-none").unwrap(),
            0,
            "no findings sums to 0, not NULL/error"
        );
        // The CONTRAST this fix exists for: the path digest's per-path cost
        // roll-up skips `task_id = NULL` findings entirely (see digest.rs
        // `cost_by_path`), so summing it loses f2+f3 — which is why the
        // goal_get `ledger_digest` reads these direct aggregates instead.
        let digest = digest_from_ledger(
            &ledger,
            "g1",
            &crate::digest::DigestOptions {
                max_chars: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        let per_path_total: u64 = digest.cost_by_path.iter().map(|p| p.tokens).sum();
        assert_eq!(
            per_path_total, 100,
            "cost_by_path only sees the task-scoped finding — the documented \
             reason ledger_digest must NOT be built from the path digest"
        );
    }

    // #1961 — an answered escalation must become `resolved` in the ledger.
    #[test]
    fn resolve_escalation_marks_the_open_row_resolved() {
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
        let esc = Escalation {
            escalation_id: "esc-picker-1".to_string(),
            goal_id: "g1".to_string(),
            task_id: None,
            peer_id: "picker".to_string(),
            question: "7 or 8?".to_string(),
            context: None,
            status: "open".to_string(),
            default_action: None,
            default_after_secs: None,
            created_at_ms: 1000,
            resolved_at_ms: None,
            resolved_by: None,
            resolution: None,
        };
        ledger.append_escalation(&esc).unwrap();

        let updated = ledger
            .resolve_escalation("picker", "[answer] 7", "master-session", 2000)
            .unwrap();
        assert_eq!(updated, 1, "the one open escalation must be resolved");

        // A second resolve is a no-op (no open rows left) — idempotent.
        let again = ledger
            .resolve_escalation("picker", "[answer] 7", "master-session", 2001)
            .unwrap();
        assert_eq!(again, 0, "double-resolve must not update anything");

        let conn = ledger.conn.lock().unwrap();
        let (status, resolution, resolved_by): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, resolution, resolved_by FROM escalations WHERE escalation_id = 'esc-picker-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "resolved");
        assert_eq!(resolution.as_deref(), Some("[answer] 7"));
        assert_eq!(resolved_by.as_deref(), Some("master-session"));
    }

    // #1957 — upsert_goal writes REAL fields on insert and refreshes the mutable
    // ones on conflict, so the ledger goals-row is no longer a stale placeholder.
    #[test]
    fn upsert_goal_writes_real_fields_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "compute 6x7".to_string(),
                status: "active".to_string(),
                tokens_used: 100,
                token_budget: 2000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        let got = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(got.objective, "compute 6x7");
        assert_eq!(got.tokens_used, 100);
        // Second upsert refreshes status/tokens; created_at is preserved.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "compute 6x7".to_string(),
                status: "complete".to_string(),
                tokens_used: 550,
                token_budget: 2000,
                continuations_used: 3,
                revision: 0,
                created_at_ms: 9999, // must be IGNORED on conflict
                updated_at_ms: 2000,
            })
            .unwrap();
        let got = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(got.status, "complete", "status must refresh");
        assert_eq!(got.tokens_used, 550, "tokens must refresh");
        assert_eq!(got.continuations_used, 3);
        assert_eq!(got.objective, "compute 6x7");
        assert_eq!(
            got.created_at_ms, 1000,
            "created_at must be preserved on conflict"
        );

        // Guard (issue #1957 codex #2): a STALE upsert — one whose
        // `updated_at_ms` is OLDER than the row's — must NOT clobber the newer
        // state. This is the finding-path-after-completion race: a peer finding
        // lands and re-upserts the goal row with the goal's pre-completion
        // status; without the `updated_at_ms >=` guard it would flip a
        // `complete` goal back to `active` in the durable ledger.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "compute 6x7".to_string(),
                status: "active".to_string(), // stale pre-completion status
                tokens_used: 120,             // stale, lower spend
                token_budget: 2000,
                continuations_used: 1,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1500, // OLDER than the row's 2000 → must be dropped
            })
            .unwrap();
        let got = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(
            got.status, "complete",
            "a stale (older updated_at_ms) upsert must not downgrade the status"
        );
        assert_eq!(
            got.tokens_used, 550,
            "a stale upsert must not roll the token counter backwards"
        );
        assert_eq!(
            got.continuations_used, 3,
            "stale continuations rejected too"
        );

        // #1965 codex round — the monotonic-counter clause, isolated from the
        // complete-protection clause (fresh ACTIVE goal): two peers can finish
        // in the SAME millisecond, so `updated_at_ms >=` alone admits the
        // smaller charge upserting second and rolls the durable counter back
        // while memory holds the higher total. tokens_used is MONOTONIC per
        // goal_id (a replacement goal mints a fresh goal_id → different ledger
        // file; re-activation never resets counters), so an equal-ms write
        // carrying a LOWER tokens_used must be rejected...
        let seed = |tokens_used: u64| Goal {
            goal_id: "g2".to_string(),
            objective: "concurrent peers".to_string(),
            status: "active".to_string(),
            tokens_used,
            token_budget: 2000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 5000,
            updated_at_ms: 5000, // every write in this block ties on ms
        };
        assert!(
            ledger.upsert_goal(&seed(300)).unwrap(),
            "the seeding insert is admitted"
        );
        // equal-ms, LOWER → rejected; #1973 fix-round: the rejection is now
        // REPORTED (`false`) so an administrative status sync can detect the
        // loss and retry with max'd counters instead of silently diverging.
        assert!(
            !ledger.upsert_goal(&seed(100)).unwrap(),
            "a guarded rejection must report false"
        );
        assert_eq!(
            ledger.get_goal("g2").unwrap().unwrap().tokens_used,
            300,
            "an equal-ms upsert with a lower tokens_used must not roll the \
             counter backwards"
        );
        // ...while an equal-ms write carrying a HIGHER tokens_used (the other
        // peer's larger charge landing second) must still be accepted.
        assert!(
            ledger.upsert_goal(&seed(450)).unwrap(),
            "an admitted refresh must report true"
        );
        assert_eq!(
            ledger.get_goal("g2").unwrap().unwrap().tokens_used,
            450,
            "an equal-ms upsert with a higher tokens_used must be accepted"
        );
    }

    /// #1973 fix-round 3 — `cas_goal_status` flips ONLY the status of a row
    /// still carrying the expected status; counters untouched; a mismatched
    /// expectation or a `complete` row is a `false` no-op.
    #[test]
    fn cas_goal_status_updates_only_on_matching_expected_status() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("l.db")).unwrap();
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "obj".to_string(),
                status: "active".to_string(),
                tokens_used: 300,
                token_budget: 2000,
                continuations_used: 1,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();

        // Matching (status, updated_at_ms) pair → status lands, counters
        // untouched.
        assert!(
            ledger
                .cas_goal_status("g1", "paused", "active", 1000, 2000)
                .unwrap()
        );
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "paused");
        assert_eq!(row.updated_at_ms, 2000);
        assert_eq!(row.tokens_used, 300, "the CAS never touches counters");
        assert_eq!(row.continuations_used, 1);

        // Stale status expectation (row moved on) → no-op.
        assert!(
            !ledger
                .cas_goal_status("g1", "cleared", "active", 2000, 3000)
                .unwrap()
        );
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "paused", "a lost CAS changes nothing");
        assert_eq!(row.updated_at_ms, 2000);

        // Stale TIMESTAMP expectation with a matching status: an interleaved
        // write bumped the row after the read → the CAS must fail (round-4
        // codex: predicating on status alone admitted this and could regress
        // the row's clock).
        assert!(
            !ledger
                .cas_goal_status("g1", "cleared", "paused", 1234, 4000)
                .unwrap()
        );
        assert_eq!(ledger.get_goal("g1").unwrap().unwrap().status, "paused");

        // Missing goal → no-op.
        assert!(
            !ledger
                .cas_goal_status("ghost", "paused", "active", 1000, 3000)
                .unwrap()
        );

        // Complete is terminal: even a MATCHING expectation pair is refused.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g2".to_string(),
                objective: "obj".to_string(),
                status: "complete".to_string(),
                tokens_used: 10,
                token_budget: 2000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        assert!(
            !ledger
                .cas_goal_status("g2", "cleared", "complete", 1000, 5000)
                .unwrap()
        );
        assert_eq!(ledger.get_goal("g2").unwrap().unwrap().status, "complete");
    }

    /// #1973 fix-round 4 — codex: an INTERVENING SAME-STATUS write between
    /// the caller's read and its CAS must defeat the CAS. With a status-only
    /// predicate, `active@1000` read → concurrent `active@3000/tok600` upsert
    /// → CAS(expected active) still matched and stamped `cleared@2000`,
    /// REGRESSING the row's clock (which then let a delayed `blocked@2500`
    /// pass the ordering gate and flip cleared→blocked). Predicating on the
    /// exact `(status, updated_at_ms)` pair read makes any interleaved write
    /// change the pair → CAS fails → the newer state wins (one shot, no
    /// re-attempt).
    #[test]
    fn cas_goal_status_is_defeated_by_an_intervening_same_status_write() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("l.db")).unwrap();
        let seed = |tokens_used: u64, updated_at_ms: u64| Goal {
            goal_id: "g1".to_string(),
            objective: "obj".to_string(),
            status: "active".to_string(),
            tokens_used,
            token_budget: 2000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms,
        };
        ledger.upsert_goal(&seed(300, 1000)).unwrap();
        // The caller reads (active, 1000)…
        let read = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!((read.status.as_str(), read.updated_at_ms), ("active", 1000));
        // …then a concurrent upsert lands the SAME status with newer fields.
        assert!(ledger.upsert_goal(&seed(600, 3000)).unwrap());
        // The CAS against the stale read must fail — and the newer row wins.
        assert!(
            !ledger
                .cas_goal_status("g1", "cleared", "active", 1000, 2000)
                .unwrap(),
            "an interleaved write must defeat the CAS",
        );
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "active");
        assert_eq!(row.updated_at_ms, 3000, "no clock regression");
        assert_eq!(row.tokens_used, 600, "the newer counters survive");
    }

    #[test]
    fn upsert_goal_never_downgrades_a_complete_goal() {
        // Issue #1957 codex #2, defence-in-depth: the `updated_at_ms >=` guard
        // alone cannot break a millisecond tie, and timestamps are only
        // millisecond-resolution. A `complete` row is terminal for a goal_id
        // (re-activation mints a fresh id), so a non-`complete` write must NEVER
        // win against it — not even one carrying an equal or NEWER timestamp.
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("l.db")).unwrap();
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "obj".to_string(),
                status: "complete".to_string(),
                tokens_used: 500,
                token_budget: 2000,
                continuations_used: 2,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 2000,
            })
            .unwrap();

        // EQUAL-ms stale `active` write (the tie the `>=` clause would admit):
        // must be rejected by the complete-protection clause.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "obj".to_string(),
                status: "active".to_string(),
                tokens_used: 10,
                token_budget: 2000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 2000, // EQUAL to the stored row
            })
            .unwrap();
        assert_eq!(
            ledger.get_goal("g1").unwrap().unwrap().status,
            "complete",
            "an equal-ms `active` write must not downgrade a `complete` goal"
        );

        // Even a strictly NEWER `active` write must not undo completion.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "obj".to_string(),
                status: "active".to_string(),
                tokens_used: 10,
                token_budget: 2000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 9999, // strictly newer
            })
            .unwrap();
        let g1 = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(
            g1.status, "complete",
            "even a newer `active` write must not downgrade a terminal `complete` goal"
        );
        assert_eq!(g1.tokens_used, 500, "counters stay at the completed values");

        // A `complete → complete` refresh with a newer ts IS allowed.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "obj".to_string(),
                status: "complete".to_string(),
                tokens_used: 600,
                token_budget: 2000,
                continuations_used: 3,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 10_000,
            })
            .unwrap();
        assert_eq!(
            ledger.get_goal("g1").unwrap().unwrap().tokens_used,
            600,
            "a complete→complete refresh with a newer ts still updates"
        );

        // `blocked` is NOT protected: a blocked goal is user-resumable to active
        // under the same id, so a newer `active` write MUST win.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g2".to_string(),
                objective: "obj2".to_string(),
                status: "blocked".to_string(),
                tokens_used: 100,
                token_budget: 2000,
                continuations_used: 1,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        ledger
            .upsert_goal(&Goal {
                goal_id: "g2".to_string(),
                objective: "obj2".to_string(),
                status: "active".to_string(),
                tokens_used: 100,
                token_budget: 2000,
                continuations_used: 1,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 2000, // newer → resume must win
            })
            .unwrap();
        assert_eq!(
            ledger.get_goal("g2").unwrap().unwrap().status,
            "active",
            "a newer resume of a `blocked` goal must be allowed"
        );
    }

    /// #1967 test helper — a minimal OPEN escalation row. The lifecycle tests
    /// below vary only id/peer/timing/defaults, so keep the noise here.
    fn open_escalation(
        escalation_id: &str,
        goal_id: &str,
        peer_id: &str,
        created_at_ms: u64,
        default_action: Option<&str>,
        default_after_secs: Option<i64>,
    ) -> Escalation {
        Escalation {
            escalation_id: escalation_id.to_string(),
            goal_id: goal_id.to_string(),
            task_id: None,
            peer_id: peer_id.to_string(),
            question: format!("question from {peer_id}"),
            context: None,
            status: "open".to_string(),
            default_action: default_action.map(str::to_string),
            default_after_secs,
            created_at_ms,
            resolved_at_ms: None,
            resolved_by: None,
            resolution: None,
        }
    }

    fn goal_row(goal_id: &str) -> Goal {
        Goal {
            goal_id: goal_id.to_string(),
            objective: "test".to_string(),
            status: "active".to_string(),
            tokens_used: 0,
            token_budget: 10000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        }
    }

    /// #1967 — the READ half of the escalation lifecycle. `append_escalation`
    /// wrote rows and the resolve paths flipped them, but no production SELECT
    /// existed: an open escalation was invisible. `list_open_escalations`
    /// returns only this goal's OPEN rows, oldest first.
    #[test]
    fn list_open_escalations_returns_open_rows_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        ledger.create_goal(&goal_row("g1")).unwrap();
        ledger.create_goal(&goal_row("g2")).unwrap();
        // Insert out of created order to prove the ORDER BY.
        ledger
            .append_escalation(&open_escalation("esc-b", "g1", "peer-b", 3000, None, None))
            .unwrap();
        ledger
            .append_escalation(&open_escalation("esc-a", "g1", "peer-a", 1000, None, None))
            .unwrap();
        ledger
            .append_escalation(&open_escalation("esc-c", "g1", "peer-c", 2000, None, None))
            .unwrap();
        // A different goal's row must not leak into g1's listing.
        ledger
            .append_escalation(&open_escalation("esc-x", "g2", "peer-x", 500, None, None))
            .unwrap();
        // A resolved row must not be listed as open.
        ledger
            .resolve_escalation("peer-c", "[answer] done", "master", 4000)
            .unwrap();

        let open = ledger.list_open_escalations("g1").unwrap();
        assert_eq!(
            open.iter()
                .map(|e| e.escalation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["esc-a", "esc-b"],
            "open-only, this goal only, oldest first"
        );
        assert_eq!(open[0].peer_id, "peer-a");
        assert_eq!(open[0].question, "question from peer-a");
    }

    /// #1967 — `resolve_escalation_by_id` flips exactly the addressed row
    /// (the peer_id-bulk `resolve_escalation` cannot target one of several
    /// rows) and refuses an already-resolved or unknown id with `Ok(false)`.
    #[test]
    fn resolve_escalation_by_id_targets_one_row_and_refuses_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        ledger.create_goal(&goal_row("g1")).unwrap();
        // TWO open rows for the SAME peer — bulk resolve would flip both.
        ledger
            .append_escalation(&open_escalation("esc-1", "g1", "picker", 1000, None, None))
            .unwrap();
        ledger
            .append_escalation(&open_escalation("esc-2", "g1", "picker", 2000, None, None))
            .unwrap();

        let flipped = ledger
            .resolve_escalation_by_id("esc-1", "[timeout] expired", "system:escalation-timeout")
            .unwrap();
        assert!(flipped, "an open row must resolve");
        let open = ledger.list_open_escalations("g1").unwrap();
        assert_eq!(open.len(), 1, "only the addressed row was flipped");
        assert_eq!(open[0].escalation_id, "esc-2");

        // Already resolved → refused (no clobber of the recorded resolution).
        let again = ledger
            .resolve_escalation_by_id("esc-1", "[answer] late", "master")
            .unwrap();
        assert!(!again, "a resolved row must not re-resolve");
        // Unknown id → refused, not an error.
        assert!(
            !ledger
                .resolve_escalation_by_id("esc-nope", "[timeout] expired", "system")
                .unwrap()
        );

        // The FIRST resolve's text landed verbatim and survived the refused
        // re-resolve (same raw-row check idiom as the #1961 test above).
        let conn = ledger.conn.lock().unwrap();
        let (status, resolution, resolved_by): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, resolution, resolved_by FROM escalations WHERE escalation_id = 'esc-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "resolved");
        assert_eq!(resolution.as_deref(), Some("[timeout] expired"));
        assert_eq!(resolved_by.as_deref(), Some("system:escalation-timeout"));
    }

    /// #1967 — timeout candidates: only OPEN rows whose `default_after_secs`
    /// has ELAPSED (`created_at_ms + s*1000 < now_ms`) qualify. Unexpired,
    /// no-default, and already-resolved rows never surface.
    #[test]
    fn list_expired_open_escalations_filters_elapsed_defaults_only() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        ledger.create_goal(&goal_row("g1")).unwrap();
        // created 1000 + 10s → expires at 11_000: EXPIRED at now=60_000.
        ledger
            .append_escalation(&open_escalation(
                "esc-expired",
                "g1",
                "p1",
                1000,
                Some("proceed"),
                Some(10),
            ))
            .unwrap();
        // created 1000 + 100s → expires at 101_000: NOT expired at now=60_000.
        ledger
            .append_escalation(&open_escalation(
                "esc-later",
                "g1",
                "p2",
                1000,
                None,
                Some(100),
            ))
            .unwrap();
        // No default timer → never a candidate.
        ledger
            .append_escalation(&open_escalation(
                "esc-forever",
                "g1",
                "p3",
                1000,
                None,
                None,
            ))
            .unwrap();
        // Expired timer but already resolved → never a candidate.
        ledger
            .append_escalation(&open_escalation(
                "esc-done",
                "g1",
                "p4",
                1000,
                None,
                Some(1),
            ))
            .unwrap();
        ledger
            .resolve_escalation("p4", "[answer] handled", "master", 5000)
            .unwrap();

        let candidates = ledger.list_expired_open_escalations(60_000).unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|e| e.escalation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["esc-expired"],
            "only the elapsed open default is a timeout candidate"
        );
        assert_eq!(candidates[0].default_action.as_deref(), Some("proceed"));
        // At the exact boundary (created + s*1000 == now) the row is NOT yet
        // expired — strict `<` matches the sweep's contract.
        assert!(
            ledger
                .list_expired_open_escalations(11_000)
                .unwrap()
                .is_empty(),
            "boundary instant is not yet expired"
        );
        assert_eq!(
            ledger.list_expired_open_escalations(11_001).unwrap().len(),
            1
        );
    }

    // ---------------------------------------------------------------------
    // #2054 — task-status writer.
    //
    // Until this landed the ledger had `create_task` / `get_task` /
    // `task_status_counts` and no `UPDATE tasks` anywhere, so every row
    // stayed on the status it was inserted with (always `"running"`, written
    // as an FK stub) and a goal could never observe one of its tasks
    // finishing.
    // ---------------------------------------------------------------------

    /// Seed a goal + one task, both at `1_000`.
    fn seed_goal_with_task(ledger: &GoalLedger, goal_id: &str, task_id: &str, status: &str) {
        ledger
            .create_goal(&Goal {
                goal_id: goal_id.to_string(),
                objective: "test".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 10_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        ledger
            .create_task(&Task {
                task_id: task_id.to_string(),
                goal_id: goal_id.to_string(),
                title: String::new(),
                detail: String::new(),
                status: status.to_string(),
                assigned_peer: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
    }

    #[test]
    fn should_write_terminal_status_when_task_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(
            ledger.update_task_status("t1", "complete", 2_000).unwrap(),
            "a running task accepts a terminal transition"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(task.updated_at_ms, 2_000);
    }

    #[test]
    fn should_report_false_when_task_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();

        assert!(
            !ledger
                .update_task_status("nope", "complete", 2_000)
                .unwrap(),
            "an absent task reports no-op rather than erroring — the terminal \
             sink fires for tasks that predate goal binding"
        );
    }

    /// Redelivery safety. The terminal sink is fire-and-forget and can fire
    /// twice for one task (the strangler wiring deliberately keeps the legacy
    /// `on_change` / `on_failure` callbacks alive alongside `on_terminal`), so
    /// a repeat MUST NOT be an error and MUST NOT move the timestamp
    /// backwards.
    #[test]
    fn should_stay_idempotent_when_the_same_terminal_is_redelivered() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(ledger.update_task_status("t1", "complete", 2_000).unwrap());
        assert!(
            !ledger.update_task_status("t1", "complete", 3_000).unwrap(),
            "redelivery is a no-op, not a second write"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(
            task.updated_at_ms, 2_000,
            "timestamp reflects the first delivery, not the duplicate"
        );
    }

    /// Ordering safety. Nothing guarantees the supervisor's terminal event
    /// reaches the ledger before a slower in-flight `running` refresh, so a
    /// late non-terminal write must not resurrect a finished task.
    #[test]
    fn should_refuse_to_regress_once_the_task_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");
        ledger.update_task_status("t1", "failed", 2_000).unwrap();

        for late in ["running", "pending"] {
            assert!(
                !ledger.update_task_status("t1", late, 3_000).unwrap(),
                "a terminal task refuses the late non-terminal write {late:?}"
            );
        }
        // #2055 review round 3 — `complete` after an OWNER-reported `failed`
        // is refused again: the correction admission is provenance-gated on
        // the `correctable` column, and the plain writer stamps
        // `correctable = 0` (owner-final). Only a row written through
        // `update_task_status_with_provenance(.., correctable = true, ..)` —
        // the observer-provisional case — admits the later completion.
        // Blanket failed→complete admission would let a genuinely-cancelled
        // row (cancellation also lands as `failed`) be flipped by a racing
        // completion from another supervisor copy (#2060).
        assert!(!ledger.update_task_status("t1", "complete", 4_000).unwrap());

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "failed");
        assert_eq!(task.updated_at_ms, 2_000);
    }

    /// #2055 review round 3 — the provenance-gated correction in full: an
    /// observer-provisional failure (written with `correctable = true`)
    /// admits exactly the owner's later `complete`
    /// (task_supervisor.rs:2527's correction semantics); the correction
    /// clears the provenance mark, and from there the row is immutable
    /// (a straggler `failed` redelivery cannot undo the correction).
    #[test]
    fn should_allow_failed_to_complete_only_with_correctable_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(
            ledger
                .update_task_status_with_provenance("t1", "failed", true, 2_000)
                .unwrap(),
            "the provisional failure lands with correctable provenance"
        );
        assert!(
            ledger.update_task_status("t1", "complete", 3_000).unwrap(),
            "failed → complete is admitted for a correctable row"
        );
        for late in ["failed", "running", "pending", "complete"] {
            assert!(
                !ledger.update_task_status("t1", late, 4_000).unwrap(),
                "complete refuses every subsequent write ({late:?})"
            );
        }

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(task.updated_at_ms, 3_000);
        // The correction cleared the provenance mark (defense in depth —
        // `complete` is immutable regardless). Same-module test, so the raw
        // column read is fine.
        let correctable: i64 = ledger
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT correctable FROM tasks WHERE task_id = 't1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(correctable, 0, "the correction clears correctable");
    }

    /// #2055 review round 3 — the owner's authoritative re-failure closes
    /// the correction window: a correctable `failed` row admits the
    /// owner-final `failed` downgrade (provenance cleared), after which the
    /// completion is refused like any other final failure.
    #[test]
    fn should_close_correction_window_when_owner_confirms_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(
            ledger
                .update_task_status_with_provenance("t1", "failed", true, 2_000)
                .unwrap()
        );
        // A provisional redelivery is a no-op, not a churn write.
        assert!(
            !ledger
                .update_task_status_with_provenance("t1", "failed", true, 2_500)
                .unwrap()
        );
        // The owner's confirming failure downgrades the provenance mark.
        assert!(
            ledger.update_task_status("t1", "failed", 3_000).unwrap(),
            "the owner-final downgrade of a correctable row is admitted"
        );
        assert!(
            !ledger.update_task_status("t1", "complete", 4_000).unwrap(),
            "after the owner confirmed the failure, the completion is refused"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "failed");
        assert_eq!(task.updated_at_ms, 3_000);
    }

    /// #2055 review round 3 — the `correctable` column arrives via a
    /// best-effort `ALTER TABLE` for databases created before the column
    /// existed; the provenance-gated correction works on such a ledger.
    #[test]
    fn should_migrate_tasks_table_missing_the_correctable_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        // A pre-round-3 database shape: tasks table WITHOUT `correctable`.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE goals (
                     goal_id TEXT PRIMARY KEY, objective TEXT NOT NULL,
                     status TEXT NOT NULL, tokens_used INTEGER NOT NULL DEFAULT 0,
                     token_budget INTEGER NOT NULL,
                     continuations_used INTEGER NOT NULL DEFAULT 0,
                     revision INTEGER NOT NULL DEFAULT 0,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL);
                 CREATE TABLE tasks (
                     task_id TEXT PRIMARY KEY, goal_id TEXT NOT NULL,
                     title TEXT NOT NULL, detail TEXT NOT NULL,
                     status TEXT NOT NULL, assigned_peer TEXT,
                     created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
                     FOREIGN KEY (goal_id) REFERENCES goals(goal_id));
                 INSERT INTO goals VALUES ('g1', 'ship', 'active', 0, 1000, 0, 0, 1, 1);
                 INSERT INTO tasks VALUES ('t1', 'g1', '', '', 'running', NULL, 1, 1);",
            )
            .unwrap();
        }

        let ledger = GoalLedger::open(&path).unwrap();
        assert!(
            ledger
                .update_task_status_with_provenance("t1", "failed", true, 2_000)
                .unwrap(),
            "the migrated column accepts the provenance write"
        );
        assert!(
            ledger.update_task_status("t1", "complete", 3_000).unwrap(),
            "the correction works on a migrated ledger"
        );
        assert_eq!(ledger.get_task("t1").unwrap().unwrap().status, "complete");
    }

    /// #2055 review round 3 — goal-scoped task listing (used by effect
    /// tests that identify rows by title when the task id is internal to a
    /// detached child supervisor).
    #[test]
    fn should_list_tasks_for_exactly_one_goal() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");
        seed_goal_with_task(&ledger, "g2", "t2", "running");

        let tasks = ledger.tasks_for_goal("g1").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "t1");
        assert!(ledger.tasks_for_goal("g3").unwrap().is_empty());
    }

    /// The goal-facing point of the whole exercise: `task_status_counts` is
    /// what goal evaluation reads, and before #2054 it could only ever report
    /// `{"running": N}`.
    #[test]
    fn should_reflect_terminal_writes_in_task_status_counts() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");
        for id in ["t2", "t3"] {
            ledger
                .create_task(&Task {
                    task_id: id.to_string(),
                    goal_id: "g1".to_string(),
                    title: String::new(),
                    detail: String::new(),
                    status: "running".to_string(),
                    assigned_peer: None,
                    created_at_ms: 1_000,
                    updated_at_ms: 1_000,
                })
                .unwrap();
        }

        ledger.update_task_status("t1", "complete", 2_000).unwrap();
        ledger.update_task_status("t2", "failed", 2_000).unwrap();

        let counts: std::collections::BTreeMap<String, u64> = ledger
            .task_status_counts("g1")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(counts.get("complete"), Some(&1));
        assert_eq!(counts.get("failed"), Some(&1));
        assert_eq!(counts.get("running"), Some(&1));
    }

    /// A task belongs to exactly one goal; a sibling goal's counts must not
    /// shift when this one's task settles.
    #[test]
    fn should_leave_other_goals_untouched_when_a_task_settles() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");
        seed_goal_with_task(&ledger, "g2", "t2", "running");

        ledger.update_task_status("t1", "complete", 2_000).unwrap();

        let g2: std::collections::BTreeMap<String, u64> = ledger
            .task_status_counts("g2")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(g2.get("running"), Some(&1));
        assert_eq!(g2.get("complete"), None);
    }

    // ---------------------------------------------------------------------
    // #2055 — idempotent row creation at task registration.
    //
    // Registration legitimately repeats (relaunch, restart, re-registration
    // of a task the supervisor restored), so row creation must be an upsert
    // that PRESERVES an existing row — including its status and timestamps,
    // and including a row that already went terminal — instead of relying on
    // a swallowed UNIQUE violation.
    // ---------------------------------------------------------------------

    #[test]
    fn should_insert_row_when_task_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        // FK enforcement is on (bundled SQLite defaults foreign_keys=1):
        // the parent goals row must exist before any task row.
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "test".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 10_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        assert!(
            ledger
                .create_task_if_absent(&Task {
                    task_id: "t1".to_string(),
                    goal_id: "g1".to_string(),
                    title: "web_probe".to_string(),
                    detail: String::new(),
                    status: "running".to_string(),
                    assigned_peer: None,
                    created_at_ms: 1_000,
                    updated_at_ms: 1_000,
                })
                .unwrap(),
            "a fresh task id inserts a row"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "running");
        assert_eq!(task.title, "web_probe");
    }

    #[test]
    fn should_preserve_existing_row_when_task_is_reregistered() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(
            !ledger
                .create_task_if_absent(&Task {
                    task_id: "t1".to_string(),
                    goal_id: "g1".to_string(),
                    title: "replacement title".to_string(),
                    detail: "replacement detail".to_string(),
                    status: "pending".to_string(),
                    assigned_peer: Some("peer-a".to_string()),
                    created_at_ms: 9_000,
                    updated_at_ms: 9_000,
                })
                .unwrap(),
            "re-registration reports no-op"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "running", "existing status is preserved");
        assert_eq!(task.title, "", "existing title is preserved");
        assert_eq!(task.assigned_peer, None);
        assert_eq!(task.updated_at_ms, 1_000);
    }

    /// The load-bearing half of idempotency: a re-registration AFTER the row
    /// went terminal (relaunch/restart replaying an old registration) must
    /// not resurrect it to `running` — first-terminal-wins extends across
    /// the create path, not just `update_task_status`.
    #[test]
    fn should_preserve_terminal_row_when_task_is_reregistered() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");
        ledger.update_task_status("t1", "complete", 2_000).unwrap();

        assert!(
            !ledger
                .create_task_if_absent(&Task {
                    task_id: "t1".to_string(),
                    goal_id: "g1".to_string(),
                    title: String::new(),
                    detail: String::new(),
                    status: "running".to_string(),
                    assigned_peer: None,
                    created_at_ms: 3_000,
                    updated_at_ms: 3_000,
                })
                .unwrap(),
            "re-registration of a terminal row reports no-op"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "complete", "terminal status survives");
        assert_eq!(task.updated_at_ms, 2_000);
    }

    // ---------------------------------------------------------------------
    // #2055 review round 2 — FK-parent goals row via if-absent insert.
    //
    // Registration must NEVER update a goals row: `upsert_goal`'s monotonic
    // guard admits equal timestamps, so a delayed stale `active` snapshot
    // could overwrite a same-millisecond administrative transition
    // (`paused` / `blocked` / `cleared`) and regress counters. The
    // registration recorder therefore only ever inserts the FK parent when
    // no row exists; the goal engine's own transition sync remains the only
    // goals-row updater.
    // ---------------------------------------------------------------------

    #[test]
    fn should_insert_goal_row_when_goal_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        assert!(
            ledger
                .create_goal_if_absent(&Goal {
                    goal_id: "g1".to_string(),
                    objective: "ship".to_string(),
                    status: "active".to_string(),
                    tokens_used: 5,
                    token_budget: 10_000,
                    continuations_used: 1,
                    revision: 0,
                    created_at_ms: 1_000,
                    updated_at_ms: 1_000,
                })
                .unwrap(),
            "a fresh goal id inserts a row"
        );
        let goal = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(goal.status, "active");
        assert_eq!(goal.objective, "ship");
    }

    #[test]
    fn should_preserve_existing_goal_row_even_with_equal_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        // The row a same-millisecond administrative transition just wrote.
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "ship".to_string(),
                status: "paused".to_string(),
                tokens_used: 500,
                token_budget: 10_000,
                continuations_used: 3,
                revision: 7,
                created_at_ms: 1_000,
                updated_at_ms: 2_000,
            })
            .unwrap();

        // The delayed stale registration snapshot: same updated_at_ms,
        // higher tokens — exactly the shape `upsert_goal`'s guard admits.
        assert!(
            !ledger
                .create_goal_if_absent(&Goal {
                    goal_id: "g1".to_string(),
                    objective: "ship".to_string(),
                    status: "active".to_string(),
                    tokens_used: 600,
                    token_budget: 10_000,
                    continuations_used: 4,
                    revision: 0,
                    created_at_ms: 1_000,
                    updated_at_ms: 2_000,
                })
                .unwrap(),
            "an existing goal row reports no-op"
        );

        let goal = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(goal.status, "paused", "the administrative status survives");
        assert_eq!(goal.tokens_used, 500, "counters are untouched");
        assert_eq!(goal.continuations_used, 3);
        assert_eq!(goal.revision, 7);
    }
}
