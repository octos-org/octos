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

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

type ApiResult<T> = Result<T, (StatusCode, Json<ErrorBody>)>;

fn api_error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            code,
            message: message.into(),
        }),
    )
}

/// GET /api/admin/users
pub async fn list_users(State(state): State<Arc<AppState>>) -> ApiResult<Json<UsersListResponse>> {
    let us = state.user_store.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "user_store_unavailable",
            "user store is unavailable",
        )
    })?;
    let users = us.list().map_err(|e| {
        tracing::error!(error = %e, "failed to list users");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "users_list_failed",
            "failed to list users",
        )
    })?;
    Ok(Json(UsersListResponse { users }))
}

/// GET /api/admin/allowed-emails
pub async fn list_allowed_emails(
    State(state): State<Arc<AppState>>,
) -> ApiResult<Json<AllowlistResponse>> {
    let allowlist = state.allowlist_store.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "allowlist_store_unavailable",
            "login allowlist store is unavailable",
        )
    })?;
    let entries = allowlist.list().map_err(|e| {
        tracing::error!(error = %e, "failed to list allowed emails");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "allowlist_list_failed",
            "failed to list allowed emails",
        )
    })?;
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
) -> ApiResult<(StatusCode, Json<AllowlistEntryResponse>)> {
    let allowlist = state.allowlist_store.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "allowlist_store_unavailable",
            "login allowlist store is unavailable",
        )
    })?;
    let email = crate::login_allowlist::normalize_email(&req.email);
    super::admin::validate_email(&email)
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, "invalid_email", message))?;

    if allowlist.contains(&email).map_err(|e| {
        tracing::error!(email = %email, error = %e, "failed to check login allowlist");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "allowlist_read_failed",
            "failed to read login allowlist",
        )
    })? {
        return Err(api_error(
            StatusCode::CONFLICT,
            "email_already_allowed",
            format!("Email \"{email}\" is already allowlisted"),
        ));
    }

    if let Some(user_store) = state.user_store.as_ref() {
        if user_store
            .get_by_email(&email)
            .map_err(|e| {
                tracing::error!(email = %email, error = %e, "failed to check registered users");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "users_read_failed",
                    "failed to read registered users",
                )
            })?
            .is_some()
        {
            return Err(api_error(
                StatusCode::CONFLICT,
                "email_already_registered",
                format!("Email \"{email}\" is already registered to an account"),
            ));
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
    allowlist.save(&entry).map_err(|e| {
        tracing::error!(email = %email, error = %e, "failed to save allowed email");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "allowlist_save_failed",
            "failed to save allowed email",
        )
    })?;

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
) -> ApiResult<Json<ActionResponse>> {
    let allowlist = state.allowlist_store.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "allowlist_store_unavailable",
            "login allowlist store is unavailable",
        )
    })?;
    match allowlist.delete(&email).map_err(|e| {
        tracing::error!(email = %email, error = %e, "failed to delete allowed email");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "allowlist_delete_failed",
            "failed to delete allowed email",
        )
    })? {
        true => Ok(Json(ActionResponse {
            ok: true,
            message: None,
        })),
        false => Err(api_error(
            StatusCode::NOT_FOUND,
            "allowed_email_not_found",
            format!("Allowlisted email \"{email}\" was not found"),
        )),
    }
}

/// DELETE /api/admin/users/{id}
pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<ActionResponse>> {
    let us = state.user_store.as_ref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "user_store_unavailable",
            "user store is unavailable",
        )
    })?;

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
            Err(api_error(
                StatusCode::NOT_FOUND,
                "user_not_found",
                format!("User \"{id}\" was not found"),
            ))
        }
        Err(e) => {
            tracing::error!(user_id = %id, error = %e, "delete_user: failed to delete");
            Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "user_delete_failed",
                "failed to delete user",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::login_allowlist::LoginAllowlistStore;
    use crate::user_store::{UserRole, UserStore};

    fn temp_user_admin_state() -> (
        tempfile::TempDir,
        Arc<AppState>,
        Arc<UserStore>,
        Arc<LoginAllowlistStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let allowlist_store = Arc::new(LoginAllowlistStore::open(dir.path()).unwrap());
        let state = Arc::new(AppState {
            user_store: Some(user_store.clone()),
            allowlist_store: Some(allowlist_store.clone()),
            ..AppState::empty_for_tests()
        });
        (dir, state, user_store, allowlist_store)
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

    #[test]
    fn error_body_serialize() {
        let resp = ErrorBody {
            code: "email_already_allowed",
            message: "Email is already allowlisted".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["code"], "email_already_allowed");
        assert_eq!(json["message"], "Email is already allowlisted");
    }

    #[tokio::test]
    async fn add_allowed_email_conflict_reports_allowlist_reason() {
        let (_dir, state, _user_store, allowlist_store) = temp_user_admin_state();
        allowlist_store
            .save(&AllowedLogin {
                email: "taken@example.com".into(),
                note: None,
                created_at: Utc::now(),
                claimed_user_id: None,
                claimed_at: None,
            })
            .unwrap();

        let err = match add_allowed_email(
            State(state),
            Json(AllowlistRequest {
                email: "taken@example.com".into(),
                note: None,
            }),
        )
        .await
        {
            Ok(_) => panic!("allowlist conflict should return a structured error"),
            Err(err) => err,
        };

        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0.code, "email_already_allowed");
        assert_eq!(
            err.1.0.message,
            "Email \"taken@example.com\" is already allowlisted"
        );
    }

    #[tokio::test]
    async fn add_allowed_email_conflict_reports_registered_user_reason() {
        let (_dir, state, user_store, _allowlist_store) = temp_user_admin_state();
        user_store
            .save(&User {
                id: "taken".into(),
                email: "taken@example.com".into(),
                name: "Taken User".into(),
                role: UserRole::User,
                created_at: Utc::now(),
                last_login_at: None,
            })
            .unwrap();

        let err = match add_allowed_email(
            State(state),
            Json(AllowlistRequest {
                email: "taken@example.com".into(),
                note: None,
            }),
        )
        .await
        {
            Ok(_) => panic!("registered email conflict should return a structured error"),
            Err(err) => err,
        };

        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.0.code, "email_already_registered");
        assert_eq!(
            err.1.0.message,
            "Email \"taken@example.com\" is already registered to an account"
        );
    }
}
