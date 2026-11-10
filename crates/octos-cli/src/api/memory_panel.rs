//! Read-only memory panel handlers (web parity audit P3 item 6: the book
//! gives the memory system top-3 prominence, but the web dashboard had ZERO
//! memory UX — MEMORY.md, daily notes and the entity bank were invisible
//! outside the CLI).
//!
//! Reached over the UI Protocol via the `memory/overview` / `memory/entity`
//! methods (gated by `auxiliary.rest_to_ws.v1`), which wrap these handlers on
//! the WS transport. The former `/api/my/memory*` REST routes were retired to
//! keep a single transport (see `router.rs`).
//!
//! Deliberately VIEWER-ONLY: writes stay with the agent tools
//! (`save_memory`, `memory_note`) and the refresh pipeline, so the
//! panel can never race a concurrent refresh sweep. Auth + tenant
//! scoping mirror `/api/my/soul`: `resolve_my_profile_id` (admin token
//! → admin profile, user session → own profile, host-scope enforced),
//! then everything is read from that profile's own data dir.
//!
//! Symlink posture (codex #1611 rounds 1–2): a tenant controls the
//! CONTENT of its memory directory (agent shell in a workspace-cwd
//! session), so any component under the data dir — `memory/`, the bank
//! dir, a page — can be (or be swapped for) a symlink at another
//! profile's data. The store's own readers follow symlinks (fine
//! agent-side, where the sandbox owns policy); this daemon-privileged
//! HTTP path must not. Canonicalize-then-open is NOT enough: the check
//! and the open are separate syscalls, and a directory swapped in
//! between hands us the victim's tree (round-2 P1). On Unix every read
//! therefore walks component-by-component with `openat(O_NOFOLLOW)`
//! anchored to the parent FD — the same structure as
//! `preview.rs::open_no_follow_walk` (issue #996) — so a mid-walk swap
//! cannot redirect resolution. Symlinked anything renders as the empty
//! state / 404.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use axum::http::StatusCode;
use serde::Serialize;

use super::AppState;

/// One recent daily-note file (`memory/YYYY-MM-DD.md`).
#[derive(Debug, Serialize)]
pub struct DailyNote {
    pub date: String,
    pub content: String,
}

