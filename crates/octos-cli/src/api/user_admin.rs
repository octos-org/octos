//! Admin user management API handlers.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::router::AuthIdentity;
use crate::login_allowlist::AllowedLogin;
use crate::profiles::{ProfileStore, UserProfile};
use crate::user_store::{User, UserRole};

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

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub role: UserRole,
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

fn error_body(
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

/// PATCH /api/admin/users/{id}
pub async fn update_user(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<AuthIdentity>,
    Path(id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<User>, StatusCode> {
    if !matches!(identity, AuthIdentity::Admin) {
        return Err(StatusCode::FORBIDDEN);
    }

    let us = state
        .user_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mut user = us
        .get(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let old_role = user.role.clone();
    if old_role == req.role {
        return Ok(Json(user));
    }

    user.role = req.role;
    us.save(&user)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tracing::info!(
        user_id = %id,
        old_role = ?old_role,
        new_role = ?user.role,
        "update_user: role changed"
    );
    Ok(Json(user))
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
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<AllowlistRequest>,
) -> Result<(StatusCode, Json<AllowlistEntryResponse>), (StatusCode, Json<ErrorBody>)> {
    let allowlist = state.allowlist_store.as_ref().ok_or_else(|| {
        error_body(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin_not_configured",
            "admin allowlist is not configured",
        )
    })?;
    let email = crate::login_allowlist::normalize_email(&req.email);
    super::admin::validate_email(&email)
        .map_err(|message| error_body(StatusCode::BAD_REQUEST, "invalid_email", message))?;

    if allowlist.contains(&email).map_err(|error| {
        tracing::error!(error = %error, "failed to check allowlist entry");
        error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "allowlist_lookup_failed",
            "failed to check allowlist entry",
        )
    })? {
        return Err(error_body(
            StatusCode::CONFLICT,
            "allowed_email_exists",
            format!("email '{email}' is already on the allowlist"),
        ));
    }

    if let Some(user_store) = state.user_store.as_ref() {
        if user_store
            .get_by_email(&email)
            .map_err(|error| {
                tracing::error!(error = %error, "failed to check registered users");
                error_body(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "user_lookup_failed",
                    "failed to check registered users",
                )
            })?
            .is_some()
        {
            return Err(error_body(
                StatusCode::CONFLICT,
                "registered_email_exists",
                format!("email '{email}' is already registered"),
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
    allowlist.save(&entry).map_err(|error| {
        tracing::error!(error = %error, "failed to save allowlist entry");
        error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "allowlist_save_failed",
            "failed to save allowlist entry",
        )
    })?;

    let response = AllowlistEntryResponse {
        email,
        note: entry.note,
        created_at: entry.created_at,
        claimed_user_id: None,
        claimed_at: None,
        registered: false,
        registered_user_id: None,
        registered_name: None,
        last_login_at: None,
    };
    super::admin_audit::record_admin_action(
        &state,
        identity.as_ref().map(|identity| &identity.0),
        "allowlist.add",
        response.email.clone(),
        None,
        super::admin_audit::summary_value(&response),
    )
    .map_err(|error| {
        tracing::error!(error = %error, "failed to record admin audit entry");
        error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "admin_audit_failed",
            "failed to record admin audit entry",
        )
    })?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// DELETE /api/admin/allowed-emails/{email}
pub async fn delete_allowed_email(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Path(email): Path<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let allowlist = state
        .allowlist_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let before = allowlist
        .get(&email)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match allowlist
        .delete(&email)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        true => {
            let normalized = crate::login_allowlist::normalize_email(&email);
            super::admin_audit::record_admin_action(
                &state,
                identity.as_ref().map(|identity| &identity.0),
                "allowlist.delete",
                normalized,
                before.as_ref().and_then(super::admin_audit::summary_value),
                None,
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(ActionResponse {
                ok: true,
                message: None,
            }))
        }
        false => Err(StatusCode::NOT_FOUND),
    }
}

/// DELETE /api/admin/users/{id}
pub async fn delete_user(
    identity: Option<axum::Extension<AuthIdentity>>,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let us = state
        .user_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let before_user = us.get(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if before_user.is_none() {
        tracing::warn!(user_id = %id, "delete_user: user not found");
        return Err(StatusCode::NOT_FOUND);
    }

    if let Some(ref pm) = state.process_manager {
        let _ = pm.stop(&id).await;
    }

    if let Some(ref ps) = state.profile_store {
        delete_user_profiles(&state, ps, &id).await?;
    }

    match us.delete(&id) {
        Ok(true) => {
            tracing::info!(user_id = %id, "delete_user: user deleted");
            super::admin_audit::record_admin_action(
                &state,
                identity.as_ref().map(|identity| &identity.0),
                "user.delete",
                id.clone(),
                before_user
                    .as_ref()
                    .and_then(super::admin_audit::summary_value),
                None,
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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

async fn delete_user_profiles(
    state: &AppState,
    profile_store: &ProfileStore,
    user_id: &str,
) -> Result<(), StatusCode> {
    let parent = profile_store
        .get(user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let sub_accounts = profile_store
        .list_sub_accounts(user_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    for sub in sub_accounts {
        delete_profile_record(state, profile_store, &sub).await?;
        // Sub-accounts can have their own login users (created with
        // `id == sub.id` in admin.rs / auth_handlers.rs). Delete those rows
        // too — a stale user row survives the parent otherwise, and with
        // self-registration enabled `verify` would auto-create a fresh
        // TOP-LEVEL profile for it, resurrecting the deleted sub-account
        // with broken parentage.
        if let Some(ref us) = state.user_store {
            match us.delete(&sub.id) {
                Ok(true) => {
                    tracing::info!(user_id = %sub.id, "delete_user: cascaded sub-account login user");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        user_id = %sub.id,
                        error = %e,
                        "delete_user: failed to delete sub-account login user"
                    );
                }
            }
        }
    }

    if let Some(parent) = parent {
        delete_profile_record(state, profile_store, &parent).await?;
    }

    Ok(())
}

async fn delete_profile_record(
    state: &AppState,
    profile_store: &ProfileStore,
    profile: &UserProfile,
) -> Result<(), StatusCode> {
    if let Some(ref pm) = state.process_manager {
        let _ = pm.stop(&profile.id).await;
    }

    let data_dir = profile_store.resolve_data_dir(profile);
    if data_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&data_dir) {
            tracing::warn!(
                profile = %profile.id,
                dir = %data_dir.display(),
                error = %e,
                "failed to clean up profile data directory"
            );
        }
    }

    profile_store
        .delete(&profile.id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    crate::api::admin::release_deleted_profiles_keychain_items(
        profile_store,
        std::iter::once(profile),
    );

    Ok(())
}
