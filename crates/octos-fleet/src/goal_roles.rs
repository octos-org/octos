// PM/Peer/Master roles for goal-driven multi-agent collaboration

use serde::{Deserialize, Serialize};

/// Role of an agent in the goal system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalRole {
    /// Master: decomposes goals, makes decisions, reviews findings.
    Master,
    /// PM (Project Manager): filters escalations, answers peer questions, escalates to master.
    PM,
    /// Peer: executes tasks, produces findings, escalates blockers.
    Peer,
}

impl GoalRole {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Master => "master",
            Self::PM => "pm",
            Self::Peer => "peer",
        }
    }
}

/// A peer agent working on a goal task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerAgent {
    pub peer_id: String,
    pub role: GoalRole,
    pub goal_id: String,
    pub task_id: Option<String>,
    pub name: String,
    pub brief: String,
}

impl PeerAgent {
    pub fn new_master(goal_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            peer_id: uuid::Uuid::new_v4().to_string(),
            role: GoalRole::Master,
            goal_id: goal_id.into(),
            task_id: None,
            name: name.into(),
            brief: String::new(),
        }
    }

    pub fn new_pm(goal_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            peer_id: uuid::Uuid::new_v4().to_string(),
            role: GoalRole::PM,
            goal_id: goal_id.into(),
            task_id: None,
            name: name.into(),
            brief: String::new(),
        }
    }

    pub fn new_peer(
        goal_id: impl Into<String>,
        task_id: impl Into<String>,
        name: impl Into<String>,
        brief: impl Into<String>,
    ) -> Self {
        Self {
            peer_id: uuid::Uuid::new_v4().to_string(),
            role: GoalRole::Peer,
            goal_id: goal_id.into(),
            task_id: Some(task_id.into()),
            name: name.into(),
            brief: brief.into(),
        }
    }
}
