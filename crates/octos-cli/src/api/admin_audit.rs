use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::AppState;
use super::router::AuthIdentity;
use crate::admin_audit_store::{
    AdminAuditPage, AdminAuditQuery, AdminAuditRecordInput, parse_audit_datetime,
};

#[derive(Debug, Deserialize)]
pub struct AdminAuditListParams {
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

impl AdminAuditListParams {
    fn into_query(self) -> Result<AdminAuditQuery, String> {
        Ok(AdminAuditQuery {
            actor: non_empty(self.actor),
            action: non_empty(self.action),
            target_id: non_empty(self.target_id),
            from: self
                .from
                .as_deref()
                .map(|value| parse_audit_datetime(value, false))
                .transpose()?,
            to: self
                .to
                .as_deref()
                .map(|value| parse_audit_datetime(value, true))
                .transpose()?,
            limit: self
                .limit
                .unwrap_or_else(|| AdminAuditQuery::default().limit),
            offset: self.offset.unwrap_or_default(),
        })
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn list_audit(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AdminAuditListParams>,
) -> Result<Json<AdminAuditPage>, (StatusCode, String)> {
    let store = state.admin_audit_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "admin audit store not configured".to_string(),
    ))?;
    let query = params
        .into_query()
        .map_err(|message| (StatusCode::BAD_REQUEST, message))?;
    let page = store
        .list(query)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(page))
}

pub(crate) fn actor_from_identity(identity: Option<&AuthIdentity>) -> String {
    match identity {
        Some(AuthIdentity::Admin) => "admin-token".to_string(),
        Some(AuthIdentity::User { id, role }) => {
            let role = match role {
                crate::user_store::UserRole::Admin => "admin",
                crate::user_store::UserRole::User => "user",
            };
            format!("user:{id}:{role}")
        }
        None => "admin-unauthenticated".to_string(),
    }
}

pub(crate) fn summary_value<T: Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value).ok()
}

pub(crate) fn record_admin_action(
    state: &AppState,
    identity: Option<&AuthIdentity>,
    action: &str,
    target_id: impl Into<String>,
    before_summary: Option<Value>,
    after_summary: Option<Value>,
) -> eyre::Result<()> {
    let Some(store) = state.admin_audit_store.as_ref() else {
        return Ok(());
    };
    store.record(AdminAuditRecordInput {
        actor: actor_from_identity(identity),
        action: action.to_string(),
        target_id: target_id.into(),
        before_summary,
        after_summary,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_audit_store::AdminAuditStore;

    #[test]
    fn list_params_parse_filters_and_dates() {
        let query = AdminAuditListParams {
            actor: Some(" admin-token ".into()),
            action: Some("profile.update".into()),
            target_id: None,
            from: Some("2026-05-24".into()),
            to: Some("2026-05-25T00:00:00Z".into()),
            limit: Some(10),
            offset: Some(5),
        }
        .into_query()
        .unwrap();

        assert_eq!(query.actor.as_deref(), Some("admin-token"));
        assert_eq!(query.action.as_deref(), Some("profile.update"));
        assert_eq!(query.limit, 10);
        assert_eq!(query.offset, 5);
        assert!(query.from.is_some());
        assert!(query.to.is_some());
    }

    #[tokio::test]
    async fn list_audit_returns_store_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AdminAuditStore::open(dir.path()).unwrap());
        store
            .record(crate::admin_audit_store::AdminAuditRecordInput {
                actor: "admin-token".into(),
                action: "profile.create".into(),
                target_id: "demo".into(),
                before_summary: None,
                after_summary: Some(serde_json::json!({ "id": "demo" })),
            })
            .unwrap();
        let state = Arc::new(AppState {
            admin_audit_store: Some(store),
            ..AppState::empty_for_tests()
        });

        let Json(page) = list_audit(
            State(state),
            Query(AdminAuditListParams {
                actor: Some("admin-token".into()),
                action: None,
                target_id: None,
                from: None,
                to: None,
                limit: None,
                offset: None,
            }),
        )
        .await
        .unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(page.entries[0].target_id, "demo");
    }
}
