//! Per-session construction fencing.
//!
//! A ticket belongs to one construction generation. Delete acquires the key's
//! write lock for its whole cleanup, cancels the current ticket, and blocks
//! replacement registration until cleanup is complete. A stale ticket is never
//! made live again.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use octos_core::SessionKey;
use tokio::sync::{OwnedRwLockWriteGuard, RwLock, RwLockReadGuard};

struct KeyFence {
    gate: Arc<RwLock<()>>,
    deleting: AtomicUsize,
    tickets: Mutex<Vec<Weak<ConstructionTicket>>>,
}

impl KeyFence {
    fn new() -> Self {
        Self {
            gate: Arc::new(RwLock::new(())),
            deleting: AtomicUsize::new(0),
            tickets: Mutex::new(Vec::new()),
        }
    }
}

fn key_fence(key: &SessionKey) -> Arc<KeyFence> {
    static FENCES: OnceLock<Mutex<HashMap<String, Weak<KeyFence>>>> = OnceLock::new();
    let mut fences = FENCES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    fences.retain(|_, fence| fence.strong_count() > 0);
    if let Some(fence) = fences.get(&key.0).and_then(Weak::upgrade) {
        return fence;
    }
    let fence = Arc::new(KeyFence::new());
    fences.insert(key.0.clone(), Arc::downgrade(&fence));
    fence
}

/// One construction generation. `read_stage` is synchronous because real
/// factories run on the blocking pool. Delete waits only for the current write
/// stage, never for the whole factory.
pub struct ConstructionTicket {
    fence: Arc<KeyFence>,
    cancelled: AtomicBool,
}

impl ConstructionTicket {
    /// Register before releasing the caller's admission guard. Returns `None`
    /// while deletion cleanup owns the key.
    pub async fn register(key: &SessionKey) -> Option<Arc<Self>> {
        let fence = key_fence(key);
        let _read = fence.gate.read().await;
        if fence.deleting.load(Ordering::Acquire) != 0 {
            return None;
        }
        let ticket = Arc::new(Self {
            fence: fence.clone(),
            cancelled: AtomicBool::new(false),
        });
        fence
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::downgrade(&ticket));
        Some(ticket)
    }

    /// Hold a read guard for one synchronous side-effect stage.
    pub fn read_stage(&self) -> Option<RwLockReadGuard<'_, ()>> {
        let guard = self.fence.gate.blocking_read();
        if self.cancelled.load(Ordering::Acquire)
            || self.fence.deleting.load(Ordering::Acquire) != 0
        {
            None
        } else {
            Some(guard)
        }
    }

    /// Cancel this generation without cancelling a later replacement.
    pub async fn cancel(&self) {
        let _write = self.fence.gate.write().await;
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.fence.deleting.load(Ordering::Acquire) != 0
    }
}

struct PendingDeletion {
    fence: Arc<KeyFence>,
    armed: bool,
}

