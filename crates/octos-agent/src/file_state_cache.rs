//! File-state cache with LRU eviction and mtime-based invalidation (M8.4).
//!
//! Mirrors Claude Code's `fileStateCache.ts`: file-read tools put file contents
//! into the cache; later invocations that hit the same `(path, mtime)` pair
//! can return a typed [`FILE_UNCHANGED_STUB`] placeholder instead of
//! re-emitting the full file. In long coding sessions this reduces token cost
//! by 30-60 %.
//!
//! # Invariants
//!
//! - LRU ordering is maintained per `get` / `put` — the most recently accessed
//!   entry moves to the back.
//! - `put` evicts until both `max_entries` and `max_total_bytes` are respected.
//! - `get` returns `None` if the cached entry's `mtime` disagrees with the
//!   caller-supplied `current_mtime`.
//! - `invalidate` drops an entry and recovers its byte allotment.
//! - `clear` drops every entry and resets `total_size_bytes` to 0.
//! - A new model context starts with `empty_for_new_context`: another
//!   context's read records are not evidence that this context saw the body.
//! - `clone_for_subagent` is a legacy independent snapshot operation; copied
//!   entries are valid only when the same visible read results are retained.
//!
//! The cache is intentionally wrapped in internal mutability (a single
//! `Mutex`) so tools can consult it through an `Arc<FileStateCache>` without
//! coordinating on a `&mut` handle. A single lock keeps the state machine
//! small and easy to reason about; the critical sections are short.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Default maximum number of cached entries. Matches Claude Code's 100.
pub const DEFAULT_MAX_ENTRIES: usize = 100;

/// Default maximum total cached bytes. Matches Claude Code's 25 MB.
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 25 * 1024 * 1024;

/// Prefix used for the typed "file unchanged" tool-result placeholder.
///
/// Callers that want to emit the stub should format it as:
/// `"[FILE_UNCHANGED] No changes since last read: {path} (cached view
/// {start}..{end}). Use the previous tool result."` — see
/// [`format_file_unchanged_stub`].
pub const FILE_UNCHANGED_STUB_PREFIX: &str = "[FILE_UNCHANGED]";

/// Format the canonical `FILE_UNCHANGED_STUB` output used by file tools.
///
/// `view_range` is optional — pass `None` for a full-file view.
pub fn format_file_unchanged_stub(path: &Path, view_range: Option<(u64, u64)>) -> String {
    match view_range {
        Some((start, end)) => format!(
            "{} No changes since last read: {} (cached view {}..{}). Use the previous tool result.",
            FILE_UNCHANGED_STUB_PREFIX,
            path.display(),
            start,
            end,
        ),
        None => format!(
            "{} No changes since last read: {} (full file cached). Use the previous tool result.",
            FILE_UNCHANGED_STUB_PREFIX,
            path.display(),
        ),
    }
}

/// Per-file record stored in [`FileStateCache`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    /// Absolute path to the cached file.
    pub path: PathBuf,
    /// File modification time at the moment we read it.
    pub mtime: SystemTime,
    /// Stable content hash (FNV-1a or similar 64-bit hash) of the cached
    /// content. Used as a secondary check so cache consumers can detect edits
    /// that preserved mtime (e.g. touch-and-restore attacks).
    pub content_hash: u64,
    /// Byte size of the cached content.
    pub size: usize,
    /// Whether the cached content reflects a partial view (line range).
    pub is_partial_view: bool,
    /// The (start, end) line range (1-indexed, inclusive) the caller viewed,
    /// or `None` for a full-file read.
    pub view_range: Option<(u64, u64)>,
}

impl CacheEntry {
    /// Create a new entry.
    pub fn new(
        path: PathBuf,
        mtime: SystemTime,
        content_hash: u64,
        size: usize,
        is_partial_view: bool,
        view_range: Option<(u64, u64)>,
    ) -> Self {
        Self {
            path,
            mtime,
            content_hash,
            size,
            is_partial_view,
            view_range,
        }
    }
}

#[derive(Debug)]
struct Inner {
    entries: HashMap<PathBuf, CacheEntry>,
    /// Order of keys from least recently used (front) to most recently used
    /// (back). On every `get`/`put` we bump the touched key to the back.
    order: VecDeque<PathBuf>,
    total_size_bytes: usize,
}

impl Inner {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_size_bytes: 0,
        }
    }

    fn bump_to_back(&mut self, path: &Path) {
        if let Some(pos) = self.order.iter().position(|p| p == path) {
            if let Some(key) = self.order.remove(pos) {
                self.order.push_back(key);
            }
        }
    }
}

