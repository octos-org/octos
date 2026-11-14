// SQLite-backed durable ledger for goals, tasks, findings, and escalations.
//
// Unlike redb (single-writer-single-process), SQLite supports multi-process
// access via WAL mode, making it suitable for the peer agent architecture
// where master/PM/peers run as independent processes sharing the same ledger.

use eyre::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct GoalLedger {
    conn: Arc<Mutex<Connection>>,
    /// #2085 — latched POSITIVE result of [`Self::goals_has_time_column`]
    /// for this connection, so `get_goal` stops paying a
    /// `pragma_table_info` round trip before every point lookup once the
    /// column has been observed. The capability is MONOTONE per connection:
    /// `ALTER TABLE … ADD COLUMN` is the only schema transition and nothing
    /// ever drops the column, so a committed "yes" can never become "no" in
    /// a later snapshot. A NEGATIVE result is deliberately never latched —
    /// an unmigrated file can migrate mid-connection through a writer's
    /// in-transaction [`Self::ensure_goals_time_column`], and a reader that
    /// latched "no" would synthesize `time_used_seconds: 0` forever.
    goals_time_column_seen: std::sync::atomic::AtomicBool,
    /// #2085 — number of `pragma_table_info` probes `get_goal` actually
    /// executed. Lets tests pin "the probe is skipped once latched"
    /// structurally instead of by timing.
    goals_time_probe_count: std::sync::atomic::AtomicU64,
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
    /// #2068 — the goal's WALL-CLOCK spend, the second cost dimension every
    /// accountant already charges in memory (`record_goal_turn_internal`,
    /// `charge_goal_tokens_gated`) and persists to the supervisor store. It
    /// had no ledger column at all, so this conversion silently dropped it
    /// and the durable row could only ever answer half the cost question.
    ///
    /// `#[serde(default)]` so a `Goal` value serialized before the field
    /// existed still deserializes (as zero — the honest "never recorded",
    /// which is also what the schema migration's column default writes).
    #[serde(default)]
    pub time_used_seconds: u64,
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
            // #2068 — `goals.time_used_seconds` migrates under the same rule
            // and in the same place. The two are INDEPENDENT transactions on
            // purpose: each is idempotent and schema-inspected, so a failure
            // of one leaves the other's committed result perfectly valid and
            // the next attempt simply skips it through its fast path.
            let attempt_result = Self::open_inner(path, Some(std::time::Duration::from_secs(1)))
                .and_then(|ledger| {
                    {
                        let mut conn = ledger.conn.lock().unwrap();
                        Self::migrate_tasks_authority_column(&mut conn, path)?;
                        Self::migrate_goals_time_column(&mut conn)?;
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
            goals_time_column_seen: std::sync::atomic::AtomicBool::new(false),
            goals_time_probe_count: std::sync::atomic::AtomicU64::new(0),
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
                updated_at_ms INTEGER NOT NULL,
                -- #2068: the goal's WALL-CLOCK spend, the second cost
                -- dimension the accountants charge alongside `tokens_used`.
                -- Declared LAST on purpose: `ALTER TABLE … ADD COLUMN`
                -- appends, so a freshly created table and a migrated legacy
                -- one end up with the IDENTICAL column order and no reader
                -- can observe which generation of the schema it opened.
                time_used_seconds INTEGER NOT NULL DEFAULT 0
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

            -- #20b (main-tree sovereignty): a generic key/value sidecar for
            -- small instance-scoped facts that must ride the goal-ledger and
            -- survive a restart. The first (and currently only) key is
            -- `main_tree_owner`, recording the goal id that first landed a
            -- non-default branch on the main tree; the shell guard consults
            -- it to refuse cross-branch checkouts from other goals.
            CREATE TABLE IF NOT EXISTS ledger_kv (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
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
        //
        // #2068 — `goals.time_used_seconds` has NO migration here either.
        // Its EAGER migration also lives in `open_with_busy_retry`; but
        // unlike `tasks.authority`, the goals writers are NOT confinable to
        // that profile (the clear stamp, the post-clear settle and the
        // sentinel-completion sync all open plainly), and on those TERMINAL
        // paths a fallback that merely avoids erroring would lose the value
        // forever. So each WRITER additionally calls
        // `ensure_goals_time_column` inside the write transaction it already
        // holds — see that function for why the DDL is safe there and why
        // the reader (`get_goal`) is deliberately excluded.
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

    /// #2068 — whether the `goals` table carries the wall-clock column yet.
    /// Same schema-inspection contract as [`Self::tasks_has_column`]: it
    /// never references the column itself, so it is safe on every schema
    /// generation, and it answers `false` (rather than erroring) on a table
    /// that does not exist at all.
    ///
    /// Every goals statement that names `time_used_seconds` is gated on this,
    /// because a pre-#2068 ledger file stays UNMIGRATED under the plain
    /// [`Self::open`] profile by design — see [`Self::create_tables`].
    fn goals_has_time_column(conn: &Connection) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('goals') WHERE name = 'time_used_seconds'",
            [],
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

    /// #2068 — bring an existing `goals` table onto the `time_used_seconds`
    /// schema, using the SAME transactional, schema-inspected shape as
    /// [`Self::migrate_tasks_authority_column`] (#2059):
    ///
    /// - the step is decided by `pragma_table_info`, never by matching an
    ///   error-message substring — `"duplicate column name"` matching would
    ///   also swallow REAL failures;
    /// - the ALTER runs inside one `BEGIN IMMEDIATE` transaction, so a
    ///   failure rolls back to the pre-migration shape and the next opener
    ///   re-runs it rather than seeing a half-migrated schema forever;
    /// - the pre-transaction fast path skips the write lock entirely on an
    ///   already-migrated database, and the check is REPEATED inside the
    ///   transaction because two blocking-context openers can race the fast
    ///   path and only the in-transaction view is serialized.
    ///
    /// NO backfill, unlike the `authority` migration: a legacy row's
    /// wall-clock spend was never recorded anywhere in the ledger, so there
    /// is nothing to derive it from and the column default (`0`) is the only
    /// honest value. The counter is monotonic and MAX-merged by its writers,
    /// so a legacy row simply resumes accruing from zero — it never
    /// contradicts a later, larger figure.
    ///
    /// Called ONLY from [`Self::open_with_busy_retry`] — the same
    /// blocking-contexts-only rule #2055/#2059 established: `goal_get` and
    /// every inline reader open through the plain [`Self::open`] on tokio
    /// worker threads, and migration DDL takes a write lock that must never
    /// run there.
    fn migrate_goals_time_column(conn: &mut Connection) -> Result<()> {
        if Self::goals_has_time_column(conn)? {
            return Ok(());
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::ensure_goals_time_column(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// #2068 round 2 (codex H6) — the LAZY half of the same migration: bring
    /// the column into existence from inside a write transaction the CALLER
    /// already holds. Every goals writer calls this before its statement, so
    /// no write can land on an unmigrated table and silently drop the
    /// dimension.
    ///
    /// ## Why the eager migration alone was not enough
    ///
    /// It runs only in [`Self::open_with_busy_retry`], and the TERMINAL
    /// writes do not open that way: the clear stamp
    /// (`stamp_goal_cleared_blocking`), the post-clear settle
    /// (`offload_cleared_goal_settle`) and the sentinel-completion transition
    /// all use the plain [`Self::open`]. `create_tables` is
    /// `CREATE TABLE IF NOT EXISTS`, so a pre-#2068 file is left completely
    /// untouched by them. A legacy-shape fallback keeps those writes from
    /// erroring — but on a terminal path "did not error" and "recorded the
    /// value" are different outcomes: clearing replaces the supervisor record
    /// with a zero-time tombstone, the goal is terminal, and nothing ever
    /// re-syncs it. The seconds are gone permanently, and a later migration
    /// only adds the column's default zero. Every deployment that predates
    /// this change would have had a `time_used_seconds` column that stayed 0
    /// forever.
    ///
    /// ## Why running this DDL here is safe
    ///
    /// #2055/#2059 established that migration DDL must stay off tokio worker
    /// threads. The invariant that rule protects is that a READER never has
    /// to take a write lock — `goal_get` and every inline consult open
    /// plainly on an executor worker, and #2059's authority migration is a
    /// `BEGIN IMMEDIATE` transaction plus a table-wide backfill `UPDATE`,
    /// which is why it runs ONLY under [`Self::open_with_busy_retry`] and
    /// never under the plain `open` a reader uses. (An earlier revision of
    /// this comment said that migration had been added *to* the reader path;
    /// it never was.) [`Self::get_goal`] runs no DDL here either — it takes
    /// only a DEFERRED read transaction, for snapshot consistency rather than
    /// for writing.
    ///
    /// For a WRITER the calculus is different in kind, not degree. It is
    /// already inside `BEGIN IMMEDIATE`, so it already holds the RESERVED
    /// lock and already paid the wait to get it — this adds no lock
    /// acquisition. `ALTER TABLE … ADD COLUMN` is O(1) in SQLite (it rewrites
    /// the schema row and treats the value as absent-defaulted in existing
    /// rows; it does not rewrite the table), and there is NO backfill, so the
    /// marginal work is one small schema write, once per file, gated by the
    /// inspection below.
    ///
    /// The alternative considered and rejected: move the clear stamp and the
    /// sentinel transition onto the blocking pool so they could use
    /// [`Self::open_with_busy_retry`]. Both run synchronously inside
    /// `handle_raw_appui_rpc` / the session actor, so that is a threading and
    /// ordering change (the clear RPC would return before its durable row was
    /// stamped) — strictly larger and riskier than one O(1) DDL statement
    /// inside a write transaction those paths already take.
    fn ensure_goals_time_column(tx: &rusqlite::Transaction) -> Result<()> {
        if Self::goals_has_time_column(tx)? {
            return Ok(());
        }
        tx.execute(
            "ALTER TABLE goals ADD COLUMN time_used_seconds INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        Ok(())
    }

    /// #2068 — the `INSERT INTO goals … VALUES …` head shared by
    /// [`Self::create_goal`], [`Self::upsert_goal`] and
    /// [`Self::create_goal_if_absent`]: identical column list, identical
    /// #2063 activation guard, `time_used_seconds` bound as `?10`.
    ///
    /// Round 2 (codex H6): there is no longer a legacy variant of this
    /// statement. Every caller runs [`Self::ensure_goals_time_column`] inside
    /// its write transaction first, so the column is guaranteed to exist by
    /// the time this executes — a writer that merely avoids ERRORING on a
    /// pre-#2068 file while silently dropping the value is the failure mode,
    /// not the fix.
    const GOALS_INSERT_HEAD: &'static str =
        "INSERT INTO goals (goal_id, objective, status, tokens_used, token_budget, \
         continuations_used, revision, created_at_ms, updated_at_ms, time_used_seconds)
         VALUES (?1, ?2,
             CASE WHEN ?3 = 'active' AND ?5 > 0 AND ?4 >= ?5
             THEN 'budget_limited' ELSE ?3 END,
             ?4, ?5, ?6, ?7, ?8, ?9, ?10)";

    /// The ten positional bindings [`Self::GOALS_INSERT_HEAD`] expects.
    fn goals_insert_params(goal: &Goal) -> [&dyn rusqlite::ToSql; 10] {
        [
            &goal.goal_id,
            &goal.objective,
            &goal.status,
            &goal.tokens_used,
            &goal.token_budget,
            &goal.continuations_used,
            &goal.revision,
            &goal.created_at_ms,
            &goal.updated_at_ms,
            &goal.time_used_seconds,
        ]
    }

    /// Create a new goal.
    /// #2066 round 2 (codex R3) — creation writers carry the same #2063
    /// activation guard as the update writers: an `active` snapshot whose own
    /// arithmetic is exhausted lands `budget_limited`. No code path may
    /// produce an active-and-exhausted row, including the very first insert.
    ///
    /// #2068 — the schema step and the insert run inside ONE `BEGIN IMMEDIATE`
    /// transaction, for the same reason [`Self::create_task`] does it: as two
    /// autocommit statements a concurrent migration could commit in between.
    /// `BEGIN IMMEDIATE` serializes against the eager migration's own
    /// `BEGIN IMMEDIATE`, so the pair runs wholly before or wholly after it.
    pub fn create_goal(&self, goal: &Goal) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::ensure_goals_time_column(&tx)?;
        tx.execute(
            Self::GOALS_INSERT_HEAD,
            Self::goals_insert_params(goal).as_slice(),
        )?;
        tx.commit()?;
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
    /// #2068 — `time_used_seconds` joins the SET list as a per-column
    /// MAX-merge, and the three admission clauses above are deliberately left
    /// alone. Adding a fourth (`excluded.time_used_seconds >=
    /// goals.time_used_seconds`) would make the guard STRICTER and newly
    /// REJECT whole writes that land today: the counters are advanced
    /// independently by each process's own in-memory record, so a peer
    /// snapshot can legitimately carry higher tokens and lower seconds, and a
    /// row-level rejection would throw away its token update too. The
    /// MAX-merge gives the new dimension the same monotonic protection clause
    /// 2 gives tokens, without touching WHICH writes are admitted.
    ///
    /// ## KNOWN LOSS — a stale writer base drops an increment (#2083)
    ///
    /// The MAX-merge keeps the stored value from going BACKWARDS; it does not
    /// guarantee the increment lands. This upsert takes an ABSOLUTE snapshot,
    /// so when the writer's own base lags the durable row the increment is
    /// unrecoverable from the two values SQL can see. Reachable today:
    /// `persist_goal_state_with_store` discards store-write errors
    /// (`let _ = store.append_event(…)`) and goal restoration trusts the
    /// supervisor metadata it finds (`supervisor_metadata_u64(…,
    /// "time_used_seconds").unwrap_or(0)`), so memory can restart behind the
    /// ledger. Row `time = 20`, memory restored at `10`, a turn adds `5`
    /// (`charge_goal_tokens_gated`) → snapshot `15`, admitted because the
    /// TOKEN clause passes, and `MAX(20, 15)` stores `20` when the true
    /// cumulative value is `25`.
    ///
    /// This is a property of the absolute-snapshot sync, not of time: a
    /// lagging TOKEN base makes clause 2 reject the whole write instead, which
    /// also fails to record the increment. Fixing it properly means
    /// delta-aware settlement, or reconciling the writer's base against the
    /// durable row inside one transaction — a change to how the orchestrator
    /// syncs, tracked in #2083 and deliberately out of scope here. Until then,
    /// ordinary NON-cleared time accounting can under-count by one turn's
    /// increment whenever the supervisor base lags the ledger. The cleared-goal
    /// settle path does NOT have this hole: it writes true deltas
    /// ([`Self::settle_cleared_goal_cost_delta`]).
    pub fn upsert_goal(&self, goal: &Goal) -> Result<bool> {
        const TAIL: &str = "
             ON CONFLICT(goal_id) DO UPDATE SET
                 objective = excluded.objective,
                 status = excluded.status,
                 tokens_used = excluded.tokens_used,
                 token_budget = excluded.token_budget,
                 continuations_used = excluded.continuations_used,
                 time_used_seconds = MAX(goals.time_used_seconds, excluded.time_used_seconds),
                 updated_at_ms = excluded.updated_at_ms
             WHERE excluded.updated_at_ms >= goals.updated_at_ms
               AND excluded.tokens_used >= goals.tokens_used
               AND NOT (goals.status = 'complete' AND excluded.status <> 'complete')
               AND NOT (goals.status = 'cleared' AND excluded.status <> 'cleared')";
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::ensure_goals_time_column(&tx)?;
        let admitted = tx.execute(
            &format!("{}{TAIL}", Self::GOALS_INSERT_HEAD),
            Self::goals_insert_params(goal).as_slice(),
        )?;
        tx.commit()?;
        Ok(admitted > 0)
    }

    /// #2083 — apply a turn's counter DELTAS additively under one Immediate
    /// transaction, instead of upserting an absolute snapshot that drops the
    /// increment when the writer's in-memory base lags the durable row.
    ///
    /// `upsert_goal` MAX-merges time and admits by tokens; both are wrong for
    /// a charge whose caller's base may be stale (restore-from-metadata lag).
    /// This write adds the turn's tokens/time to whatever the row currently
    /// holds, so the increment survives regardless of the caller's base.
    /// Status, objective, and budget are NOT touched — the caller is a charge
    /// path, not a transition. `updated_at_ms` MAX-merges (monotonic clock).
    ///
    /// Returns whether a row was updated (false ⇒ no such goal row; the
    /// caller is expected to have synced one already).
    pub fn charge_goal_counters(
        &self,
        goal_id: &str,
        tokens_delta: u64,
        time_delta_seconds: u64,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::ensure_goals_time_column(&tx)?;
        let changed = tx.execute(
            "UPDATE goals SET tokens_used = tokens_used + ?1,
                 time_used_seconds = time_used_seconds + ?2,
                 updated_at_ms = MAX(updated_at_ms, ?3)
             WHERE goal_id = ?4",
            params![tokens_delta, time_delta_seconds, updated_at_ms, goal_id],
        )?;
        tx.commit()?;
        Ok(changed > 0)
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
        const TAIL: &str = "\n             ON CONFLICT(goal_id) DO NOTHING";
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::ensure_goals_time_column(&tx)?;
        let inserted = tx.execute(
            &format!("{}{TAIL}", Self::GOALS_INSERT_HEAD),
            Self::goals_insert_params(goal).as_slice(),
        )?;
        tx.commit()?;
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
    ///
    /// #2068 — the schema probe and the SELECT it chooses run inside ONE
    /// DEFERRED transaction, so they observe a single database state.
    ///
    /// They used to run as two bare statements, on the reasoning that a
    /// migration committing between them was benign because the legacy shape
    /// still executes and reports the dimension as `0`, "which is exactly the
    /// value the freshly-added column holds anyway". That was wrong: the
    /// `ALTER` and the writer's non-zero row write **commit together** in the
    /// writer's `BEGIN IMMEDIATE` (see [`Self::ensure_goals_time_column`]), so
    /// after that commit the column does NOT hold its default. A reader that
    /// probed before it and selects after would return the writer's fresh
    /// `status` / `tokens_used` alongside a synthesized `time_used_seconds: 0`
    /// — a `Goal` that never existed as one database state. Narrow (only on an
    /// existing ledger's first migration, since `has_time` is true forever
    /// after) but a torn read all the same.
    ///
    /// DEFERRED is what makes this safe to add here. The invariant #2055/#2059
    /// protect is that a READER never takes a write lock — `goal_get` and every
    /// inline consult open plainly on a tokio worker. A deferred transaction
    /// acquires only a shared read lock, and only on first access, so this
    /// still runs no DDL and takes no write lock. It is rolled back on drop,
    /// which for a read is a no-op.
    ///
    /// #2085 — once one probe has answered "yes", later calls skip the
    /// `pragma_table_info` round trip via `goals_time_column_seen` (see the
    /// field doc). That does NOT reopen the torn-read window above: the
    /// window was probe-says-NO in one state + SELECT in a later state; a
    /// latched YES means the column existed in a COMMITTED state this
    /// connection already observed, and column existence is monotone, so it
    /// exists in this transaction's snapshot too and the SELECT reads the
    /// real value. While the answer is still "no", the probe continues to
    /// run — inside the same DEFERRED transaction as the SELECT, exactly as
    /// before.
    pub fn get_goal(&self, goal_id: &str) -> Result<Option<Goal>> {
        use std::sync::atomic::Ordering;
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let has_time = if self.goals_time_column_seen.load(Ordering::Acquire) {
            true
        } else {
            self.goals_time_probe_count.fetch_add(1, Ordering::Relaxed);
            let probed = Self::goals_has_time_column(&tx)?;
            if probed {
                self.goals_time_column_seen.store(true, Ordering::Release);
            }
            probed
        };
        let sql = if has_time {
            "SELECT goal_id, objective, status, tokens_used, token_budget, continuations_used, \
             revision, created_at_ms, updated_at_ms, time_used_seconds
             FROM goals WHERE goal_id = ?1"
        } else {
            "SELECT goal_id, objective, status, tokens_used, token_budget, continuations_used, \
             revision, created_at_ms, updated_at_ms
             FROM goals WHERE goal_id = ?1"
        };
        let goal = {
            let mut stmt = tx.prepare(sql)?;
            let mut rows = stmt.query_map(params![goal_id], |row| {
                Ok(Goal {
                    goal_id: row.get(0)?,
                    objective: row.get(1)?,
                    status: row.get(2)?,
                    tokens_used: row.get(3)?,
                    token_budget: row.get(4)?,
                    time_used_seconds: if has_time { row.get(9)? } else { 0 },
                    continuations_used: row.get(5)?,
                    revision: row.get(6)?,
                    created_at_ms: row.get(7)?,
                    updated_at_ms: row.get(8)?,
                })
            })?;
            rows.next().transpose()?
        };
        Ok(goal)
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

    /// #2056 — every task row of `goal_id` a reconciliation pass may still
    /// legitimately move, paired with the authority rank it currently holds,
    /// oldest first. The cut is `authority < 1`
    /// ([`TaskSettleAuthority::Completion`]'s rank), which is deliberately NOT
    /// the same thing as "no verdict yet":
    ///
    /// - **rank −1** (no verdict) — any terminal verdict outranks it.
    /// - **rank 0** (observer-provisional failure) — explicitly correctable:
    ///   the owner that watched the worker finish may still land a completion
    ///   over it, which is the behaviour
    ///   `should_allow_failed_to_complete_only_with_provisional_authority`
    ///   pins. Excluding rank 0 would strand exactly that correction whenever
    ///   its delivery is lost, which is the failure a reconcile exists for.
    /// - **ranks 1 and 2** (completion, final failure) — excluded. The only
    ///   write that could still be admitted over a completion is a final
    ///   failure, and at restore time the only way a supervisor produces one
    ///   for a task the ledger already completed is the orphan sweep reaping a
    ///   row whose completion append was lost — i.e. spurious. Within a single
    ///   supervisor the terminal guard forbids completed → failed outright, so
    ///   nothing legitimate is lost by refusing to look.
    ///
    /// The returned rank lets the caller skip rows its own verdict would not
    /// outrank, so a reconcile that owes nothing issues no write at all;
    /// [`Self::settle_task_status`]'s strictly-greater rule remains the
    /// correctness guarantee underneath.
    ///
    /// Scoped to one goal, and each goal owns its own ledger file. Note the
    /// available index is `(goal_id, status)`, so `goal_id` is served by the
    /// index prefix and the `authority` predicate is applied as a filter over
    /// that goal's rows — the scan is bounded by the goal's TOTAL task count,
    /// not by the candidate count.
    ///
    /// Requires the migrated schema (the `authority` column), i.e. a
    /// connection from [`Self::open_with_busy_retry`].
    pub fn tasks_open_to_correction(&self, goal_id: &str) -> Result<Vec<(Task, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT task_id, goal_id, title, detail, status, assigned_peer, created_at_ms, updated_at_ms, authority
             FROM tasks WHERE goal_id = ?1 AND authority < ?2 ORDER BY created_at_ms, task_id",
        )?;
        let rows = stmt.query_map(
            params![goal_id, TaskSettleAuthority::Completion.rank()],
            |row| {
                Ok((
                    Task {
                        task_id: row.get(0)?,
                        goal_id: row.get(1)?,
                        title: row.get(2)?,
                        detail: row.get(3)?,
                        status: row.get(4)?,
                        assigned_peer: row.get(5)?,
                        created_at_ms: row.get(6)?,
                        updated_at_ms: row.get(7)?,
                    },
                    row.get(8)?,
                ))
            },
        )?;
        let mut tasks = Vec::new();
        for task in rows {
            tasks.push(task?);
        }
        Ok(tasks)
    }

    /// #2056 — the persisted terminal-authority rank of one task row, or
    /// `None` when the row does not exist. `-1` means no terminal verdict has
    /// been recorded; see [`TaskSettleAuthority::rank`] for the ordering.
    pub fn task_authority(&self, task_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT authority FROM tasks WHERE task_id = ?1")?;
        let mut rows = stmt.query(params![task_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// #2056 — SQLite's `PRAGMA data_version` for this connection: a counter
    /// that changes when a DIFFERENT connection commits to the database, and
    /// never for this connection's own commits. Lets a caller (or a test)
    /// prove that some other writer landed nothing at all, rather than
    /// inferring it from the absence of a visible change.
    pub fn data_version(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let version = conn.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        Ok(version)
    }

    /// #20b (main-tree sovereignty) — the ledger key under which the
    /// main-tree owner goal id is stored.
    pub const MAIN_TREE_OWNER_KEY: &'static str = "main_tree_owner";

    /// Read a value from the `ledger_kv` sidecar; `Ok(None)` when the key was
    /// never written. Backed by `CREATE TABLE IF NOT EXISTS` in `open`, so
    /// every generation of the ledger file answers this read.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT value FROM ledger_kv WHERE key = ?1")?;
        let value = stmt
            .query_row(params![key], |row| row.get::<_, String>(0))
            .optional()?;
        Ok(value)
    }

    /// #34c — value + claim timestamp for the multi-ledger sovereignty scan.
    /// The scan must pick the EARLIEST claim across ledgers (first-writer-
    /// wins ACROSS files, not just within one), so it needs the row's
    /// `updated_at_ms`, which `kv_get` alone does not expose.
    pub fn kv_get_with_time(&self, key: &str) -> Result<Option<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT value, updated_at_ms FROM ledger_kv WHERE key = ?1")?;
        let value = stmt
            .query_row(params![key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .optional()?;
        Ok(value)
    }

    /// Write a value into the `ledger_kv` sidecar, first-writer-wins when
    /// `overwrite` is false: a pre-existing row is left untouched and the
    /// EXISTING value is returned, so a racing second claimant observes the
    /// true owner rather than silently overwriting it. Returns the value now
    /// stored under `key` (the caller's on a fresh write, the incumbent's on
    /// a lost race / `overwrite = false`).
    pub fn kv_put(
        &self,
        key: &str,
        value: &str,
        overwrite: bool,
        updated_at_ms: i64,
    ) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        if overwrite {
            conn.execute(
                "INSERT INTO ledger_kv (key, value, updated_at_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
                params![key, value, updated_at_ms],
            )?;
            return Ok(value.to_owned());
        }
        // INSERT OR IGNORE, then read back the winner — one connection mutex
        // holds both statements, so the read-back always observes the row the
        // claim resolved to.
        conn.execute(
            "INSERT OR IGNORE INTO ledger_kv (key, value, updated_at_ms) VALUES (?1, ?2, ?3)",
            params![key, value, updated_at_ms],
        )?;
        let stored: String = conn.query_row(
            "SELECT value FROM ledger_kv WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )?;
        Ok(stored)
    }

    /// #20b — convenience readers for the main-tree owner goal. `None` until
    /// a goal claims the tree; persists across restarts with the ledger file.
    pub fn main_tree_owner_goal(&self) -> Result<Option<String>> {
        self.kv_get(Self::MAIN_TREE_OWNER_KEY)
    }

    /// #20b — claim main-tree ownership for `goal_id`, first-writer-wins.
    /// Returns the goal id that actually owns the tree after the call (the
    /// incumbent's id when the tree was already claimed by another goal).
    pub fn claim_main_tree_owner(&self, goal_id: &str, updated_at_ms: i64) -> Result<String> {
        self.kv_put(Self::MAIN_TREE_OWNER_KEY, goal_id, false, updated_at_ms)
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
    /// #2068 — `time_used_seconds` is MAX-merged here for exactly the same
    /// reason, and its settle MAX-folds its own frozen base: the counter is
    /// monotonic per goal_id, the row lags the clear-time base the same way,
    /// and the identical arithmetic makes the two writers commute.
    ///
    /// Status: `cleared` unless the row is the stronger `complete` terminal
    /// (clearing an already-complete goal keeps the row `complete`).
    /// `objective`/`token_budget` take the clear-time values — the clear is
    /// this row's single stamp writer, and the settle never touches them.
    ///
    /// Returns the STORED status so the caller can gate its audit decision
    /// on what actually landed (`cleared` ⇒ append; `complete` ⇒ skip).
    pub fn stamp_goal_cleared(&self, goal: &Goal) -> Result<String> {
        const STAMP: &str = "INSERT INTO goals (goal_id, objective, status, tokens_used, \
             token_budget, continuations_used, revision, created_at_ms, updated_at_ms, \
             time_used_seconds)
             VALUES (?1, ?2, 'cleared', ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(goal_id) DO UPDATE SET
                 objective = excluded.objective,
                 status = CASE WHEN goals.status = 'complete' THEN goals.status ELSE 'cleared' END,
                 tokens_used = MAX(goals.tokens_used, excluded.tokens_used),
                 token_budget = excluded.token_budget,
                 continuations_used = MAX(goals.continuations_used, excluded.continuations_used),
                 time_used_seconds = MAX(goals.time_used_seconds, excluded.time_used_seconds),
                 updated_at_ms = MAX(goals.updated_at_ms, excluded.updated_at_ms)";
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // #2068 round 2 (codex H6) — THE terminal write. On a pre-#2068 file
        // a fallback that dropped the dimension here lost it permanently:
        // clearing replaces the supervisor record with a zero-time tombstone
        // and nothing re-syncs a terminal goal.
        Self::ensure_goals_time_column(&tx)?;
        tx.execute(
            STAMP,
            params![
                goal.goal_id,
                goal.objective,
                goal.tokens_used,
                goal.token_budget,
                goal.continuations_used,
                goal.revision,
                goal.created_at_ms,
                goal.updated_at_ms,
                goal.time_used_seconds,
            ],
        )?;
        tx.commit()?;
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
    /// #2068 — `time_used_seconds` settles the SAME way, and the MAX-fold is
    /// REQUIRED for it, not merely copied from tokens. Two properties carry
    /// the argument, both independently true of seconds: the counter is
    /// per-goal MONOTONIC (every accountant `saturating_add`s it, and a
    /// replacement goal mints a fresh goal_id), and each late charge is a
    /// positive increment. The SQL then commutes for ANY row value `L` —
    /// stamp→settle = `MAX(L,B)+D`; settle→stamp = `MAX(MAX(L,B)+D, B) =
    /// MAX(L,B)+D`, since `MAX(L,B)+D ≥ B` for `D ≥ 0`. `L ≤ B` (the row
    /// lags the frozen base, because nonterminal turns never sync it) is the
    /// operational reality but is NOT needed for the identity — round 2
    /// (codex H1) corrected an earlier version of this comment that leaned
    /// on it.
    ///
    /// PLAIN accumulation (`time_used_seconds = time_used_seconds + D`) would
    /// be order-DEPENDENT in exactly the common case: settle-first stores
    /// `L+D`, and the later stamp's MAX-merge then collapses it to `B`
    /// whenever `L+D ≤ B` — silently eating the delta — while stamp-first
    /// stores `B+D`. The lag is a whole goal's history and the delta is one
    /// turn, so `L+D ≤ B` is the NORMAL case, not a corner.
    ///
    /// SCOPE OF THE GUARANTEE (#2084): this commutes over DELIVERED deltas
    /// only. A tombstone evicted at the cap (or purged past its horizon)
    /// makes a later charge resolve to nothing, and this write's own
    /// failures are logged and dropped — so with a drop in the mix the
    /// settled row IS order-dependent (`L=0, B=100, D=10`:
    /// settle→evict→stamp = 110; stamp→evict→settle = 100). Durable charge
    /// identities with retry/dedupe are tracked in #2084.
    ///
    /// Returns whether a row changed (false ⇒ no such goal row — the caller
    /// creates it first via [`Self::create_goal_if_absent`] and retries).
    pub fn settle_cleared_goal_cost_delta(
        &self,
        goal_id: &str,
        frozen_base_tokens: u64,
        tokens_delta: u64,
        frozen_base_time_seconds: u64,
        time_delta: u64,
        updated_at_ms: u64,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // #2068 round 2 (codex H6) — the other terminal write; same reason as
        // the clear stamp. This is the LAST chance to record the in-flight
        // turn's seconds on a goal that is already gone from memory.
        Self::ensure_goals_time_column(&tx)?;
        let changed = tx.execute(
            "UPDATE goals SET tokens_used = MAX(tokens_used, ?1) + ?2,
                 time_used_seconds = MAX(time_used_seconds, ?5) + ?6,
                 updated_at_ms = MAX(updated_at_ms, ?3)
             WHERE goal_id = ?4",
            params![
                frozen_base_tokens,
                tokens_delta,
                updated_at_ms,
                goal_id,
                frozen_base_time_seconds,
                time_delta
            ],
        )?;
        tx.commit()?;
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
mod test_support {
    use super::{Finding, Goal, GoalLedger, Task};
    use tempfile::TempDir;

    pub fn open_ledger() -> (TempDir, GoalLedger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = GoalLedger::open(dir.path().join("ledger.db")).unwrap();
        (dir, ledger)
    }

    pub fn goal(id: &str) -> Goal {
        Goal {
            goal_id: id.into(),
            objective: "test".into(),
            status: "active".into(),
            tokens_used: 0,
            token_budget: 10_000,
            time_used_seconds: 0,
            continuations_used: 0,
            revision: 0,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    pub fn task(id: &str, goal_id: &str) -> Task {
        Task {
            task_id: id.into(),
            goal_id: goal_id.into(),
            title: "test task".into(),
            detail: "test".into(),
            status: "pending".into(),
            assigned_peer: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        }
    }

    pub fn finding(id: &str, goal_id: &str) -> Finding {
        Finding {
            rowid: None,
            finding_id: id.into(),
            seq: 1,
            task_id: None,
            goal_id: goal_id.into(),
            kind: "observation".into(),
            lifecycle: "verified".into(),
            confidence: "high".into(),
            review_state: "peer_reviewed".into(),
            assertion: "test".into(),
            evidence: None,
            config_version: None,
            derived_from: None,
            supersedes: Vec::new(),
            cost_tokens: 0,
            created_at_ms: 2_000,
            created_by: "peer-a".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{finding, goal, open_ledger, task};
    use super::*;

    #[test]
    fn sqlite_ledger_multi_process_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");

        // Process 1: create and write
        let ledger1 = GoalLedger::open(&path).unwrap();
        let goal = Goal {
            objective: "test goal".into(),
            ..goal("g1")
        };
        ledger1.create_goal(&goal).unwrap();

        // Process 2: open the same file (WAL mode allows this)
        let ledger2 = GoalLedger::open(&path).unwrap();
        let retrieved = ledger2.get_goal("g1").unwrap().unwrap();
        assert_eq!(retrieved.objective, "test goal");
        assert_eq!(retrieved.status, "active");
    }

    /// #1964 — `list_decisions` round-trips appended decisions for ONE goal in
    /// decided_at order. The fleet-convergence tests (octos-cli) use it to
    /// assert an eager fleet terminal appended exactly one audit decision.
    #[test]
    fn list_decisions_round_trips_per_goal_in_order() {
        let (_dir, ledger) = open_ledger();
        for gid in ["g1", "g2"] {
            ledger.create_goal(&goal(gid)).unwrap();
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

    /// #2068 — an OLD serialized `Goal` (JSON written before the field
    /// existed) must still deserialize; `#[serde(default)]` makes the absent
    /// field read as zero instead of failing the whole value.
    #[test]
    fn should_default_time_used_seconds_when_deserializing_a_pre_2068_goal() {
        let legacy = r#"{
            "goal_id": "g1",
            "objective": "ship",
            "status": "active",
            "tokens_used": 7,
            "token_budget": 1000,
            "continuations_used": 2,
            "revision": 3,
            "created_at_ms": 1,
            "updated_at_ms": 2
        }"#;
        let goal: Goal = serde_json::from_str(legacy).expect("a pre-#2068 Goal must deserialize");
        assert_eq!(goal.time_used_seconds, 0);
        assert_eq!(goal.tokens_used, 7, "the surviving fields are unchanged");
        assert_eq!(goal.continuations_used, 2);
        assert_eq!(goal.revision, 3);
    }

    /// #2083 — the charge path settles a turn's spend as an ADDITIVE delta,
    /// so a writer whose in-memory base lags the durable row never loses the
    /// increment (the absolute-snapshot `upsert_goal` stores
    /// `MAX(row, base + delta)` for time and admits by tokens, silently
    /// dropping the turn whenever the row is ahead). Independent scopes: the
    /// lagging-base scenario and the no-row skip.
    #[test]
    fn should_settle_charge_deltas_additively_when_the_writers_base_lags_the_row() {
        {
            let (_dir, ledger) = open_ledger();
            // The durable row is already ahead (20/20) — e.g. synced by a
            // fresher lifetime before this writer restored at a stale base.
            ledger
                .create_goal(&Goal {
                    tokens_used: 20,
                    time_used_seconds: 20,
                    ..goal("g1")
                })
                .unwrap();
            // The lagging writer's turn spent 5 more: the delta lands on the
            // row as it stands — not on the writer's stale base (which would
            // store 15), nor by MAX (which would keep 20); the true total is
            // 25.
            assert!(ledger.charge_goal_counters("g1", 5, 5, 2_000).unwrap());
            let row = ledger.get_goal("g1").unwrap().unwrap();
            assert_eq!(row.tokens_used, 25);
            assert_eq!(row.time_used_seconds, 25);
            assert_eq!(row.updated_at_ms, 2_000);
            assert_eq!(row.status, "active", "a charge never touches status");
            assert_eq!(row.objective, "test", "a charge never touches objective");
            // `updated_at_ms` MAX-merges: an older clock never rewinds it.
            assert!(ledger.charge_goal_counters("g1", 1, 1, 1_500).unwrap());
            let row = ledger.get_goal("g1").unwrap().unwrap();
            assert_eq!(row.tokens_used, 26);
            assert_eq!(row.updated_at_ms, 2_000);
            // The writer's later absolute transition sync still carries its
            // stale base: the monotonic admission clause rejects it, leaving
            // the additively-settled counters intact.
            assert!(
                !ledger
                    .upsert_goal(&Goal {
                        tokens_used: 15,
                        time_used_seconds: 15,
                        updated_at_ms: 3_000,
                        ..goal("g1")
                    })
                    .unwrap(),
                "a snapshot whose token base lags the row must not be admitted"
            );
            let row = ledger.get_goal("g1").unwrap().unwrap();
            assert_eq!(row.tokens_used, 26);
            assert_eq!(row.time_used_seconds, 26);
        }
        {
            let (_dir, ledger) = open_ledger();
            // No row yet (the goal's first charge preceded its first
            // transition sync): the charge skips and must NOT create one —
            // row creation stays with the absolute sync.
            assert!(!ledger.charge_goal_counters("ghost", 5, 5, 2_000).unwrap());
            assert!(ledger.get_goal("ghost").unwrap().is_none());
        }
    }

    #[test]
    fn append_finding_rejects_cross_goal_task() {
        let (_dir, ledger) = open_ledger();

        // Create two goals
        for gid in ["g1", "g2"] {
            ledger.create_goal(&goal(gid)).unwrap();
        }
        ledger.create_task(&task("t1", "g1")).unwrap();

        // Try to append a finding for g2 that references g1's task
        let finding = Finding {
            task_id: Some("t1".into()), // t1 belongs to g1, not g2
            ..finding("f1", "g2")
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
    fn commit_state_with_audit_atomicity() {
        // Success and rollback need independent stores: reusing the completed
        // goal would make terminal status, not the intended CAS revision
        // mismatch, responsible for the second outcome.
        {
            let (_dir, ledger) = open_ledger();
            ledger.create_goal(&goal("g1")).unwrap();
            let finding = Finding {
                assertion: "all tests pass".into(),
                ..finding("f1", "g1")
            };
            ledger
                .commit_state_with_audit("g1", "complete", 0, 3000, Some(&finding), None)
                .unwrap();
            let goal = ledger.get_goal("g1").unwrap().unwrap();
            assert_eq!(goal.status, "complete");
            assert_eq!(goal.revision, 1);
            let findings = ledger.list_findings_since("g1", 0).unwrap();
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].assertion, "all tests pass");
        }
        {
            let (_dir, ledger) = open_ledger();
            ledger
                .create_goal(&Goal {
                    revision: 5,
                    ..goal("g1")
                })
                .unwrap();
            let finding = finding("f1", "g1");
            let result =
                ledger.commit_state_with_audit("g1", "complete", 0, 3000, Some(&finding), None);
            assert!(result.is_err(), "expected revision 0, actual revision 5");
            let goal = ledger.get_goal("g1").unwrap().unwrap();
            assert_eq!(goal.status, "active");
            assert_eq!(goal.revision, 5);
            assert!(ledger.list_findings_since("g1", 0).unwrap().is_empty());
        }
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
    use super::test_support::{finding, goal, open_ledger, task};
    use super::*;

    #[test]
    fn digest_from_ledger_end_to_end() {
        let (_dir, ledger) = open_ledger();
        ledger
            .create_goal(&Goal {
                objective: "test digest".into(),
                ..goal("g1")
            })
            .unwrap();

        // Add findings
        for i in 1..=5 {
            // Create task first (required by FK validation)
            ledger
                .create_task(&Task {
                    title: format!("task {i}"),
                    ..task(&format!("task-{i}"), "g1")
                })
                .unwrap();
            let finding = Finding {
                seq: i,
                task_id: Some(format!("task-{i}")),
                assertion: format!("claim {i}"),
                cost_tokens: 100 * i,
                created_at_ms: 1000 + i * 100,
                ..finding(&format!("f{i}"), "g1")
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

    // #1945 — the goal_get `ledger_digest` read path reduces the digest to
    // COUNTS; this proves the counts it derives are right over a ledger with
    // mixed lifecycles, a supersession, and per-path cost. The lifecycle →
    // FindingStatus mapping is the `From<&Finding>` conversion above:
    // verified/reproduced → Confirmed, proposed/observed → Predicted,
    // refuted/superseded/retracted → RuledOut.
    #[test]
    fn digest_from_ledger_counts_mixed_lifecycles_and_supersession() {
        let (_dir, ledger) = open_ledger();
        ledger
            .create_goal(&Goal {
                objective: "count me".into(),
                ..goal("g1")
            })
            .unwrap();
        for (task_id, status) in [("t-run", "running"), ("t-done", "complete")] {
            ledger
                .create_task(&Task {
                    title: task_id.into(),
                    status: status.into(),
                    ..task(task_id, "g1")
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
                    seq: 0, // assigned by store
                    task_id: Some(task_id.into()),
                    kind: kind.into(),
                    lifecycle: lifecycle.into(),
                    confidence: "medium".into(),
                    review_state: "unreviewed".into(),
                    assertion: format!("claim {finding_id}"),
                    supersedes: supersedes.into_iter().map(str::to_string).collect(),
                    cost_tokens: cost,
                    ..finding(finding_id, "g1")
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

    // #1945 codex round — AGGREGATE UNIT TEST (direct-SQL seeding is fine
    // here; the integration-shaped tests in octos-cli drive the production
    // writer instead). The lifecycle/kind/cost aggregates must count EVERY
    // finding of the goal, including `task_id = NULL` rows — exactly the rows
    // ordinary peers write (peer_handoff stages no fleet task) and the rows
    // the digest's per-path cost roll-up drops on the floor.

    // #1961 — an answered escalation must become `resolved` in the ledger.

    // #1957 — upsert_goal writes REAL fields on insert and refreshes the mutable
    // ones on conflict, so the ledger goals-row is no longer a stale placeholder.

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
            objective: "obj".into(),
            tokens_used,
            token_budget: 2000,
            updated_at_ms,
            ..goal("g1")
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
        ledger.create_goal(&goal(goal_id)).unwrap();
        ledger
            .create_task(&Task {
                title: String::new(),
                detail: String::new(),
                status: status.into(),
                ..task(task_id, goal_id)
            })
            .unwrap();
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

    /// #2068 review — `get_goal`'s schema probe and its SELECT must observe
    /// ONE database state.
    ///
    /// The first revision ran them as two bare statements, reasoning that a
    /// migration landing between them was benign because the legacy shape
    /// still executes and reports `0`, "which is exactly the value the
    /// freshly-added column holds anyway". The `ALTER` and the writer's
    /// non-zero row write commit TOGETHER, so after that commit the column
    /// does not hold its default: a reader that probed before it and selected
    /// after returned fresh `tokens_used` beside a synthesized
    /// `time_used_seconds: 0` — a `Goal` that never existed as one state.
    ///
    /// The writer below only ever writes rows with BOTH dimensions non-zero,
    /// so any observation of `tokens_used > 0 && time_used_seconds == 0` is a
    /// torn read by construction, and no timing assumption is needed to
    /// interpret it.
    ///
    /// This is a race-window test: it drives the window rather than pausing
    /// inside it, so it is not a proof. It is kept because it reproduced the
    /// defect on the unfixed reader on every attempt, and because the
    /// structural argument (probe and SELECT inside one DEFERRED transaction)
    /// is what actually closes it.
    #[test]
    fn should_never_read_a_torn_goal_when_a_writer_migrates_concurrently() {
        for _round in 0..12 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("torn.db");
            {
                let conn = rusqlite::Connection::open(&path).unwrap();
                create_legacy_schema(&conn, "");
            }

            // Both handles are opened BEFORE the threads start: `open` runs
            // `create_tables`, which needs the write lock, and the plain-open
            // profile carries a zero busy timeout by design (#2059) so it
            // must not race another open.
            let writer_ledger = GoalLedger::open(&path).unwrap();
            let reader = GoalLedger::open(&path).unwrap();

            let writer = std::thread::spawn(move || {
                // Both dimensions non-zero, in one transaction that also
                // performs the ALTER.
                writer_ledger
                    .upsert_goal(&Goal {
                        goal_id: "g1".into(),
                        objective: "ship".into(),
                        status: "active".into(),
                        tokens_used: 4_242,
                        token_budget: 1_000_000,
                        time_used_seconds: 77,
                        continuations_used: 0,
                        revision: 1,
                        created_at_ms: 1,
                        updated_at_ms: 2,
                    })
                    .unwrap();
            });

            let mut torn = None;
            for _ in 0..600 {
                if let Some(goal) = reader.get_goal("g1").unwrap()
                    && goal.tokens_used > 0
                    && goal.time_used_seconds == 0
                {
                    torn = Some(goal);
                    break;
                }
            }
            writer.join().unwrap();

            assert!(
                torn.is_none(),
                "get_goal returned a goal that never existed as one database \
                 state: tokens_used={} with time_used_seconds=0, while the only \
                 writer writes both non-zero in a single transaction",
                torn.map(|g| g.tokens_used).unwrap_or_default(),
            );
        }
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

    // ---------------------------------------------------------------------
    // #2055 — idempotent row creation at task registration.
    //
    // Registration legitimately repeats (relaunch, restart, re-registration
    // of a task the supervisor restored), so row creation must be an upsert
    // that PRESERVES an existing row — including its status and timestamps,
    // and including a row that already went terminal — instead of relying on
    // a swallowed UNIQUE violation.
    // ---------------------------------------------------------------------

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
}
