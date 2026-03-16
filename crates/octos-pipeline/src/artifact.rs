//! Artifact store for pipeline node outputs.
//!
//! Small artifacts are kept in memory; large ones spill to disk under
//! the run directory. Provides a unified API for storing and retrieving
//! node outputs regardless of backing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::graph::validate_pipeline_id;

/// Threshold in bytes above which artifacts are written to disk.
const SPILL_THRESHOLD: usize = 64 * 1024; // 64 KB

/// Where an artifact is stored.
#[derive(Debug, Clone)]
enum ArtifactBacking {
    Memory(String),
    Disk(PathBuf),
}

/// Manages node output artifacts with memory/disk hybrid storage.
pub struct ArtifactStore {
    artifacts: HashMap<String, ArtifactBacking>,
    run_dir: Option<PathBuf>,
}

impl ArtifactStore {
    /// Create a store that spills large artifacts to `{run_dir}/artifacts/`.
    pub fn new(run_dir: &Path) -> Self {
        Self {
            artifacts: HashMap::new(),
            run_dir: Some(run_dir.to_path_buf()),
        }
    }

    /// Create an in-memory-only store (no disk spilling).
    pub fn in_memory() -> Self {
        Self {
            artifacts: HashMap::new(),
            run_dir: None,
        }
    }

    /// Store an artifact for a node. Automatically chooses memory vs disk.
    pub fn put(&mut self, node_id: &str, content: &str) -> std::io::Result<()> {
        validate_pipeline_id(node_id).map_err(|e| std::io::Error::other(e.to_string()))?;
        if content.len() > SPILL_THRESHOLD {
            if let Some(ref dir) = self.run_dir {
                let artifact_dir = dir.join("artifacts");
                std::fs::create_dir_all(&artifact_dir)?;
                let path = artifact_dir.join(format!("{node_id}.txt"));
                std::fs::write(&path, content)?;
                self.artifacts
                    .insert(node_id.to_string(), ArtifactBacking::Disk(path));
                return Ok(());
            }
        }
        self.artifacts.insert(
            node_id.to_string(),
            ArtifactBacking::Memory(content.to_string()),
        );
        Ok(())
    }

    /// Retrieve an artifact. Reads from disk if spilled.
    pub fn get(&self, node_id: &str) -> std::io::Result<Option<String>> {
        match self.artifacts.get(node_id) {
            Some(ArtifactBacking::Memory(s)) => Ok(Some(s.clone())),
            Some(ArtifactBacking::Disk(path)) => {
                let content = std::fs::read_to_string(path)?;
                Ok(Some(content))
            }
            None => Ok(None),
        }
    }

    /// Check if an artifact exists for a node.
    pub fn contains(&self, node_id: &str) -> bool {
        self.artifacts.contains_key(node_id)
    }

    /// Number of stored artifacts.
    pub fn len(&self) -> usize {
        self.artifacts.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty()
    }

    /// List all stored node IDs.
    pub fn node_ids(&self) -> Vec<&str> {
        self.artifacts.keys().map(|s| s.as_str()).collect()
    }

    /// Check if a specific artifact is stored on disk.
    pub fn is_on_disk(&self, node_id: &str) -> bool {
        matches!(self.artifacts.get(node_id), Some(ArtifactBacking::Disk(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn should_store_small_in_memory() {
        let dir = TempDir::new().unwrap();
        let mut store = ArtifactStore::new(dir.path());

        store.put("node1", "small output").unwrap();
        assert!(store.contains("node1"));
        assert!(!store.is_on_disk("node1"));
        assert_eq!(store.get("node1").unwrap().unwrap(), "small output");
    }

    #[test]
    fn should_spill_large_to_disk() {
        let dir = TempDir::new().unwrap();
        let mut store = ArtifactStore::new(dir.path());

        let large = "x".repeat(SPILL_THRESHOLD + 1);
        store.put("big_node", &large).unwrap();

        assert!(store.contains("big_node"));
        assert!(store.is_on_disk("big_node"));
        assert_eq!(store.get("big_node").unwrap().unwrap(), large);

        // Verify file exists on disk
        assert!(dir.path().join("artifacts/big_node.txt").exists());
    }

    #[test]
    fn should_keep_large_in_memory_when_no_run_dir() {
        let mut store = ArtifactStore::in_memory();
        let large = "x".repeat(SPILL_THRESHOLD + 1);
        store.put("node1", &large).unwrap();

        assert!(store.contains("node1"));
        assert!(!store.is_on_disk("node1"));
        assert_eq!(store.get("node1").unwrap().unwrap(), large);
    }

    #[test]
    fn should_return_none_for_missing() {
        let store = ArtifactStore::in_memory();
        assert!(store.get("missing").unwrap().is_none());
        assert!(!store.contains("missing"));
    }

    #[test]
    fn should_track_count() {
        let mut store = ArtifactStore::in_memory();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        store.put("a", "aaa").unwrap();
        store.put("b", "bbb").unwrap();
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
    }

    #[test]
    fn should_list_node_ids() {
        let mut store = ArtifactStore::in_memory();
        store.put("alpha", "a").unwrap();
        store.put("beta", "b").unwrap();

        let mut ids = store.node_ids();
        ids.sort();
        assert_eq!(ids, vec!["alpha", "beta"]);
    }
}
