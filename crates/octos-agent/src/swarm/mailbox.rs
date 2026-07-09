//! Mailbox backends for sub-agent coordination.
//!
//! The file backend stores each mailbox under:
//!
//! `<root>/<mailbox>/tmp`
//! `<root>/<mailbox>/inbox`
//! `<root>/<mailbox>/acked`
//!
//! Writers serialize a full [`MailboxEnvelope`] to `tmp`, `sync_all` the file,
//! then atomically rename it into `inbox`. Readers only scan `inbox`, so a
//! crash during write leaves an ignored orphan in `tmp` rather than a partial
//! message.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

pub const MAILBOX_SCHEMA_VERSION: u32 = 1;

static NEXT_MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A message queued in a sub-agent mailbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailboxMessage {
    /// Operator or supervisor steering intended to become a follow-up turn for
    /// the addressed sub-agent.
    Steering {
        from: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// A status notification that the sender is idle.
    ///
    /// This is intentionally not steering input. Consumers can surface or ack
    /// it without injecting a new user turn into the sub-agent loop.
    IdleNotification {
        agent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        idle_since: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_activity_at: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

impl MailboxMessage {
    pub fn steering(from: impl Into<String>, body: impl Into<String>) -> Self {
        Self::Steering {
            from: from.into(),
            body: body.into(),
            task_id: None,
            summary: None,
        }
    }

    pub fn idle_notification(
        agent_id: impl Into<String>,
        task_id: Option<String>,
        idle_since: DateTime<Utc>,
        last_activity_at: Option<DateTime<Utc>>,
        summary: Option<String>,
    ) -> Self {
        Self::IdleNotification {
            agent_id: agent_id.into(),
            task_id,
            idle_since,
            last_activity_at,
            summary,
        }
    }

    pub fn is_idle_notification(&self) -> bool {
        matches!(self, Self::IdleNotification { .. })
    }

    pub fn is_steering_input(&self) -> bool {
        matches!(self, Self::Steering { .. })
    }
}

/// Durable queue record written by mailbox backends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxEnvelope {
    pub schema_version: u32,
    pub id: String,
    pub mailbox: String,
    pub created_at: DateTime<Utc>,
    pub message: MailboxMessage,
}

impl MailboxEnvelope {
    pub fn new(mailbox: impl Into<String>, message: MailboxMessage) -> Self {
        Self {
            schema_version: MAILBOX_SCHEMA_VERSION,
            id: next_message_id(),
            mailbox: mailbox.into(),
            created_at: Utc::now(),
            message,
        }
    }
}

/// Result returned by mailbox recovery scans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxRecovery {
    pub pending_messages: usize,
    pub acked_messages: usize,
    pub orphaned_tmp_files: usize,
}

/// Backend contract for sub-agent mailboxes.
#[async_trait]
pub trait MailboxBackend: Send + Sync {
    /// Queue a message for `mailbox`.
    async fn send(&self, mailbox: &str, message: MailboxMessage) -> Result<MailboxEnvelope>;

    /// Return all pending messages for `mailbox`, ordered by creation time.
    async fn pending(&self, mailbox: &str) -> Result<Vec<MailboxEnvelope>>;

    /// Acknowledge a pending message. Returns `true` when this call consumed it.
    async fn ack(&self, mailbox: &str, message_id: &str) -> Result<bool>;

    /// Scan backend state after a restart.
    async fn recover(&self, mailbox: &str) -> Result<MailboxRecovery>;
}

/// Process-local mailbox backend for tests and single-runtime deployments.
#[derive(Clone, Default)]
pub struct InProcessMailbox {
    inner: Arc<Mutex<HashMap<String, VecDeque<MailboxEnvelope>>>>,
}

impl InProcessMailbox {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MailboxBackend for InProcessMailbox {
    async fn send(&self, mailbox: &str, message: MailboxMessage) -> Result<MailboxEnvelope> {
        let envelope = MailboxEnvelope::new(mailbox.to_string(), message);
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| eyre!("in-process mailbox lock poisoned"))?;
        guard
            .entry(mailbox.to_string())
            .or_default()
            .push_back(envelope.clone());
        Ok(envelope)
    }

    async fn pending(&self, mailbox: &str) -> Result<Vec<MailboxEnvelope>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| eyre!("in-process mailbox lock poisoned"))?;
        Ok(guard
            .get(mailbox)
            .map(|messages| messages.iter().cloned().collect())
            .unwrap_or_default())
    }

    async fn ack(&self, mailbox: &str, message_id: &str) -> Result<bool> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| eyre!("in-process mailbox lock poisoned"))?;
        let Some(messages) = guard.get_mut(mailbox) else {
            return Ok(false);
        };
        let Some(index) = messages.iter().position(|message| message.id == message_id) else {
            return Ok(false);
        };
        messages.remove(index);
        Ok(true)
    }

    async fn recover(&self, mailbox: &str) -> Result<MailboxRecovery> {
        let pending_messages = self.pending(mailbox).await?.len();
        Ok(MailboxRecovery {
            pending_messages,
            ..MailboxRecovery::default()
        })
    }
}

