//! `/api/my/memory*` — read-only memory panel endpoints (web parity
//! audit P3 item 6: the book gives the memory system top-3 prominence,
//! but the web dashboard had ZERO memory UX — MEMORY.md, daily notes
//! and the entity bank were invisible outside the CLI).
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

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Serialize;

use super::AppState;
use super::router::AuthIdentity;
use crate::api::auth_handlers::resolve_my_profile_id;

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
        let Ok(mut entries) = rustix::fs::Dir::read_from(dir) else {
            return (Vec::new(), true);
        };
        let mut names = Vec::new();
        let mut raw_seen = 0usize;
        while let Some(next) = entries.next() {
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
        let Ok(mut entries) = rustix::fs::Dir::read_from(dir) else {
            return (0, true);
        };
        let mut count = 0;
        let mut raw_seen = 0usize;
        while let Some(next) = entries.next() {
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

/// GET /api/my/memory
pub async fn my_memory(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
) -> Result<Json<MemoryOverviewResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let profile = ps
        .get(&profile_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let data_dir = ps.resolve_data_dir(&profile);

    // Path-derivation ONLY — `MemoryStore::open` would create_dir_all
    // the memory tree BEFORE the anchored validation, turning a
    // symlinked `memory` into a directory-existence oracle (target
    // exists → empty state; dangling → 500) and materializing dirs on
    // a read endpoint (codex #1611 r8 P2). The no-follow walk below is
    // the sole authority on what exists.
    let store = octos_memory::MemoryStore::at_memory_dir(data_dir.join("memory"));

    let empty = |state: &AppState, profile| {
        Json(MemoryOverviewResponse {
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
        })
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

    Ok(Json(MemoryOverviewResponse {
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
    }))
}

/// GET /api/my/memory/entities/{name}
pub async fn my_memory_entity(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<MemoryEntityResponse>, StatusCode> {
    let ps = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = resolve_my_profile_id(&identity, ps, &state, &headers)?;
    let profile = ps
        .get(&profile_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let data_dir = ps.resolve_data_dir(&profile);

    // Path-derivation only — no create_dir_all on a read endpoint
    // (codex #1611 r8 P2, same rationale as the overview handler).
    let store = octos_memory::MemoryStore::at_memory_dir(data_dir.join("memory"));
    // Same traversal-character sanitization `read_entity` applies, on
    // top of the FD-anchored walk — a symlinked page, bank dir or any
    // ancestor is a plain 404.
    let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
    let rel_bank = rel_under(&data_dir, &store.bank_entities_dir()).ok_or(StatusCode::NOT_FOUND)?;
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
    .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(MemoryEntityResponse {
        ok: true,
        name,
        content,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::ProfileStore;
    use crate::user_store::UserRole;

    fn make_user_profile(id: &str, name: &str) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.into(),
            name: name.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn temp_state() -> (tempfile::TempDir, Arc<AppState>, Arc<ProfileStore>) {
        let dir = tempfile::tempdir().unwrap();
        let profile_store = Arc::new(ProfileStore::open(dir.path()).unwrap());
        let state = Arc::new(AppState {
            profile_store: Some(profile_store.clone()),
            ..AppState::empty_for_tests()
        });
        (dir, state, profile_store)
    }

    fn user_identity(id: &str) -> axum::Extension<AuthIdentity> {
        axum::Extension(AuthIdentity::User {
            id: id.into(),
            role: UserRole::User,
        })
    }

    async fn seed_memory(ps: &ProfileStore, profile: &crate::profiles::UserProfile) {
        let data_dir = ps.resolve_data_dir(profile);
        let store = octos_memory::MemoryStore::open(&data_dir).await.unwrap();
        store
            .write_long_term("# MEMORY\n\n- remembers things\n")
            .await
            .unwrap();
        store.append_today("did a thing today").await.unwrap();
        store
            .write_entity("fleet", "# fleet\n\nAbstract: five minis\n")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn my_memory_returns_profile_scoped_overview() {
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant", "Tenant");
        ps.save(&profile).unwrap();
        seed_memory(&ps, &profile).await;

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("tenant"))
            .await
            .unwrap();
        assert!(resp.ok);
        assert!(resp.long_term.contains("remembers things"));
        assert!(resp.long_term_updated_at.is_some());
        assert!(resp.today.contains("did a thing today"));
        assert_eq!(resp.entities.len(), 1);
        assert_eq!(resp.entities[0].name, "fleet");
        // DEFAULT-ON refresh with no host/profile opt-out.
        assert!(resp.refresh_enabled);
        assert_eq!(resp.staging_notes, 0);
    }

    #[tokio::test]
    async fn my_memory_is_empty_but_ok_for_a_fresh_profile() {
        // A profile that has never written memory must get an empty
        // overview, not an error — the panel renders its zero state.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("fresh", "Fresh");
        ps.save(&profile).unwrap();

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("fresh"))
            .await
            .unwrap();
        assert!(resp.ok);
        assert_eq!(resp.long_term, "");
        assert_eq!(resp.recent.len(), 0);
        assert_eq!(resp.entities.len(), 0);
    }

    #[tokio::test]
    async fn empty_memory_md_still_reports_mtime() {
        // Consolidation can hard-delete the last entry, leaving an
        // EMPTY MEMORY.md — the panel must still say when it changed
        // (codex #1611 r2 P2: mtime derives from existence, not
        // content).
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("emptied", "Emptied");
        ps.save(&profile).unwrap();
        let data_dir = ps.resolve_data_dir(&profile);
        tokio::fs::create_dir_all(data_dir.join("memory"))
            .await
            .unwrap();
        tokio::fs::write(data_dir.join("memory/MEMORY.md"), "")
            .await
            .unwrap();

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("emptied"))
            .await
            .unwrap();
        assert_eq!(resp.long_term, "");
        assert!(
            resp.long_term_updated_at.is_some(),
            "existing-but-empty MEMORY.md must keep its mtime"
        );
    }

    #[tokio::test]
    async fn entity_summary_matches_store_parser_exactly() {
        // codex #1611 r2 P2: a second frontmatter parser drifted from
        // `MemoryStore::strip_frontmatter` (`---abc` is NOT an opener
        // for the store). The panel now calls the store's own
        // `extract_abstract` — hold it to byte-equality on the shape
        // that diverged.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("parity", "Parity");
        ps.save(&profile).unwrap();
        let data_dir = ps.resolve_data_dir(&profile);
        let store = octos_memory::MemoryStore::open(&data_dir).await.unwrap();
        let tricky = "---abc\nnote\n---\nBody\n";
        store.write_entity("tricky", tricky).await.unwrap();

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("parity"))
            .await
            .unwrap();
        let entity = resp
            .entities
            .iter()
            .find(|e| e.name == "tricky")
            .expect("tricky entity listed");
        assert_eq!(entity.summary, octos_memory::extract_abstract(tricky));
        assert_eq!(entity.summary, "---abc", "not-a-frontmatter-opener stays");
    }

    #[tokio::test]
    async fn my_memory_respects_host_level_refresh_opt_out() {
        // Host opts out of (default-on) refresh; a profile that says
        // nothing must inherit the opt-out — same merge the runtime
        // bootstrap applies.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant2", "Tenant Two");
        ps.save(&profile).unwrap();
        let state = Arc::new(AppState {
            host_memory: Some(crate::config::MemoryConfig {
                refresh: Some(crate::config::MemoryRefreshConfig {
                    enabled: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            profile_store: Some(ps.clone()),
            ..AppState::empty_for_tests()
        });

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("tenant2"))
            .await
            .unwrap();
        assert!(!resp.refresh_enabled);
    }

    #[tokio::test]
    async fn my_memory_entity_reads_one_page_and_404s_on_missing() {
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("tenant3", "Tenant Three");
        ps.save(&profile).unwrap();
        seed_memory(&ps, &profile).await;

        let Json(resp) = my_memory_entity(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("tenant3"),
            AxumPath("fleet".into()),
        )
        .await
        .unwrap();
        assert!(resp.content.contains("five minis"));

        let missing = my_memory_entity(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("tenant3"),
            AxumPath("nope".into()),
        )
        .await;
        assert_eq!(missing.err(), Some(StatusCode::NOT_FOUND));

        // Traversal-shaped names sanitize inside the store and miss.
        let traversal = my_memory_entity(
            State(state),
            HeaderMap::new(),
            user_identity("tenant3"),
            AxumPath("../../etc/passwd".into()),
        )
        .await;
        assert_eq!(traversal.err(), Some(StatusCode::NOT_FOUND));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_memory_files_and_dirs_are_refused() {
        // codex #1611 P1: a tenant can place symlinks inside its own
        // memory dir (workspace-cwd session + shell); the daemon-
        // privileged panel must not follow them into foreign files.
        // (The round-2 swap RACE is closed structurally by the
        // FD-anchored walk; these assert the static shapes through
        // the same code path.)
        let (_dir, state, ps) = temp_state();
        let victim = make_user_profile("victim", "Victim");
        let attacker = make_user_profile("attacker", "Attacker");
        ps.save(&victim).unwrap();
        ps.save(&attacker).unwrap();
        seed_memory(&ps, &victim).await;

        // Attacker's MEMORY.md is a symlink at the victim's.
        let attacker_dir = ps.resolve_data_dir(&attacker);
        let attacker_mem = attacker_dir.join("memory");
        tokio::fs::create_dir_all(&attacker_mem).await.unwrap();
        let victim_md = ps.resolve_data_dir(&victim).join("memory/MEMORY.md");
        std::os::unix::fs::symlink(&victim_md, attacker_mem.join("MEMORY.md")).unwrap();
        // …and an entity page symlink.
        let bank = attacker_mem.join("bank/entities");
        tokio::fs::create_dir_all(&bank).await.unwrap();
        let victim_entity = ps
            .resolve_data_dir(&victim)
            .join("memory/bank/entities/fleet.md");
        std::os::unix::fs::symlink(&victim_entity, bank.join("stolen.md")).unwrap();

        let Json(resp) = my_memory(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("attacker"),
        )
        .await
        .unwrap();
        assert_eq!(resp.long_term, "", "symlinked MEMORY.md must not be read");
        assert!(
            resp.entities.is_empty(),
            "symlinked entity pages must not be listed"
        );
        let stolen = my_memory_entity(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("attacker"),
            AxumPath("stolen".into()),
        )
        .await;
        assert_eq!(stolen.err(), Some(StatusCode::NOT_FOUND));

        // Whole-directory symlink: memory/ → victim's memory/.
        let attacker2 = make_user_profile("attacker2", "Attacker Two");
        ps.save(&attacker2).unwrap();
        let a2_dir = ps.resolve_data_dir(&attacker2);
        tokio::fs::create_dir_all(&a2_dir).await.unwrap();
        // `ProfileStore::save` scaffolds memory/ eagerly; replace it
        // the way a tenant shell would (rm + ln -s).
        tokio::fs::remove_dir(a2_dir.join("memory")).await.unwrap();
        std::os::unix::fs::symlink(
            ps.resolve_data_dir(&victim).join("memory"),
            a2_dir.join("memory"),
        )
        .unwrap();
        let Json(resp2) = my_memory(State(state), HeaderMap::new(), user_identity("attacker2"))
            .await
            .unwrap();
        assert_eq!(resp2.long_term, "");
        assert!(resp2.entities.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_note_count_is_anchored_not_path_followed() {
        // codex #1611 r8 P1: the staging count used the normal-path
        // store helper, which followed a symlinked `staging/notes`
        // into another profile's data — disclosing the victim's
        // pending-note count — and ran OUTSIDE the scan permit. It now
        // rides the same FD-anchored walk as every other panel read:
        // a symlinked staging tree counts as zero.
        let (_dir, state, ps) = temp_state();
        let victim = make_user_profile("victim", "Victim");
        let attacker = make_user_profile("attacker", "Attacker");
        ps.save(&victim).unwrap();
        ps.save(&attacker).unwrap();

        // Two real pending notes for the victim.
        let victim_notes = ps.resolve_data_dir(&victim).join("memory/staging/notes");
        tokio::fs::create_dir_all(&victim_notes).await.unwrap();
        tokio::fs::write(victim_notes.join("a.md"), "note a")
            .await
            .unwrap();
        tokio::fs::write(victim_notes.join("b.md"), "note b")
            .await
            .unwrap();

        // Attacker: memory/staging/notes → victim's notes dir.
        let attacker_staging = ps.resolve_data_dir(&attacker).join("memory/staging");
        tokio::fs::create_dir_all(&attacker_staging).await.unwrap();
        std::os::unix::fs::symlink(&victim_notes, attacker_staging.join("notes")).unwrap();

        let Json(own) = my_memory(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("victim"),
        )
        .await
        .unwrap();
        assert_eq!(own.staging_notes, 2, "own notes count through the walk");
        assert!(
            !own.staging_truncated,
            "a fully-enumerated staging tree reports an exact count"
        );

        let Json(stolen) = my_memory(State(state), HeaderMap::new(), user_identity("attacker"))
            .await
            .unwrap();
        assert_eq!(
            stolen.staging_notes, 0,
            "a symlinked staging tree must count as zero, not disclose the victim's count"
        );
    }

    #[tokio::test]
    async fn entity_bank_enumeration_is_capped_with_truncation_flag() {
        // codex #1611 r8 P2: per-file caps bound single reads, not
        // aggregate work — a tenant-planted bank with thousands of
        // pages must not make one overview enumerate and open all of
        // them while holding a scan permit.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("hoarder", "Hoarder");
        ps.save(&profile).unwrap();
        let bank = ps.resolve_data_dir(&profile).join("memory/bank/entities");
        tokio::fs::create_dir_all(&bank).await.unwrap();
        for i in 0..(MAX_PANEL_ENTITIES + 5) {
            std::fs::write(bank.join(format!("e{i:04}.md")), "Abstract: x\n").unwrap();
        }

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("hoarder"))
            .await
            .unwrap();
        assert_eq!(
            resp.entities.len(),
            MAX_PANEL_ENTITIES,
            "enumeration must stop at the cap"
        );
        assert!(resp.entities_truncated, "over-cap bank must be flagged");
    }

    #[tokio::test]
    async fn raw_dir_budget_bounds_scans_of_non_md_junk() {
        // codex #1611 r9 P1: the .md caps bound MATCHING entries only —
        // a directory flooded with non-Markdown junk was still scanned
        // to EOF while holding a global permit. The raw readdir budget
        // must stop the scan regardless of what the filter matches.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("junkyard", "Junkyard");
        ps.save(&profile).unwrap();
        let data_dir = ps.resolve_data_dir(&profile);
        let bank = data_dir.join("memory/bank/entities");
        let staging = data_dir.join("memory/staging/notes");
        tokio::fs::create_dir_all(&bank).await.unwrap();
        tokio::fs::create_dir_all(&staging).await.unwrap();
        // Junk past the raw budget in BOTH scanned directories, plus a
        // handful of real .md entries whose visibility is readdir-order
        // dependent — the deterministic assertions are the truncation
        // flag and prompt completion, not which entries surfaced.
        for i in 0..(MAX_DIR_SCAN_ENTRIES + 50) {
            std::fs::write(bank.join(format!("j{i:05}.txt")), "").unwrap();
            std::fs::write(staging.join(format!("j{i:05}.txt")), "").unwrap();
        }
        std::fs::write(bank.join("real.md"), "Abstract: x\n").unwrap();
        std::fs::write(staging.join("real.md"), "note").unwrap();

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            my_memory(State(state), HeaderMap::new(), user_identity("junkyard")),
        )
        .await
        .expect("junk-flooded directories must not stall the scan")
        .unwrap();
        assert!(
            resp.0.entities_truncated,
            "exhausting the raw budget must report truncation"
        );
        assert!(
            resp.0.entities.len() <= MAX_PANEL_ENTITIES,
            "matching cap still holds under junk flooding"
        );
        // r11 P2: junk-flooded staging holds ≤1 real note — the count
        // must report what was OBSERVED (never a fabricated cap), with
        // the truncation flag carrying the uncertainty.
        assert!(
            resp.0.staging_notes <= 1,
            "raw-budget exhaustion must not fabricate notes, got {}",
            resp.0.staging_notes
        );
        assert!(
            resp.0.staging_truncated,
            "an early-stopped staging scan must be flagged truncated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_endpoints_do_not_create_or_probe_the_memory_tree() {
        // codex #1611 r8 P2: `MemoryStore::open` ran create_dir_all
        // BEFORE the anchored validation. With `memory` symlinked, an
        // existing daemon-readable target succeeded (empty state) while
        // a dangling target 500'd — a directory-existence oracle — and
        // a plain GET materialized directories. The handlers now derive
        // paths without creating; the no-follow walk decides.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("probe", "Probe");
        ps.save(&profile).unwrap();
        let data_dir = ps.resolve_data_dir(&profile);
        // Replace the scaffolded memory/ with a DANGLING symlink.
        tokio::fs::remove_dir(data_dir.join("memory"))
            .await
            .unwrap();
        std::os::unix::fs::symlink(data_dir.join("does-not-exist"), data_dir.join("memory"))
            .unwrap();

        let resp = my_memory(
            State(state.clone()),
            HeaderMap::new(),
            user_identity("probe"),
        )
        .await
        .expect("dangling memory symlink must render the empty state, not 500");
        assert_eq!(resp.0.long_term, "");
        assert_eq!(resp.0.staging_notes, 0);

        let entity = my_memory_entity(
            State(state),
            HeaderMap::new(),
            user_identity("probe"),
            AxumPath("fleet".into()),
        )
        .await;
        assert_eq!(entity.err(), Some(StatusCode::NOT_FOUND));

        // The GETs must not have created anything: the symlink is
        // intact and its target still absent.
        let meta = std::fs::symlink_metadata(data_dir.join("memory")).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "memory must remain a symlink"
        );
        assert!(
            !data_dir.join("does-not-exist").exists(),
            "read endpoints must not materialize the symlink target"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_planted_as_memory_md_does_not_hang_the_request() {
        // codex #1611 r3 P1: opening a FIFO with O_RDONLY blocks until
        // a writer appears — a tenant-planted FIFO must neither hang
        // the request (NONBLOCK) nor be served (regular-files-only).
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("fifo", "Fifo");
        ps.save(&profile).unwrap();
        let data_dir = ps.resolve_data_dir(&profile);
        tokio::fs::create_dir_all(data_dir.join("memory"))
            .await
            .unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(data_dir.join("memory/MEMORY.md"))
            .status()
            .expect("mkfifo");
        assert!(status.success());

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            my_memory(State(state), HeaderMap::new(), user_identity("fifo")),
        )
        .await
        .expect("request must not block on the FIFO")
        .unwrap();
        assert_eq!(resp.0.long_term, "", "FIFO content must not be served");
        assert!(resp.0.long_term_updated_at.is_none());
    }

    #[tokio::test]
    async fn oversized_memory_md_is_refused_not_truncated() {
        // codex #1611 r6 P1: a file past MAX_PANEL_FILE_BYTES must
        // render as absent, not as a 2MB truncated prefix.
        let (_dir, state, ps) = temp_state();
        let profile = make_user_profile("big", "Big");
        ps.save(&profile).unwrap();
        let data_dir = ps.resolve_data_dir(&profile);
        tokio::fs::create_dir_all(data_dir.join("memory"))
            .await
            .unwrap();
        let oversized = "x".repeat((MAX_PANEL_FILE_BYTES as usize) + 1024);
        tokio::fs::write(data_dir.join("memory/MEMORY.md"), &oversized)
            .await
            .unwrap();

        let Json(resp) = my_memory(State(state), HeaderMap::new(), user_identity("big"))
            .await
            .unwrap();
        assert_eq!(
            resp.long_term, "",
            "an over-cap MEMORY.md must be refused, not truncated"
        );
    }

    #[tokio::test]
    async fn my_memory_is_profile_scoped_not_cross_tenant() {
        // Two tenants; each sees ONLY its own memory.
        let (_dir, state, ps) = temp_state();
        let a = make_user_profile("tenant-a", "A");
        let b = make_user_profile("tenant-b", "B");
        ps.save(&a).unwrap();
        ps.save(&b).unwrap();
        seed_memory(&ps, &a).await;

        let Json(resp_b) = my_memory(State(state), HeaderMap::new(), user_identity("tenant-b"))
            .await
            .unwrap();
        assert_eq!(resp_b.long_term, "");
        assert_eq!(resp_b.entities.len(), 0);
    }
}
