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
    /// Staged capture notes waiting for the next refresh sweep.
    pub staging_notes: usize,
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

    /// Names (stems) of `*.md` direct children.
    fn list_md_stems(&self) -> Vec<String> {
        imp::list_md_stems(&self.0)
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
        if !meta.is_file() {
            return None;
        }
        let mtime = meta.modified().ok();
        let mut file = file;
        let mut content = String::new();
        file.read_to_string(&mut content).ok()?;
        Some((content, mtime))
    }

    pub(super) fn list_md_stems(dir: &OwnedFd) -> Vec<String> {
        let Ok(mut entries) = rustix::fs::Dir::read_from(dir) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        while let Some(Ok(entry)) = entries.next() {
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if let Some(stem) = name.strip_suffix(".md")
                && !stem.is_empty()
            {
                names.push(stem.to_string());
            }
        }
        names
    }
}

#[cfg(not(unix))]
mod imp {
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
        if meta.file_type().is_symlink() || !meta.is_file() {
            return None;
        }
        let mtime = meta.modified().ok();
        std::fs::read_to_string(&path).ok().map(|c| (c, mtime))
    }

    pub(super) fn list_md_stems(handle: &Handle) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&handle.1) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".md"))
                    .filter(|stem| !stem.is_empty())
                    .map(str::to_string)
            })
            .collect()
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

    let store = octos_memory::MemoryStore::open(&data_dir)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let empty = |state: &AppState, profile| {
        Json(MemoryOverviewResponse {
            ok: true,
            long_term: String::new(),
            long_term_updated_at: None,
            today: String::new(),
            recent: Vec::new(),
            entities: Vec::new(),
            staging_notes: 0,
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
    let walk_root = data_dir.clone();
    let walked = tokio::task::spawn_blocking(move || {
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
        // can be symlinks independently of memory/ itself).
        let mut entities = Vec::new();
        if let Some(bank) = rel_bank.and_then(|rel| mem.open_beneath(&rel)) {
            for name in bank.list_md_stems() {
                if let Some((content, _)) = bank.read_file(&format!("{name}.md")) {
                    entities.push(EntitySummary {
                        // The store's own parser — byte-identical to the
                        // agent-prompt summaries (codex #1611 r2 P2).
                        summary: octos_memory::extract_abstract(&content),
                        name,
                    });
                }
            }
            entities.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Some((long_term, today, recent, entities))
    })
    .await
    .ok()
    .flatten();
    let Some((long_term_read, today, recent, entities)) = walked else {
        return Ok(empty(&state, &profile));
    };
    let (long_term, long_term_updated_at) = match long_term_read {
        // mtime whenever the file EXISTS — an empty long-term memory
        // still reports when it last changed (codex #1611 r2 P2).
        Some((content, mtime)) => (content, mtime.map(rfc3339)),
        None => (String::new(), None),
    };
    let staging_notes = store.count_staging_notes().await;

    Ok(Json(MemoryOverviewResponse {
        ok: true,
        long_term,
        long_term_updated_at,
        today,
        recent,
        entities,
        staging_notes,
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

    let store = octos_memory::MemoryStore::open(&data_dir)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Same traversal-character sanitization `read_entity` applies, on
    // top of the FD-anchored walk — a symlinked page, bank dir or any
    // ancestor is a plain 404.
    let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
    let rel_bank = rel_under(&data_dir, &store.bank_entities_dir()).ok_or(StatusCode::NOT_FOUND)?;
    // Blocking pool: synchronous walk + read (codex #1611 r3 P1).
    let content = tokio::task::spawn_blocking(move || {
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
