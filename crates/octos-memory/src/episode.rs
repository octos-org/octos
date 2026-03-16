//! Episode model: summary of a completed task.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use octos_core::{AgentId, TaskId};
use serde::{Deserialize, Serialize};

/// An episode is a summary of a completed task.
///
/// Episodes are stored in the episodic memory for future retrieval,
/// allowing agents to learn from past experiences.
/// Current schema version for Episode serialization.
const CURRENT_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    /// Schema version for forward-compatible deserialization.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Unique episode ID.
    pub id: String,
    /// The task this episode summarizes.
    pub task_id: TaskId,
    /// The agent that executed the task.
    pub agent_id: AgentId,
    /// Working directory where the task was executed.
    pub working_dir: PathBuf,
    /// LLM-generated summary of what happened.
    pub summary: String,
    /// Outcome of the task.
    pub outcome: EpisodeOutcome,
    /// Key decisions made during execution.
    pub key_decisions: Vec<String>,
    /// Files that were modified.
    pub files_modified: Vec<PathBuf>,
    /// When this episode was created.
    pub created_at: DateTime<Utc>,
}

impl Episode {
    /// Create a new episode.
    pub fn new(
        task_id: TaskId,
        agent_id: AgentId,
        working_dir: PathBuf,
        summary: String,
        outcome: EpisodeOutcome,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: uuid::Uuid::now_v7().to_string(),
            task_id,
            agent_id,
            working_dir,
            summary,
            outcome,
            key_decisions: Vec::new(),
            files_modified: Vec::new(),
            created_at: Utc::now(),
        }
    }
}

/// Outcome of a task episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    /// Task completed successfully.
    Success,
    /// Task failed.
    Failure,
    /// Task was blocked and needs human intervention.
    Blocked,
    /// Task was cancelled.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_episode_new() {
        let ep = Episode::new(
            TaskId::new(),
            AgentId::new("worker-1"),
            PathBuf::from("/tmp/project"),
            "Fixed a bug in parser".into(),
            EpisodeOutcome::Success,
        );
        assert_eq!(ep.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(!ep.id.is_empty());
        assert_eq!(ep.summary, "Fixed a bug in parser");
        assert_eq!(ep.outcome, EpisodeOutcome::Success);
        assert!(ep.key_decisions.is_empty());
        assert!(ep.files_modified.is_empty());
    }

    #[test]
    fn test_episode_unique_ids() {
        let ep1 = Episode::new(
            TaskId::new(),
            AgentId::new("a"),
            PathBuf::from("/tmp"),
            "first".into(),
            EpisodeOutcome::Success,
        );
        let ep2 = Episode::new(
            TaskId::new(),
            AgentId::new("a"),
            PathBuf::from("/tmp"),
            "second".into(),
            EpisodeOutcome::Success,
        );
        assert_ne!(ep1.id, ep2.id);
    }

    #[test]
    fn test_episode_serde_roundtrip() {
        let mut ep = Episode::new(
            TaskId::new(),
            AgentId::new("agent-1"),
            PathBuf::from("/home/user/project"),
            "Refactored module".into(),
            EpisodeOutcome::Failure,
        );
        ep.key_decisions = vec!["split into two files".into()];
        ep.files_modified = vec![PathBuf::from("src/lib.rs")];

        let json = serde_json::to_string(&ep).unwrap();
        let back: Episode = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, ep.id);
        assert_eq!(back.summary, "Refactored module");
        assert_eq!(back.outcome, EpisodeOutcome::Failure);
        assert_eq!(back.key_decisions, vec!["split into two files"]);
        assert_eq!(back.files_modified, vec![PathBuf::from("src/lib.rs")]);
        assert_eq!(back.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn test_episode_outcome_all_variants_serde() {
        for outcome in [
            EpisodeOutcome::Success,
            EpisodeOutcome::Failure,
            EpisodeOutcome::Blocked,
            EpisodeOutcome::Cancelled,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: EpisodeOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, outcome);
        }
    }

    #[test]
    fn test_episode_schema_version_defaults_on_missing() {
        // Simulate deserializing an old episode without schema_version
        let json = r#"{
            "id": "test-id",
            "task_id": "01234567-89ab-cdef-0123-456789abcdef",
            "agent_id": "agent-1",
            "working_dir": "/tmp",
            "summary": "old episode",
            "outcome": "success",
            "key_decisions": [],
            "files_modified": [],
            "created_at": "2025-01-01T00:00:00Z"
        }"#;
        let ep: Episode = serde_json::from_str(json).unwrap();
        assert_eq!(ep.schema_version, CURRENT_SCHEMA_VERSION);
    }
}