/// File-backed mailbox backend.
#[derive(Clone, Debug)]
pub struct FileMailbox {
    root: PathBuf,
}

impl FileMailbox {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mailbox_dir(&self, mailbox: &str) -> Result<PathBuf> {
        Ok(self.root.join(mailbox_path_component(mailbox)?))
    }
}

#[async_trait]
impl MailboxBackend for FileMailbox {
    async fn send(&self, mailbox: &str, message: MailboxMessage) -> Result<MailboxEnvelope> {
        let root = self.root.clone();
        let mailbox = mailbox.to_string();
        tokio::task::spawn_blocking(move || file_send_blocking(&root, &mailbox, message))
            .await
            .wrap_err("file mailbox send task panicked")?
    }

    async fn pending(&self, mailbox: &str) -> Result<Vec<MailboxEnvelope>> {
        let root = self.root.clone();
        let mailbox = mailbox.to_string();
        tokio::task::spawn_blocking(move || file_pending_blocking(&root, &mailbox))
            .await
            .wrap_err("file mailbox pending task panicked")?
    }

    async fn ack(&self, mailbox: &str, message_id: &str) -> Result<bool> {
        let root = self.root.clone();
        let mailbox = mailbox.to_string();
        let message_id = message_id.to_string();
        tokio::task::spawn_blocking(move || file_ack_blocking(&root, &mailbox, &message_id))
            .await
            .wrap_err("file mailbox ack task panicked")?
    }

    async fn recover(&self, mailbox: &str) -> Result<MailboxRecovery> {
        let root = self.root.clone();
        let mailbox = mailbox.to_string();
        tokio::task::spawn_blocking(move || file_recover_blocking(&root, &mailbox))
            .await
            .wrap_err("file mailbox recovery task panicked")?
    }
}

#[derive(Clone, Debug)]
struct MailboxPaths {
    inbox: PathBuf,
    tmp: PathBuf,
    acked: PathBuf,
}

