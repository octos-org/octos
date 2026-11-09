//! Cross-process deletion barrier. A deleted incarnation cannot be written by
//! an old handle; fresh delivery helpers must name a thread admitted afterwards.
use super::*;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;

#[derive(Clone, Debug)]
pub(super) struct Epoch(std::result::Result<u64, String>);
impl Epoch {
    pub(super) fn failed(error: &eyre::Report) -> Self {
        Self(Err(format!("{error:#}")))
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    generation: u64,
    deleted: bool,
}

pub(super) struct Guard {
    _lock: File,
    dir: PathBuf,
    state: State,
}

impl Guard {
    pub(super) fn acquire(root: &Path, key: &SessionKey) -> Result<Self> {
        let dir = root
            .join(".session-lifecycle")
            .join(format!("{:x}", Sha256::digest(key.0.as_bytes())));
        std::fs::create_dir_all(&dir).wrap_err("create session lifecycle directory")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("lock"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                    if std::time::Instant::now() >= deadline {
                        return Err(eyre::eyre!("session storage is busy; retry"));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => return Err(e).wrap_err("lock session lifecycle"),
            }
        }
        #[cfg(test)]
        {
            let ms = TEST_ACQUIRE_DELAY_MS.load(std::sync::atomic::Ordering::Relaxed);
            if ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }
        let state = match std::fs::read(dir.join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).wrap_err("read session lifecycle state")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e).wrap_err("read session lifecycle state"),
        };
        Ok(Self {
            _lock: lock,
            dir,
            state,
        })
    }

    /// T-0296: run the blocking acquire (lock-poll + sync state-file I/O) on a
    /// blocking worker so a slow/locked disk cannot stall the async executor —
    /// and with it every session's admission, dispatch, and /health.
    #[allow(dead_code)]
    pub(super) async fn acquire_async(root: &Path, key: &SessionKey) -> Result<Self> {
        let root = root.to_path_buf();
        let key = key.clone();
        tokio::task::spawn_blocking(move || Self::acquire(&root, &key))
            .await
            .map_err(|e| eyre::eyre!("session lifecycle acquire task: {e}"))?
    }

    pub(super) fn epoch(&self) -> Epoch {
        Epoch(Ok(self.state.generation))
    }

    pub(super) fn check_epoch(&self, epoch: &Epoch) -> Result<()> {
        let generation = epoch
            .0
            .as_ref()
            .map_err(|e| eyre::eyre!("capture session incarnation: {e}"))?;
        if *generation != self.state.generation {
            return Err(eyre::eyre!("session was deleted; stale writer rejected"));
        }
        Ok(())
    }

    pub(super) fn check_live(&self, epoch: &Epoch) -> Result<()> {
        self.check_epoch(epoch)?;
        if self.state.deleted {
            return Err(eyre::eyre!("session was deleted"));
        }
        Ok(())
    }

    fn thread_path(&self, message: &Message) -> Result<PathBuf> {
        let thread = message
            .thread_id
            .as_ref()
            .or(message.client_message_id.as_ref())
            .ok_or_else(|| eyre::eyre!("reopened session write requires an admitted thread"))?;
        Ok(self
            .dir
            .join("threads")
            .join(format!("{:x}", Sha256::digest(thread.as_str().as_bytes()))))
    }

    pub(super) fn check_message(
        &self,
        epoch: &Epoch,
        message: &Message,
        admit_user: bool,
        owned_system: bool,
    ) -> Result<()> {
        self.check_epoch(epoch)?;
        if message.thread_id.is_some() || message.client_message_id.is_some() {
            match std::fs::read_to_string(self.thread_path(message)?) {
                Ok(generation) => {
                    let generation: u64 = generation.parse().wrap_err("read thread incarnation")?;
                    if generation != self.state.generation {
                        return Err(eyre::eyre!(
                            "thread belongs to a deleted session incarnation"
                        ));
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).wrap_err("read thread incarnation"),
            }
        }
        if self.state.generation != 0
            && !(admit_user
                && (message.role == MessageRole::User
                    || message.source.is_some()
                    || (owned_system
                        && message.role == MessageRole::System
                        && !self.state.deleted)))
        {
            return Err(eyre::eyre!(
                "delivery does not name a thread admitted after session deletion"
            ));
        }
        Ok(())
    }

    // Called only after the user's row is on disk. Delivery-only helpers never
    // admit a new thread: a new foreground user message establishes it.
    // Keep old thread tickets across deletion: interrupted-prompt recovery may
    // construct a fresh User message, but it still belongs to the revoked turn.
    pub(super) fn committed(&mut self, message: &Message) -> Result<()> {
        if message.role != MessageRole::User && message.source.is_none() {
            return Ok(());
        }
        if message.thread_id.is_some() || message.client_message_id.is_some() {
            let path = self.thread_path(message)?;
            std::fs::create_dir_all(path.parent().expect("thread parent"))?;
            if !path.try_exists()? {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                write!(file, "{}", self.state.generation)?;
                file.sync_all()?;
            }
            sync_dir(path.parent().expect("thread parent"))?;
            sync_dir(&self.dir)?;
        }
        if self.state.deleted {
            self.state.deleted = false;
            self.save()?;
        }
        Ok(())
    }

    pub(super) fn delete(&mut self) -> Result<()> {
        self.state.generation = self
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("session generation overflow"))?;
        self.state.deleted = true;
        self.save()
    }

    fn save(&self) -> Result<()> {
        let path = self.dir.join("state.json");
        let tmp = rewrite_tmp_path(&path);
        let result = (|| -> Result<()> {
            let mut file = File::create(&tmp)?;
            serde_json::to_writer(&mut file, &self.state)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            sync_dir(&self.dir)?;
            if let Some(parent) = self.dir.parent() {
                sync_dir(parent)?;
                if let Some(root) = parent.parent() {
                    sync_dir(root)?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}

pub(super) fn is_deleted(root: &Path, key: &SessionKey) -> Result<bool> {
    // State is saved by atomic rename. Read it without acquiring/creating the
    // lifecycle lock so status and other read-only RPCs leave no discovery
    // files behind, including for ids that have never existed.
    let path = root
        .join(".session-lifecycle")
        .join(format!("{:x}", Sha256::digest(key.0.as_bytes())))
        .join("state.json");
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<State>(&bytes)
            .wrap_err("read session lifecycle state")?
            .deleted),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).wrap_err("read session lifecycle state"),
    }
}

pub(super) fn capture(root: &Path, key: &SessionKey) -> Epoch {
    Epoch(
        Guard::acquire(root, key)
            .map(|guard| guard.state.generation)
            .map_err(|e| format!("{e:#}")),
    )
}

pub(super) fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?
        .sync_all()
        .wrap_err("sync session lifecycle directory")?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
static TEST_ACQUIRE_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    /// T-0296: a slow storage acquire on a blocking worker must NOT stall the
    /// async executor — a concurrent fast task (another session's admission, a
    /// /health tick) finishes while the slow session waits honestly. Without
    /// the spawn_blocking boundary the same delay would stall the executor and
    /// the fast task would not finish until the delay elapsed.
    // alan0x: current_thread so a blocking acquire on the executor genuinely
    // stalls the fast task (multi_thread moves work to a second worker and
    // masks the regression). With spawn_blocking the blocking pool is separate,
    // so this stays green; inline-blocking goes red (verified by mutation).
    #[tokio::test(flavor = "current_thread")]
    async fn slow_acquire_does_not_block_the_executor() {
        let root = tempfile::tempdir().unwrap();
        let key = SessionKey("local:slow-acquire".into());
        struct ResetDelay;
        impl Drop for ResetDelay {
            fn drop(&mut self) {
                TEST_ACQUIRE_DELAY_MS.store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _reset = ResetDelay;
        TEST_ACQUIRE_DELAY_MS.store(1200, std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        let slow = {
            let root = root.path().to_path_buf();
            let key = key.clone();
            tokio::spawn(async move { Guard::acquire_async(&root, &key).await })
        };
        let fast =
            tokio::spawn(async { tokio::time::sleep(std::time::Duration::from_millis(50)).await });
        tokio::time::timeout(std::time::Duration::from_millis(600), fast)
            .await
            .expect("a fast task must not be blocked by a slow acquire")
            .unwrap();
        let slow = slow.await.unwrap();
        assert!(
            slow.is_ok(),
            "slow acquire still completes: {:?}",
            slow.err()
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(1100),
            "the slow session itself waited honestly for its storage"
        );
    }
}
