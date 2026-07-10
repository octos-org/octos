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

    let long_term = store.read_long_term().await.unwrap_or_default();
    let long_term_updated_at = tokio::fs::metadata(store.memory_md_path())
        .await
        .ok()
        .and_then(|meta| meta.modified().ok())
        .map(|mtime| chrono::DateTime::<chrono::Utc>::from(mtime).to_rfc3339());
    let today = store.read_today().await.unwrap_or_default();
    let recent = store
        .read_recent(7)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(date, content)| DailyNote { date, content })
        .collect();
    let entities = store
        .list_entities()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(name, summary)| EntitySummary { name, summary })
        .collect();
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
    // `read_entity` sanitizes path-traversal characters itself; a name
    // that sanitizes away to a missing page is a plain 404.
    let content = store
        .read_entity(&name)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
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
