//! Admin user management API handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::login_allowlist::AllowedLogin;
use crate::user_store::User;

#[derive(Serialize)]
pub struct UsersListResponse {
    pub users: Vec<User>,
}

#[derive(Deserialize)]
pub struct AllowlistRequest {
    pub email: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Serialize)]
pub struct AllowlistEntryResponse {
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<chrono::DateTime<Utc>>,
    pub registered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_login_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct AllowlistResponse {
    pub entries: Vec<AllowlistEntryResponse>,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// GET /api/admin/users
pub async fn list_users(
    State(state): State<Arc<AppState>>,
) -> Result<Json<UsersListResponse>, StatusCode> {
    let us = state
        .user_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let users = us.list().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(UsersListResponse { users }))
}

/// GET /api/admin/allowed-emails
pub async fn list_allowed_emails(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AllowlistResponse>, StatusCode> {
    let allowlist = state
        .allowlist_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let entries = allowlist
        .list()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let user_store = state.user_store.as_ref();
    let mapped = entries
        .into_iter()
        .map(|entry| {
            let registered_user =
                user_store.and_then(|store| store.get_by_email(&entry.email).ok().flatten());
            AllowlistEntryResponse {
                email: entry.email,
                note: entry.note,
                created_at: entry.created_at,
                claimed_user_id: entry.claimed_user_id,
                claimed_at: entry.claimed_at,
                registered: registered_user.is_some(),
                registered_user_id: registered_user.as_ref().map(|user| user.id.clone()),
                registered_name: registered_user.as_ref().map(|user| user.name.clone()),
                last_login_at: registered_user.and_then(|user| user.last_login_at),
            }
        })
        .collect();
    Ok(Json(AllowlistResponse { entries: mapped }))
}

/// POST /api/admin/allowed-emails
pub async fn add_allowed_email(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AllowlistRequest>,
) -> Result<(StatusCode, Json<AllowlistEntryResponse>), StatusCode> {
    let allowlist = state
        .allowlist_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let email = crate::login_allowlist::normalize_email(&req.email);
    super::admin::validate_email(&email).map_err(|_| StatusCode::BAD_REQUEST)?;

    if allowlist
        .contains(&email)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Err(StatusCode::CONFLICT);
    }

    if let Some(user_store) = state.user_store.as_ref() {
        if user_store
            .get_by_email(&email)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .is_some()
        {
            return Err(StatusCode::CONFLICT);
        }
    }

    let entry = AllowedLogin {
        email: email.clone(),
        note: req.note.and_then(|note| {
            let trimmed = note.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }),
        created_at: Utc::now(),
        claimed_user_id: None,
        claimed_at: None,
    };
    allowlist
        .save(&entry)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Json(AllowlistEntryResponse {
            email,
            note: entry.note,
            created_at: entry.created_at,
            claimed_user_id: None,
            claimed_at: None,
            registered: false,
            registered_user_id: None,
            registered_name: None,
            last_login_at: None,
        }),
    ))
}

/// DELETE /api/admin/allowed-emails/{email}
pub async fn delete_allowed_email(
    State(state): State<Arc<AppState>>,
    Path(email): Path<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let allowlist = state
        .allowlist_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    match allowlist
        .delete(&email)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        true => Ok(Json(ActionResponse {
            ok: true,
            message: None,
        })),
        false => Err(StatusCode::NOT_FOUND),
    }
}

/// DELETE /api/admin/users/{id}
pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let us = state
        .user_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    if let Some(ref pm) = state.process_manager {
        let _ = pm.stop(&id).await;
    }

    if let Some(ref ps) = state.profile_store {
        let _ = ps.delete(&id);
    }

    match us.delete(&id) {
        Ok(true) => {
            tracing::info!(user_id = %id, "delete_user: user deleted");
            Ok(Json(ActionResponse {
                ok: true,
                message: None,
            }))
        }
        Ok(false) => {
            tracing::warn!(user_id = %id, "delete_user: user not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            tracing::error!(user_id = %id, error = %e, "delete_user: failed to delete");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_list_response_serialize() {
        let resp = UsersListResponse { users: vec![] };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["users"].as_array().unwrap().is_empty());
    }

    #[test]
    fn action_response_serialize_ok() {
        let resp = ActionResponse {
            ok: true,
            message: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert!(json.get("message").is_none());
    }

    #[test]
    fn action_response_serialize_with_message() {
        let resp = ActionResponse {
            ok: false,
            message: Some("not found".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["message"], "not found");
    }
}
