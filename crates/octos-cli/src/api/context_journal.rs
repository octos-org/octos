//! Incremental post-turn context persistence. Full checkpoints remain the
//! semantic-boundary format; transitions replace an explicitly dirty suffix.
//! This is a single-runtime writer protocol, not cross-process coordination.

use super::*;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, OnceLock, Weak};

const TRANSITION_SCHEMA: &str = "octos.context-transition.v1";

#[derive(Debug, Clone, Default)]
struct Cursor {
    path: Option<PathBuf>,
    checkpoint_id: Option<String>,
    offset: u64,
    dirty_from: usize,
    items: usize,
}

#[derive(Debug, Default)]
pub(super) struct Tracking(Mutex<Cursor>);

impl Clone for Tracking {
    fn clone(&self) -> Self {
        // Scratch clones must not clear one another's dirty markers or inherit
        // an acknowledgement for state they never incorporated.
        Self(Mutex::new(self.get()))
    }
}

impl Tracking {
    fn get(&self) -> Cursor {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub(super) fn same_lineage(&self, other: &Self) -> bool {
        let left = self.get();
        let right = other.get();
        left.path == right.path
            && left.checkpoint_id == right.checkpoint_id
            && left.offset == right.offset
    }
    pub(super) fn rebase(&self, newer: &Self, difference: usize) {
        let mut cursor = newer.get();
        cursor.dirty_from = cursor.dirty_from.min(difference);
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = cursor;
    }
    pub(super) fn dirty(&self, index: usize) {
        let mut cursor = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cursor.dirty_from = cursor.dirty_from.min(index);
    }
    fn saved(&self, path: PathBuf, checkpoint_id: Option<String>, offset: u64, items: usize) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Cursor {
            path: Some(path),
            checkpoint_id,
            offset,
            dirty_from: items,
            items,
        };
    }
}

// Covers the root checkpoint and transition file as one logical resource.
// Public persistence/load entry points all use this lock, including direct
// callers which do not participate in AppUI's per-session serialization.
fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    type Locks = Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>;
    static LOCKS: OnceLock<Locks> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}

pub(super) fn journal_path(data_dir: &Path, session_id: &str) -> PathBuf {
    let root = context_ledger_path(data_dir, session_id);
    journal_for(&root, checkpoint_id(&root).ok().flatten().as_deref())
}

fn journal_for(root: &Path, id: Option<&str>) -> PathBuf {
    // A checkpoint owns a distinct sidecar. A crash between root promotion
    // and old-sidecar cleanup cannot mix two lineages, even if the obsolete
    // file ends in partial or corrupt bytes. Hashing also confines filenames.
    let suffix = id
        .map(|id| sha256_prefixed(id.as_bytes()).replace(':', "-"))
        .unwrap_or_else(|| "legacy".into());
    let session = sha256_prefixed(
        root.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .as_bytes(),
    )
    .replace(':', "-");
    root.with_file_name(format!("{session}-{suffix}.transitions.jsonl"))
}

fn file_len(path: &Path) -> Result<u64, String> {
    match path.metadata() {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!("read context journal metadata: {error}")),
    }
}

// New checkpoints write this first field. Reading a bounded header avoids
// parsing the full historical checkpoint on each ordinary batch append.
fn checkpoint_id(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let mut prefix = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(4096).read_to_end(&mut prefix))
        .map_err(|error| format!("read context checkpoint header: {error}"))?;
    let prefix = String::from_utf8_lossy(&prefix);
    let Some((_, value)) = prefix.split_once("\"journal_checkpoint_id\":") else {
        return Ok(None);
    };
    serde_json::Deserializer::from_str(value)
        .into_iter::<Option<String>>()
        .next()
        .transpose()
        .map_err(|error| format!("invalid context checkpoint identity: {error}"))
        .map(Option::flatten)
}

#[derive(Debug, Serialize, Deserialize)]
struct Transition {
    schema: String,
    checkpoint_id: String,
    offset: u64,
    previous_items: usize,
    tail_from: usize,
    items: Vec<TranscriptItem>,
    active_item_ids: Vec<TranscriptItemId>,
    state: ContextState,
    source_history: Option<SourceHistoryChain>,
    compactions: Vec<ContextCompactionRecord>,
    cache_epoch: Option<PromptCacheEpochState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    transition: Transition,
    checksum: String,
}

