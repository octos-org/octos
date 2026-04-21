//! Pipeline checkpoint save/resume for fault-tolerant execution.
//!
//! Two complementary APIs live here:
//!
//! 1. `Checkpoint` + `PersistedCheckpoint` — data model.
//!    * `Checkpoint` holds the in-flight run state (completed node outcomes,
//!      resume pointer), persisted to `{run_dir}/checkpoint.json`.
//!    * `PersistedCheckpoint` is a single point-in-time mission snapshot,
//!      written each time a node declares a `MissionCheckpoint`. Multiple
//!      snapshots can exist per run, and executors select the most recent
//!      one when resuming.
//!
//! 2. `CheckpointStore` trait + `FileSystemCheckpointStore` impl.
//!    * The trait lets callers plug alternative backends in tests.
//!    * The filesystem impl persists snapshots atomically via
//!      `std::fs::rename` from a sibling `.tmp` file so partial writes
//!      cannot corrupt the active file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::graph::{MissionCheckpoint, NodeOutcome, OutcomeStatus};

/// Serializable checkpoint state for a pipeline run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Graph ID this checkpoint belongs to.
    pub graph_id: String,
    /// Completed node outcomes keyed by node_id.
    pub completed: HashMap<String, NodeOutcome>,
    /// The node to resume from (first incomplete node).
    pub resume_from: Option<String>,
}

impl Checkpoint {
    /// Create an empty checkpoint for a new pipeline run.
    pub fn new(graph_id: &str) -> Self {
        Self {
            graph_id: graph_id.to_string(),
            completed: HashMap::new(),
            resume_from: None,
        }
    }

    /// Record a completed node outcome.
    pub fn record(&mut self, outcome: NodeOutcome) {
        self.completed.insert(outcome.node_id.clone(), outcome);
    }

    /// Check if a node has already been completed.
    pub fn is_completed(&self, node_id: &str) -> bool {
        self.completed.contains_key(node_id)
    }

    /// Get the outcome for a completed node.
    pub fn get_outcome(&self, node_id: &str) -> Option<&NodeOutcome> {
        self.completed.get(node_id)
    }

    /// Set the resume point.
    pub fn set_resume_from(&mut self, node_id: &str) {
        self.resume_from = Some(node_id.to_string());
    }

    /// Count completed nodes by status.
    pub fn count_by_status(&self, status: OutcomeStatus) -> usize {
        self.completed
            .values()
            .filter(|o| o.status == status)
            .count()
    }
}

/// A single mission-checkpoint snapshot written when a node declares one.
///
/// Stored in `{run_dir}/mission_checkpoints.json` as an append-only vector so
/// a pipeline can resume from any prior snapshot. `node_id` identifies which
/// node just finished when this snapshot was taken — resuming will skip every
/// node up to and including that id in the run's completion log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedCheckpoint {
    /// Graph ID this snapshot belongs to.
    pub graph_id: String,
    /// Node ID that had just completed when this snapshot was captured.
    pub node_id: String,
    /// Operator-visible name copied from the source `MissionCheckpoint`.
    pub name: String,
    /// Whether a resume can replay from this checkpoint.
    pub resumable: bool,
    /// Monotonic sequence number within the run (0 = first).
    pub sequence: u64,
    /// Unix timestamp in milliseconds when the checkpoint was persisted.
    pub timestamp_ms: u128,
}

impl PersistedCheckpoint {
    /// Build a new persisted checkpoint from a declaration, stamping
    /// `timestamp_ms` with `SystemTime::now()`.
    pub fn from_declaration(
        graph_id: impl Into<String>,
        node_id: impl Into<String>,
        declaration: &MissionCheckpoint,
        sequence: u64,
    ) -> Self {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            graph_id: graph_id.into(),
            node_id: node_id.into(),
            name: declaration.name.clone(),
            resumable: declaration.resumable,
            sequence,
            timestamp_ms,
        }
    }
}

/// Pluggable persistence backend for pipeline checkpoints.
///
/// Implementations MUST write atomically — a crashed `persist` call must
/// leave the backing store in either the pre-call state or the post-call
/// state, never a mixed/truncated state.
pub trait CheckpointStore: Send + Sync {
    /// Persist an additional mission checkpoint snapshot.
    fn persist(&self, checkpoint: &PersistedCheckpoint) -> std::io::Result<()>;

