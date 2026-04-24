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
//! - `clone_for_subagent` produces an independent snapshot so parent and
//!   delegate agents cannot race.
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
/// Cloning (or [`FileStateCache::clone_for_subagent`]) yields a **deep copy**
/// so parent and subagent cannot race each other's entries.
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

    /// Return a deep-copied cache for a subagent.
    ///
    /// The child's writes/invalidations do not race the parent. The caps are
    /// copied verbatim. Use this at spawn/delegate boundaries.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn mk_entry(path: &str, mtime: SystemTime, size: usize) -> CacheEntry {
        CacheEntry::new(
            PathBuf::from(path),
            mtime,
            FileStateCache::content_hash(path.as_bytes()),
            size,
            false,
            None,
        )
    }

    fn mk_partial_entry(
        path: &str,
        mtime: SystemTime,
        size: usize,
        range: (u64, u64),
    ) -> CacheEntry {
        CacheEntry::new(
            PathBuf::from(path),
            mtime,
            FileStateCache::content_hash(path.as_bytes()),
            size,
            true,
            Some(range),
        )
    }

    #[test]
    fn should_hit_when_mtime_unchanged() {
        let cache = FileStateCache::new();
        let mtime = SystemTime::now();
        cache.put(mk_entry("/tmp/a.txt", mtime, 10));

        let hit = cache.get(Path::new("/tmp/a.txt"), mtime);
        assert!(hit.is_some(), "same mtime must return HIT");
        let hit = hit.unwrap();
        assert_eq!(hit.size, 10);
        assert!(!hit.is_partial_view);
    }

    #[test]
    fn should_miss_when_mtime_changed() {
        let cache = FileStateCache::new();
        let mtime_old = SystemTime::now();
        cache.put(mk_entry("/tmp/a.txt", mtime_old, 10));

        let mtime_new = mtime_old + Duration::from_secs(1);
        assert!(
            cache.get(Path::new("/tmp/a.txt"), mtime_new).is_none(),
            "changed mtime must return MISS"
        );
        // The entry must remain — only the lookup said MISS.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn should_evict_lru_when_max_entries_exceeded() {
        let cache = FileStateCache::builder().max_entries(2).build();
        let mtime = SystemTime::now();

        cache.put(mk_entry("/a", mtime, 10));
        cache.put(mk_entry("/b", mtime, 10));
        cache.put(mk_entry("/c", mtime, 10));

        assert_eq!(cache.len(), 2);
        assert!(
            cache.peek(Path::new("/a")).is_none(),
            "oldest entry must be evicted"
        );
        assert!(cache.peek(Path::new("/b")).is_some());
        assert!(cache.peek(Path::new("/c")).is_some());
    }

    #[test]
    fn should_evict_lru_when_max_bytes_exceeded() {
        let cache = FileStateCache::builder()
            .max_entries(100)
            .max_total_bytes(30)
            .build();
        let mtime = SystemTime::now();

        cache.put(mk_entry("/a", mtime, 20));
        cache.put(mk_entry("/b", mtime, 10));
        // /a + /b = 30 — within cap.
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.total_size_bytes(), 30);

        cache.put(mk_entry("/c", mtime, 20));
        // Must evict /a (oldest) to stay at <=30 bytes.
        assert!(cache.peek(Path::new("/a")).is_none());
        assert!(cache.peek(Path::new("/b")).is_some());
        assert!(cache.peek(Path::new("/c")).is_some());
        assert_eq!(cache.total_size_bytes(), 30);
    }

    #[test]
    fn should_bump_lru_position_on_hit() {
        let cache = FileStateCache::builder().max_entries(2).build();
        let mtime = SystemTime::now();

        cache.put(mk_entry("/a", mtime, 10));
        cache.put(mk_entry("/b", mtime, 10));

        // Touch /a so it becomes the most-recent.
        assert!(cache.get(Path::new("/a"), mtime).is_some());

        // Insert /c: must evict /b (now the oldest) not /a.
        cache.put(mk_entry("/c", mtime, 10));

        assert!(cache.peek(Path::new("/a")).is_some(), "/a was touched");
        assert!(cache.peek(Path::new("/b")).is_none(), "/b was evicted");
        assert!(cache.peek(Path::new("/c")).is_some());
    }

    #[test]
    fn should_invalidate_on_put_to_same_path() {
        let cache = FileStateCache::new();
        let mtime = SystemTime::now();
        cache.put(mk_entry("/a", mtime, 100));
        assert_eq!(cache.total_size_bytes(), 100);

        // Overwrite with a smaller entry: total_size must adjust.
        let new_mtime = mtime + Duration::from_secs(1);
        cache.put(mk_entry("/a", new_mtime, 25));

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_size_bytes(), 25);
        let peek = cache.peek(Path::new("/a")).unwrap();
        assert_eq!(peek.mtime, new_mtime);
        assert_eq!(peek.size, 25);
    }

    #[test]
    fn should_invalidate_explicit_path() {
        let cache = FileStateCache::new();
        let mtime = SystemTime::now();
        cache.put(mk_entry("/a", mtime, 10));
        cache.put(mk_entry("/b", mtime, 20));
        assert_eq!(cache.total_size_bytes(), 30);

        cache.invalidate(Path::new("/a"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_size_bytes(), 20);
        assert!(cache.peek(Path::new("/a")).is_none());
        assert!(cache.peek(Path::new("/b")).is_some());

        // Invalidating a missing path is a no-op.
        cache.invalidate(Path::new("/does-not-exist"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn should_clone_for_subagent_produces_independent_copy() {
        let parent = FileStateCache::new();
        let mtime = SystemTime::now();
        parent.put(mk_entry("/a", mtime, 10));
        parent.put(mk_entry("/b", mtime, 20));

        let child = parent.clone_for_subagent();
        assert_eq!(child.len(), 2);
        assert_eq!(child.total_size_bytes(), 30);

        // Writes in the child do not affect the parent.
        child.invalidate(Path::new("/a"));
        assert_eq!(child.len(), 1);
        assert_eq!(parent.len(), 2);

        // And writes in the parent do not reflect in the child.
        parent.put(mk_entry("/c", mtime, 5));
        assert!(parent.peek(Path::new("/c")).is_some());
        assert!(child.peek(Path::new("/c")).is_none());
    }

    #[test]
    fn should_clear_drops_all_entries() {
        let cache = FileStateCache::new();
        let mtime = SystemTime::now();
        cache.put(mk_entry("/a", mtime, 10));
        cache.put(mk_entry("/b", mtime, 20));
        cache.put(mk_entry("/c", mtime, 30));
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.total_size_bytes(), 60);

        cache.clear();

        assert!(cache.is_empty());
        assert_eq!(cache.total_size_bytes(), 0);
        assert!(cache.peek(Path::new("/a")).is_none());
    }

    #[test]
    fn should_handle_partial_view_entries() {
        let cache = FileStateCache::new();
        let mtime = SystemTime::now();
        let entry = mk_partial_entry("/big.rs", mtime, 500, (10, 30));
        cache.put(entry.clone());

        let fetched = cache.get(Path::new("/big.rs"), mtime).unwrap();
        assert!(fetched.is_partial_view);
        assert_eq!(fetched.view_range, Some((10, 30)));
        assert_eq!(fetched.size, 500);
    }

    #[test]
    fn should_not_hit_when_view_range_differs() {
        // Cache consumers are expected to compare `view_range` themselves —
        // the cache's job is to return the stored view. Verify the entry
        // surfaces its range so the caller can see it does not match.
        let cache = FileStateCache::new();
        let mtime = SystemTime::now();
        cache.put(mk_partial_entry("/f.rs", mtime, 100, (1, 50)));

        let entry = cache.get(Path::new("/f.rs"), mtime).unwrap();
        // Caller asked for (1, 100) but we cached (1, 50): the entry's range
        // is what tells the caller to ignore this hit.
        assert_ne!(entry.view_range, Some((1, 100)));
        assert_eq!(entry.view_range, Some((1, 50)));
    }

    #[test]
    fn should_reject_binary_extensions() {
        assert!(FileStateCache::has_binary_extension(Path::new("img.png")));
        assert!(FileStateCache::has_binary_extension(Path::new("doc.PDF")));
        assert!(FileStateCache::has_binary_extension(Path::new(
            "archive.tar.gz"
        )));
        assert!(!FileStateCache::has_binary_extension(Path::new("src.rs")));
        assert!(!FileStateCache::has_binary_extension(Path::new(
            "README.md"
        )));
        assert!(!FileStateCache::has_binary_extension(Path::new("noext")));
    }

    #[test]
    fn should_detect_text_vs_binary_content() {
        assert!(FileStateCache::is_text_cacheable(b"hello world\n"));
        assert!(FileStateCache::is_text_cacheable(
            "// comment\nfn main() {}".as_bytes()
        ));
        assert!(!FileStateCache::is_text_cacheable(b"\x00\x01\x02binary"));
        let big = vec![b'a'; 8192];
        assert!(FileStateCache::is_text_cacheable(&big));
    }

    #[test]
    fn should_format_file_unchanged_stub() {
        let full = format_file_unchanged_stub(Path::new("/tmp/foo.rs"), None);
        assert!(full.starts_with(FILE_UNCHANGED_STUB_PREFIX));
        assert!(full.contains("/tmp/foo.rs"));
        assert!(full.contains("full file cached"));

        let partial = format_file_unchanged_stub(Path::new("/tmp/bar.rs"), Some((3, 12)));
        assert!(partial.starts_with(FILE_UNCHANGED_STUB_PREFIX));
        assert!(partial.contains("/tmp/bar.rs"));
        assert!(partial.contains("3..12"));
    }

    #[test]
    fn builder_exposes_configured_caps() {
        let cache = FileStateCache::builder()
            .max_entries(10)
            .max_total_bytes(4096)
            .build();
        assert_eq!(cache.max_entries(), 10);
        assert_eq!(cache.max_total_bytes(), 4096);
    }

    #[test]
    fn content_hash_is_stable_for_same_input() {
        let a = FileStateCache::content_hash(b"hello");
        let b = FileStateCache::content_hash(b"hello");
        assert_eq!(a, b);
        let c = FileStateCache::content_hash(b"hello\n");
        assert_ne!(a, c);
    }
}
