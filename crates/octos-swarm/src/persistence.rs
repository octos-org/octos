//! Session-durable state for swarm dispatches.
//!
//! The primitive owns its own redb database under the caller-provided
//! data directory (default `<session_dir>/swarm-state.redb`). Each
//! dispatch writes a single [`DispatchRecord`] row keyed by
//! `dispatch_id`. The record carries every per-subtask state
//! transition, so a supervisor that restarts mid-dispatch can re-open
//! the ledger and resume.
//!
//! This follows the same pattern M6.5's credential pool + the episode
//! store use: redb is synchronous, so we dispatch every open/read/write
//! through `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use redb::{Database, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::result::SubtaskOutcome;
use crate::topology::SwarmTopology;

/// Current schema for the persisted dispatch record. Bumping this
/// invalidates prior records — the primitive drops any row with a
/// higher version than it understands.
pub const DISPATCH_RECORD_SCHEMA_VERSION: u32 = 1;

const DISPATCH_TABLE: TableDefinition<&str, &str> = TableDefinition::new("swarm_dispatch");
const DEFAULT_DB_FILENAME: &str = "swarm-state.redb";

/// A single durable dispatch record. Updated in-place on every retry
/// round so the supervisor can inspect live state without replaying the
/// whole history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DispatchRecord {
    pub schema_version: u32,
    pub dispatch_id: String,
    pub session_id: String,
    pub task_id: String,
    pub topology: SwarmTopology,
    /// Per-subtask state. Ordering matches the resolved contracts list.
    pub subtasks: Vec<SubtaskOutcome>,
    /// Retry rounds already consumed (0-indexed).
    pub retry_rounds_used: u32,
    /// `true` once [`crate::Swarm::dispatch`] returns. Restart logic
    /// treats finalized dispatches as idempotent no-ops.
    pub finalized: bool,
}

impl DispatchRecord {
    pub fn new(
        dispatch_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        topology: SwarmTopology,
        subtasks: Vec<SubtaskOutcome>,
    ) -> Self {
        Self {
            schema_version: DISPATCH_RECORD_SCHEMA_VERSION,
            dispatch_id: dispatch_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
            topology,
            subtasks,
            retry_rounds_used: 0,
            finalized: false,
        }
    }
}

/// Redb-backed dispatch ledger shared across the primitive. Thread-safe
/// via the underlying `Arc<Database>`.
#[derive(Clone)]
pub struct DispatchStore {
    db: Arc<Database>,
    path: Arc<PathBuf>,
}

impl DispatchStore {
    /// Open (or create) the ledger under `dir`. Creates `dir` if it
    /// does not exist. The ledger file is always named
    /// `swarm-state.redb` so multiple primitives can share a data dir.
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&dir)
            .await
            .wrap_err("create swarm state dir")?;

        let db_path = dir.join(DEFAULT_DB_FILENAME);
        let path_for_spawn = db_path.clone();
        let db = tokio::task::spawn_blocking(move || {
            let db = Database::create(&path_for_spawn).wrap_err("open swarm state redb")?;
            let write_txn = db.begin_write().wrap_err("begin swarm state write")?;
            {
                let _ = write_txn
                    .open_table(DISPATCH_TABLE)
                    .wrap_err("open swarm dispatch table")?;
            }
            write_txn.commit().wrap_err("commit swarm table init")?;
            Ok::<_, eyre::Report>(db)
        })
        .await
        .wrap_err("join swarm state open")??;

        Ok(Self {
            db: Arc::new(db),
            path: Arc::new(db_path),
        })
    }

    /// Path to the underlying redb file. Exposed for tests and
    /// supervisor UIs that want to show the storage location.
    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    /// Load a dispatch record if one exists. Returns `Ok(None)` for a
    /// miss so callers can treat "never seen" as "fresh dispatch".
    pub async fn load(&self, dispatch_id: &str) -> Result<Option<DispatchRecord>> {
        let db = self.db.clone();
        let dispatch_id = dispatch_id.to_string();
        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().wrap_err("begin swarm state read")?;
            let table = read_txn
                .open_table(DISPATCH_TABLE)
                .wrap_err("open swarm dispatch table")?;
            let Some(value) = table
                .get(dispatch_id.as_str())
                .wrap_err("read swarm dispatch row")?
            else {
                return Ok::<_, eyre::Report>(None);
            };
            let record: DispatchRecord =
                serde_json::from_str(value.value()).wrap_err("parse swarm dispatch row")?;
            if record.schema_version > DISPATCH_RECORD_SCHEMA_VERSION {
                return Ok(None);
            }
            Ok(Some(record))
        })
        .await
        .wrap_err("join swarm state load")?
    }

    /// Upsert the full dispatch record. Overwrites the previous row so
    /// the primitive only pays one commit per state update.
    pub async fn store(&self, record: &DispatchRecord) -> Result<()> {
        let db = self.db.clone();
        let key = record.dispatch_id.clone();
        let json = serde_json::to_string(record).wrap_err("serialize swarm dispatch row")?;
        tokio::task::spawn_blocking(move || {
            let write_txn = db.begin_write().wrap_err("begin swarm state write")?;
            {
                let mut table = write_txn
                    .open_table(DISPATCH_TABLE)
                    .wrap_err("open swarm dispatch table")?;
                table
                    .insert(key.as_str(), json.as_str())
                    .wrap_err("insert swarm dispatch row")?;
            }
            write_txn.commit().wrap_err("commit swarm dispatch row")?;
            Ok::<_, eyre::Report>(())
        })
        .await
        .wrap_err("join swarm state store")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::SubtaskStatus;
    use std::num::NonZeroUsize;
    use tempfile::TempDir;

    fn sample_record() -> DispatchRecord {
        DispatchRecord::new(
            "d1",
            "session-1",
            "task-1",
            SwarmTopology::Parallel {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
            },
            vec![SubtaskOutcome {
                contract_id: "c1".into(),
                label: None,
                status: SubtaskStatus::Completed,
                attempts: 1,
                last_dispatch_outcome: "success".into(),
                output: "done".into(),
                files_to_send: vec![],
                error: None,
            }],
        )
    }

    #[tokio::test]
    async fn store_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = DispatchStore::open(dir.path()).await.unwrap();
        let rec = sample_record();
        store.store(&rec).await.unwrap();
        let loaded = store.load("d1").await.unwrap().expect("row present");
        assert_eq!(loaded, rec);
    }

    #[tokio::test]
    async fn load_returns_none_for_missing_dispatch() {
        let dir = TempDir::new().unwrap();
        let store = DispatchStore::open(dir.path()).await.unwrap();
        assert!(store.load("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_overwrites_existing_row() {
        let dir = TempDir::new().unwrap();
        let store = DispatchStore::open(dir.path()).await.unwrap();
        let mut rec = sample_record();
        store.store(&rec).await.unwrap();
        rec.retry_rounds_used = 2;
        rec.finalized = true;
        store.store(&rec).await.unwrap();
        let loaded = store.load("d1").await.unwrap().expect("row present");
        assert_eq!(loaded.retry_rounds_used, 2);
        assert!(loaded.finalized);
    }

    #[tokio::test]
    async fn reopening_recovers_prior_rows() {
        let dir = TempDir::new().unwrap();
        {
            let store = DispatchStore::open(dir.path()).await.unwrap();
            store.store(&sample_record()).await.unwrap();
        }
        let reopened = DispatchStore::open(dir.path()).await.unwrap();
        let loaded = reopened.load("d1").await.unwrap().expect("row present");
        assert_eq!(loaded.dispatch_id, "d1");
    }
}