    /// Return every persisted checkpoint, sorted by `sequence` ascending.
    fn list(&self) -> std::io::Result<Vec<PersistedCheckpoint>>;

    /// Return the most recent (highest-sequence) persisted checkpoint, if any.
    fn latest(&self) -> std::io::Result<Option<PersistedCheckpoint>> {
        let mut all = self.list()?;
        Ok(all.pop())
    }

    /// Remove every persisted checkpoint (typically called on successful
    /// completion of the run).
    fn clear_all(&self) -> std::io::Result<()>;
}

/// Filesystem-backed `CheckpointStore` impl.
///
/// Snapshots are stored in a single JSON file written via the
/// write-then-rename pattern. `std::fs::rename` is atomic on POSIX and NTFS
/// when source and destination live on the same filesystem, which is always
/// the case because the temporary path is `{path}.tmp`.
///
/// This type also still owns the run-level `Checkpoint` persistence for
/// backward compatibility with callers of `save` / `load` / `clear`.
pub struct FileSystemCheckpointStore {
    /// Path to the run-level `Checkpoint` JSON file.
    run_path: PathBuf,
    /// Path to the append-only mission-checkpoint log.
    mission_path: PathBuf,
}

impl FileSystemCheckpointStore {
    /// Create a store that writes to `{run_dir}/checkpoint.json` for
    /// run-level state and `{run_dir}/mission_checkpoints.json` for mission
    /// snapshots.
    pub fn new(run_dir: &Path) -> Self {
        Self {
            run_path: run_dir.join("checkpoint.json"),
            mission_path: run_dir.join("mission_checkpoints.json"),
        }
    }

    /// Save run-level checkpoint to disk (atomic write-then-rename).
    pub fn save(&self, checkpoint: &Checkpoint) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(checkpoint).map_err(std::io::Error::other)?;
        atomic_write(&self.run_path, json.as_bytes())
    }

    /// Load run-level checkpoint from disk, if it exists.
    pub fn load(&self) -> std::io::Result<Option<Checkpoint>> {
        if !self.run_path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&self.run_path)?;
        let checkpoint: Checkpoint = serde_json::from_str(&data).map_err(std::io::Error::other)?;
        Ok(Some(checkpoint))
    }

    /// Delete the run-level checkpoint file (e.g., after successful completion).
    pub fn clear(&self) -> std::io::Result<()> {
        if self.run_path.exists() {
            std::fs::remove_file(&self.run_path)?;
        }
        Ok(())
    }

    /// Check if a run-level checkpoint file exists.
    pub fn exists(&self) -> bool {
        self.run_path.exists()
    }
}

impl CheckpointStore for FileSystemCheckpointStore {
    fn persist(&self, checkpoint: &PersistedCheckpoint) -> std::io::Result<()> {
        let mut all = self.list()?;
        all.push(checkpoint.clone());
        let json = serde_json::to_string_pretty(&all).map_err(std::io::Error::other)?;
        atomic_write(&self.mission_path, json.as_bytes())
    }

    fn list(&self) -> std::io::Result<Vec<PersistedCheckpoint>> {
        if !self.mission_path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&self.mission_path)?;
        if data.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut list: Vec<PersistedCheckpoint> =
            serde_json::from_str(&data).map_err(std::io::Error::other)?;
        list.sort_by_key(|c| c.sequence);
        Ok(list)
    }

    fn clear_all(&self) -> std::io::Result<()> {
        if self.mission_path.exists() {
            std::fs::remove_file(&self.mission_path)?;
        }
        Ok(())
    }
}

