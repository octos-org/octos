//! Run directory management for pipeline audit trails.
//!
//! Creates `{working_dir}/.octos/runs/{run_id}/{node_id}/status.json` files
//! that record execution outcomes for each pipeline node.

use std::path::{Path, PathBuf};

use octos_core::TokenUsage;
use serde::Serialize;

use crate::graph::{OutcomeStatus, validate_pipeline_id};

/// Minimum content length to write a separate output file.
const MIN_CONTENT_LENGTH: usize = 100;

/// Manages a run directory for a single pipeline execution.
pub struct RunDir {
    base: Option<PathBuf>,
}

/// Per-node status record written to `status.json`.
#[derive(Debug, Clone, Serialize)]
pub struct NodeStatus {
    pub node_id: String,
    pub label: Option<String>,
    pub status: OutcomeStatus,
    pub model: Option<String>,
    pub duration_ms: u64,
    pub token_usage: TokenUsage,
    pub start_time: String,
    pub content_length: usize,
}

impl RunDir {
    /// Create a new run directory under `{working_dir}/.octos/runs/{run_id}/`.
    pub fn new(working_dir: &Path, run_id: &str) -> std::io::Result<Self> {
        validate_pipeline_id(run_id).map_err(|e| std::io::Error::other(e.to_string()))?;
        let base = working_dir.join(".octos/runs").join(run_id);
        std::fs::create_dir_all(&base)?;
        Ok(Self { base: Some(base) })
    }

    /// Create a RunDir that silently discards writes (for testing or disabled mode).
    pub fn noop() -> Self {
        Self { base: None }
    }

    /// Return the base directory path, if active.
    pub fn path(&self) -> Option<&Path> {
        self.base.as_deref()
    }

    /// Write a node's status to `{run_dir}/{node_id}/status.json`.
    pub fn write_node_status(&self, status: &NodeStatus) -> std::io::Result<()> {
        let Some(ref base) = self.base else {
            return Ok(());
        };

        validate_pipeline_id(&status.node_id).map_err(|e| std::io::Error::other(e.to_string()))?;
        let node_dir = base.join(&status.node_id);
        std::fs::create_dir_all(&node_dir)?;

        let json = serde_json::to_string_pretty(status).map_err(std::io::Error::other)?;

        std::fs::write(node_dir.join("status.json"), json)?;
        Ok(())
    }

    /// Write large content to a separate file, returning the relative path.
    pub fn write_node_content(
        &self,
        node_id: &str,
        content: &str,
    ) -> std::io::Result<Option<PathBuf>> {
        let Some(ref base) = self.base else {
            return Ok(None);
        };

        if content.len() < MIN_CONTENT_LENGTH {
            return Ok(None);
        }

        validate_pipeline_id(node_id).map_err(|e| std::io::Error::other(e.to_string()))?;
        let node_dir = base.join(node_id);
        std::fs::create_dir_all(&node_dir)?;

        let content_path = node_dir.join("output.txt");
        std::fs::write(&content_path, content)?;
        Ok(Some(content_path))
    }

    /// Write pipeline-level summary.
    pub fn write_summary(&self, summary: &PipelineRunSummary) -> std::io::Result<()> {
        let Some(ref base) = self.base else {
            return Ok(());
        };

        let json = serde_json::to_string_pretty(summary).map_err(std::io::Error::other)?;

        std::fs::write(base.join("summary.json"), json)?;
        Ok(())
    }
}

/// Pipeline-level execution summary.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineRunSummary {
    pub graph_id: String,
    pub success: bool,
    pub duration_ms: u64,
    pub total_tokens: TokenUsage,
    pub nodes_executed: usize,
    pub start_time: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn should_create_run_directory() {
        let dir = TempDir::new().unwrap();
        let run_dir = RunDir::new(dir.path(), "run_001").unwrap();
        let path = run_dir.path().unwrap();
        assert!(path.exists());
        assert!(path.ends_with("run_001"));
    }

    #[test]
    fn should_write_node_status() {
        let dir = TempDir::new().unwrap();
        let run_dir = RunDir::new(dir.path(), "run_002").unwrap();

        let status = NodeStatus {
            node_id: "analyze".into(),
            label: Some("Code Analysis".into()),
            status: OutcomeStatus::Pass,
            model: Some("gpt-4o".into()),
            duration_ms: 1500,
            token_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            start_time: "2026-03-06T12:00:00Z".into(),
            content_length: 500,
        };

        run_dir.write_node_status(&status).unwrap();

        let status_path = run_dir.path().unwrap().join("analyze/status.json");
        assert!(status_path.exists());

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(status_path).unwrap()).unwrap();
        assert_eq!(json["node_id"], "analyze");
        assert_eq!(json["status"], "pass");
        assert_eq!(json["duration_ms"], 1500);
    }

    #[test]
    fn should_write_node_content() {
        let dir = TempDir::new().unwrap();
        let run_dir = RunDir::new(dir.path(), "run_003").unwrap();

        let content = "x".repeat(200);
        let path = run_dir.write_node_content("node1", &content).unwrap();
        assert!(path.is_some());
        assert!(path.unwrap().ends_with("output.txt"));
    }

    #[test]
    fn should_skip_small_content() {
        let dir = TempDir::new().unwrap();
        let run_dir = RunDir::new(dir.path(), "run_004").unwrap();

        let path = run_dir.write_node_content("node1", "short").unwrap();
        assert!(path.is_none());
    }

    #[test]
    fn should_write_summary() {
        let dir = TempDir::new().unwrap();
        let run_dir = RunDir::new(dir.path(), "run_005").unwrap();

        let summary = PipelineRunSummary {
            graph_id: "test_pipeline".into(),
            success: true,
            duration_ms: 5000,
            total_tokens: TokenUsage::default(),
            nodes_executed: 3,
            start_time: "2026-03-06T12:00:00Z".into(),
        };

        run_dir.write_summary(&summary).unwrap();
        let summary_path = run_dir.path().unwrap().join("summary.json");
        assert!(summary_path.exists());
    }

    #[test]
    fn should_handle_noop_gracefully() {
        let run_dir = RunDir::noop();
        assert!(run_dir.path().is_none());
        let status = NodeStatus {
            node_id: "test".into(),
            label: None,
            status: OutcomeStatus::Pass,
            model: None,
            duration_ms: 0,
            token_usage: TokenUsage::default(),
            start_time: "".into(),
            content_length: 0,
        };
        // Should not error
        run_dir.write_node_status(&status).unwrap();
        run_dir.write_node_content("test", "content").unwrap();
    }
}
