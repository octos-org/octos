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

/// What produced an episode: a task run, or a conversation's compaction
/// summary (#1587 write side). Task recall filters to `Task` so a task's
/// generic query (e.g. the fixed "code review") can't BM25-admit an
/// unrelated conversation summary past the similarity floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeSource {
    #[default]
    Task,
    Conversation,
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
    /// Producer of this episode (#1587). Defaults to `Task` so episodes
    /// written before this field existed deserialize unchanged.
    #[serde(default)]
    pub source: EpisodeSource,
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
            source: EpisodeSource::Task,
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
