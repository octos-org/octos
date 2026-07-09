use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use eyre::Result;
use octos_agent::{FileMailbox, InProcessMailbox, MailboxBackend, MailboxEnvelope, MailboxMessage};

#[tokio::test]
async fn in_process_mailbox_sends_idle_and_acks() -> Result<()> {
    let mailbox = InProcessMailbox::new();
    let idle_since = fixed_time();

    let steering = mailbox
        .send(
            "agent-a",
            MailboxMessage::steering("supervisor", "look here"),
        )
        .await?;
    let idle = mailbox
        .send(
            "agent-a",
            MailboxMessage::idle_notification(
                "agent-a",
                Some("task-1".to_string()),
                idle_since,
                None,
                Some("waiting for input".to_string()),
            ),
        )
        .await?;

    let pending = mailbox.pending("agent-a").await?;
    assert_eq!(pending.len(), 2);
    assert!(pending[1].message.is_idle_notification());
    assert!(!pending[1].message.is_steering_input());

    assert!(mailbox.ack("agent-a", &steering.id).await?);
    assert!(!mailbox.ack("agent-a", &steering.id).await?);

    let pending = mailbox.pending("agent-a").await?;
    assert_eq!(pending, vec![idle]);
    Ok(())
}

#[test]
fn idle_notification_serializes_as_mailbox_message_kind() -> Result<()> {
    let idle_since = fixed_time();
    let message = MailboxMessage::idle_notification(
        "agent-a",
        Some("task-1".to_string()),
        idle_since,
        Some(idle_since),
        Some("idle".to_string()),
    );

    let encoded = serde_json::to_string(&message)?;
    assert!(encoded.contains("\"kind\":\"idle_notification\""));

    let decoded: MailboxMessage = serde_json::from_str(&encoded)?;
    assert!(decoded.is_idle_notification());
    assert!(!decoded.is_steering_input());
    Ok(())
}

#[tokio::test]
async fn file_mailbox_writes_via_tmp_then_rename() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mailbox = FileMailbox::new(temp.path());
    let envelope = mailbox
        .send(
            "agent/a",
            MailboxMessage::steering("supervisor", "checkpoint"),
        )
        .await?;

    let mailbox_dir = mailbox.mailbox_dir("agent/a")?;
    assert_eq!(count_files(&mailbox_dir.join("tmp"))?, 0);
    assert_eq!(count_json_files(&mailbox_dir.join("inbox"))?, 1);

    let raw = fs::read_to_string(
        mailbox_dir
            .join("inbox")
            .join(format!("{}.json", envelope.id)),
    )?;
    assert!(raw.ends_with('\n'));
    let decoded: MailboxEnvelope = serde_json::from_str(&raw)?;
    assert_eq!(decoded, envelope);
    Ok(())
}

#[tokio::test]
async fn file_mailbox_recovers_without_reading_orphan_tmp_files() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mailbox = FileMailbox::new(temp.path());
    mailbox
        .send(
            "agent-a",
            MailboxMessage::steering("supervisor", "survived"),
        )
        .await?;

    let mailbox_dir = mailbox.mailbox_dir("agent-a")?;
    fs::write(
        mailbox_dir.join("tmp").join("partial.tmp"),
        b"{\"not\":\"complete\"",
    )?;

    let recovery = mailbox.recover("agent-a").await?;
    assert_eq!(recovery.pending_messages, 1);
    assert_eq!(recovery.acked_messages, 0);
    assert_eq!(recovery.orphaned_tmp_files, 1);

    let pending = mailbox.pending("agent-a").await?;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].mailbox, "agent-a");
    Ok(())
}

#[tokio::test]
async fn file_mailbox_ack_moves_message_out_of_pending() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mailbox = FileMailbox::new(temp.path());
    let envelope = mailbox
        .send("agent-a", MailboxMessage::steering("supervisor", "done"))
        .await?;

    assert!(mailbox.ack("agent-a", &envelope.id).await?);
    assert!(!mailbox.ack("agent-a", &envelope.id).await?);
    assert!(mailbox.pending("agent-a").await?.is_empty());

    let mailbox_dir = mailbox.mailbox_dir("agent-a")?;
    assert!(
        mailbox_dir
            .join("acked")
            .join(format!("{}.json", envelope.id))
            .exists()
    );

    let recovery = mailbox.recover("agent-a").await?;
    assert_eq!(recovery.pending_messages, 0);
    assert_eq!(recovery.acked_messages, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn file_mailbox_handles_concurrent_writers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mailbox = Arc::new(FileMailbox::new(temp.path()));
    let mut joins = Vec::new();

    for index in 0..48 {
        let mailbox = Arc::clone(&mailbox);
        joins.push(tokio::spawn(async move {
            mailbox
                .send(
                    "agent-a",
                    MailboxMessage::steering("supervisor", format!("message-{index}")),
                )
                .await
        }));
    }

    let mut ids = HashSet::new();
    for join in joins {
        let envelope = join.await??;
        assert!(ids.insert(envelope.id));
    }

    let pending = mailbox.pending("agent-a").await?;
    assert_eq!(pending.len(), 48);

    let bodies: HashSet<String> = pending
        .iter()
        .filter_map(|envelope| match &envelope.message {
            MailboxMessage::Steering { body, .. } => Some(body.clone()),
            MailboxMessage::IdleNotification { .. } => None,
        })
        .collect();
    assert_eq!(bodies.len(), 48);
    assert!(bodies.contains("message-0"));
    assert!(bodies.contains("message-47"));
    Ok(())
}

fn fixed_time() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("valid timestamp")
}

fn count_json_files(dir: &Path) -> Result<usize> {
    count_files_with(dir, |path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("json")
    })
}

fn count_files(dir: &Path) -> Result<usize> {
    count_files_with(dir, |_| true)
}

fn count_files_with(dir: &Path, include: impl Fn(&Path) -> bool) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file() && include(&path) {
            count += 1;
        }
    }
    Ok(count)
}
