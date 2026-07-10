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

use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// First abstract/summary line of the page (the store's own
    /// `extract_abstract` — same string `list_entities` feeds the
    /// agent prompt).
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryOverviewResponse {
    pub ok: bool,
    /// Full `MEMORY.md` content ("" when absent).
    pub long_term: String,
    /// RFC 3339 mtime of `MEMORY.md`, when it exists.
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

/// Canonicalize `dir` and require it to stay under the profile's
/// canonicalized data dir. A tenant controls the CONTENT of its memory
/// directory (agent shell in a workspace-cwd session), so `memory/`,
/// the bank dir — or any ancestor a page lives under — could be a
/// symlink pointing at another profile's data; `read_no_follow` only
/// rejects a symlinked FINAL component (codex #1611 P1). `None` when
/// absent or escaping.
async fn anchored_dir(data_dir: &Path, dir: &Path) -> Option<PathBuf> {
    let canon_root = tokio::fs::canonicalize(data_dir).await.ok()?;
    let canon = tokio::fs::canonicalize(dir).await.ok()?;
    canon.starts_with(&canon_root).then_some(canon)
}

/// Symlink-refusing read: `octos_agent::tools::read_no_follow`
/// (O_NOFOLLOW on Unix — atomic, no TOCTOU on the final component).
/// Any error (absent, symlink, non-UTF-8) renders as "no content".
async fn panel_read(path: &std::path::Path) -> Option<String> {
    octos_agent::tools::read_no_follow(path).await.ok()
}

/// First non-heading line, truncated — mirrors the (private)
/// `octos_memory::memory_store::extract_abstract` the agent prompt
/// uses, so the panel shows the same summary strings.
fn first_summary_line(content: &str) -> String {
    let body = content
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---").map(|(_, b)| b))
        .unwrap_or(content);
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");
    if line.len() > 100 {
        let mut end = 97;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &line[..end])
    } else {
        line.to_string()
    }
}

/// Effective refresh flag for a profile under host policy. Mirrors the
/// runtime bootstrap: profile block merged over the host block
/// field-by-field, then DEFAULT-ON unless an explicit `false` survives.
fn effective_refresh_enabled(state: &AppState, profile: &crate::profiles::UserProfile) -> bool {
    let mut memory = profile.config.memory.clone();
    crate::config::merge_host_memory_into_profile(&mut memory, state.host_memory.as_ref());
    crate::config::MemoryConfig::refresh_enabled(memory.as_ref())
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

    // The store's own readers FOLLOW symlinks (fine agent-side, where
    // the sandbox owns policy) — this daemon-privileged HTTP path must
    // not (codex #1611 P1). All reads are anchored + no-follow; a
    // symlinked memory dir renders as the empty state.
    let memory_dir = match anchored_dir(
        &data_dir,
        store.memory_md_path().parent().unwrap_or(&data_dir),
    )
    .await
    {
        Some(dir) => dir,
        None => {
            return Ok(Json(MemoryOverviewResponse {
                ok: true,
                long_term: String::new(),
                long_term_updated_at: None,
                today: String::new(),
                recent: Vec::new(),
                entities: Vec::new(),
                staging_notes: 0,
                refresh_enabled: effective_refresh_enabled(&state, &profile),
            }));
        }
    };

    let memory_md = memory_dir.join("MEMORY.md");
    let long_term = panel_read(&memory_md).await.unwrap_or_default();
    let long_term_updated_at = if long_term.is_empty() {
        None
    } else {
        tokio::fs::symlink_metadata(&memory_md)
            .await
            .ok()
            .and_then(|meta| meta.modified().ok())
            .map(|mtime| chrono::DateTime::<chrono::Utc>::from(mtime).to_rfc3339())
    };
    let local_today = chrono::Local::now().date_naive();
    let today = panel_read(&memory_dir.join(format!("{local_today}.md")))
        .await
        .unwrap_or_default();
    // Last 7 days, newest first — mirrors `MemoryStore::read_recent`.
    let mut recent = Vec::new();
    for i in 1..=7 {
        let date = local_today - chrono::Duration::days(i);
        let date_str = date.format("%Y-%m-%d").to_string();
        if let Some(content) = panel_read(&memory_dir.join(format!("{date_str}.md"))).await {
            recent.push(DailyNote {
                date: date_str,
                content,
            });
        }
    }
    // Entity bank — the bank dir gets its own anchor (it nests under
    // memory/ and can be a symlink independently).
    let mut entities = Vec::new();
    if let Some(bank_dir) = anchored_dir(&data_dir, &store.bank_entities_dir()).await {
        if let Ok(mut dir_entries) = tokio::fs::read_dir(&bank_dir).await {
            while let Ok(Some(entry)) = dir_entries.next_entry().await {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "md") {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Some(content) = panel_read(&path).await {
                    entities.push(EntitySummary {
                        name: name.to_string(),
                        summary: first_summary_line(&content),
                    });
                }
            }
        }
        entities.sort_by(|a, b| a.name.cmp(&b.name));
    }
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
    // Same traversal-character sanitization `read_entity` applies, but
    // through the anchored no-follow read path (codex #1611 P1) — a
    // symlinked page or bank dir is a plain 404.
    let safe_name = name.replace(['/', '\\', '\0', '~', '.'], "_");
    let bank_dir = anchored_dir(&data_dir, &store.bank_entities_dir())
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let content = panel_read(&bank_dir.join(format!("{safe_name}.md")))
        .await
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
        assert!(resp.long_term_updated_at.is_none());
        assert_eq!(resp.recent.len(), 0);
        assert_eq!(resp.entities.len(), 0);
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