fn encode(transition: Transition) -> Result<Vec<u8>, String> {
    let payload = serde_json::to_vec(&transition).map_err(|e| e.to_string())?;
    let mut bytes = serde_json::to_vec(&Record {
        transition,
        checksum: sha256_prefixed(&payload),
    })
    .map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<Transition, String> {
    let record: Record =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid context transition: {e}"))?;
    if record.transition.schema != TRANSITION_SCHEMA
        || sha256_prefixed(&serde_json::to_vec(&record.transition).map_err(|e| e.to_string())?)
            != record.checksum
    {
        return Err("context transition schema/checksum mismatch".into());
    }
    Ok(record.transition)
}

// Check the whole lineage before either append OR full checkpoint promotion.
// Never let a stale scratch clone replace a newer acknowledged transition.
// An incomplete final write is not committed; only that suffix may be repaired.
fn verify_cursor(
    root: &Path,
    journal: &Path,
    cursor: &Cursor,
    rebuilt: bool,
) -> Result<(), String> {
    let size = file_len(journal)?;
    if cursor.path.as_deref() != Some(root) || cursor.checkpoint_id.is_none() {
        if size > 0 && !rebuilt {
            return Err(
                "context checkpoint reset requires a loaded lineage or explicit history rebuild"
                    .into(),
            );
        }
        return Ok(());
    }
    if checkpoint_id(root)? != cursor.checkpoint_id {
        return Err("stale context checkpoint lineage".into());
    }
    if size == cursor.offset {
        return Ok(());
    }
    if size < cursor.offset {
        return Err("context journal shortened since last acknowledgement".into());
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(journal)
        .map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(cursor.offset))
        .map_err(|e| e.to_string())?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).map_err(|e| e.to_string())?;
    if tail.contains(&b'\n') {
        return Err("stale context journal cursor; newer complete record exists".into());
    }
    file.set_len(cursor.offset).map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) fn persist_checkpoint(
    data_dir: &Path,
    session_id: &str,
    manager: &ContextManager,
) -> Result<PathBuf, String> {
    let root = context_ledger_path(data_dir, session_id);
    let lock = path_lock(&root);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    persist_checkpoint_locked(data_dir, session_id, manager)
}

fn persist_checkpoint_locked(
    data_dir: &Path,
    session_id: &str,
    manager: &ContextManager,
) -> Result<PathBuf, String> {
    if manager.session_id != session_id {
        return Err("context checkpoint session mismatch".into());
    }
    let root = context_ledger_path(data_dir, session_id);
    let journal = journal_path(data_dir, session_id);
    verify_cursor(
        &root,
        &journal,
        &manager.journal.get(),
        manager.ledger_rebuilt,
    )?;
    persist_tool_output_artifacts(data_dir, manager)?;
    static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = format!(
        "{}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut snapshot = manager.snapshot();
    snapshot.journal_checkpoint_id = Some(id.clone());
    let bytes = serde_json::to_vec_pretty(&snapshot).map_err(|e| e.to_string())?;
    let next_journal = journal_for(&root, Some(&id));
    atomic_write_bytes(&next_journal, &[])?;
    atomic_write_bytes(&root, &bytes)?;
    // Root now names the new journal. The old one is only garbage, never a
    // recovery candidate; interruption here leaves an unreferenced file.
    if journal != next_journal
        && let Err(error) = std::fs::remove_file(&journal)
    {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, "obsolete context journal cleanup deferred");
        }
    }
    manager
        .journal
        .saved(root.clone(), Some(id), 0, manager.ledger_items.len());
    Ok(root)
}