fn file_send_blocking(
    root: &Path,
    mailbox: &str,
    message: MailboxMessage,
) -> Result<MailboxEnvelope> {
    let paths = ensure_mailbox_dirs(root, mailbox)?;
    let envelope = MailboxEnvelope::new(mailbox.to_string(), message);
    let tmp_path = paths
        .tmp
        .join(format!("{}.{}.tmp", envelope.id, next_message_id()));
    let final_path = paths.inbox.join(format!("{}.json", envelope.id));
    let mut data = serde_json::to_vec(&envelope).wrap_err("serialize mailbox envelope")?;
    data.push(b'\n');

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .wrap_err_with(|| format!("create mailbox temp file {}", tmp_path.display()))?;
        file.write_all(&data)
            .wrap_err_with(|| format!("write mailbox temp file {}", tmp_path.display()))?;
        file.sync_all()
            .wrap_err_with(|| format!("sync mailbox temp file {}", tmp_path.display()))?;
    }

    fs::rename(&tmp_path, &final_path).wrap_err_with(|| {
        format!(
            "rename mailbox temp file {} to {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;
    sync_dir_best_effort(&paths.inbox);
    sync_dir_best_effort(&paths.tmp);

    Ok(envelope)
}

fn file_pending_blocking(root: &Path, mailbox: &str) -> Result<Vec<MailboxEnvelope>> {
    let paths = ensure_mailbox_dirs(root, mailbox)?;
    read_envelopes(&paths.inbox)
}

fn file_ack_blocking(root: &Path, mailbox: &str, message_id: &str) -> Result<bool> {
    let paths = ensure_mailbox_dirs(root, mailbox)?;
    let pending_path = paths.inbox.join(format!("{message_id}.json"));
    let acked_path = paths.acked.join(format!("{message_id}.json"));
    match fs::rename(&pending_path, &acked_path) {
        Ok(()) => {
            sync_dir_best_effort(&paths.inbox);
            sync_dir_best_effort(&paths.acked);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).wrap_err_with(|| {
            format!(
                "ack mailbox message {} from {} to {}",
                message_id,
                pending_path.display(),
                acked_path.display()
            )
        }),
    }
}

fn file_recover_blocking(root: &Path, mailbox: &str) -> Result<MailboxRecovery> {
    let paths = ensure_mailbox_dirs(root, mailbox)?;
    Ok(MailboxRecovery {
        pending_messages: count_json_files(&paths.inbox)?,
        acked_messages: count_json_files(&paths.acked)?,
        orphaned_tmp_files: count_files(&paths.tmp)?,
    })
}

fn ensure_mailbox_dirs(root: &Path, mailbox: &str) -> Result<MailboxPaths> {
    let mailbox_dir = root.join(mailbox_path_component(mailbox)?);
    let paths = MailboxPaths {
        inbox: mailbox_dir.join("inbox"),
        tmp: mailbox_dir.join("tmp"),
        acked: mailbox_dir.join("acked"),
    };
    fs::create_dir_all(&paths.inbox)
        .wrap_err_with(|| format!("create mailbox inbox dir {}", paths.inbox.display()))?;
    fs::create_dir_all(&paths.tmp)
        .wrap_err_with(|| format!("create mailbox tmp dir {}", paths.tmp.display()))?;
    fs::create_dir_all(&paths.acked)
        .wrap_err_with(|| format!("create mailbox acked dir {}", paths.acked.display()))?;
    Ok(paths)
}

fn read_envelopes(dir: &Path) -> Result<Vec<MailboxEnvelope>> {
    let mut envelopes = Vec::new();
    for entry in
        fs::read_dir(dir).wrap_err_with(|| format!("read mailbox dir {}", dir.display()))?
    {
        let entry = entry.wrap_err_with(|| format!("read mailbox entry in {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let path_display = path.display().to_string();
        let raw = fs::read_to_string(&path)
            .wrap_err_with(|| format!("read mailbox message {path_display}"))?;
        let envelope: MailboxEnvelope = serde_json::from_str(&raw)
            .wrap_err_with(|| format!("parse mailbox message {path_display}"))?;
        envelopes.push(envelope);
    }
    envelopes.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(envelopes)
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
    for entry in
        fs::read_dir(dir).wrap_err_with(|| format!("read mailbox dir {}", dir.display()))?
    {
        let entry = entry.wrap_err_with(|| format!("read mailbox entry in {}", dir.display()))?;
        let path = entry.path();
        if path.is_file() && include(&path) {
            count += 1;
        }
    }
    Ok(count)
}

fn sync_dir_best_effort(path: &Path) {
    if let Ok(file) = File::open(path) {
        let _ = file.sync_all();
    }
}

fn mailbox_path_component(mailbox: &str) -> Result<String> {
    let mailbox = mailbox.trim();
    if mailbox.is_empty() {
        return Err(eyre!("mailbox name cannot be empty"));
    }

    let mut encoded = String::with_capacity(mailbox.len());
    for byte in mailbox.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'@' | b':' => {
                encoded.push(char::from(byte))
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }

    if encoded == "." || encoded == ".." {
        return Err(eyre!("mailbox name cannot resolve to a parent directory"));
    }

    Ok(encoded)
}

fn next_message_id() -> String {
    let counter = NEXT_MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "mbx-{}-{}-{counter}",
        Utc::now().timestamp_micros(),
        std::process::id()
    )
}
