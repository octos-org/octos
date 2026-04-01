//! Pipeline checkpoint save/resume for fault-tolerant execution.
//!
//! Persists per-node outcomes to `{run_dir}/checkpoint.json` so a pipeline
//! can resume from the last completed node after a crash or restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::graph::{NodeOutcome, OutcomeStatus};

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

/// Manages checkpoint persistence to disk.
pub struct CheckpointStore {
    path: PathBuf,
}

impl CheckpointStore {
    /// Create a store that writes to `{run_dir}/checkpoint.json`.
    pub fn new(run_dir: &Path) -> Self {
        Self {
            path: run_dir.join("checkpoint.json"),
        }
    }

    /// Save checkpoint to disk (atomic write-then-rename).
    pub fn save(&self, checkpoint: &Checkpoint) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(checkpoint).map_err(std::io::Error::other)?;
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, &self.path)
    }

    /// Load checkpoint from disk, if it exists.
    pub fn load(&self) -> std::io::Result<Option<Checkpoint>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let data = std::fs::read_to_string(&self.path)?;
        let checkpoint: Checkpoint = serde_json::from_str(&data).map_err(std::io::Error::other)?;
        Ok(Some(checkpoint))
    }

    /// Delete the checkpoint file (e.g., after successful completion).
    pub fn clear(&self) -> std::io::Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }

    /// Check if a checkpoint file exists.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }
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
        let store = CheckpointStore::new(dir.path());

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
        let store = CheckpointStore::new(dir.path());
        assert!(!store.exists());
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn should_clear_checkpoint() {
        let dir = TempDir::new().unwrap();
        let store = CheckpointStore::new(dir.path());

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
}
