// Typed mailbox for peer-to-peer communication in the goal system

use eyre::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A typed message in the peer mailbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MailboxMessage {
    /// A finding shared by a peer.
    FindingShared {
        finding_id: String,
        goal_id: String,
        task_id: Option<String>,
        assertion: String,
        confidence: String,
        shared_by: String,
        shared_at_ms: u64,
    },
    /// An escalation from a peer to PM/master.
    EscalationRaised {
        escalation_id: String,
        goal_id: String,
        task_id: Option<String>,
        question: String,
        raised_by: String,
        raised_at_ms: u64,
    },
    /// A decision made by master.
    DecisionMade {
        decision_id: String,
        goal_id: String,
        task_id: Option<String>,
        question: String,
        choice: String,
        decided_by: String,
        decided_at_ms: u64,
    },
    /// A task assignment from master/PM to peer.
    TaskAssigned {
        task_id: String,
        goal_id: String,
        title: String,
        detail: String,
        assigned_to: String,
        assigned_at_ms: u64,
    },
}

/// File-based typed mailbox for peer communication.
pub struct TypedMailbox {
    root: PathBuf,
}

impl TypedMailbox {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Write a message to a peer's mailbox.
    pub fn write(&self, peer_id: &str, message: &MailboxMessage) -> Result<()> {
        let peer_dir = self.root.join(peer_id);
        std::fs::create_dir_all(&peer_dir)?;

        let msg_id = uuid::Uuid::new_v4().to_string();
        let msg_path = peer_dir.join(format!("{}.json", msg_id));
        let json = serde_json::to_string_pretty(message)?;
        std::fs::write(msg_path, json)?;
        Ok(())
    }

    /// Read all messages from a peer's mailbox (and clear them).
    pub fn read(&self, peer_id: &str) -> Result<Vec<MailboxMessage>> {
        let peer_dir = self.root.join(peer_id);
        if !peer_dir.exists() {
            return Ok(Vec::new());
        }

        let mut messages = Vec::new();
        for entry in std::fs::read_dir(&peer_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let content = std::fs::read_to_string(&path)?;
                let message: MailboxMessage = serde_json::from_str(&content)?;
                messages.push(message);
                std::fs::remove_file(&path)?; // Clear after reading
            }
        }
        Ok(messages)
    }

    /// Broadcast a message to all peers.
    pub fn broadcast(&self, peer_ids: &[String], message: &MailboxMessage) -> Result<()> {
        for peer_id in peer_ids {
            self.write(peer_id, message)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_mailbox_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = TypedMailbox::new(dir.path()).unwrap();

        let msg = MailboxMessage::FindingShared {
            finding_id: "f1".to_string(),
            goal_id: "g1".to_string(),
            task_id: Some("t1".to_string()),
            assertion: "test finding".to_string(),
            confidence: "high".to_string(),
            shared_by: "peer-a".to_string(),
            shared_at_ms: 1000,
        };

        mailbox.write("peer-b", &msg).unwrap();
        let messages = mailbox.read("peer-b").unwrap();
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            MailboxMessage::FindingShared { assertion, .. } => {
                assert_eq!(assertion, "test finding");
            }
            _ => panic!("wrong message type"),
        }

        // Read again — should be empty (cleared after read)
        let messages = mailbox.read("peer-b").unwrap();
        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn typed_mailbox_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = TypedMailbox::new(dir.path()).unwrap();

        let msg = MailboxMessage::DecisionMade {
            decision_id: "d1".to_string(),
            goal_id: "g1".to_string(),
            task_id: None,
            question: "which approach?".to_string(),
            choice: "approach-a".to_string(),
            decided_by: "master".to_string(),
            decided_at_ms: 2000,
        };

        let peers = vec!["peer-a".to_string(), "peer-b".to_string()];
        mailbox.broadcast(&peers, &msg).unwrap();

        for peer in &peers {
            let messages = mailbox.read(peer).unwrap();
            assert_eq!(messages.len(), 1);
        }
    }
}
