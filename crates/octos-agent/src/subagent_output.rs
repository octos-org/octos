//! Per-task disk output router for spawn_only sub-agents (M8.7).
//!
//! `SubAgentOutputRouter` captures stdout/stderr from long-running sub-agents
//! to per-task files on disk so the parent agent's in-memory message log
//! stays small even when a sub-agent emits megabytes of output. A small
//! preview (first 4 KB by default) is still retained for task status
//! displays.
//!
//! Key properties:
//! - **O_NOFOLLOW safety**: on Unix, output files are opened with
//!   `libc::O_NOFOLLOW` to atomically reject symlink targets. On other
//!   platforms we fall back to a pre-check via `symlink_metadata`.
//! - **Per-task byte cap**: each task may append up to `max_bytes_per_task`
//!   bytes before `append` returns `OverCapTruncated`.
//! - **Total byte cap**: across all tasks the router enforces
//!   `max_bytes_total`. When exceeded, the router signals
//!   `TotalOverCapKilled` so callers can cancel the offending task.
//! - **LRU eviction**: when a task transitions to a terminal state it is
//!   marked for eviction and the oldest terminal files are deleted first
//!   when the total cap is exceeded. Running tasks are never evicted.
//! - **GC sweep**: `gc_old_terminal` removes terminal files whose last
//!   update is older than a configurable duration (default 7 days).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// Default total-byte budget across all tasks (5 GB).
pub const DEFAULT_MAX_BYTES_TOTAL: u64 = 5 * 1024 * 1024 * 1024;

/// Default per-task byte budget (500 MB).
pub const DEFAULT_MAX_BYTES_PER_TASK: u64 = 500 * 1024 * 1024;

/// Default preview window kept in memory for task status displays (4 KB).
pub const DEFAULT_PREVIEW_BYTES: usize = 4 * 1024;

/// Default GC age threshold — terminal files older than this are swept.
pub const DEFAULT_GC_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Outcome of an `append` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendResult {
    /// Write succeeded in full.
    Ok,
    /// The write was truncated because the per-task byte cap would have
    /// been exceeded. Some bytes may have been written (up to the cap).
    OverCapTruncated,
    /// The global byte cap is exhausted — the task MUST be cancelled.
    /// No bytes were written for this call.
    TotalOverCapKilled,
}

/// Phase of a per-task output file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Terminal,
}

/// Metadata tracked per task.
#[derive(Debug)]
struct LogHandle {
    path: PathBuf,
    bytes_written: u64,
    overflow_bytes: u64,
    last_updated: SystemTime,
    phase: Phase,
    preview: Vec<u8>,
}

impl LogHandle {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            bytes_written: 0,
            overflow_bytes: 0,
            last_updated: SystemTime::now(),
            phase: Phase::Running,
            preview: Vec::new(),
        }
    }
}

/// Builder-configurable router that routes sub-agent textual output to
/// per-task files under `<root>/<session_id>/<task_id>.out`.
pub struct SubAgentOutputRouter {
    root: PathBuf,
    max_bytes_total: u64,
    max_bytes_per_task: u64,
    preview_cap: usize,
    open_handles: Mutex<HashMap<String, LogHandle>>,
    total_bytes: Mutex<u64>,
}

impl std::fmt::Debug for SubAgentOutputRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentOutputRouter")
            .field("root", &self.root)
            .field("max_bytes_total", &self.max_bytes_total)
            .field("max_bytes_per_task", &self.max_bytes_per_task)
            .field("preview_cap", &self.preview_cap)
            .finish()
    }
}