/// One entity page summary from the bank (`memory/bank/entities/*.md`).
#[derive(Debug, Serialize)]
pub struct EntitySummary {
    pub name: String,
    /// First abstract/summary line of the page — the store's OWN
    /// `extract_abstract`, so the panel shows byte-identical strings to
    /// what `list_entities` feeds the agent prompt.
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryOverviewResponse {
    pub ok: bool,
    /// Full `MEMORY.md` content ("" when absent).
    pub long_term: String,
    /// RFC 3339 mtime of `MEMORY.md`, whenever the file exists — an
    /// EMPTY long-term memory (e.g. after consolidation hard-deletes
    /// the last entry) still reports when it last changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_term_updated_at: Option<String>,
    /// Today's daily note ("" when absent).
    pub today: String,
    /// Last 7 days of daily notes, newest first (excludes empty files).
    pub recent: Vec<DailyNote>,
    /// Entity bank pages (name + first summary line), name-sorted.
    pub entities: Vec<EntitySummary>,
    /// True when the bank held more than [`MAX_PANEL_ENTITIES`] pages
    /// and the list was cut off (codex #1611 r8 P2).
    pub entities_truncated: bool,
    /// Staged capture notes OBSERVED waiting for the next refresh
    /// sweep. Exact when `staging_truncated` is false; a lower bound
    /// when true.
    pub staging_notes: usize,
    /// True when the staging scan stopped early — at
    /// [`MAX_STAGING_NOTE_COUNT`] matches or the raw directory budget
    /// — so `staging_notes` is a lower bound, not an exact count
    /// (codex #1611 r10+r11 P2: never fabricate, never understate as
    /// exact).
    pub staging_truncated: bool,
    /// Effective `memory.refresh.enabled` for this profile (host-level
    /// policy merged in — same semantics the runtime bootstrap uses).
    pub refresh_enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct MemoryEntityResponse {
    pub ok: bool,
    pub name: String,
    pub content: String,
}

/// Caps CONCURRENT panel scans on the shared blocking pool. The walk
/// reads tenant-controlled files; without a bound, parallel overview
/// requests could occupy blocking threads wholesale and starve every
/// other spawn_blocking/tokio::fs user in the process (codex #1611 r5
/// P1). Waiters queue as plain futures — cheap — while at most
/// PANEL_SCAN_PERMITS scans touch disk at once.
static PANEL_SCAN_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Per-file read cap. Panel files are KB-scale markdown; a planted
/// multi-GB "MEMORY.md" must not be slurped into memory. Over-cap
/// files render as absent.
const MAX_PANEL_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Caps entity-bank enumeration per overview response (codex #1611 r8
/// P2). The per-file cap and scan permits bound single reads and
/// concurrency, not AGGREGATE work: a tenant-planted bank with tens of
/// thousands of pages would otherwise make one request allocate every
/// filename and open every page while holding a permit. Enumeration
/// stops at the cap (+1 probe to detect overflow); the response flags
/// `entities_truncated`.
const MAX_PANEL_ENTITIES: usize = 256;

/// Staging-note count cap (codex #1611 r8 P1). The count is status
/// surfacing ("notes waiting for the next sweep"), not an inventory —
/// the anchored walk stops counting here so a planted million-entry
/// staging tree costs bounded readdir work. A scan stopped early (this
/// cap or the raw budget) reports the OBSERVED count plus
/// `staging_truncated` (r10+r11 P2).
const MAX_STAGING_NOTE_COUNT: usize = 1000;

/// Raw readdir budget per directory scan (codex #1611 r9 P1). The
/// `.md` caps above bound MATCHING entries only — a tenant-planted
/// directory of millions of non-Markdown entries (or junk after the
/// last matching name) would still be scanned to EOF while holding a
/// global scan permit, stalling other profiles' panel reads. Every
/// directory iteration stops after this many RAW entries regardless of
/// filtering; exhaustion reports truncation (listing) or the count so
/// far (staging saturation). Generous headroom over the legitimate
/// maxima (256 entities / 1000 staging notes plus stray dotfiles).
const MAX_DIR_SCAN_ENTRIES: usize = 10_000;

/// A directory handle every panel read is anchored to.
///
/// Unix: an `OwnedFd` opened `O_NOFOLLOW|O_DIRECTORY`; children are
/// resolved with `openat` relative to it, so no path is ever re-walked
/// (TOCTOU-free — a rename/swap after the open cannot redirect us).
///
/// Non-Unix: falls back to canonicalize-anchored paths with a leaf
/// `symlink_metadata` check. This keeps the multi-syscall TOCTOU
/// window `preview.rs` documents for the same fallback — Windows
/// serve deployments are dev-only today (matching #996's posture).
struct AnchoredDir(imp::Handle);

impl AnchoredDir {
    /// Open the profile data-dir root. `None` if it cannot be opened
    /// (or is itself a symlink, on Unix).
    fn open_root(dir: &Path) -> Option<Self> {
        imp::open_root(dir).map(Self)
    }

    /// Walk `rel` (Normal components only) strictly beneath this
    /// handle, refusing symlinks at every component.
    fn open_beneath(&self, rel: &Path) -> Option<Self> {
        imp::open_beneath(&self.0, rel).map(Self)
    }

    /// Read a direct child file (symlink-refusing). Returns the
    /// content and the file's mtime (fstat on the opened handle).
    fn read_file(&self, name: &str) -> Option<(String, Option<SystemTime>)> {
        imp::read_file(&self.0, name)
    }

    /// Names (stems) of `*.md` direct children, at most `cap` of them.
    /// The bool is true when more entries existed beyond the cap.
    fn list_md_stems(&self, cap: usize) -> (Vec<String>, bool) {
        imp::list_md_stems(&self.0, cap)
    }

    /// Count `*.md` direct children, stopping at `cap` matches or the
    /// raw scan budget. Same no-follow anchoring as every other panel
    /// read — used for the staging-note count so it can never follow a
    /// symlinked staging tree into another profile (codex #1611 r8
    /// P1). Returns `(observed, truncated)`: the count is EXACT when
    /// `truncated` is false, and a lower bound when true (scan stopped
    /// at the match cap or the raw budget). The two dimensions stay
    /// separate because neither fabricating notes (saturating a
    /// junk-flooded dir to `cap` — r11 P2) nor silently undercounting
    /// (returning a partial tally as exact — r10 P2) is acceptable.
    fn count_md_entries(&self, cap: usize) -> (usize, bool) {
        imp::count_md_entries(&self.0, cap)
    }
}

#[cfg(unix)]
mod imp {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::time::SystemTime;

    use rustix::fs::{Mode, OFlags, openat};

    pub(super) type Handle = OwnedFd;

    pub(super) fn open_root(dir: &Path) -> Option<OwnedFd> {
        // O_NOFOLLOW on the root too: the data dir itself is
        // daemon-resolved (not tenant-writable), but refusing a
        // symlinked root is free belt-and-braces.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)
            .ok()
            .map(Into::into)
    }

    pub(super) fn open_beneath(root: &OwnedFd, rel: &Path) -> Option<OwnedFd> {
        let mut current: Option<OwnedFd> = None;
        for comp in rel.components() {
            let std::path::Component::Normal(name) = comp else {
                // `.`/`..`/absolute never appear in the store-derived
                // relative paths we pass; refuse rather than resolve.
                return None;
            };
            let parent = current.as_ref().unwrap_or(root);
            // Anchored to the parent FD, not the path string — a
            // mid-walk swap of any on-disk name leaves the resolution
            // chain intact. NOFOLLOW makes a symlink component ELOOP.
            let next = openat(
                parent,
                name,
                OFlags::RDONLY
                    | OFlags::NOFOLLOW
                    | OFlags::DIRECTORY
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?;
            current = Some(next);
        }
        current
    }

    pub(super) fn read_file(dir: &OwnedFd, name: &str) -> Option<(String, Option<SystemTime>)> {
        // NONBLOCK: opening a FIFO with plain O_RDONLY blocks until a
        // writer appears — a tenant-planted FIFO must not park a
        // request thread (codex #1611 r3 P1). Harmless for the regular
        // files we actually serve.
        let fd = openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()?;
        let file: std::fs::File = fd.into();
        // fstat on the OPENED handle — no separate stat-by-path race.
        let meta = file.metadata().ok()?;
        // Regular files only: FIFOs, sockets and devices don't belong
        // in a memory dir and read()s on them can block or misbehave.
        // Size-capped: panel files are KB-scale markdown.
        if !meta.is_file() || meta.len() > super::MAX_PANEL_FILE_BYTES {
            return None;
        }
        let mtime = meta.modified().ok();
        // Bound the ACTUAL read, not just the fstat snapshot: a tenant
        // can append past the cap between stat and read (codex #1611 r6
        // P1). `take(cap+1)` lets us DETECT an over-cap file (a full
        // cap+1 bytes) and reject it rather than serving a truncated
        // prefix.
        let cap = super::MAX_PANEL_FILE_BYTES;
        let mut content = String::new();
        let read = std::io::Read::take(file, cap + 1)
            .read_to_string(&mut content)
            .ok()?;
        if read as u64 > cap {
            return None;
        }
        Some((content, mtime))
    }

    pub(super) fn list_md_stems(dir: &OwnedFd, cap: usize) -> (Vec<String>, bool) {
        // Enumeration failures are TRUNCATION, not emptiness (codex
        // #1611 r12 P2): a listing we could not start — or an entry we
        // could not read — must never let a partial result pass as
        // exact/complete.
        let Ok(entries) = rustix::fs::Dir::read_from(dir) else {
            return (Vec::new(), true);
        };
        let mut names = Vec::new();
        let mut raw_seen = 0usize;
        for next in entries {
            let Ok(entry) = next else {
                return (names, true);
            };
            // RAW budget BEFORE any filtering (codex #1611 r9 P1): a
            // planted directory of millions of non-.md entries must not
            // be scanned to EOF while a global permit is held.
            raw_seen += 1;
            if raw_seen > super::MAX_DIR_SCAN_ENTRIES {
                return (names, true);
            }
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if let Some(stem) = name.strip_suffix(".md")
                && !stem.is_empty()
            {
                if names.len() == cap {
                    // One entry beyond the cap proves truncation; stop
                    // enumerating so a huge bank costs bounded work.
                    return (names, true);
                }
                names.push(stem.to_string());
            }
        }
        (names, false)
    }

    pub(super) fn count_md_entries(dir: &OwnedFd, cap: usize) -> (usize, bool) {
        // Enumeration failures are truncation — see list_md_stems
        // (codex #1611 r12 P2): a zero/partial count from a failed
        // scan must not read as exact.
        let Ok(entries) = rustix::fs::Dir::read_from(dir) else {
            return (0, true);
        };
        let mut count = 0;
        let mut raw_seen = 0usize;
        for next in entries {
            let Ok(entry) = next else {
                return (count, true);
            };
            // Same raw budget as list_md_stems (codex #1611 r9 P1).
            // Exhaustion reports (observed, truncated=true) — never a
            // fabricated `cap` (r11 P2) and never a partial tally
            // masquerading as exact (r10 P2).
            raw_seen += 1;
            if raw_seen > super::MAX_DIR_SCAN_ENTRIES {
                return (count, true);
            }
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name.len() > ".md".len() && name.ends_with(".md") {
                count += 1;
                if count >= cap {
                    return (count, true);
                }
            }
        }
        (count, false)
    }
}

#[cfg(not(unix))]
mod imp {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    /// (canonical root, current dir). The root is carried so every
    /// descend re-anchors, mirroring the Unix walk's guarantee as
    /// closely as path-based checks allow.
    pub(super) type Handle = (PathBuf, PathBuf);

    pub(super) fn open_root(dir: &Path) -> Option<Handle> {
        let canon = std::fs::canonicalize(dir).ok()?;
        Some((canon.clone(), canon))
    }

    pub(super) fn open_beneath(handle: &Handle, rel: &Path) -> Option<Handle> {
        let (root, dir) = handle;
        let canon = std::fs::canonicalize(dir.join(rel)).ok()?;
        canon.starts_with(root).then(|| (root.clone(), canon))
    }

    pub(super) fn read_file(handle: &Handle, name: &str) -> Option<(String, Option<SystemTime>)> {
        let path = handle.1.join(name);
        // Leaf symlink check + read. TOCTOU window documented on
        // `AnchoredDir` — dev-only platforms.
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if meta.file_type().is_symlink()
            || !meta.is_file()
            || meta.len() > super::MAX_PANEL_FILE_BYTES
        {
            return None;
        }
        let mtime = meta.modified().ok();
        // Bound the actual read (codex #1611 r6 P1) — see the Unix arm.
        let cap = super::MAX_PANEL_FILE_BYTES;
        let file = std::fs::File::open(&path).ok()?;
        let mut content = String::new();
        let read = std::io::Read::take(file, cap + 1)
            .read_to_string(&mut content)
            .ok()?;
        if read as u64 > cap {
            return None;
        }
        Some((content, mtime))
    }

    pub(super) fn list_md_stems(handle: &Handle, cap: usize) -> (Vec<String>, bool) {
        // Enumeration failures are truncation, not emptiness — see the
        // Unix arm (codex #1611 r12 P2). No `flatten()`: it would skip
        // errored entries and let a partial listing pass as complete.
        let Ok(entries) = std::fs::read_dir(&handle.1) else {
            return (Vec::new(), true);
        };
        let mut names = Vec::new();
        let mut raw_seen = 0usize;
        for next in entries {
            let Ok(entry) = next else {
                return (names, true);
            };
            // RAW budget BEFORE any filtering (codex #1611 r9 P1) —
            // see the Unix arm.
            raw_seen += 1;
            if raw_seen > super::MAX_DIR_SCAN_ENTRIES {
                return (names, true);
            }
            let Some(stem) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".md"))
                .filter(|stem| !stem.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            if names.len() == cap {
                // One entry beyond the cap proves truncation; stop
                // enumerating so a huge bank costs bounded work.
                return (names, true);
            }
            names.push(stem);
        }
        (names, false)
    }

    pub(super) fn count_md_entries(handle: &Handle, cap: usize) -> (usize, bool) {
        // Enumeration failures are truncation — see the Unix arm
        // (codex #1611 r12 P2).
        let Ok(entries) = std::fs::read_dir(&handle.1) else {
            return (0, true);
        };
        let mut count = 0;
        let mut raw_seen = 0usize;
        for next in entries {
            let Ok(entry) = next else {
                return (count, true);
            };
            // Same raw budget as list_md_stems (codex #1611 r9 P1);
            // exhaustion reports (observed, truncated) — see the Unix
            // arm (r10/r11 P2).
            raw_seen += 1;
            if raw_seen > super::MAX_DIR_SCAN_ENTRIES {
                return (count, true);
            }
            let is_md = entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.len() > ".md".len() && n.ends_with(".md"));
            if is_md {
                count += 1;
                if count >= cap {
                    return (count, true);
                }
            }
        }
        (count, false)
    }
}

/// Relative path of `child` under `base` — both are store-derived
/// (constructed by joins, no fs access), so this never touches disk.
fn rel_under(base: &Path, child: &Path) -> Option<PathBuf> {
    child.strip_prefix(base).ok().map(Path::to_path_buf)
}

/// Effective refresh flag for a profile under host policy. Mirrors the
/// runtime bootstrap: profile block merged over the host block
/// field-by-field, then DEFAULT-ON unless an explicit `false` survives.
fn effective_refresh_enabled(state: &AppState, profile: &crate::profiles::UserProfile) -> bool {
    let mut memory = profile.config.memory.clone();
    crate::config::merge_host_memory_into_profile(&mut memory, state.host_memory.as_ref());
    crate::config::MemoryConfig::refresh_enabled(memory.as_ref())
}

fn rfc3339(mtime: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(mtime).to_rfc3339()
}

/// Typed application boundary for read-only memory panel queries. The caller
/// supplies the already-authorized effective profile; this service preserves
/// the same store lookup and never falls back to another profile.
pub(crate) struct MemoryQueryService {
    state: Arc<AppState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryQueryError {
    Unavailable,
    NotFound,
    Internal,
}

impl MemoryQueryError {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl MemoryQueryService {
    pub(crate) fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl MemoryQueryService {
    pub(crate) async fn overview(
        &self,
        profile_id: &str,
    ) -> Result<MemoryOverviewResponse, MemoryQueryError> {
        let state = self.state.clone();
        let ps = state
            .profile_store
            .as_ref()
            .ok_or(MemoryQueryError::Unavailable)?;
        let profile = ps
            .get(profile_id)
            .map_err(|_| MemoryQueryError::Internal)?
            .ok_or(MemoryQueryError::NotFound)?;
        let data_dir = ps.resolve_data_dir(&profile);

        // Path-derivation ONLY — `MemoryStore::open` would create_dir_all
        // the memory tree BEFORE the anchored validation, turning a
        // symlinked `memory` into a directory-existence oracle (target
        // exists → empty state; dangling → 500) and materializing dirs on
        // a read endpoint (codex #1611 r8 P2). The no-follow walk below is
        // the sole authority on what exists.
        let store = octos_memory::MemoryStore::at_memory_dir(data_dir.join("memory"));

        let empty = |state: &AppState, profile| MemoryOverviewResponse {
            ok: true,
            long_term: String::new(),
            long_term_updated_at: None,
            today: String::new(),
            recent: Vec::new(),
            entities: Vec::new(),
            entities_truncated: false,
            staging_notes: 0,
            staging_truncated: false,
            refresh_enabled: effective_refresh_enabled(state, profile),
        };

        // FD-anchored walk to the memory dir (see module doc). A missing
        // or symlinked memory dir renders as the empty state. The WHOLE
        // walk runs on the blocking pool: it is synchronous disk I/O over
        // tenant-controlled content, and O_NONBLOCK only de-fangs FIFO
        // opens — large/slow regular files would still stall a Tokio
        // worker inline (codex #1611 r3/r4 P1).
        let memory_dir_path = store
            .memory_md_path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.clone());
        let Some(rel_mem) = rel_under(&data_dir, &memory_dir_path) else {
            return Ok(empty(&state, &profile));
        };
        let rel_bank = rel_under(&memory_dir_path, &store.bank_entities_dir());
        let rel_staging = rel_under(&memory_dir_path, &store.staging_notes_dir());
        let walk_root = data_dir.clone();
        // Bounded blocking-pool usage; waiters park as futures. The permit
        // is MOVED INTO the spawned task and dropped only when the blocking
        // work finishes: if the request is cancelled, dropping the
        // JoinHandle does NOT stop the already-running blocking task, so a
        // permit released with the handle would let a tenant pile up more
        // than PANEL_SCAN_PERMITS live scans via repeated cancels (codex
        // #1611 r6 P1).
        let scan_permit = PANEL_SCAN_PERMITS.acquire().await.ok();
        let walked = tokio::task::spawn_blocking(move || {
            let _scan_permit = scan_permit;
            let root = AnchoredDir::open_root(&walk_root)?;
            let mem = root.open_beneath(&rel_mem)?;

            let long_term = mem.read_file("MEMORY.md");
            let local_today = chrono::Local::now().date_naive();
            let today = mem
                .read_file(&format!("{local_today}.md"))
                .map(|(content, _)| content)
                .unwrap_or_default();
            // Last 7 days, newest first — mirrors `MemoryStore::read_recent`.
            let mut recent = Vec::new();
            for i in 1..=7 {
                let date = local_today - chrono::Duration::days(i);
                let date_str = date.format("%Y-%m-%d").to_string();
                if let Some((content, _)) = mem.read_file(&format!("{date_str}.md")) {
                    recent.push(DailyNote {
                        date: date_str,
                        content,
                    });
                }
            }
            // Entity bank — walked from the memory-dir FD (its components
            // can be symlinks independently of memory/ itself). Names are
            // enumerated up to MAX_PANEL_ENTITIES (codex #1611 r8 P2),
            // sorted, THEN read — so aggregate page-open work is bounded
            // and the rendered subset is deterministic for a given listing.
            let mut entities = Vec::new();
            let mut entities_truncated = false;
            if let Some(bank) = rel_bank.and_then(|rel| mem.open_beneath(&rel)) {
                let (mut names, truncated) = bank.list_md_stems(MAX_PANEL_ENTITIES);
                entities_truncated = truncated;
                names.sort();
                for name in names {
                    if let Some((content, _)) = bank.read_file(&format!("{name}.md")) {
                        entities.push(EntitySummary {
                            // The store's own parser — byte-identical to the
                            // agent-prompt summaries (codex #1611 r2 P2).
                            summary: octos_memory::extract_abstract(&content),
                            name,
                        });
                    }
                }
            }
            // Staging count through the SAME no-follow chain and permit as
            // every other panel read (codex #1611 r8 P1 ×2): the previous
            // normal-path count followed a symlinked `staging/notes` into
            // other profiles' data (pending-note count disclosure) and ran
            // AFTER the permit-owning task, so repeated requests could pile
            // unpermitted, unbounded scans onto the runtime. An absent or
            // symlinked staging tree counts as zero. The observed count and
            // the truncation state travel SEPARATELY (r10+r11 P2): a scan
            // stopped early must neither fabricate notes nor pass a partial
            // tally off as exact.
            let (staging_notes, staging_truncated) = rel_staging
                .and_then(|rel| mem.open_beneath(&rel))
                .map(|staging| staging.count_md_entries(MAX_STAGING_NOTE_COUNT))
                .unwrap_or((0, false));
            Some((
                long_term,
                today,
                recent,
                entities,
                entities_truncated,
                staging_notes,
                staging_truncated,
            ))
        })
        .await
        .ok()
        .flatten();
        let Some((
            long_term_read,
            today,
            recent,
            entities,
            entities_truncated,
            staging_notes,
            staging_truncated,
        )) = walked
        else {
            return Ok(empty(&state, &profile));
        };
        let (long_term, long_term_updated_at) = match long_term_read {
            // mtime whenever the file EXISTS — an empty long-term memory
            // still reports when it last changed (codex #1611 r2 P2).
            Some((content, mtime)) => (content, mtime.map(rfc3339)),
            None => (String::new(), None),
        };

        Ok(MemoryOverviewResponse {
            ok: true,
            long_term,
            long_term_updated_at,
            today,
            recent,
            entities,
            entities_truncated,
            staging_notes,
            staging_truncated,
            refresh_enabled: effective_refresh_enabled(&state, &profile),
        })
    }
}

impl MemoryQueryService {
    pub(crate) async fn entity(
        &self,
        profile_id: &str,
        name: String,
    ) -> Result<MemoryEntityResponse, MemoryQueryError> {
        let state = self.state.clone();
        let ps = state
            .profile_store
            .as_ref()
            .ok_or(MemoryQueryError::Unavailable)?;
        let profile = ps
            .get(profile_id)
            .map_err(|_| MemoryQueryError::Internal)?
            .ok_or(MemoryQueryError::NotFound)?;
        let data_dir = ps.resolve_data_dir(&profile);

        // Path-derivation only — no create_dir_all on a read endpoint
        // (codex #1611 r8 P2, same rationale as the overview handler).
        let store = octos_memory::MemoryStore::at_memory_dir(data_dir.join("memory"));
        // Same traversal-character sanitization `read_entity` applies, on
        // top of the FD-anchored walk — a symlinked page, bank dir or any
        // ancestor is a plain 404.
        let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
        let rel_bank =
            rel_under(&data_dir, &store.bank_entities_dir()).ok_or(MemoryQueryError::NotFound)?;
        // Blocking pool: synchronous walk + read (codex #1611 r3 P1),
        // bounded like the overview scan. Permit MOVED into the task so a
        // cancelled request cannot orphan the running scan while freeing
        // its permit (codex #1611 r6 P1).
        let scan_permit = PANEL_SCAN_PERMITS.acquire().await.ok();
        let content = tokio::task::spawn_blocking(move || {
            let _scan_permit = scan_permit;
            AnchoredDir::open_root(&data_dir)
                .and_then(|root| root.open_beneath(&rel_bank))
                .and_then(|bank| bank.read_file(&format!("{safe_name}.md")))
                .map(|(content, _)| content)
        })
        .await
        .ok()
        .flatten()
        .ok_or(MemoryQueryError::NotFound)?;
        Ok(MemoryEntityResponse {
            ok: true,
            name,
            content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{ProfileConfig, ProfileStore, UserProfile};

    #[tokio::test]
    async fn oversized_memory_md_is_refused_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = Arc::new(ProfileStore::open_unified(dir.path()).unwrap());
        let profile = UserProfile {
            id: "big".into(),
            name: "Big".into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        profiles.save(&profile).unwrap();
        let data_dir = profiles.resolve_data_dir(&profile);
        tokio::fs::create_dir_all(data_dir.join("memory"))
            .await
            .unwrap();
        let oversized = "x".repeat((MAX_PANEL_FILE_BYTES as usize) + 1024);
        tokio::fs::write(data_dir.join("memory/MEMORY.md"), &oversized)
            .await
            .unwrap();
        let state = Arc::new(AppState {
            profile_store: Some(profiles),
            ..AppState::empty_for_tests()
        });

        let response = MemoryQueryService::new(state)
            .overview("big")
            .await
            .unwrap();
        assert_eq!(
            response.long_term, "",
            "an over-cap MEMORY.md must be refused, not truncated"
        );
    }
}