/// LRU cache of file-state entries with mtime-based invalidation.
///
/// Cloning yields a deep snapshot; it does not establish what another model
/// context has seen. Use [`FileStateCache::empty_for_new_context`] for children.
#[derive(Debug)]
pub struct FileStateCache {
    max_entries: usize,
    max_total_bytes: usize,
    inner: Mutex<Inner>,
}

impl Default for FileStateCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FileStateCache {
    /// Create a cache with default capacity (100 entries / 25 MB).
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Return a builder for tuning `max_entries` / `max_total_bytes`.
    pub fn builder() -> FileStateCacheBuilder {
        FileStateCacheBuilder::default()
    }

    /// Maximum number of cached entries.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Maximum total cached bytes across all entries.
    pub fn max_total_bytes(&self) -> usize {
        self.max_total_bytes
    }

    /// Total bytes currently cached.
    pub fn total_size_bytes(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.total_size_bytes
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.len()
    }

    /// Whether the cache currently has no recorded entries.
    pub fn is_empty(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.is_empty()
    }

    /// Look up `path` and return the cached entry if the stored `mtime`
    /// matches `current_mtime`. On HIT, bumps the entry to the most-recent
    /// position. Mismatched mtime returns `None` without evicting — the next
    /// `put` is expected to overwrite.
    pub fn get(&self, path: &Path, current_mtime: SystemTime) -> Option<CacheEntry> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stored = inner.entries.get(path)?;
        if stored.mtime != current_mtime {
            return None;
        }
        let entry = stored.clone();
        inner.bump_to_back(path);
        Some(entry)
    }

    /// Look up `path` without mtime validation.
    ///
    /// Useful for diagnostics and for the "did we ever see this file" check
    /// an integration test needs. Most callers should prefer [`Self::get`]
    /// which enforces the mtime invariant.
    pub fn peek(&self, path: &Path) -> Option<CacheEntry> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.get(path).cloned()
    }

    /// Insert or update an entry, bumping it to the most-recent LRU slot and
    /// evicting stale entries until both caps hold.
    pub fn put(&self, entry: CacheEntry) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = entry.path.clone();

        if let Some(old) = inner.entries.remove(&key) {
            inner.total_size_bytes = inner.total_size_bytes.saturating_sub(old.size);
            if let Some(pos) = inner.order.iter().position(|p| p == &key) {
                inner.order.remove(pos);
            }
        }

        inner.total_size_bytes = inner.total_size_bytes.saturating_add(entry.size);
        inner.entries.insert(key.clone(), entry);
        inner.order.push_back(key);

        // Evict until within caps.
        while (inner.entries.len() > self.max_entries
            || inner.total_size_bytes > self.max_total_bytes)
            && !inner.order.is_empty()
        {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(dropped) = inner.entries.remove(&oldest) {
                inner.total_size_bytes = inner.total_size_bytes.saturating_sub(dropped.size);
            }
        }
    }

    /// Drop the entry for `path` (e.g. after a successful write).
    pub fn invalidate(&self, path: &Path) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dropped) = inner.entries.remove(path) {
            inner.total_size_bytes = inner.total_size_bytes.saturating_sub(dropped.size);
        }
        if let Some(pos) = inner.order.iter().position(|p| p == path) {
            inner.order.remove(pos);
        }
    }

    /// Clear every entry. Call at compaction boundaries (M8.5 tier-3) so
    /// file-identity claims from an un-summarised read do not leak across the
    /// compaction line.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.clear();
        inner.order.clear();
        inner.total_size_bytes = 0;
    }

    /// M8.4/M8.6 fix-first item 7: seed the cache from recovered
    /// resume refs.
    ///
    /// The legacy session-actor hand-off consumed
    /// [`octos_core::ReplacementStateRef`] entries but did nothing with
    /// them — the TODO(M8.4) comment explicitly flagged this as a
    /// gap. The recovered refs carry the file path + optional content
    /// hash the transcript claimed was last read. We can NOT fully
    /// reconstruct the cache from a ref alone (the content bytes live
    /// only in the original tool result, which has been pruned), but
    /// we can record a best-effort entry with the ref's hash so a
    /// later `read_file` can at least detect a changed mtime.
    ///
    /// When `content_hash` is missing on the ref (the pre-M8.4
    /// transcript-only path) we skip that entry — a placeholder with a
    /// zero hash would turn every subsequent read into a false
    /// [FILE_UNCHANGED]. Returns the number of entries actually seeded.
    pub fn seed_from_replacement_refs<'a, I>(&self, refs: I) -> usize
    where
        I: IntoIterator<Item = &'a octos_core::ReplacementStateRef>,
    {
        let mut seeded = 0_usize;
        for r in refs {
            let Some(hash_str) = r.content_hash.as_deref() else {
                continue;
            };
            let Ok(hash) = hash_str.parse::<u64>() else {
                continue;
            };
            // Build a "best guess" CacheEntry: no mtime yet (set to
            // UNIX_EPOCH so the first real read will always miss and
            // repopulate); hash comes from the recovered ref; file size
            // is unknown (0).
            let entry = CacheEntry::new(
                r.path.clone(),
                std::time::SystemTime::UNIX_EPOCH,
                hash,
                0,
                false,
                None,
            );
            self.put(entry);
            seeded += 1;
        }
        seeded
    }

    /// Preserve cache policy while starting a context with no read knowledge.
    /// Use at independent child boundaries, including sanitized context forks.
    pub fn empty_for_new_context(&self) -> Self {
        Self {
            max_entries: self.max_entries,
            max_total_bytes: self.max_total_bytes,
            inner: Mutex::new(Inner::new()),
        }
    }

    /// Return a deep-copied snapshot (legacy method name).
    ///
    /// The child's writes/invalidations do not race the parent. The caps are
    /// copied verbatim. This does NOT make the read claims valid for a new
    /// transcript. Independent spawned contexts must use
    /// [`Self::empty_for_new_context`] instead.
    pub fn clone_for_subagent(&self) -> Self {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = Inner {
            entries: inner.entries.clone(),
            order: inner.order.clone(),
            total_size_bytes: inner.total_size_bytes,
        };
        Self {
            max_entries: self.max_entries,
            max_total_bytes: self.max_total_bytes,
            inner: Mutex::new(snapshot),
        }
    }

    /// Cheap 64-bit FNV-1a hash of a byte slice.
    ///
    /// Good enough for cache identity — we're not protecting against
    /// adversarial collisions, just defending against accidental mismatches.
    pub fn content_hash(bytes: &[u8]) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
        let mut hash = FNV_OFFSET;
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    /// Heuristically decide whether `content` looks like text that is safe to
    /// cache. Returns `false` for obvious binary blobs (images, PDFs,
    /// archives) so those do not occupy cache space.
    ///
    /// The check is deliberately cheap: if the first 4 KB is valid UTF-8 and
    /// contains no `\0` byte, we treat it as text.
    pub fn is_text_cacheable(content: &[u8]) -> bool {
        let prefix_len = content.len().min(4096);
        let prefix = &content[..prefix_len];
        if prefix.contains(&0u8) {
            return false;
        }
        std::str::from_utf8(prefix).is_ok()
    }

    /// File extensions we never cache even if UTF-8 validation slips through
    /// (e.g. JSON blobs that wrap base64 images).
    const NON_TEXT_EXTS: &'static [&'static str] = &[
        "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "tiff", "svg", "pdf", "mp3", "mp4",
        "wav", "flac", "ogg", "mov", "zip", "tar", "gz", "bz2", "xz", "7z", "exe", "dll", "so",
        "dylib", "class", "jar",
    ];

    /// Whether `path`'s extension is in the known-binary deny list.
    pub fn has_binary_extension(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| {
                let lower = ext.to_ascii_lowercase();
                Self::NON_TEXT_EXTS.iter().any(|&b| b == lower)
            })
            .unwrap_or(false)
    }
}

impl Clone for FileStateCache {
    fn clone(&self) -> Self {
        self.clone_for_subagent()
    }
}

/// Builder for [`FileStateCache`].
#[derive(Debug, Clone)]
pub struct FileStateCacheBuilder {
    max_entries: usize,
    max_total_bytes: usize,
}

impl Default for FileStateCacheBuilder {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

impl FileStateCacheBuilder {
    /// Maximum number of cached entries (must be >= 1).
    pub fn max_entries(mut self, value: usize) -> Self {
        self.max_entries = value.max(1);
        self
    }

    /// Maximum total cached bytes (must be >= 1).
    pub fn max_total_bytes(mut self, value: usize) -> Self {
        self.max_total_bytes = value.max(1);
        self
    }

    /// Construct the cache.
    pub fn build(self) -> FileStateCache {
        FileStateCache {
            max_entries: self.max_entries,
            max_total_bytes: self.max_total_bytes,
            inner: Mutex::new(Inner::new()),
        }
    }
}
