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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::login_allowlist::LoginAllowlistStore;
    use crate::profiles::ProfileConfig;
    use crate::user_store::{User, UserRole, UserStore};

    fn state_with_stores(
        allowlist_store: Arc<LoginAllowlistStore>,
        user_store: Option<Arc<UserStore>>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            allowlist_store: Some(allowlist_store),
            user_store,
            ..AppState::empty_for_tests()
        })
    }

    fn test_user(id: &str, role: UserRole) -> User {
        User {
            id: id.to_string(),
            email: format!("{id}@example.com"),
            name: id.to_string(),
            role,
            created_at: Utc::now(),
            last_login_at: None,
        }
    }

    fn test_profile(id: &str, parent_id: Option<&str>) -> UserProfile {
        UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            public_subdomain: None,
            enabled: true,
            data_dir: None,
            parent_id: parent_id.map(ToString::to_string),
            config: ProfileConfig::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn state_with_user(user: User) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::user_store::UserStore::open(dir.path()).unwrap());
        store.save(&user).unwrap();
        let state = Arc::new(AppState {
            user_store: Some(store),
            ..AppState::empty_for_tests()
        });
        (state, dir)
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

    #[tokio::test]
    async fn delete_user_cascades_sub_account_profiles_and_data_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let profile_store = Arc::new(ProfileStore::open(dir.path()).unwrap());
        let parent = test_profile("alice", None);
        let sub_account = test_profile("alice--bot", Some("alice"));
        let parent_data_dir = profile_store.resolve_data_dir(&parent);
        let sub_data_dir = profile_store.resolve_data_dir(&sub_account);

        user_store
            .save(&test_user("alice", UserRole::User))
            .unwrap();
        profile_store.save(&parent).unwrap();
        profile_store.save(&sub_account).unwrap();
        assert!(parent_data_dir.exists());
        assert!(sub_data_dir.exists());

        let state = Arc::new(AppState {
            user_store: Some(user_store.clone()),
            profile_store: Some(profile_store.clone()),
            ..AppState::empty_for_tests()
        });

        let response = delete_user(None, State(state), Path("alice".to_string()))
            .await
            .unwrap()
            .0;

        assert!(response.ok);
        assert!(user_store.get("alice").unwrap().is_none());
        assert!(profile_store.get("alice").unwrap().is_none());
        assert!(profile_store.get("alice--bot").unwrap().is_none());
        assert!(!parent_data_dir.exists());
        assert!(!sub_data_dir.exists());
    }

    #[tokio::test]
    async fn delete_user_missing_user_does_not_delete_matching_profile() {
        let dir = tempfile::tempdir().unwrap();
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        let profile_store = Arc::new(ProfileStore::open(dir.path()).unwrap());
        let profile = test_profile("alice", None);
        let data_dir = profile_store.resolve_data_dir(&profile);
        profile_store.save(&profile).unwrap();

        let state = Arc::new(AppState {
            user_store: Some(user_store),
            profile_store: Some(profile_store.clone()),
            ..AppState::empty_for_tests()
        });

        let result = delete_user(None, State(state), Path("alice".to_string())).await;

        assert!(matches!(result, Err(StatusCode::NOT_FOUND)));
        assert!(profile_store.get("alice").unwrap().is_some());
        assert!(data_dir.exists());
    }

    #[test]
    fn error_body_serializes_code_and_message() {
        let body = ErrorBody {
            code: "registered_email_exists",
            message: "email 'alice@example.com' is already registered".into(),
        };

        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["code"], "registered_email_exists");
        assert_eq!(
            json["message"],
            "email 'alice@example.com' is already registered"
        );
    }

    #[tokio::test]
    async fn add_allowed_email_returns_json_conflict_for_existing_allowlist_entry() {
        let dir = tempfile::tempdir().unwrap();
        let allowlist = Arc::new(LoginAllowlistStore::open(dir.path()).unwrap());
        allowlist
            .save(&AllowedLogin {
                email: "alice@example.com".into(),
                note: None,
                created_at: Utc::now(),
                claimed_user_id: None,
                claimed_at: None,
            })
            .unwrap();
        let state = state_with_stores(allowlist, None);

        let result = add_allowed_email(
            None,
            State(state),
            Json(AllowlistRequest {
                email: "ALICE@example.com".into(),
                note: None,
            }),
        )
        .await;

        let Err((status, Json(body))) = result else {
            panic!("expected duplicate allowlist entry to return a conflict");
        };
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.code, "allowed_email_exists");
        assert_eq!(
            body.message,
            "email 'alice@example.com' is already on the allowlist"
        );
    }

    #[tokio::test]
    async fn add_allowed_email_returns_json_conflict_for_existing_user() {
        let dir = tempfile::tempdir().unwrap();
        let allowlist = Arc::new(LoginAllowlistStore::open(dir.path()).unwrap());
        let user_store = Arc::new(UserStore::open(dir.path()).unwrap());
        user_store
            .save(&User {
                id: "alice".into(),
                email: "alice@example.com".into(),
                name: "Alice".into(),
                role: UserRole::User,
                created_at: Utc::now(),
                last_login_at: None,
            })
            .unwrap();
        let state = state_with_stores(allowlist, Some(user_store));

        let result = add_allowed_email(
            None,
            State(state),
            Json(AllowlistRequest {
                email: "alice@example.com".into(),
                note: None,
            }),
        )
        .await;

        let Err((status, Json(body))) = result else {
            panic!("expected registered user email to return a conflict");
        };
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.code, "registered_email_exists");
        assert_eq!(
            body.message,
            "email 'alice@example.com' is already registered"
        );
    }

    #[tokio::test]
    async fn add_allowed_email_records_admin_audit_entry() {
        let dir = tempfile::tempdir().unwrap();
        let allowlist =
            Arc::new(crate::login_allowlist::LoginAllowlistStore::open(dir.path()).unwrap());
        let audit = Arc::new(crate::admin_audit_store::AdminAuditStore::open(dir.path()).unwrap());
        let state = Arc::new(AppState {
            allowlist_store: Some(allowlist),
            admin_audit_store: Some(audit.clone()),
            ..AppState::empty_for_tests()
        });

        let (status, Json(response)) = add_allowed_email(
            Some(axum::Extension(AuthIdentity::Admin)),
            State(state),
            Json(AllowlistRequest {
                email: "Ada@Example.com".into(),
                note: Some("Pilot".into()),
            }),
        )
        .await
        .unwrap();

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(response.email, "ada@example.com");
        let page = audit
            .list(crate::admin_audit_store::AdminAuditQuery::default())
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.entries[0].action, "allowlist.add");
        assert_eq!(page.entries[0].target_id, "ada@example.com");
    }

    #[tokio::test]
    async fn update_user_role_promotes_with_global_admin_token() {
        let (state, _dir) = state_with_user(test_user("alice", UserRole::User));

        let Json(updated) = update_user(
            State(state.clone()),
            Extension(AuthIdentity::Admin),
            Path("alice".to_string()),
            Json(UpdateUserRequest {
                role: UserRole::Admin,
            }),
        )
        .await
        .unwrap();

        assert_eq!(updated.role, UserRole::Admin);
        let persisted = state
            .user_store
            .as_ref()
            .unwrap()
            .get("alice")
            .unwrap()
            .unwrap();
        assert_eq!(persisted.role, UserRole::Admin);
    }

    #[tokio::test]
    async fn update_user_role_rejects_user_session_admins() {
        let (state, _dir) = state_with_user(test_user("alice", UserRole::User));

        let err = update_user(
            State(state.clone()),
            Extension(AuthIdentity::User {
                id: "operator".to_string(),
                role: UserRole::Admin,
            }),
            Path("alice".to_string()),
            Json(UpdateUserRequest {
                role: UserRole::Admin,
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err, StatusCode::FORBIDDEN);
        let persisted = state
            .user_store
            .as_ref()
            .unwrap()
            .get("alice")
            .unwrap()
            .unwrap();
        assert_eq!(persisted.role, UserRole::User);
    }

    #[tokio::test]
    async fn update_user_role_returns_not_found_for_missing_user() {
        let (state, _dir) = state_with_user(test_user("alice", UserRole::User));

        let err = update_user(
            State(state),
            Extension(AuthIdentity::Admin),
            Path("bob".to_string()),
            Json(UpdateUserRequest {
                role: UserRole::Admin,
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err, StatusCode::NOT_FOUND);
    }
}