impl SubAgentOutputRouter {
    /// Create a router with default caps.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes_total: DEFAULT_MAX_BYTES_TOTAL,
            max_bytes_per_task: DEFAULT_MAX_BYTES_PER_TASK,
            preview_cap: DEFAULT_PREVIEW_BYTES,
            open_handles: Mutex::new(HashMap::new()),
            total_bytes: Mutex::new(0),
        }
    }

    /// Override the total byte cap (builder-style).
    #[must_use]
    pub fn with_max_bytes_total(mut self, max: u64) -> Self {
        self.max_bytes_total = max;
        self
    }

    /// Override the per-task byte cap (builder-style).
    #[must_use]
    pub fn with_max_bytes_per_task(mut self, max: u64) -> Self {
        self.max_bytes_per_task = max;
        self
    }

    /// Override the in-memory preview cap (builder-style).
    #[must_use]
    pub fn with_preview_bytes(mut self, cap: usize) -> Self {
        self.preview_cap = cap;
        self
    }

    /// Root directory under which per-session subtrees live.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve the per-task output file path.
    pub fn path_for(&self, session_id: &str, task_id: &str) -> PathBuf {
        self.root.join(session_id).join(format!("{task_id}.out"))
    }

    /// Append raw bytes to the output file for `task_id`, honoring both
    /// the per-task and total byte caps. Returns an IO error if the
    /// underlying file could not be created or written.
    pub fn append(
        &self,
        session_id: &str,
        task_id: &str,
        bytes: &[u8],
    ) -> std::io::Result<AppendResult> {
        if bytes.is_empty() {
            return Ok(AppendResult::Ok);
        }

        // Lock order: handles -> total_bytes. Keep it consistent.
        let mut handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_handle(&mut handles, session_id, task_id)?;

        // Evaluate the per-task cap up-front so we can decide how many
        // bytes to actually write.
        let (handle_path, remaining_task) = {
            let handle = handles.get(task_id).expect("handle was just ensured");
            let remaining = self.max_bytes_per_task.saturating_sub(handle.bytes_written);
            (handle.path.clone(), remaining)
        };

        let (write_bytes, truncated) = if (bytes.len() as u64) > remaining_task {
            let cut = remaining_task as usize;
            (&bytes[..cut], true)
        } else {
            (bytes, false)
        };

        // Total cap check — evaluated against the write size we actually plan.
        {
            let total_now = *self.total_bytes.lock().unwrap_or_else(|e| e.into_inner());
            if total_now + write_bytes.len() as u64 > self.max_bytes_total {
                // Before giving up, try to evict terminal tasks so the
                // incoming write can fit.
                let required = total_now + write_bytes.len() as u64 - self.max_bytes_total;
                self.evict_terminal_to_free(&mut handles, required)?;
                let total_retry = *self.total_bytes.lock().unwrap_or_else(|e| e.into_inner());
                if total_retry + write_bytes.len() as u64 > self.max_bytes_total {
                    return Ok(AppendResult::TotalOverCapKilled);
                }
            }
        }

        if write_bytes.is_empty() {
            // Per-task cap already exhausted; record overflow and bail.
            if let Some(handle) = handles.get_mut(task_id) {
                handle.overflow_bytes = handle.overflow_bytes.saturating_add(bytes.len() as u64);
            }
            return Ok(AppendResult::OverCapTruncated);
        }

        // Open the file for append each time — cheap on modern filesystems and
        // avoids holding an fd across long idle periods.
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(not(unix))]
        {
            if handle_path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink rejected",
                ));
            }
        }
        let mut file = opts.open(&handle_path)?;
        file.write_all(write_bytes)?;

        // Commit the write.
        if let Some(handle) = handles.get_mut(task_id) {
            handle.bytes_written = handle
                .bytes_written
                .saturating_add(write_bytes.len() as u64);
            handle.last_updated = SystemTime::now();
            if truncated {
                handle.overflow_bytes = handle
                    .overflow_bytes
                    .saturating_add((bytes.len() as u64).saturating_sub(write_bytes.len() as u64));
            }
            let preview_room = self.preview_cap.saturating_sub(handle.preview.len());
            if preview_room > 0 {
                let take = preview_room.min(write_bytes.len());
                handle.preview.extend_from_slice(&write_bytes[..take]);
            }
        }

        let mut total = self.total_bytes.lock().unwrap_or_else(|e| e.into_inner());
        *total = total.saturating_add(write_bytes.len() as u64);

        if truncated {
            Ok(AppendResult::OverCapTruncated)
        } else {
            Ok(AppendResult::Ok)
        }
    }

    /// Mark a task as terminal (Completed / Failed / Cancelled). The file
    /// stays on disk and is eligible for LRU eviction or GC.
    pub fn mark_terminal(&self, task_id: &str) {
        let mut handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = handles.get_mut(task_id) {
            handle.phase = Phase::Terminal;
            handle.last_updated = SystemTime::now();
        }
    }

    /// Return the per-task preview bytes, if any.
    pub fn preview(&self, task_id: &str) -> Option<Vec<u8>> {
        let handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.get(task_id).map(|h| h.preview.clone())
    }

    /// Return the last N lines currently written to disk for `task_id`.
    /// Used by `AgentSummaryGenerator` to summarize recent activity.
    pub fn tail_lines(&self, task_id: &str, lines: usize) -> std::io::Result<Vec<String>> {
        let path = {
            let handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
            match handles.get(task_id) {
                Some(h) => h.path.clone(),
                None => return Ok(Vec::new()),
            }
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let collected: Vec<String> = contents
                    .lines()
                    .rev()
                    .take(lines)
                    .map(String::from)
                    .collect();
                Ok(collected.into_iter().rev().collect())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Return the bytes recorded for `task_id`, or 0 if untracked.
    pub fn bytes_written(&self, task_id: &str) -> u64 {
        let handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.get(task_id).map(|h| h.bytes_written).unwrap_or(0)
    }

    /// Return the overflow (truncated) byte count for `task_id`.
    pub fn overflow_bytes(&self, task_id: &str) -> u64 {
        let handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.get(task_id).map(|h| h.overflow_bytes).unwrap_or(0)
    }

    /// Sum of per-task bytes currently tracked.
    pub fn total_bytes(&self) -> u64 {
        *self.total_bytes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Is the task currently marked terminal?
    pub fn is_terminal(&self, task_id: &str) -> bool {
        let handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        handles
            .get(task_id)
            .map(|h| matches!(h.phase, Phase::Terminal))
            .unwrap_or(false)
    }

    /// Sweep terminal files older than `older_than` off disk. Returns the
    /// number of files removed.
    pub fn gc_old_terminal(&self, older_than: Duration) -> std::io::Result<usize> {
        let now = SystemTime::now();
        let mut handles = self.open_handles.lock().unwrap_or_else(|e| e.into_inner());
        let mut total = self.total_bytes.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = 0usize;
        let victims: Vec<String> = handles
            .iter()
            .filter_map(|(id, h)| {
                if !matches!(h.phase, Phase::Terminal) {
                    return None;
                }
                match now.duration_since(h.last_updated) {
                    Ok(age) if age >= older_than => Some(id.clone()),
                    _ => None,
                }
            })
            .collect();
        for id in victims {
            if let Some(handle) = handles.remove(&id) {
                if handle.path.exists() {
                    let _ = std::fs::remove_file(&handle.path);
                }
                *total = total.saturating_sub(handle.bytes_written);
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn ensure_handle(
        &self,
        handles: &mut MutexGuard<'_, HashMap<String, LogHandle>>,
        session_id: &str,
        task_id: &str,
    ) -> std::io::Result<()> {
        if !handles.contains_key(task_id) {
            let parent = self.root.join(session_id);
            std::fs::create_dir_all(&parent)?;
            let path = parent.join(format!("{task_id}.out"));

            // Reject symlink targets up front — create() with O_NOFOLLOW will
            // error on Unix, but we also pre-check on all platforms so the
            // error message is uniform and portable.
            if path.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "symlink rejected",
                ));
            }
            handles.insert(task_id.to_string(), LogHandle::new(path));
        }
        Ok(())
    }

    /// Evict terminal tasks (oldest first) until at least `required` bytes
    /// have been freed, or no more terminal tasks remain.
    fn evict_terminal_to_free(
        &self,
        handles: &mut MutexGuard<'_, HashMap<String, LogHandle>>,
        required: u64,
    ) -> std::io::Result<()> {
        let mut freed = 0u64;
        let mut total = self.total_bytes.lock().unwrap_or_else(|e| e.into_inner());
        // Order terminal handles by last_updated ascending (oldest first).
        let mut terminal: Vec<(String, SystemTime, u64, PathBuf)> = handles
            .iter()
            .filter(|(_, h)| matches!(h.phase, Phase::Terminal))
            .map(|(id, h)| (id.clone(), h.last_updated, h.bytes_written, h.path.clone()))
            .collect();
        terminal.sort_by_key(|(_, last, _, _)| *last);
        for (id, _, bytes, path) in terminal {
            if freed >= required {
                break;
            }
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            handles.remove(&id);
            *total = total.saturating_sub(bytes);
            freed = freed.saturating_add(bytes);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn make_router(dir: &Path) -> SubAgentOutputRouter {
        SubAgentOutputRouter::new(dir)
    }

    #[test]
    fn should_write_output_to_disk_when_append_called() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = make_router(dir.path());
        let result = router.append("sess-a", "task-1", b"hello world").unwrap();
        assert_eq!(result, AppendResult::Ok);

        let path = router.path_for("sess-a", "task-1");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(router.bytes_written("task-1"), 11);
    }

    #[test]
    fn should_create_task_specific_file_per_task_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = make_router(dir.path());
        router.append("sess-a", "task-1", b"alpha").unwrap();
        router.append("sess-a", "task-2", b"beta").unwrap();

        let p1 = router.path_for("sess-a", "task-1");
        let p2 = router.path_for("sess-a", "task-2");
        assert_ne!(p1, p2);
        assert_eq!(std::fs::read_to_string(&p1).unwrap(), "alpha");
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "beta");
    }

    #[test]
    fn should_honor_per_task_byte_cap_and_truncate() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = SubAgentOutputRouter::new(dir.path()).with_max_bytes_per_task(5);
        let result = router.append("sess-a", "task-1", b"hello world").unwrap();
        assert_eq!(result, AppendResult::OverCapTruncated);
        assert_eq!(router.bytes_written("task-1"), 5);
        assert!(router.overflow_bytes("task-1") > 0);

        // Subsequent writes on the same task should continue to be truncated.
        let result2 = router.append("sess-a", "task-1", b"more").unwrap();
        assert_eq!(result2, AppendResult::OverCapTruncated);
        assert_eq!(router.bytes_written("task-1"), 5);
    }

    #[test]
    fn should_kill_task_when_total_byte_cap_exceeded() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = SubAgentOutputRouter::new(dir.path())
            .with_max_bytes_total(10)
            .with_max_bytes_per_task(100);
        assert_eq!(
            router.append("sess-a", "task-1", b"abcdefghij").unwrap(),
            AppendResult::Ok
        );
        // Task-1 is still Running so eviction cannot reclaim. Next write kills.
        let r = router.append("sess-a", "task-2", b"x").unwrap();
        assert_eq!(r, AppendResult::TotalOverCapKilled);
    }

    #[test]
    fn should_evict_terminal_task_files_lru_when_over_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = SubAgentOutputRouter::new(dir.path())
            .with_max_bytes_total(10)
            .with_max_bytes_per_task(100);
        assert_eq!(
            router.append("sess-a", "task-1", b"abcde").unwrap(),
            AppendResult::Ok
        );
        assert_eq!(
            router.append("sess-a", "task-2", b"fghij").unwrap(),
            AppendResult::Ok
        );
        router.mark_terminal("task-1");
        // Sleep briefly so LRU ordering by SystemTime is unambiguous.
        sleep(Duration::from_millis(5));
        router.mark_terminal("task-2");

        // Next write should trigger eviction of task-1 (oldest terminal).
        let r = router.append("sess-a", "task-3", b"XY").unwrap();
        assert_eq!(r, AppendResult::Ok);
        assert!(!router.path_for("sess-a", "task-1").exists());
    }

    #[test]
    fn should_keep_running_task_files_during_eviction() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = SubAgentOutputRouter::new(dir.path())
            .with_max_bytes_total(10)
            .with_max_bytes_per_task(100);
        router
            .append("sess-a", "task-running", b"abcdefghij")
            .unwrap();

        // task-running is still Running, so it cannot be evicted — new
        // writes should be killed rather than freeing its bytes.
        let r = router.append("sess-a", "task-new", b"X").unwrap();
        assert_eq!(r, AppendResult::TotalOverCapKilled);
        assert!(router.path_for("sess-a", "task-running").exists());
    }

    #[test]
    fn should_reject_symlink_target_paths() {
        #[cfg(unix)]
        {
            let dir = tempfile::TempDir::new().unwrap();
            let router = make_router(dir.path());
            // Pre-create parent + a symlink where the router's target file
            // would land.
            let session_dir = dir.path().join("sess-a");
            std::fs::create_dir_all(&session_dir).unwrap();
            let target = session_dir.join("task-1.out");
            let other = dir.path().join("elsewhere.txt");
            std::fs::write(&other, "outside").unwrap();
            std::os::unix::fs::symlink(&other, &target).unwrap();

            let err = router.append("sess-a", "task-1", b"payload").unwrap_err();
            // Either ELOOP (O_NOFOLLOW) or PermissionDenied (fallback)
            assert!(
                matches!(err.kind(), std::io::ErrorKind::PermissionDenied)
                    || err
                        .raw_os_error()
                        .map(|c| c == libc::ELOOP)
                        .unwrap_or(false)
            );
            // Ensure the real target file was not overwritten with payload.
            assert_eq!(std::fs::read_to_string(&other).unwrap(), "outside");
        }
    }

    #[test]
    fn should_gc_terminal_files_older_than_duration() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = make_router(dir.path());
        router.append("sess-a", "task-1", b"x").unwrap();
        router.mark_terminal("task-1");
        // Force last_updated backwards by manipulating the handle: the
        // simplest approach is a zero-age GC and then non-zero age test.
        assert_eq!(router.gc_old_terminal(Duration::from_secs(999)).unwrap(), 0);
        assert!(router.path_for("sess-a", "task-1").exists());
        // With zero threshold, the terminal file is now eligible.
        assert_eq!(router.gc_old_terminal(Duration::from_secs(0)).unwrap(), 1);
        assert!(!router.path_for("sess-a", "task-1").exists());
    }

    #[test]
    fn should_preview_first_bytes_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = SubAgentOutputRouter::new(dir.path()).with_preview_bytes(5);
        router.append("sess-a", "task-1", b"hello world").unwrap();
        let preview = router.preview("task-1").unwrap();
        assert_eq!(preview, b"hello".to_vec());
    }

    #[test]
    fn should_tail_lines_return_last_n() {
        let dir = tempfile::TempDir::new().unwrap();
        let router = make_router(dir.path());
        router
            .append("sess-a", "task-1", b"line1\nline2\nline3\nline4\n")
            .unwrap();
        let tail = router.tail_lines("task-1", 2).unwrap();
        assert_eq!(tail, vec!["line3".to_string(), "line4".to_string()]);
    }
}
