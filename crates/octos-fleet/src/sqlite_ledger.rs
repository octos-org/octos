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

/// #2055 review round 4 — the terminal authority of a task-settle write.
/// Ranked to make the settled outcome ORDER-INDEPENDENT across scrambled
/// write delivery (offloaded/retried writes give no ordering guarantee):
/// a write lands iff its rank is strictly greater than the row's stored
/// rank, first-wins within equal rank. The ledger status is DERIVED from
/// the authority, so an illegal status/authority pairing is
/// unrepresentable.
///
/// The ranking mirrors the authority model `TaskSupervisor` enforces
/// internally (task_supervisor.rs:2575): an observer-provisional failure is
/// the weakest verdict (the owner's completion corrects it), and a final
/// failure — owner-reported failure or cancellation — refuses a later
/// completion. Rank 2 beating rank 1 additionally makes the D-present
/// orderings converge on `failed` even when the completion write was
/// DELIVERED first; within one supervisor a final failure after a
/// completion cannot be emitted at all (the supervisor's terminal guard
/// refuses it), and whether a STALE supervisor copy's cancel should outrank
/// the true owner's completion is cross-supervisor truth — #2060's problem,
/// not this rule's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSettleAuthority {
    /// An OBSERVER classified a mid-run signal as fatal without watching
    /// the worker stop; the owner may still correct it. Lands `failed`.
    ProvisionalFailure,
    /// The owner watched its worker actually finish. Lands `complete`.
    Completion,
    /// Owner-reported failure or cancellation. Lands `failed`.
    FinalFailure,
}

impl TaskSettleAuthority {
    /// The persisted rank (`tasks.authority`); `-1` in the column means no
    /// terminal verdict has been recorded yet.
    pub fn rank(self) -> i64 {
        match self {
            TaskSettleAuthority::ProvisionalFailure => 0,
            TaskSettleAuthority::Completion => 1,
            TaskSettleAuthority::FinalFailure => 2,
        }
    }