pub(super) fn persist_incremental(
    data_dir: &Path,
    session_id: &str,
    manager: &ContextManager,
) -> Result<PathBuf, String> {
    if manager.session_id != session_id {
        return Err("context transition session mismatch".into());
    }
    let root = context_ledger_path(data_dir, session_id);
    let lock = path_lock(&root);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let cursor = manager.journal.get();
    if cursor.path.as_deref() != Some(root.as_path()) || cursor.checkpoint_id.is_none() {
        // Legacy/missing checkpoint migration installs a complete baseline.
        return persist_checkpoint_locked(data_dir, session_id, manager);
    }
    let journal = journal_path(data_dir, session_id);
    verify_cursor(&root, &journal, &cursor, false)?;
    persist_tool_output_artifacts(data_dir, manager)?;
    let from = cursor.dirty_from.min(manager.ledger_items.len());
    let transition = Transition {
        schema: TRANSITION_SCHEMA.into(),
        checkpoint_id: cursor.checkpoint_id.clone().unwrap(),
        offset: cursor.offset,
        previous_items: cursor.items,
        tail_from: from,
        items: manager.ledger_items[from..].to_vec(),
        active_item_ids: manager.items.iter().map(|item| item.id.clone()).collect(),
        state: manager.state(),
        source_history: manager.source_history.clone(),
        compactions: manager.compactions.clone(),
        cache_epoch: manager.cache_epoch.clone(),
    };
    let bytes = encode(transition)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&journal)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.flush())
        .map_err(|e| format!("append context transition: {e}"))?;
    manager.journal.saved(
        root.clone(),
        cursor.checkpoint_id,
        cursor.offset + bytes.len() as u64,
        manager.ledger_items.len(),
    );
    Ok(root)
}

pub(super) fn load_snapshot(
    data_dir: &Path,
    session_id: &str,
) -> Result<Option<(ContextSnapshot, u64)>, String> {
    let root = context_ledger_path(data_dir, session_id);
    let lock = path_lock(&root);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if !root.exists() {
        return Ok(None);
    }
    let mut snapshot: ContextSnapshot =
        serde_json::from_slice(&std::fs::read(&root).map_err(|e| e.to_string())?)
            .map_err(|e| format!("parse context checkpoint: {e}"))?;
    if snapshot.state.session_id != session_id
        || (snapshot.schema != CONTEXT_MANAGER_SCHEMA
            && snapshot.schema != LEGACY_CONTEXT_MANAGER_SCHEMA)
    {
        return Err("context checkpoint session/schema mismatch".into());
    }
    if let Some(expected) = snapshot.source_head_hash.as_ref()
        && expected != &source_head_hash_for_items(&snapshot.items)
    {
        return Err("context checkpoint source-head hash mismatch".into());
    }
    let journal = journal_path(data_dir, session_id);
    let Some(id) = snapshot.journal_checkpoint_id.clone() else {
        return Ok(Some((snapshot, 0)));
    };
    let bytes = match std::fs::read(&journal) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.to_string()),
    };
    let mut offset = 0;
    while let Some(end) = bytes[offset..].iter().position(|b| *b == b'\n') {
        let record = decode(&bytes[offset..offset + end])?;
        if record.checkpoint_id != id {
            return Err("context transition checkpoint mismatch".into());
        }
        if record.offset != offset as u64
            || record.previous_items != snapshot.items.len()
            || record.tail_from > snapshot.items.len()
            || record.state.session_id != session_id
        {
            return Err("context transition order/source mismatch".into());
        }
        snapshot.items.truncate(record.tail_from);
        snapshot.items.extend(record.items);
        snapshot.active_item_ids = record.active_item_ids;
        snapshot.state = record.state;
        snapshot.source_history = record.source_history;
        snapshot.compactions = record.compactions;
        snapshot.cache_epoch = record.cache_epoch;
        offset += end + 1;
    }
    if offset > 0 {
        snapshot.source_head_hash = Some(source_head_hash_for_items(&snapshot.items));
        // These are rebuildable indexes; never serialize/rebuild the full
        // historical semantic index on the ordinary append path.
        snapshot.source_index.clear();
        snapshot.semantic_blocks.clear();
    }
    Ok(Some((snapshot, offset as u64)))
}

pub(super) fn bind_loaded(
    manager: &ContextManager,
    data_dir: &Path,
    session_id: &str,
    id: Option<String>,
    offset: u64,
    persisted_items: &[TranscriptItem],
) {
    manager.journal.saved(
        context_ledger_path(data_dir, session_id),
        id,
        offset,
        persisted_items.len(),
    );
    let first_difference = persisted_items
        .iter()
        .zip(manager.ledger_items.iter())
        .position(|(saved, normalized)| saved != normalized)
        .unwrap_or(persisted_items.len().min(manager.ledger_items.len()));
    manager.journal.dirty(first_difference);
}
