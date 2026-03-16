//! Admin user management API handlers.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::user_store::{User, UserRole, email_to_user_id};

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub name: String,
    #[serde(default = "default_role")]
    pub role: UserRole,
}

fn default_role() -> UserRole {
    UserRole::User
}

#[derive(Serialize)]
pub struct UserResponse {
    pub user: User,
}

#[derive(Serialize)]
pub struct UsersListResponse {
    pub users: Vec<User>,
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

/// POST /api/admin/users
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserResponse>), StatusCode> {
    let us = state
        .user_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Check if email already registered
    if let Ok(Some(_)) = us.get_by_email(&req.email) {
        tracing::warn!(email = %req.email, "create_user: email already registered");
        return Err(StatusCode::CONFLICT);
    }

    // Generate unique ID from email
    let base_id = email_to_user_id(&req.email);
    let mut id = base_id.clone();
    let mut suffix = 1u32;
    while us.get(&id).unwrap_or(None).is_some() {
        id = format!("{base_id}-{suffix}");
        suffix += 1;
    }

    let user = User {
        id,
        email: req.email.to_lowercase(),
        name: req.name,
        role: req.role,
        created_at: Utc::now(),
        last_login_at: None,
    };

    us.save(&user).map_err(|e| {
        tracing::error!(email = %user.email, error = %e, "create_user: failed to save user");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!(user_id = %user.id, email = %user.email, role = ?user.role, "create_user: user created");

    // Also create a default profile for the user
    if let Some(ref ps) = state.profile_store {
        let profile = crate::profiles::UserProfile {
            id: user.id.clone(),
            name: user.name.clone(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        if let Err(e) = ps.save(&profile) {
            tracing::warn!(user_id = %user.id, error = %e, "failed to create profile for new user");
        }
    }

    Ok((StatusCode::CREATED, Json(UserResponse { user })))
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

    // Stop gateway if running
    if let Some(ref pm) = state.process_manager {
        let _ = pm.stop(&id).await;
    }

    // Delete profile
    if let Some(ref ps) = state.profile_store {
        let _ = ps.delete(&id);
    }

    // Delete user
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
    fn create_user_request_deserialize_defaults() {
        let json = r#"{"email": "alice@example.com", "name": "Alice"}"#;
        let req: CreateUserRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.email, "alice@example.com");
        assert_eq!(req.name, "Alice");
        assert!(matches!(req.role, UserRole::User));
    }

    #[test]
    fn create_user_request_deserialize_admin_role() {
        let json = r#"{"email": "admin@co.com", "name": "Admin", "role": "admin"}"#;
        let req: CreateUserRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.role, UserRole::Admin));
    }

    #[test]
    fn default_role_is_user() {
        assert!(matches!(default_role(), UserRole::User));
    }

    #[test]
    fn user_response_serialize() {
        let resp = UserResponse {
            user: User {
                id: "u1".into(),
                email: "a@b.com".into(),
                name: "Test".into(),
                role: UserRole::User,
                created_at: chrono::Utc::now(),
                last_login_at: None,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["user"]["id"], "u1");
        assert_eq!(json["user"]["email"], "a@b.com");
        assert_eq!(json["user"]["name"], "Test");
    }

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