impl Drop for PendingDeletion {
    fn drop(&mut self) {
        if self.armed {
            self.fence.deleting.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Holds deletion exclusion for the complete filesystem/registry cleanup.
pub struct ConstructionDeletion {
    fence: Arc<KeyFence>,
    _guard: OwnedRwLockWriteGuard<()>,
}

impl Drop for ConstructionDeletion {
    fn drop(&mut self) {
        self.fence.deleting.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Cancel every live construction and exclude replacements until the returned
/// guard is dropped. All delete/mutation entry points must hold this guard for
/// their entire cleanup, not only while setting a flag.
pub async fn begin_session_deletion(key: &SessionKey) -> ConstructionDeletion {
    let fence = key_fence(key);
    fence.deleting.fetch_add(1, Ordering::AcqRel);
    let mut pending = PendingDeletion {
        fence: fence.clone(),
        armed: true,
    };
    let guard = fence.gate.clone().write_owned().await;
    pending.armed = false;
    let tickets = {
        let mut registered = fence
            .tickets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        registered.retain(|ticket| ticket.strong_count() > 0);
        registered
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>()
    };
    for ticket in tickets {
        ticket.cancelled.store(true, Ordering::Release);
    }
    ConstructionDeletion {
        fence,
        _guard: guard,
    }
}

#[cfg(test)]
type ClearExistingHook = Arc<dyn Fn() + Send + Sync + 'static>;

#[cfg(test)]
fn clear_existing_hooks() -> &'static Mutex<HashMap<String, ClearExistingHook>> {
    static HOOKS: OnceLock<Mutex<HashMap<String, ClearExistingHook>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn set_clear_existing_hook(key: &SessionKey, hook: ClearExistingHook) {
    clear_existing_hooks()
        .lock()
        .unwrap()
        .insert(key.0.clone(), hook);
}

#[cfg(test)]
pub(crate) fn remove_clear_existing_hook(key: &SessionKey) {
    clear_existing_hooks().lock().unwrap().remove(&key.0);
}

#[cfg(test)]
pub(crate) fn run_clear_existing_hook(key: &SessionKey) {
    let hook = clear_existing_hooks().lock().unwrap().get(&key.0).cloned();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deletion_cancels_all_live_tickets_and_blocks_replacement() {
        let key = SessionKey("cli:construction-fence".into());
        let first = ConstructionTicket::register(&key).await.unwrap();
        let second_before_delete = ConstructionTicket::register(&key).await.unwrap();
        let deletion = begin_session_deletion(&key).await;
        let replacement = {
            let key = key.clone();
            tokio::spawn(async move { ConstructionTicket::register(&key).await })
        };
        tokio::task::yield_now().await;
        assert!(!replacement.is_finished());
        drop(deletion);
        let second = replacement.await.unwrap().unwrap();
        assert!(first.is_cancelled());
        assert!(second_before_delete.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(
            tokio::task::spawn_blocking(move || first.read_stage().is_none())
                .await
                .unwrap()
        );
        assert!(
            tokio::task::spawn_blocking(move || second.read_stage().is_some())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn clear_existing_waits_for_write_stage_and_fences_next_stage() {
        let dir = tempfile::tempdir().unwrap();
        let key = SessionKey("cli:construction-fence-storage".into());
        let ticket = ConstructionTicket::register(&key).await.unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocking_dir = dir.path().to_owned();
        let blocking_key = key.clone();
        let factory = tokio::task::spawn_blocking(move || {
            let stage = ticket.read_stage().expect("first write stage is live");
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            let _handle = crate::SessionHandle::open(&blocking_dir, &blocking_key);
            drop(stage);
            ticket.read_stage().is_none()
        });
        started_rx.recv().unwrap();
        let deleting_dir = dir.path().to_owned();
        let deleting_key = key.clone();
        let deleting = tokio::spawn(async move {
            let mut sessions = crate::SessionManager::open(&deleting_dir).unwrap();
            sessions.clear_existing(&deleting_key).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!deleting.is_finished());
        release_tx.send(()).unwrap();
        deleting.await.unwrap().unwrap();
        assert!(factory.await.unwrap());
        let replacement = ConstructionTicket::register(&key).await.unwrap();
        assert!(!replacement.is_cancelled());
    }

    #[tokio::test]
    async fn aborted_clear_existing_keeps_replacement_fenced_until_blocking_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let key = SessionKey("cli:construction-fence-abort".into());
        let _handle = crate::SessionHandle::open(dir.path(), &key);
        let phase = Arc::new(AtomicUsize::new(0));
        let hook_phase = phase.clone();
        set_clear_existing_hook(
            &key,
            Arc::new(move || {
                hook_phase.store(1, Ordering::Release);
                while hook_phase.load(Ordering::Acquire) != 2 {
                    std::thread::yield_now();
                }
            }),
        );
        let deleting_dir = dir.path().to_owned();
        let deleting_key = key.clone();
        let deleting = tokio::spawn(async move {
            let mut sessions = crate::SessionManager::open(&deleting_dir).unwrap();
            sessions.clear_existing(&deleting_key).await
        });
        while phase.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
        deleting.abort();
        assert!(deleting.await.unwrap_err().is_cancelled());
        let replacement = {
            let key = key.clone();
            tokio::spawn(async move { ConstructionTicket::register(&key).await })
        };
        tokio::task::yield_now().await;
        assert!(!replacement.is_finished());
        phase.store(2, Ordering::Release);
        let ticket = tokio::time::timeout(std::time::Duration::from_secs(2), replacement)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!ticket.is_cancelled());
        remove_clear_existing_hook(&key);
    }
}