/// Write `bytes` to `path` atomically using temp file + rename.
///
/// `std::fs::rename` is atomic within a single filesystem — since the
/// temporary file is `{path}.tmp` (same directory, same filesystem) the
/// rename cannot be non-atomic. A crash mid-write leaves the destination
/// file in its prior state, never a partial write.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_path = path.to_path_buf();
    let tmp_name = match path.file_name() {
        Some(n) => {
            let mut s = n.to_os_string();
            s.push(".tmp");
            s
        }
        None => std::ffi::OsString::from("checkpoint.tmp"),
    };
    tmp_path.set_file_name(tmp_name);
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::TokenUsage;
    use tempfile::TempDir;

    fn make_outcome(node_id: &str, status: OutcomeStatus) -> NodeOutcome {
        NodeOutcome {
            node_id: node_id.into(),
            status,
            content: format!("output from {node_id}"),
            token_usage: TokenUsage::default(),
            files_modified: vec![],
        }
    }

    #[test]
    fn should_record_and_check_completed() {
        let mut cp = Checkpoint::new("test_graph");
        assert!(!cp.is_completed("node1"));

        cp.record(make_outcome("node1", OutcomeStatus::Pass));
        assert!(cp.is_completed("node1"));
        assert!(!cp.is_completed("node2"));
    }

    #[test]
    fn should_count_by_status() {
        let mut cp = Checkpoint::new("test_graph");
        cp.record(make_outcome("a", OutcomeStatus::Pass));
        cp.record(make_outcome("b", OutcomeStatus::Pass));
        cp.record(make_outcome("c", OutcomeStatus::Fail));

        assert_eq!(cp.count_by_status(OutcomeStatus::Pass), 2);
        assert_eq!(cp.count_by_status(OutcomeStatus::Fail), 1);
        assert_eq!(cp.count_by_status(OutcomeStatus::Error), 0);
    }

    #[test]
    fn should_save_and_load_checkpoint() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemCheckpointStore::new(dir.path());

        let mut cp = Checkpoint::new("my_pipeline");
        cp.record(make_outcome("step1", OutcomeStatus::Pass));
        cp.set_resume_from("step2");

        store.save(&cp).unwrap();
        assert!(store.exists());

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.graph_id, "my_pipeline");
        assert!(loaded.is_completed("step1"));
        assert_eq!(loaded.resume_from.as_deref(), Some("step2"));
    }

    #[test]
    fn should_return_none_when_no_file() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemCheckpointStore::new(dir.path());
        assert!(!store.exists());
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn should_clear_checkpoint() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemCheckpointStore::new(dir.path());

        let cp = Checkpoint::new("g1");
        store.save(&cp).unwrap();
        assert!(store.exists());

        store.clear().unwrap();
        assert!(!store.exists());
    }

    #[test]
    fn should_get_outcome() {
        let mut cp = Checkpoint::new("g1");
        cp.record(make_outcome("n1", OutcomeStatus::Pass));

        let outcome = cp.get_outcome("n1").unwrap();
        assert_eq!(outcome.content, "output from n1");
        assert!(cp.get_outcome("n2").is_none());
    }

    #[test]
    fn should_persist_and_list_mission_checkpoints_in_order() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemCheckpointStore::new(dir.path());

        for (i, node) in ["a", "b", "c"].iter().enumerate() {
            let decl = MissionCheckpoint {
                name: format!("after_{node}"),
                resumable: true,
            };
            let cp = PersistedCheckpoint::from_declaration("g", *node, &decl, i as u64);
            store.persist(&cp).unwrap();
        }

        let list = store.list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].node_id, "a");
        assert_eq!(list[2].node_id, "c");
        assert_eq!(store.latest().unwrap().unwrap().node_id, "c");
    }

    #[test]
    fn should_clear_mission_checkpoints() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemCheckpointStore::new(dir.path());
        let decl = MissionCheckpoint {
            name: "cp1".into(),
            resumable: true,
        };
        let cp = PersistedCheckpoint::from_declaration("g", "n", &decl, 0);
        store.persist(&cp).unwrap();
        store.clear_all().unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn should_write_atomically_via_temp_rename() {
        // Persisting should not leave the `.tmp` file behind, which proves the
        // temp+rename path was taken.
        let dir = TempDir::new().unwrap();
        let store = FileSystemCheckpointStore::new(dir.path());
        let decl = MissionCheckpoint {
            name: "atomic".into(),
            resumable: true,
        };
        let cp = PersistedCheckpoint::from_declaration("g", "n", &decl, 0);
        store.persist(&cp).unwrap();

        let tmp_path = dir.path().join("mission_checkpoints.json.tmp");
        assert!(
            !tmp_path.exists(),
            "temp file should not remain after atomic rename"
        );
        assert!(dir.path().join("mission_checkpoints.json").exists());
    }
}
