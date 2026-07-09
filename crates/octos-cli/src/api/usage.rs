//! Usage analytics API handlers.

use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::AppState;
use super::auth_handlers;
use super::router::AuthIdentity;
use crate::profiles::ProfileStore;
use crate::usage_ledger::{PersistentUsageLedger, UsageAnalytics, UsageQuery};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageApiQuery {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub to: Option<DateTime<Utc>>,
}

impl UsageApiQuery {
    fn into_ledger_query(self, profile_id: Option<String>) -> UsageQuery {
        UsageQuery {
            profile_id,
            session_id: self.session_id,
            from: self.from,
            to: self.to,
        }
    }
}

pub async fn my_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    Query(query): Query<UsageApiQuery>,
) -> Result<Json<UsageAnalytics>, StatusCode> {
    let (profile_id, data_dir) = resolve_my_usage_profile(&state, &headers, &identity)?;
    usage_for_data_dir(&data_dir, query.into_ledger_query(Some(profile_id)))
        .await
        .map(Json)
}

pub async fn my_session_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(identity): axum::Extension<AuthIdentity>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<UsageApiQuery>,
) -> Result<Json<UsageAnalytics>, StatusCode> {
    let (profile_id, data_dir) = resolve_my_usage_profile(&state, &headers, &identity)?;
    let mut query = query.into_ledger_query(Some(profile_id));
    query.session_id = Some(session_id);
    usage_for_data_dir(&data_dir, query).await.map(Json)
}

pub async fn admin_usage(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageApiQuery>,
) -> Result<Json<UsageAnalytics>, StatusCode> {
    let store = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    aggregate_profiles_usage(store, query).await.map(Json)
}

pub async fn admin_profile_usage(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<UsageApiQuery>,
) -> Result<Json<UsageAnalytics>, StatusCode> {
    let store = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let data_dir = resolve_profile_data_dir(store, &id)?;
    usage_for_data_dir(&data_dir, query.into_ledger_query(Some(id)))
        .await
        .map(Json)
}

pub async fn admin_profile_session_usage(
    State(state): State<Arc<AppState>>,
    AxumPath((id, session_id)): AxumPath<(String, String)>,
    Query(query): Query<UsageApiQuery>,
) -> Result<Json<UsageAnalytics>, StatusCode> {
    let store = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let data_dir = resolve_profile_data_dir(store, &id)?;
    let mut query = query.into_ledger_query(Some(id));
    query.session_id = Some(session_id);
    usage_for_data_dir(&data_dir, query).await.map(Json)
}

pub(crate) async fn usage_for_data_dir(
    data_dir: &Path,
    query: UsageQuery,
) -> Result<UsageAnalytics, StatusCode> {
    let ledger = PersistentUsageLedger::open(data_dir)
        .await
        .map_err(|error| {
            tracing::warn!(data_dir = %data_dir.display(), error = %error, "failed to open usage ledger");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    ledger.analytics(query).await.map_err(|error| {
        tracing::warn!(data_dir = %data_dir.display(), error = %error, "failed to read usage analytics");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

pub(crate) async fn aggregate_profiles_usage(
    store: &ProfileStore,
    query: UsageApiQuery,
) -> Result<UsageAnalytics, StatusCode> {
    let profiles = store
        .list()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut aggregate = UsageAnalytics::default();
    for profile in profiles {
        let data_dir = store.resolve_data_dir(&profile);
        let analytics = usage_for_data_dir(
            &data_dir,
            query.clone().into_ledger_query(Some(profile.id.clone())),
        )
        .await?;
        aggregate.merge(analytics);
    }
    Ok(aggregate)
}

fn resolve_my_usage_profile(
    state: &AppState,
    headers: &HeaderMap,
    identity: &AuthIdentity,
) -> Result<(String, std::path::PathBuf), StatusCode> {
    let store = state
        .profile_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let profile_id = auth_handlers::resolve_my_profile_id(identity, store, state, headers)?;
    let data_dir = resolve_profile_data_dir(store, &profile_id)?;
    Ok((profile_id, data_dir))
}

fn resolve_profile_data_dir(
    store: &ProfileStore,
    id: &str,
) -> Result<std::path::PathBuf, StatusCode> {
    let profile = store
        .get(id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(store.resolve_data_dir(&profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{ProfileConfig, UserProfile};
    use crate::usage_ledger::{UsageCostSource, UsageEvent};

    fn profile(id: &str) -> UserProfile {
        UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            public_subdomain: None,
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn usage(profile_id: &str, session_id: &str, input_tokens: u64) -> UsageEvent {
        UsageEvent::completed_run(
            profile_id,
            session_id,
            format!("run-{session_id}"),
            Some("openai".to_string()),
            Some("gpt-4.1".to_string()),
            None,
            input_tokens,
            7,
            Some(0.01),
            UsageCostSource::CatalogEstimate,
            "appui",
            None,
        )
    }

    #[tokio::test]
    async fn aggregate_profiles_usage_reads_every_profile_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(dir.path()).unwrap();
        store.save(&profile("alpha")).unwrap();
        store.save(&profile("beta")).unwrap();

        for id in ["alpha", "beta"] {
            let profile = store.get(id).unwrap().unwrap();
            let ledger = PersistentUsageLedger::open(store.resolve_data_dir(&profile))
                .await
                .unwrap();
            ledger
                .record(usage(id, &format!("{id}-session"), 10))
                .await
                .unwrap();
        }

        let analytics = aggregate_profiles_usage(&store, UsageApiQuery::default())
            .await
            .unwrap();
        assert_eq!(analytics.totals.run_count, 2);
        assert_eq!(analytics.totals.input_tokens, 20);
        assert_eq!(analytics.by_profile.len(), 2);
    }

    #[tokio::test]
    async fn data_dir_usage_query_filters_session() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = PersistentUsageLedger::open(dir.path()).await.unwrap();
        ledger.record(usage("alpha", "one", 10)).await.unwrap();
        ledger.record(usage("alpha", "two", 20)).await.unwrap();
        drop(ledger);

        let analytics = usage_for_data_dir(
            dir.path(),
            UsageQuery {
                profile_id: Some("alpha".to_string()),
                session_id: Some("two".to_string()),
                from: None,
                to: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(analytics.totals.run_count, 1);
        assert_eq!(analytics.totals.input_tokens, 20);
    }
}