    /// The ledger status this authority writes.
    pub fn ledger_status(self) -> &'static str {
        match self {
            TaskSettleAuthority::ProvisionalFailure | TaskSettleAuthority::FinalFailure => "failed",
            TaskSettleAuthority::Completion => "complete",
        }
    }
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
            // #2055 review round 4 — the `authority` schema migration runs
            // HERE, inside the attempt (never in the common `open`, which
            // executes on tokio workers): this profile is only ever called
            // from blocking contexts, and a migration transaction that loses
            // a lock race is retried exactly like a contended open.
            let attempt_result = Self::open_inner(path, Some(std::time::Duration::from_secs(1)))
                .and_then(|ledger| {
                    {
                        let mut conn = ledger.conn.lock().unwrap();
                        Self::migrate_tasks_authority_column(&mut conn, path)?;
                    }
                    Ok(ledger)
                });
            match attempt_result {
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
                -- #2055 review round 4: persisted TERMINAL AUTHORITY rank.
                -- -1 = no terminal verdict recorded; 0 = observer-provisional
                -- failure; 1 = completion; 2 = final failure (owner failure /
                -- cancellation). A settle write lands iff its rank is
                -- STRICTLY greater than the stored rank, making the outcome
                -- order-independent across scrambled write delivery.
                authority INTEGER NOT NULL DEFAULT -1,
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
        // #2055 review round 4: NO column migration here. `create_tables`
        // runs inside the COMMON `open`, which executes on tokio workers
        // (`goal_get` and every inline reader), and migration DDL is exactly
        // the synchronous work this PR keeps off the executor. The
        // `authority` migration lives in [`Self::open_with_busy_retry`],
        // which is only ever called from blocking contexts — and every code
        // path that references the column opens through it. Plain-`open`
        // readers reference only pre-migration columns by name.
        Ok(())
    }

    /// #2055 review round 7 (W3 test hook) — a test-only hook invoked INSIDE
    /// the migration transaction, between the schema statements and the
    /// commit, keyed by ledger path so parallel tests' migrations pass
    /// through untouched. `None` in production builds by construction
    /// (`cfg(test)` — zero cost). The callback is cloned out of the slot
    /// before invocation so a blocking hook cannot stall other migrations
    /// on the slot lock.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    fn migration_mid_transaction_hook()
    -> &'static Mutex<Option<(std::path::PathBuf, Arc<dyn Fn() + Send + Sync>)>> {
        static HOOK: std::sync::OnceLock<
            Mutex<Option<(std::path::PathBuf, Arc<dyn Fn() + Send + Sync>)>>,
        > = std::sync::OnceLock::new();
        HOOK.get_or_init(|| Mutex::new(None))
    }

    /// Whether the `tasks` table currently carries `column` — schema
    /// inspection via `pragma_table_info`, which never references the column
    /// itself and returns an empty set for a missing table, so it is safe on
    /// every schema generation.
    fn tasks_has_column(conn: &Connection, column: &str) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = ?1",
            params![column],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// #2055 review round 5 — bring an existing `tasks` table onto the
    /// `authority` schema, ATOMICALLY. All steps run inside ONE
    /// `BEGIN IMMEDIATE` transaction and are decided by SCHEMA INSPECTION
    /// (`pragma_table_info`), never by error-message substrings:
    ///
    /// - the previous statement-by-statement shape could commit the
    ///   ALTER-ADD and then fail before the backfill; every later opener
    ///   then saw the column present, skipped the backfill forever, and
    ///   legacy terminal rows sat at authority `-1` (correctable).
    /// - the `"no such column"` substring also matched REAL failures (a
    ///   dependent index on the dropped column reports
    ///   `error in index … no such column`), silently eating them.
    ///
    /// Steps, inside the transaction: (1) when `authority` is absent, add
    /// it (default `-1`, identical to the `CREATE TABLE` shape) and stamp
    /// every pre-existing terminal row FINAL (`authority = 2`) — a legacy
    /// `failed` row must NOT become correctable, and the backfill can never
    /// touch rows the current code writes because it runs only in the same
    /// transaction as the column creation; (2) when the short-lived round-3
    /// `correctable` column is present, drop it. Any statement failure
    /// rolls the WHOLE migration back (the `Transaction` drop path), so no
    /// partial schema state can persist; any error propagates to the caller
    /// (`open_with_busy_retry`'s attempt loop, which retries the
    /// lock-contention class and surfaces the rest).
    ///
    /// The pre-transaction fast path skips the write lock entirely on a
    /// fully-migrated database; the checks are REPEATED inside the
    /// transaction because two blocking-context openers can race the fast
    /// path, and only the in-transaction view is serialized.
    fn migrate_tasks_authority_column(conn: &mut Connection, path: &Path) -> Result<()> {
        #[cfg(not(test))]
        let _ = path;
        if Self::tasks_has_column(conn, "authority")?
            && !Self::tasks_has_column(conn, "correctable")?
        {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !Self::tasks_has_column(&tx, "authority")? {
            tx.execute(
                "ALTER TABLE tasks ADD COLUMN authority INTEGER NOT NULL DEFAULT -1",
                [],
            )?;
            tx.execute(
                "UPDATE tasks SET authority = 2 WHERE status IN ('complete', 'failed')",
                [],
            )?;
        }
        if Self::tasks_has_column(&tx, "correctable")? {
            tx.execute("ALTER TABLE tasks DROP COLUMN correctable", [])?;
        }
        // W3 test hook: lets a test hold THIS transaction provably open
        // while it fires concurrent creation attempts. Path-keyed; absent
        // in production builds.
        #[cfg(test)]
        {
            let hook = Self::migration_mid_transaction_hook()
                .lock()
                .unwrap()
                .as_ref()
                .filter(|(hook_path, _)| hook_path == path)
                .map(|(_, callback)| Arc::clone(callback));
            if let Some(callback) = hook {
                callback();
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Create a new goal.
    /// #2066 round 2 (codex R3) — creation writers carry the same #2063
    /// activation guard as the update writers: an `active` snapshot whose own
    /// arithmetic is exhausted lands `budget_limited`. No code path may
    /// produce an active-and-exhausted row, including the very first insert.
    pub fn create_goal(&self, goal: &Goal) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2,
                 CASE WHEN ?3 = 'active' AND ?5 > 0 AND ?4 >= ?5
                 THEN 'budget_limited' ELSE ?3 END,
                 ?4, ?5, ?6, ?7, ?8, ?9)",
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
    ///     #2066 round 2 (codex R3) — `cleared` joins `complete` as an
    ///     immutable STATUS: an under-budget snapshot must not resurrect a
    ///     cleared row to `active`. Counters on a cleared row may still
    ///     accrue via [`Self::settle_cleared_goal_cost_delta`] (the
    ///     post-clear settle path), which never touches status.
    ///
    /// #1973 fix-round — returns whether the write was ADMITTED (`true`: the
    /// row was inserted, or the guarded update fired), via SQLite's
    /// rows-changed count. A guarded rejection returns `false`, so an
    /// administrative STATUS sync (park/clear) whose snapshot carries stale
    /// lower counters can detect the loss and retry once with max'd monotonic
    /// fields — instead of silently leaving the row on its old status while a
    /// decision row claims the transition happened.
    /// #2063 — the VALUES status expression enforces the activation guard on
    /// the snapshot's OWN arithmetic (`active` while `tokens_used >=
    /// token_budget` lands `budget_limited`); `excluded.status` picks up the
    /// transformed value, so the guard covers the insert arm AND the update
    /// arm with one expression. Defense-in-depth here — the reachable hole is
    /// the status-only [`Self::cas_goal_status`] — but the invariant lives in
    /// every write of this family, not in its callers.
    pub fn upsert_goal(&self, goal: &Goal) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let admitted = conn.execute(
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2,
                 CASE WHEN ?3 = 'active' AND ?5 > 0 AND ?4 >= ?5
                 THEN 'budget_limited' ELSE ?3 END,
                 ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(goal_id) DO UPDATE SET
                 objective = excluded.objective,
                 status = excluded.status,
                 tokens_used = excluded.tokens_used,
                 token_budget = excluded.token_budget,
                 continuations_used = excluded.continuations_used,
                 updated_at_ms = excluded.updated_at_ms
             WHERE excluded.updated_at_ms >= goals.updated_at_ms
               AND excluded.tokens_used >= goals.tokens_used
               AND NOT (goals.status = 'complete' AND excluded.status <> 'complete')
               AND NOT (goals.status = 'cleared' AND excluded.status <> 'cleared')",
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
             VALUES (?1, ?2,
                 CASE WHEN ?3 = 'active' AND ?5 > 0 AND ?4 >= ?5
                 THEN 'budget_limited' ELSE ?3 END,
                 ?4, ?5, ?6, ?7, ?8, ?9)
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
    ///
    /// #2063 — activation is CONDITIONAL ON THE ROW'S OWN ARITHMETIC:
    /// `active` while `tokens_used >= token_budget` (budget > 0) writes
    /// `budget_limited` instead, in the same statement. This is a status-only
    /// write over counters the CALLER cannot see (the ledger is multi-process
    /// WAL, and the #1973 retry that reaches this CAS fires precisely when
    /// the caller's snapshot carries STALE LOWER counters than the row) — so
    /// the in-memory resume guard is structurally unable to protect this
    /// write, and the invariant must live here. The CAS still reports
    /// `changed` (the pair matched and a stamp landed); the transition-sync
    /// caller re-reads the row before appending its decision, so a flip to
    /// the safe status suppresses the `active` audit row.
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
            "UPDATE goals SET status = CASE
                 WHEN ?1 = 'active' AND token_budget > 0 AND tokens_used >= token_budget
                 THEN 'budget_limited' ELSE ?1 END,
                 updated_at_ms = ?2
             WHERE goal_id = ?3 AND status = ?4 AND updated_at_ms = ?5
               AND status NOT IN ('complete', 'cleared')",
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

    /// #2055 review round 5 — the authority rank a freshly CREATED row
    /// starts at, derived from the status it is created with: a terminal
    /// status written at creation is an owner-recorded fact, so `complete`
    /// starts at the completion rank and `failed` at the FINAL-failure rank
    /// (an observer-provisional failure is only ever expressible through
    /// [`Self::settle_task_status`], never at creation). Without this, a
    /// terminal creation sat at `-1` and the nonterminal-refresh guard
    /// admitted a plain `running` write over a valid terminal row.
    fn creation_authority_for_status(status: &str) -> i64 {
        match status {
            "complete" => TaskSettleAuthority::Completion.rank(),
            "failed" => TaskSettleAuthority::FinalFailure.rank(),
            _ => -1,
        }
    }

    /// Create a new task.
    ///
    /// Round 5: the initial `authority` is derived from `task.status` (see
    /// [`Self::creation_authority_for_status`]) when the column exists; on a
    /// not-yet-migrated legacy database (reachable through the plain
    /// [`Self::open`], which deliberately runs no migration) the statement
    /// falls back to the legacy shape and never references the column.
    ///
    /// Round 6 (V3): the schema inspection and the insert run inside ONE
    /// `BEGIN IMMEDIATE` transaction. As two autocommit statements they were
    /// a TOCTOU: a concurrent connection could commit the migration between
    /// the check and the insert, and the busy handler makes that the COMMON
    /// interleaving, not a rare one — the legacy-shape insert blocks on the
    /// migration's write lock and then executes on the migrated table,
    /// landing a terminal row at authority `-1` AFTER the backfill already
    /// ran (permanently wrong-ranked). `BEGIN IMMEDIATE` serializes against
    /// the migration's own `BEGIN IMMEDIATE` transaction, so the pair
    /// either runs fully before the migration (and the backfill stamps the
    /// row) or fully after it (and the derived-authority shape is chosen) —
    /// the interleaving is impossible, not merely unlikely. Structural
    /// argument only; the concurrency test is a tripwire.
    pub fn create_task(&self, task: &Task) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if Self::tasks_has_column(&tx, "authority")? {
            tx.execute(
                "INSERT INTO tasks (task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms, authority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    task.task_id,
                    task.goal_id,
                    task.title,
                    task.detail,
                    task.status,
                    task.assigned_peer,
                    task.created_at_ms,
                    task.updated_at_ms,
                    Self::creation_authority_for_status(&task.status),
                ],
            )?;
        } else {
            tx.execute(
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
        }
        tx.commit()?;
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
    ///
    /// Round 5: the initial `authority` is derived from `task.status` (see
    /// [`Self::creation_authority_for_status`]) when the column exists; the
    /// legacy fallback mirrors [`Self::create_task`].
    ///
    /// Round 6 (V3): inspection + insert in ONE `BEGIN IMMEDIATE`
    /// transaction — see [`Self::create_task`] for the TOCTOU this closes
    /// and the serialization argument.
    pub fn create_task_if_absent(&self, task: &Task) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = if Self::tasks_has_column(&tx, "authority")? {
            tx.execute(
                "INSERT INTO tasks (task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms, authority)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
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
                    Self::creation_authority_for_status(&task.status),
                ],
            )?
        } else {
            tx.execute(
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
            )?
        };
        tx.commit()?;
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
    /// - absent row                   → 0 rows → `false`
    /// - no verdict (`-1`) → any rank → 1 row  → `true`
    /// - equal rank redelivery        → guard excludes → `false` (first wins)
    /// - lower rank after higher      → guard excludes → `false`
    /// - higher rank after lower      → 1 row  → `true`
    /// - non-terminal refresh after any verdict → guard excludes → `false`
    ///
    /// #2055 review round 4 — terminal writes route through
    /// [`Self::settle_task_status`]'s authority-rank rule (see
    /// [`TaskSettleAuthority`]); this string-status compatibility form maps
    /// `"complete"` → `Completion` and `"failed"` → `FinalFailure`
    /// (owner-final — the historical semantics of this API), and treats any
    /// other status as a non-terminal refresh admitted only while the row
    /// carries no terminal verdict.
    ///
    /// Requires the migrated schema: every statement here references the
    /// `authority` column, so production callers reach this only through
    /// connections opened by [`Self::open_with_busy_retry`] (which runs the
    /// migration) or on databases created by the current schema batch.
    ///
    /// Fidelity note: cancellation is collapsed into failure upstream
    /// (`TerminalOutcome` has no `Cancelled`; the change-feed settle maps
    /// `TaskStatus::Cancelled` to `FinalFailure` too), so a cancelled task
    /// lands here as `"failed"` and the two are indistinguishable in the
    /// ledger — distinguishable only through the authority rank.
    pub fn update_task_status(
        &self,
        task_id: &str,
        new_status: &str,
        updated_at_ms: u64,
    ) -> Result<bool> {
        match new_status {
            "complete" => {
                self.settle_task_status(task_id, TaskSettleAuthority::Completion, updated_at_ms)
            }
            "failed" => {
                self.settle_task_status(task_id, TaskSettleAuthority::FinalFailure, updated_at_ms)
            }
            _ => {
                let conn = self.conn.lock().unwrap();
                let changed = conn.execute(
                    "UPDATE tasks SET status = ?1, updated_at_ms = ?2
                     WHERE task_id = ?3 AND authority < 0",
                    params![new_status, updated_at_ms, task_id],
                )?;
                Ok(changed > 0)
            }
        }
    }

    /// #2055 review round 4 — the ONE terminal admission rule: the write
    /// lands iff its authority rank is STRICTLY greater than the row's
    /// stored rank (first-wins within equal rank), and both the status and
    /// the stored rank come from the [`TaskSettleAuthority`] itself, so an
    /// illegal status/authority pairing cannot be expressed. `Ok(false)` is
    /// a normal no-op (absent row, redelivery, or an outranked write).
    ///
    /// Order-independence this buys (P = provisional failure, C =
    /// completion, D = final failure): every delivery order containing a D
    /// ends `failed`; P and C alone end `complete` in either order.
    pub fn settle_task_status(
        &self,
        task_id: &str,
        authority: TaskSettleAuthority,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE tasks SET status = ?1, updated_at_ms = ?2, authority = ?4
             WHERE task_id = ?3 AND ?4 > authority",
            params![
                authority.ledger_status(),
                updated_at_ms,
                task_id,
                authority.rank()
            ],
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
    ///
    /// #2063 — carries the same activation guard as [`Self::cas_goal_status`]:
    /// `active` over an exhausted row writes `budget_limited` instead.
    /// #2066 round 2 (codex R3) — and the same terminal protection: a
    /// `complete`/`cleared` row refuses any revision-CAS status overwrite
    /// (surfaces as the same Err as a revision mismatch).
    pub fn update_goal_status(
        &self,
        goal_id: &str,
        status: &str,
        expected_revision: u64,
        updated_at_ms: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rows_affected = conn.execute(
            "UPDATE goals SET status = CASE
                 WHEN ?1 = 'active' AND token_budget > 0 AND tokens_used >= token_budget
                 THEN 'budget_limited' ELSE ?1 END,
                 revision = revision + 1, updated_at_ms = ?2
             WHERE goal_id = ?3 AND revision = ?4
               AND status NOT IN ('complete', 'cleared')",
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
    ///
    /// #2066 round 2 (codex R3 + HIGH) — the status write carries the #2063
    /// activation guard and the terminal protection, and the AUDIT row is
    /// derived from the STORED outcome, not the caller's request: when the
    /// guard overrode the requested status (asked `active`, stored
    /// `budget_limited`), inserting the caller's decision unchanged would
    /// make the audit trail contradict the goals row. The override is
    /// surfaced instead — the decision insert is SKIPPED and the returned
    /// stored status tells the caller what actually landed (the finding, if
    /// any, still lands: it is evidence, not a status claim).
    ///
    /// Returns the status the row carries after the commit.
    pub fn commit_state_with_audit(
        &self,
        goal_id: &str,
        new_status: &str,
        expected_revision: u64,
        updated_at_ms: u64,
        finding: Option<&Finding>,
        decision: Option<&Decision>,
    ) -> Result<String> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // Step 1: Update goal state (CAS). #2063 — same activation guard as
        // `cas_goal_status`; #2066 round 2 — same terminal protection.
        let rows_affected = tx.execute(
            "UPDATE goals SET status = CASE
                 WHEN ?1 = 'active' AND token_budget > 0 AND tokens_used >= token_budget
                 THEN 'budget_limited' ELSE ?1 END,
                 revision = revision + 1, updated_at_ms = ?2
             WHERE goal_id = ?3 AND revision = ?4
               AND status NOT IN ('complete', 'cleared')",
            params![new_status, updated_at_ms, goal_id, expected_revision],
        )?;

        if rows_affected == 0 {
            return Err(eyre::eyre!(
                "commit_state_with_audit failed: goal {} not found, revision mismatch, \
                 or terminal status",
                goal_id
            ));
        }

        // The STORED status — re-read inside the same transaction so the
        // audit decision below can never claim a status the row does not
        // carry (the activation guard may have written `budget_limited`
        // instead of a requested `active`).
        let stored_status: String = tx.query_row(
            "SELECT status FROM goals WHERE goal_id = ?1",
            params![goal_id],
            |row| row.get(0),
        )?;

        // Step 2: Append finding (if provided) — uses SHARED validated insert
        if let Some(f) = finding {
            Self::insert_finding_validated(&tx, f, goal_id)?;
        }

        // Step 3: Append decision (if provided) — ONLY when the stored status
        // matches the caller's request; a guard override skips the insert so
        // the audit trail cannot contradict the goals row.
        if let Some(d) = decision {
            if stored_status == new_status {
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
        }

        tx.commit()?;
        Ok(stored_status)
    }

    /// #2066 round 3 (codex fix 1) — the CLEAR STAMP: per-column MAX-merge on
    /// counters, CASE on status, insert-if-absent. This writer deliberately
    /// drops the row-level all-or-nothing guard of [`Self::upsert_goal`] —
    /// that reject-whole-row shape is exactly what made the round-2 mixed
    /// model order-dependent (a same-millisecond stamp landing after the
    /// settle OVERWROTE the settled delta; a newer-timestamp settle made a
    /// later stamp reject and lose the entire pre-clear lag). With both the
    /// stamp and the settle MAX-merging on `tokens_used`, order-independence
    /// is arithmetic: for row lag `L`, frozen clear-time base `B`, late
    /// delta `D` — stamp→settle = `MAX(L,B)+D = B+D`; settle→stamp =
    /// `MAX(MAX(L,B)+D, B) = B+D` (deltas are positive); every interleaving
    /// of further deltas commutes the same way.
    ///
    /// Status: `cleared` unless the row is the stronger `complete` terminal
    /// (clearing an already-complete goal keeps the row `complete`).
    /// `objective`/`token_budget` take the clear-time values — the clear is
    /// this row's single stamp writer, and the settle never touches them.
    ///
    /// Returns the STORED status so the caller can gate its audit decision
    /// on what actually landed (`cleared` ⇒ append; `complete` ⇒ skip).
    pub fn stamp_goal_cleared(&self, goal: &Goal) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, continuations_used, revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 'cleared', ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(goal_id) DO UPDATE SET
                 objective = excluded.objective,
                 status = CASE WHEN goals.status = 'complete' THEN goals.status ELSE 'cleared' END,
                 tokens_used = MAX(goals.tokens_used, excluded.tokens_used),
                 token_budget = excluded.token_budget,
                 continuations_used = MAX(goals.continuations_used, excluded.continuations_used),
                 updated_at_ms = MAX(goals.updated_at_ms, excluded.updated_at_ms)",
            params![
                goal.goal_id,
                goal.objective,
                goal.tokens_used,
                goal.token_budget,
                goal.continuations_used,
                goal.revision,
                goal.created_at_ms,
                goal.updated_at_ms,
            ],
        )?;
        let stored: String = conn.query_row(
            "SELECT status FROM goals WHERE goal_id = ?1",
            params![goal.goal_id],
            |row| row.get(0),
        )?;
        Ok(stored)
    }

    /// #2066 round 2 (codex R6) / round 3 (codex fix 1 + 5) — GENUINELY
    /// counters-only settle for a post-clear turn charge: `tokens_used`
    /// MAX-merges the frozen clear-time base and adds the charge's DELTA,
    /// `updated_at_ms` only ever moves forward, and STATUS is never touched
    /// (round 3 removed the round-2 `cleared` CASE — the stamp owns the
    /// status dimension, and a settle must never stamp a row it races). The
    /// MAX-fold of the base is what makes settle-first converge: a stamp
    /// arriving later MAX-merges to the same total (see
    /// [`Self::stamp_goal_cleared`] for the arithmetic).
    ///
    /// Idempotency assumption: per-charge SINGLE DELIVERY — each in-process
    /// accountant charge settles exactly once (one offload per charge); this
    /// write has no dedupe key, so a replayed delta would double-count.
    ///
    /// Returns whether a row changed (false ⇒ no such goal row — the caller
    /// creates it first via [`Self::create_goal_if_absent`] and retries).
    pub fn settle_cleared_goal_cost_delta(
        &self,
        goal_id: &str,
        frozen_base_tokens: u64,
        tokens_delta: u64,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE goals SET tokens_used = MAX(tokens_used, ?1) + ?2,
                 updated_at_ms = MAX(updated_at_ms, ?3)
             WHERE goal_id = ?4",
            params![frozen_base_tokens, tokens_delta, updated_at_ms, goal_id],
        )?;
        Ok(changed > 0)
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

    /// #2063 — helper: a ledger with one goal row at the given spend/budget.
    /// Returns the tempdir alongside so the db file outlives the helper.
    fn ledger_with_goal(
        tokens_used: u64,
        token_budget: u64,
        status: &str,
    ) -> (tempfile::TempDir, GoalLedger) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = GoalLedger::open(&path).unwrap();
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "budget-guarded".to_string(),
                status: status.to_string(),
                tokens_used,
                token_budget,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            })
            .unwrap();
        (dir, ledger)
    }

    /// #2063 — `budget_limited` exists to stop spend, and whether a resume is
    /// safe depends on an arithmetic fact (`tokens_used < token_budget`) the
    /// status-only CAS cannot see in its caller: the in-memory guard evaluates
    /// MEMORY's counters, but the multi-process row can be AHEAD of them (the
    /// exact stale-lower-counters case the #1973 CAS retry exists for). The
    /// write itself must therefore refuse to produce an active-and-exhausted
    /// row: activating a row whose own counters are exhausted writes
    /// `budget_limited` instead, in the same statement.
    #[test]
    fn should_write_budget_limited_when_cas_activates_exhausted_row() {
        let (_dir, ledger) = ledger_with_goal(1_500, 1_000, "budget_limited");
        let changed = ledger
            .cas_goal_status("g1", "active", "budget_limited", 1000, 2000)
            .unwrap();
        assert!(
            changed,
            "the CAS fired (pair matched) — it wrote the SAFE status"
        );
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(
            row.status, "budget_limited",
            "activating an exhausted row must land budget_limited, never active-and-exhausted"
        );
        assert_eq!(row.updated_at_ms, 2000, "the stamp still lands");
    }

    /// #2063 — the legitimate resume: once the row's own budget covers its
    /// spend, the same CAS writes `active` unchanged.
    #[test]
    fn should_write_active_when_cas_activates_row_within_budget() {
        let (_dir, ledger) = ledger_with_goal(500, 1_000, "budget_limited");
        let changed = ledger
            .cas_goal_status("g1", "active", "budget_limited", 1000, 2000)
            .unwrap();
        assert!(changed);
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "active");
    }

    /// #2063 — same invariant on the snapshot writer: an incoming snapshot
    /// whose own arithmetic is exhausted must not land `active`, on either the
    /// insert arm (fresh row) or the update arm (existing row).
    #[test]
    fn should_flip_exhausted_active_snapshot_to_budget_limited_on_upsert() {
        let (_dir, ledger) = ledger_with_goal(500, 1_000, "active");
        // Update arm: the snapshot claims active but its own counters are
        // exhausted (e.g. the budget field was lowered below the spend).
        let admitted = ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "budget-guarded".to_string(),
                status: "active".to_string(),
                tokens_used: 1_500,
                token_budget: 1_000,
                continuations_used: 1,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 2000,
            })
            .unwrap();
        assert!(admitted);
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "budget_limited");
        assert_eq!(row.tokens_used, 1_500, "the counters land verbatim");
        // Insert arm: a fresh goal_id with an exhausted active snapshot.
        assert!(
            ledger
                .upsert_goal(&Goal {
                    goal_id: "g2".to_string(),
                    objective: "fresh but exhausted".to_string(),
                    status: "active".to_string(),
                    tokens_used: 900,
                    token_budget: 800,
                    continuations_used: 0,
                    revision: 0,
                    created_at_ms: 3000,
                    updated_at_ms: 3000,
                })
                .unwrap()
        );
        assert_eq!(
            ledger.get_goal("g2").unwrap().unwrap().status,
            "budget_limited",
            "the insert arm enforces the same arithmetic"
        );
        // Control: an under-budget active snapshot still lands active.
        assert!(
            ledger
                .upsert_goal(&Goal {
                    goal_id: "g1".to_string(),
                    objective: "budget-guarded".to_string(),
                    status: "active".to_string(),
                    tokens_used: 1_500,
                    token_budget: 5_000,
                    continuations_used: 1,
                    revision: 0,
                    created_at_ms: 1000,
                    updated_at_ms: 4000,
                })
                .unwrap()
        );
        assert_eq!(ledger.get_goal("g1").unwrap().unwrap().status, "active");
    }

    /// #2063 — the remaining status writers of the same family
    /// (`update_goal_status`, `commit_state_with_audit`) carry the identical
    /// guard: no code path may produce an active-and-exhausted row. A zero
    /// budget is exempt (matches the in-memory guards' `token_budget > 0`).
    #[test]
    fn should_write_budget_limited_when_revision_cas_activates_exhausted_row() {
        let (_dir, ledger) = ledger_with_goal(1_500, 1_000, "budget_limited");
        ledger.update_goal_status("g1", "active", 0, 2000).unwrap();
        assert_eq!(
            ledger.get_goal("g1").unwrap().unwrap().status,
            "budget_limited"
        );
        // Raise the budget above the spend (upsert), then the same transition
        // legitimately activates.
        ledger
            .upsert_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "budget-guarded".to_string(),
                status: "budget_limited".to_string(),
                tokens_used: 1_500,
                token_budget: 5_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 3000,
            })
            .unwrap();
        ledger.update_goal_status("g1", "active", 1, 4000).unwrap();
        assert_eq!(ledger.get_goal("g1").unwrap().unwrap().status, "active");
    }

    /// #2063 — `commit_state_with_audit`'s in-transaction status write is the
    /// same UPDATE shape; it must carry the same activation guard. #2066
    /// round 2 — the returned status is the STORED outcome, so the caller
    /// learns about the override.
    #[test]
    fn should_write_budget_limited_when_audited_commit_activates_exhausted_row() {
        let (_dir, ledger) = ledger_with_goal(1_500, 1_000, "budget_limited");
        let stored = ledger
            .commit_state_with_audit("g1", "active", 0, 2000, None, None)
            .unwrap();
        assert_eq!(stored, "budget_limited", "the caller sees what landed");
        assert_eq!(
            ledger.get_goal("g1").unwrap().unwrap().status,
            "budget_limited"
        );
    }

    /// #2066 round 2 (codex R3) — the CREATION writers enforce the same
    /// activation guard: the very first insert of an exhausted `active`
    /// snapshot lands `budget_limited`, on both `create_goal` and
    /// `create_goal_if_absent`.
    #[test]
    fn should_write_budget_limited_when_creation_writers_insert_exhausted_active() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        let exhausted = |goal_id: &str| Goal {
            goal_id: goal_id.to_string(),
            objective: "born exhausted".to_string(),
            status: "active".to_string(),
            tokens_used: 1_500,
            token_budget: 1_000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        ledger.create_goal(&exhausted("g1")).unwrap();
        assert_eq!(
            ledger.get_goal("g1").unwrap().unwrap().status,
            "budget_limited"
        );
        assert!(ledger.create_goal_if_absent(&exhausted("g2")).unwrap());
        assert_eq!(
            ledger.get_goal("g2").unwrap().unwrap().status,
            "budget_limited"
        );
    }

    /// #2066 round 2 (codex R3) — `cleared` is an immutable STATUS on every
    /// writer: an under-budget `active` write must not resurrect a cleared
    /// row, while the counters-only delta settle still accrues on it.
    #[test]
    fn should_refuse_to_resurrect_a_cleared_row() {
        let (_dir, ledger) = ledger_with_goal(500, 100_000, "cleared");
        // upsert: whole write refused (status clause), counters untouched.
        assert!(
            !ledger
                .upsert_goal(&Goal {
                    goal_id: "g1".to_string(),
                    objective: "resurrect?".to_string(),
                    status: "active".to_string(),
                    tokens_used: 9_000,
                    token_budget: 100_000,
                    continuations_used: 0,
                    revision: 0,
                    created_at_ms: 1000,
                    updated_at_ms: 5000,
                })
                .unwrap(),
            "an under-budget active snapshot must not resurrect a cleared row"
        );
        // status CAS: refused by the terminal clause.
        assert!(
            !ledger
                .cas_goal_status("g1", "active", "cleared", 1000, 5000)
                .unwrap()
        );
        // revision CAS: refused (same Err class as a revision mismatch).
        assert!(ledger.update_goal_status("g1", "active", 0, 5000).is_err());
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "cleared");
        assert_eq!(row.tokens_used, 500);
        // The counters-only settle still lands on the cleared tombstone.
        assert!(
            ledger
                .settle_cleared_goal_cost_delta("g1", 500, 4_000, 6000)
                .unwrap()
        );
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "cleared");
        assert_eq!(row.tokens_used, 4_500);
        assert_eq!(row.updated_at_ms, 6000);
    }

    /// #2066 round 2 (codex HIGH) — the audit row derives from the STORED
    /// outcome: when the activation guard overrides a requested `active` to
    /// `budget_limited`, the supplied decision is SKIPPED (an inserted
    /// "active" decision would contradict the goals row) and the returned
    /// status surfaces the override. A non-overridden transition still
    /// inserts its decision.
    #[test]
    fn should_skip_contradicting_decision_when_audited_commit_overrides_activation() {
        let (_dir, ledger) = ledger_with_goal(1_500, 1_000, "budget_limited");
        let decision = |id: &str, choice: &str| Decision {
            decision_id: id.to_string(),
            goal_id: "g1".to_string(),
            task_id: None,
            question: format!("transition goal to `{choice}`"),
            options_considered: None,
            choice: choice.to_string(),
            rationale: "test".to_string(),
            based_on_findings: None,
            based_on_rev: 0,
            decided_at_ms: 2000,
            decided_by: "tester".to_string(),
        };
        let stored = ledger
            .commit_state_with_audit(
                "g1",
                "active",
                0,
                2000,
                None,
                Some(&decision("d1", "active")),
            )
            .unwrap();
        assert_eq!(stored, "budget_limited");
        assert_eq!(
            ledger.count_decisions("g1").unwrap(),
            0,
            "a decision claiming `active` must not land beside a budget_limited row"
        );
        // Control: a legitimate transition (paused — no guard involvement)
        // inserts its decision and returns the requested status.
        let stored = ledger
            .commit_state_with_audit(
                "g1",
                "paused",
                1,
                3000,
                None,
                Some(&decision("d2", "paused")),
            )
            .unwrap();
        assert_eq!(stored, "paused");
        assert_eq!(ledger.count_decisions("g1").unwrap(), 1);
    }

    /// #2066 round 3 (codex fix 1) — THE order-independence pin, with the
    /// adversarial fixture the round-2 test masked (it seeded ledger == base):
    /// row lag `L=0` STRICTLY BELOW the frozen clear-time base `B=100`, late
    /// delta `D=10`, and ALL writers stamping the SAME millisecond (the tie
    /// that let the round-2 guarded upsert overwrite a settled delta). Every
    /// order must land exactly `B + ΣD` with status `cleared`:
    /// stamp→delta = `MAX(0,100)+10`; delta→stamp = `MAX(MAX(0,100)+10,100)`;
    /// delta-only then a late stamp retry converges the same way.
    #[test]
    fn should_settle_identical_rows_for_clear_stamp_and_delta_in_both_orders() {
        // Frozen clear-time snapshot: B=100 (the ledger row lags at L=0
        // because ordinary nonterminal turns deliberately never sync it).
        let clear_snapshot = Goal {
            goal_id: "g1".to_string(),
            objective: "both orders".to_string(),
            status: "cleared".to_string(),
            tokens_used: 100,
            token_budget: 100_000,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1000,
            updated_at_ms: 2000, // clear time == charge time (same-ms tie)
        };
        let stamp_cleared = |ledger: &GoalLedger| {
            let stored = ledger.stamp_goal_cleared(&clear_snapshot).unwrap();
            assert_eq!(stored, "cleared");
        };
        let settle = |ledger: &GoalLedger, delta: u64| {
            // The production settle: create-if-absent (frozen base), then the
            // MAX-merged delta — same-millisecond timestamp as the stamp.
            assert!(ledger.create_goal_if_absent(&clear_snapshot).is_ok());
            assert!(
                ledger
                    .settle_cleared_goal_cost_delta("g1", 100, delta, 2000)
                    .unwrap()
            );
        };

        // Order A: stamp, then delta.
        let (_dir_a, ledger_a) = ledger_with_goal(0, 100_000, "active");
        stamp_cleared(&ledger_a);
        settle(&ledger_a, 10);
        let row_a = ledger_a.get_goal("g1").unwrap().unwrap();

        // Order B: delta first, then the SAME-MILLISECOND stamp (the round-2
        // loss case: the all-or-nothing upsert admitted and overwrote 110
        // back to 100; the MAX-merge stamp must not).
        let (_dir_b, ledger_b) = ledger_with_goal(0, 100_000, "active");
        settle(&ledger_b, 10);
        stamp_cleared(&ledger_b);
        let row_b = ledger_b.get_goal("g1").unwrap().unwrap();

        assert_eq!(row_a.status, "cleared");
        assert_eq!(
            (row_a.status, row_a.tokens_used, row_a.updated_at_ms),
            (row_b.status, row_b.tokens_used, row_b.updated_at_ms),
            "the settled row must be identical in both arrival orders"
        );
        assert_eq!(
            row_b.tokens_used, 110,
            "B(100) + D(10): neither the late delta nor the pre-clear lag may be lost"
        );

        // Multi-delta interleaving: delta, delta, stamp — still B + ΣD.
        let (_dir_c, ledger_c) = ledger_with_goal(0, 100_000, "active");
        settle(&ledger_c, 10);
        settle(&ledger_c, 5);
        stamp_cleared(&ledger_c);
        assert_eq!(
            ledger_c.get_goal("g1").unwrap().unwrap().tokens_used,
            115,
            "B(100) + D1(10) + D2(5) in every interleaving"
        );

        // Fix 5 pin: the settle is GENUINELY counters-only — before the stamp
        // lands, a settled row keeps whatever status it had (the stamp owns
        // the status dimension).
        let (_dir_d, ledger_d) = ledger_with_goal(0, 100_000, "active");
        settle(&ledger_d, 10);
        assert_eq!(
            ledger_d.get_goal("g1").unwrap().unwrap().status,
            "active",
            "a settle racing ahead of the stamp must not flip status"
        );
        stamp_cleared(&ledger_d);
        assert_eq!(ledger_d.get_goal("g1").unwrap().unwrap().status, "cleared");
    }

    /// #2066 round 2 (codex R6) — the settle sequence on a goal whose ledger
    /// row does not exist yet: the delta alone matches nothing; the caller's
    /// create-if-absent + delta sequence creates the cleared tombstone (the
    /// frozen base INSERT carries status `cleared`) and lands the charge.
    #[test]
    fn should_create_the_cleared_row_when_settling_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        assert!(
            !ledger
                .settle_cleared_goal_cost_delta("g1", 500, 4_000, 3000)
                .unwrap(),
            "a bare delta on a missing row changes nothing"
        );
        assert!(
            ledger
                .create_goal_if_absent(&Goal {
                    goal_id: "g1".to_string(),
                    objective: "late row".to_string(),
                    status: "cleared".to_string(),
                    tokens_used: 500,
                    token_budget: 100_000,
                    continuations_used: 0,
                    revision: 0,
                    created_at_ms: 1000,
                    updated_at_ms: 2000,
                })
                .unwrap()
        );
        assert!(
            ledger
                .settle_cleared_goal_cost_delta("g1", 500, 4_000, 3000)
                .unwrap()
        );
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.tokens_used), ("cleared", 4_500));
    }

    /// #2066 round 3 (codex fix 1) — the stamp keeps `complete` (terminal
    /// parity) while still MAX-merging counters, and reports the stored
    /// status so the caller's decision gate can skip the audit row.
    #[test]
    fn should_keep_complete_when_stamping_cleared_over_a_complete_row() {
        let (_dir, ledger) = ledger_with_goal(500, 100_000, "complete");
        let stored = ledger
            .stamp_goal_cleared(&Goal {
                goal_id: "g1".to_string(),
                objective: "clear a finished goal".to_string(),
                status: "cleared".to_string(),
                tokens_used: 700,
                token_budget: 100_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1000,
                updated_at_ms: 2000,
            })
            .unwrap();
        assert_eq!(stored, "complete", "complete is the stronger terminal");
        let row = ledger.get_goal("g1").unwrap().unwrap();
        assert_eq!(row.status, "complete");
        assert_eq!(row.tokens_used, 700, "counters still MAX-merge");
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
        // #2055 review round 4 — `complete` after an OWNER-reported `failed`
        // is refused: the plain writer maps `"failed"` to `FinalFailure`
        // (rank 2), which outranks `Completion` (rank 1). Only an
        // observer-provisional failure (rank 0, written through
        // `settle_task_status`) admits the later completion. Blanket
        // failed→complete admission would let a genuinely-cancelled row
        // (cancellation also lands as `failed`) be flipped by a racing
        // completion from another supervisor copy (#2060).
        assert!(!ledger.update_task_status("t1", "complete", 4_000).unwrap());

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "failed");
        assert_eq!(task.updated_at_ms, 2_000);
    }

    /// #2055 review round 4 — the authority-gated correction: an
    /// observer-provisional failure (rank 0) admits exactly the owner's
    /// later completion (rank 1, task_supervisor.rs:2527's correction
    /// semantics); afterwards the row refuses provisional redeliveries,
    /// non-terminal refreshes, and completion redeliveries — only a FINAL
    /// failure (rank 2) can still land, which is the deliberate
    /// order-independence rule verified exhaustively below.
    #[test]
    fn should_allow_failed_to_complete_only_with_provisional_authority() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(
            ledger
                .settle_task_status("t1", TaskSettleAuthority::ProvisionalFailure, 2_000)
                .unwrap(),
            "the provisional failure lands at rank 0"
        );
        assert!(
            ledger.update_task_status("t1", "complete", 3_000).unwrap(),
            "failed → complete is admitted over a provisional verdict"
        );
        assert!(
            !ledger
                .settle_task_status("t1", TaskSettleAuthority::ProvisionalFailure, 4_000)
                .unwrap(),
            "a straggler provisional write cannot undo the correction"
        );
        assert!(
            !ledger.update_task_status("t1", "complete", 4_000).unwrap(),
            "a completion redelivery is a no-op (first wins within a rank)"
        );
        for late in ["running", "pending"] {
            assert!(
                !ledger.update_task_status("t1", late, 4_000).unwrap(),
                "a settled row refuses the non-terminal refresh {late:?}"
            );
        }

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "complete");
        assert_eq!(task.updated_at_ms, 3_000);
        // The stored rank is the completion's. Same-module test, so the raw
        // column read is fine.
        let authority: i64 = ledger
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT authority FROM tasks WHERE task_id = 't1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(authority, 1);
    }

    /// #2055 review round 4 — the owner's authoritative re-failure closes
    /// the correction window: rank 2 lands over the provisional rank 0,
    /// after which the completion (rank 1) is refused. A provisional
    /// redelivery in between is a no-op, not a retention refresh.
    #[test]
    fn should_close_correction_window_when_owner_confirms_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        seed_goal_with_task(&ledger, "g1", "t1", "running");

        assert!(
            ledger
                .settle_task_status("t1", TaskSettleAuthority::ProvisionalFailure, 2_000)
                .unwrap()
        );
        // A provisional redelivery is a no-op, not a churn write.
        assert!(
            !ledger
                .settle_task_status("t1", TaskSettleAuthority::ProvisionalFailure, 2_500)
                .unwrap()
        );
        // The owner's confirming failure outranks the provisional verdict.
        assert!(
            ledger.update_task_status("t1", "failed", 3_000).unwrap(),
            "the owner-final failure is admitted over the provisional rank"
        );
        assert!(
            !ledger.update_task_status("t1", "complete", 4_000).unwrap(),
            "after the owner confirmed the failure, the completion is refused"
        );

        let task = ledger.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, "failed");
        assert_eq!(task.updated_at_ms, 3_000);
    }

    /// #2055 review round 4 — the load-bearing order-independence matrix:
    /// with P = provisional failure, C = completion, D = final failure,
    /// EVERY delivery order containing a D ends `failed`, and P/C alone end
    /// `complete` in either order. The write layer alone guarantees this —
    /// no delivery-order assumptions anywhere above it.
    #[test]
    fn should_converge_every_authority_delivery_order() {
        use TaskSettleAuthority::{Completion, FinalFailure, ProvisionalFailure};
        let three_event_orders: [[TaskSettleAuthority; 3]; 6] = [
            [ProvisionalFailure, Completion, FinalFailure],
            [ProvisionalFailure, FinalFailure, Completion],
            [Completion, ProvisionalFailure, FinalFailure],
            [Completion, FinalFailure, ProvisionalFailure],
            [FinalFailure, ProvisionalFailure, Completion],
            [FinalFailure, Completion, ProvisionalFailure],
        ];
        for (index, order) in three_event_orders.iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
            seed_goal_with_task(&ledger, "g1", "t1", "running");
            for authority in order {
                let _ = ledger.settle_task_status("t1", *authority, 2_000).unwrap();
            }
            let task = ledger.get_task("t1").unwrap().unwrap();
            assert_eq!(
                task.status, "failed",
                "order #{index} {order:?} contains a final failure and must end failed"
            );
        }
        for order in [
            [ProvisionalFailure, Completion],
            [Completion, ProvisionalFailure],
        ] {
            let dir = tempfile::tempdir().unwrap();
            let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
            seed_goal_with_task(&ledger, "g1", "t1", "running");
            for authority in order {
                let _ = ledger.settle_task_status("t1", authority, 2_000).unwrap();
            }
            let task = ledger.get_task("t1").unwrap().unwrap();
            assert_eq!(
                task.status, "complete",
                "{order:?} has no final failure and must end complete"
            );
        }
    }

    /// #2055 review round 4 — the `authority` column arrives via the
    /// migration in `open_with_busy_retry` (never the common `open`, which
    /// runs on tokio workers). Pre-existing terminal rows are stamped FINAL
    /// — a legacy `failed` row must NOT become correctable — while legacy
    /// non-terminal rows settle exactly like fresh ones.
    #[test]
    fn should_migrate_tasks_table_missing_the_authority_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        // A pre-#2055 database shape: tasks table with neither `authority`
        // nor the short-lived round-3 `correctable`.
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
                 INSERT INTO tasks VALUES ('legacy-failed', 'g1', '', '', 'failed', NULL, 1, 1);
                 INSERT INTO tasks VALUES ('legacy-live', 'g1', '', '', 'running', NULL, 1, 1);",
            )
            .unwrap();
        }

        let ledger = GoalLedger::open_with_busy_retry(&path).unwrap();
        assert!(
            !ledger
                .update_task_status("legacy-failed", "complete", 2_000)
                .unwrap(),
            "a legacy failed row is stamped FINAL by the migration backfill \
             and must not become correctable"
        );
        assert_eq!(
            ledger.get_task("legacy-failed").unwrap().unwrap().status,
            "failed"
        );
        // A legacy live row settles exactly like a fresh one.
        assert!(
            ledger
                .settle_task_status(
                    "legacy-live",
                    TaskSettleAuthority::ProvisionalFailure,
                    2_000
                )
                .unwrap()
        );
        assert!(
            ledger
                .update_task_status("legacy-live", "complete", 3_000)
                .unwrap(),
            "the provisional-then-correction flow works on a migrated ledger"
        );
    }

    /// #2055 review round 4 — a database created from the short-lived
    /// round-3 shape (with `correctable`) migrates too: the column is
    /// dropped and its terminal rows are stamped FINAL alongside the
    /// `authority` addition.
    #[test]
    fn should_migrate_round3_correctable_column_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
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
                     correctable INTEGER NOT NULL DEFAULT 0,
                     FOREIGN KEY (goal_id) REFERENCES goals(goal_id));
                 INSERT INTO goals VALUES ('g1', 'ship', 'active', 0, 1000, 0, 0, 1, 1);
                 INSERT INTO tasks VALUES ('t1', 'g1', '', '', 'failed', NULL, 1, 1, 1);",
            )
            .unwrap();
        }

        let ledger = GoalLedger::open_with_busy_retry(&path).unwrap();
        let correctable_exists: i64 = ledger
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'correctable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(correctable_exists, 0, "the round-3 column is dropped");
        assert!(
            !ledger.update_task_status("t1", "complete", 2_000).unwrap(),
            "the round-3 terminal row is stamped FINAL, not carried over as correctable"
        );
    }

    /// #2055 review round 4/5 — a migration failure propagates; nothing is
    /// swallowed by error-message matching (steps are decided by schema
    /// inspection). Forced here by presenting a connection whose `tasks`
    /// table does not exist.
    #[test]
    fn should_propagate_non_duplicate_migration_failures() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        // No `tasks` table at all: the inspection reports the column absent
        // and the in-transaction ALTER fails with "no such table".
        let error =
            GoalLedger::migrate_tasks_authority_column(&mut conn, Path::new("unused-in-memory"))
                .expect_err("a migration failure must propagate");
        assert!(
            error.to_string().contains("no such table"),
            "unexpected error: {error}"
        );
        // And on an already-migrated table the helper is a clean no-op that
        // takes no write transaction at all (fast path).
        conn.execute_batch(
            "CREATE TABLE tasks (
                 task_id TEXT PRIMARY KEY, goal_id TEXT NOT NULL,
                 title TEXT NOT NULL, detail TEXT NOT NULL,
                 status TEXT NOT NULL, assigned_peer TEXT,
                 created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
                 authority INTEGER NOT NULL DEFAULT -1);",
        )
        .unwrap();
        GoalLedger::migrate_tasks_authority_column(&mut conn, Path::new("unused-in-memory"))
            .expect("an already-migrated table is a no-op");
    }

    /// The raw pre-#2055 schema (no `authority`, no `correctable`), used by
    /// the atomicity fixtures below.
    fn create_legacy_schema(conn: &rusqlite::Connection, extra: &str) {
        conn.execute_batch(&format!(
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
                 created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL{extra},
                 FOREIGN KEY (goal_id) REFERENCES goals(goal_id));
             INSERT INTO goals VALUES ('g1', 'ship', 'active', 0, 1000, 0, 0, 1, 1);
             INSERT INTO tasks (task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms)
             VALUES ('legacy-failed', 'g1', '', '', 'failed', NULL, 1, 1);",
        ))
        .unwrap();
    }

    /// #2055 review round 5 (migration atomicity) — a failure AFTER the
    /// ALTER-ADD rolls the WHOLE migration back: the pre-migration shape is
    /// restored, so the next opener re-runs everything instead of seeing
    /// the column present and skipping the terminal backfill forever. The
    /// mid-migration failure is forced with an aborting trigger on the
    /// backfill's UPDATE — a real table, a real partial-failure point.
    #[test]
    fn should_roll_back_whole_migration_when_a_step_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            create_legacy_schema(&conn, "");
            conn.execute_batch(
                "CREATE TRIGGER abort_backfill BEFORE UPDATE ON tasks
                 BEGIN SELECT RAISE(ABORT, 'backfill aborted by test trigger'); END;",
            )
            .unwrap();
        }

        let error = GoalLedger::open_with_busy_retry(&path)
            .err()
            .expect("the aborted backfill must fail the open");
        assert!(
            error.to_string().contains("backfill aborted"),
            "unexpected error: {error}"
        );
        // Partial state must NOT persist: the ALTER-ADD was rolled back
        // together with the failed backfill.
        let conn = rusqlite::Connection::open(&path).unwrap();
        let authority_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'authority'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            authority_present, 0,
            "a failed migration must leave the pre-migration shape intact"
        );

        // With the trigger gone, the SAME database migrates cleanly and the
        // legacy terminal row is FINAL — the backfill was not lost.
        conn.execute_batch("DROP TRIGGER abort_backfill;").unwrap();
        drop(conn);
        let ledger = GoalLedger::open_with_busy_retry(&path).unwrap();
        assert!(
            !ledger
                .update_task_status("legacy-failed", "complete", 2_000)
                .unwrap(),
            "the re-run migration stamps the legacy terminal row FINAL"
        );
    }

    /// #2055 review round 5 — a second opener after a completed migration is
    /// a clean no-op (the fast path sees the migrated shape and takes no
    /// write transaction).
    #[test]
    fn should_treat_second_open_after_migration_as_clean_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            create_legacy_schema(&conn, "");
        }
        let first = GoalLedger::open_with_busy_retry(&path).expect("first open migrates");
        drop(first);
        let second = GoalLedger::open_with_busy_retry(&path).expect("second open is a no-op");
        assert!(
            !second
                .update_task_status("legacy-failed", "complete", 2_000)
                .unwrap(),
            "the migrated FINAL stamp survives the second open"
        );
    }

    /// #2055 review round 5 — a dependent index on the round-3 column makes
    /// the DROP fail LOUDLY (the old substring match ate exactly this error),
    /// and the failure rolls back the whole migration.
    #[test]
    fn should_propagate_drop_failure_when_an_index_depends_on_the_old_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            create_legacy_schema(&conn, ", correctable INTEGER NOT NULL DEFAULT 0");
            conn.execute_batch("CREATE INDEX idx_tasks_correctable ON tasks(correctable);")
                .unwrap();
        }

        let error = GoalLedger::open_with_busy_retry(&path)
            .err()
            .expect("a dependent index must fail the column drop loudly");
        assert!(
            error.to_string().contains("index"),
            "unexpected error: {error}"
        );
        // The whole migration rolled back: `correctable` is still there and
        // `authority` never landed.
        let conn = rusqlite::Connection::open(&path).unwrap();
        let count_columns = |name: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count_columns("correctable"), 1);
        assert_eq!(count_columns("authority"), 0);
    }

    /// #2055 review rounds 6/7/9 (V3/W3/Y4) — creation-vs-migration
    /// concurrency pin, deterministic BY CONSTRUCTION: the STRUCTURAL
    /// argument is what closes the race (the creation APIs run inspection +
    /// insert inside `BEGIN IMMEDIATE`, which serializes against the
    /// migration's own `BEGIN IMMEDIATE` transaction, so a creation either
    /// fully precedes the migration — legacy shape, then the backfill
    /// stamps the row FINAL — or fully follows it, taking the
    /// derived-authority shape; the poisoned interleaving cannot be
    /// scheduled). This test PROVES contention with a single-threaded
    /// busy-probe rather than timing: while a test-only hook parks the
    /// migration transaction verifiably open, a zero-busy-timeout terminal
    /// creation ON THE TEST THREAD must fail with lock contention — an
    /// assertion no thread schedule can fake or break — and after release
    /// the same creation succeeds with its real rank.
    #[test]
    fn should_never_leave_terminal_rows_unranked_when_creation_races_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            create_legacy_schema(&conn, "");
        }
        // The probe's connection is opened at test start, BEFORE the hook is
        // installed and the migration can take the write lock, so the open
        // itself (a plain `GoalLedger::open` — no migration attempt, which
        // would deadlock against the parked transaction) cannot block on
        // anything below.
        let probe_ledger = GoalLedger::open(&path).unwrap();

        // Install the in-transaction hook: signals the test, then blocks
        // this migration open until released.
        let (in_transaction_tx, in_transaction_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        *GoalLedger::migration_mid_transaction_hook().lock().unwrap() = Some((
            path.clone(),
            Arc::new(move || {
                let _ = in_transaction_tx.send(());
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(30));
            }),
        ));

        let migration_path = path.clone();
        let migration =
            std::thread::spawn(move || GoalLedger::open_with_busy_retry(migration_path));
        in_transaction_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the migration transaction is provably open");

        // Round 9 — the busy-probe, ON THIS THREAD, deterministic by
        // construction: with the hook parked the migration transaction is
        // provably holding the write lock AT THIS INSTANT, so a zero-
        // busy-timeout terminal creation can only fail SQLITE_BUSY — and it
        // can only fail SQLITE_BUSY because that lock is held. Both sides
        // of the interleaving are sequenced by the test thread itself: no
        // schedule can fake the contention and no deschedule can break it.
        // (The previous timing shape could pass under a valid schedule that
        // descheduled a creator thread between its begun-signal and its
        // create call, running the whole creation post-release — timestamps
        // cannot prove blocking.)
        probe_ledger
            .conn
            .lock()
            .unwrap()
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let probe_error = probe_ledger
            .create_task(&Task {
                task_id: "race-task".to_string(),
                goal_id: "g1".to_string(),
                title: String::new(),
                detail: String::new(),
                status: "failed".to_string(),
                assigned_peer: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .expect_err("a zero-timeout creation must observe the held migration lock");
        assert!(
            error_is_lock_contention(&probe_error),
            "the probe must fail with lock contention, not a structural error: {probe_error}"
        );

        release_tx.send(()).expect("release the migration hook");
        let migrated = migration
            .join()
            .expect("migration thread")
            .expect("migration open succeeds");
        *GoalLedger::migration_mid_transaction_hook().lock().unwrap() = None;

        // With the migration committed, the same terminal creation succeeds
        // on a normal-timeout connection and takes the derived-authority
        // shape.
        migrated
            .create_task(&Task {
                task_id: "race-task".to_string(),
                goal_id: "g1".to_string(),
                title: String::new(),
                detail: String::new(),
                status: "failed".to_string(),
                assigned_peer: None,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .expect("creation succeeds after the migration commits");

        // And the outcome invariant: no terminal row at authority -1 — the
        // legacy row was backfilled FINAL and the post-migration creation
        // took the derived-authority shape.
        let conn = migrated.conn.lock().unwrap();
        let unranked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks
                 WHERE status IN ('complete', 'failed') AND authority = -1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unranked, 0, "no terminal row may ever end at authority -1");
        let raced_authority: i64 = conn
            .query_row(
                "SELECT authority FROM tasks WHERE task_id = 'race-task'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            raced_authority, 2,
            "the racing terminal creation lands post-migration with its real rank"
        );
    }

    /// #2055 review round 5 (terminal-creation authority) — a row CREATED
    /// with a terminal status starts at the matching authority rank, so the
    /// nonterminal-refresh guard cannot let a plain `running` write
    /// overwrite it (the reproduced `failed/-1 → running/-1` hole).
    #[test]
    fn should_refuse_running_refresh_on_freshly_created_terminal_row() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        ledger
            .create_goal(&Goal {
                goal_id: "g1".to_string(),
                objective: "ship".to_string(),
                status: "active".to_string(),
                tokens_used: 0,
                token_budget: 10_000,
                continuations_used: 0,
                revision: 0,
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .unwrap();
        let terminal_task = |id: &str, status: &str| Task {
            task_id: id.to_string(),
            goal_id: "g1".to_string(),
            title: String::new(),
            detail: String::new(),
            status: status.to_string(),
            assigned_peer: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        };
        ledger
            .create_task_if_absent(&terminal_task("t-failed", "failed"))
            .unwrap();
        ledger
            .create_task(&terminal_task("t-complete", "complete"))
            .unwrap();

        for late in ["running", "pending"] {
            assert!(
                !ledger.update_task_status("t-failed", late, 2_000).unwrap(),
                "a created terminal row refuses the nonterminal refresh {late:?}"
            );
        }
        assert_eq!(
            ledger.get_task("t-failed").unwrap().unwrap().status,
            "failed"
        );
        // Rank consistency: a created `failed` is FINAL (refuses completion),
        // a created `complete` sits at the completion rank (a final failure
        // still outranks it, per the order-independence rule).
        assert!(
            !ledger
                .update_task_status("t-failed", "complete", 2_000)
                .unwrap()
        );
        assert!(
            !ledger
                .update_task_status("t-complete", "running", 2_000)
                .unwrap()
        );
        assert!(
            ledger
                .settle_task_status("t-complete", TaskSettleAuthority::FinalFailure, 2_000)
                .unwrap()
        );
    }

    /// #2055 review round 4 — fresh-schema and migrated-schema databases
    /// behave identically under the same write sequence.
    #[test]
    fn should_behave_identically_on_fresh_and_migrated_schemas() {
        let dir = tempfile::tempdir().unwrap();
        // Fresh: created by the current schema batch.
        let fresh = GoalLedger::open_with_busy_retry(dir.path().join("fresh.db")).unwrap();
        // Migrated: created without the column, then opened through the
        // migrating profile.
        let migrated_path = dir.path().join("migrated.db");
        {
            let conn = rusqlite::Connection::open(&migrated_path).unwrap();
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
                     FOREIGN KEY (goal_id) REFERENCES goals(goal_id));",
            )
            .unwrap();
        }
        let migrated = GoalLedger::open_with_busy_retry(&migrated_path).unwrap();

        for ledger in [&fresh, &migrated] {
            seed_goal_with_task(ledger, "g1", "t1", "running");
            assert!(
                ledger
                    .settle_task_status("t1", TaskSettleAuthority::ProvisionalFailure, 2_000)
                    .unwrap()
            );
            assert!(ledger.update_task_status("t1", "complete", 3_000).unwrap());
            assert!(
                ledger
                    .settle_task_status("t1", TaskSettleAuthority::FinalFailure, 4_000)
                    .unwrap(),
                "a final failure outranks the completion on both shapes"
            );
            let task = ledger.get_task("t1").unwrap().unwrap();
            assert_eq!(task.status, "failed");
            assert_eq!(task.updated_at_ms, 4_000);
        }
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
