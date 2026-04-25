//! First-run admin setup endpoints.
//!
//! Exposes the status + rotation handlers that back the dashboard
//! `BootstrapGate` and `SetupRotateToken` page. All routes live under
//! `/api/admin/...` and are gated by the admin auth middleware.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::admin_token_store::AdminTokenRecord;
use crate::setup_state_store::SetupState;
use crate::smtp_secret_store::SmtpSecretStore;

#[derive(Serialize)]
pub struct TokenStatus {
    pub rotated: bool,
}

#[derive(Deserialize)]
pub struct RotateBody {
    pub new_token: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

/// GET `/api/admin/token/status` — whether the bootstrap token has been
/// rotated into a hashed persistent record.
pub async fn token_status(State(state): State<Arc<AppState>>) -> Json<TokenStatus> {
    Json(TokenStatus {
        rotated: state.admin_token_store.exists(),
    })
}

/// POST `/api/admin/token/rotate` — replace the bootstrap token with a
/// hashed persistent record. Refuses if a record already exists (operator
/// must `octos admin reset-token` to restart).
pub async fn rotate_token(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RotateBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    validate_token_strength(&body.new_token)?;
    if state.admin_token_store.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorBody {
                code: "already_rotated",
                message:
                    "admin token has already been rotated; use `octos admin reset-token` to start over"
                        .into(),
            }),
        ));
    }
    let record = AdminTokenRecord::from_plaintext(&body.new_token);
    state.admin_token_store.save(&record).map_err(|e| {
        tracing::error!(error = ?e, "failed to persist rotated admin token");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
    tracing::info!("admin token rotated via dashboard");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct StepBody {
    pub step: u32,
}

/// GET `/api/admin/setup/state` — current wizard state (completion, skip,
/// last step reached). Returns a default (all-empty) state when no file
/// exists yet.
pub async fn get_setup_state(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SetupState>, (StatusCode, Json<ErrorBody>)> {
    state.setup_state_store.load().map(Json).map_err(|e| {
        tracing::error!(error = ?e, "failed to load setup state");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "load_failed",
                message: e.to_string(),
            }),
        )
    })
}

/// POST `/api/admin/setup/step` — record the furthest wizard step reached.
/// Rejects `step > 4` with `invalid_step`.
pub async fn post_setup_step(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StepBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    if body.step > 4 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "invalid_step",
                message: "step must be between 0 and 4 inclusive".into(),
            }),
        ));
    }
    state
        .setup_state_store
        .update_last_step(body.step)
        .map_err(|e| {
            tracing::error!(error = ?e, "failed to persist setup step");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "save_failed",
                    message: e.to_string(),
                }),
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST `/api/admin/setup/complete` — mark the wizard as completed.
pub async fn post_setup_complete(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state.setup_state_store.mark_complete().map_err(|e| {
        tracing::error!(error = ?e, "failed to mark setup complete");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST `/api/admin/setup/skip` — mark the wizard as skipped.
pub async fn post_setup_skip(
    State(state): State<Arc<AppState>>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    state.setup_state_store.mark_skipped().map_err(|e| {
        tracing::error!(error = ?e, "failed to mark setup skipped");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_token_strength(t: &str) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    if t.len() < 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "weak_token",
                message: "token must be at least 32 characters".into(),
            }),
        ));
    }
    let mut classes = 0;
    if t.chars().any(|c| c.is_ascii_lowercase()) {
        classes += 1;
    }
    if t.chars().any(|c| c.is_ascii_uppercase()) {
        classes += 1;
    }
    if t.chars().any(|c| c.is_ascii_digit()) {
        classes += 1;
    }
    if t.chars().any(|c| !c.is_ascii_alphanumeric()) {
        classes += 1;
    }
    if classes < 3 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "weak_token",
                message: "token must contain at least 3 of: lowercase, uppercase, digits, symbols"
                    .into(),
            }),
        ));
    }
    Ok(())
}

// ------------------------- SMTP configuration ----------------------------

#[derive(Debug, Serialize)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub from_address: String,
    /// Whether `smtp_secret.json` currently holds a password. The password
    /// itself is never returned by the API.
    pub password_configured: bool,
}

#[derive(Debug, Deserialize)]
pub struct SmtpSettingsBody {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub from_address: String,
    /// Optional — when `Some` and non-empty, overwrites `smtp_secret.json`.
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SmtpTestBody {
    pub to: String,
}

#[derive(Debug, Serialize)]
pub struct SmtpTestResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Derive the data directory from the admin-token store, which always lives
/// at `{data_dir}/admin_token.json`.
fn data_dir_from_state(state: &AppState) -> Option<PathBuf> {
    state
        .admin_token_store
        .path()
        .parent()
        .map(|p| p.to_path_buf())
}

fn require_config_path(state: &AppState) -> Result<PathBuf, (StatusCode, Json<ErrorBody>)> {
    match &state.config_path {
        Some(p) => Ok(p.clone()),
        None => {
            let data_dir = data_dir_from_state(state).ok_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "no_data_dir",
                    message: "server has no resolvable data directory".into(),
                }),
            ))?;
            Ok(crate::config::Config::data_dir_config_path(&data_dir))
        }
    }
}

struct SmtpFromConfig {
    host: String,
    port: u16,
    username: String,
    from_address: String,
}

fn read_smtp_from_config(
    state: &AppState,
) -> Result<SmtpFromConfig, (StatusCode, Json<ErrorBody>)> {
    let path = require_config_path(state)?;
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SmtpFromConfig {
                host: String::new(),
                port: 465,
                username: String::new(),
                from_address: String::new(),
            });
        }
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "read_failed",
                    message: e.to_string(),
                }),
            ));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "parse_failed",
                message: e.to_string(),
            }),
        )
    })?;
    let smtp = value
        .get("dashboard_auth")
        .and_then(|v| v.get("smtp"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let host = smtp
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let port = smtp.get("port").and_then(|v| v.as_u64()).unwrap_or(465) as u16;
    let username = smtp
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let from_address = smtp
        .get("from_address")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(SmtpFromConfig {
        host,
        port,
        username,
        from_address,
    })
}

/// GET `/api/admin/smtp` — current SMTP settings plus whether the password
/// file is populated. The password itself is never returned.
pub async fn get_smtp(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SmtpSettings>, (StatusCode, Json<ErrorBody>)> {
    let cfg = read_smtp_from_config(&state)?;
    let password_configured = data_dir_from_state(&state)
        .map(|dir| SmtpSecretStore::new(&dir).exists())
        .unwrap_or(false);
    Ok(Json(SmtpSettings {
        host: cfg.host,
        port: cfg.port,
        username: cfg.username,
        from_address: cfg.from_address,
        password_configured,
    }))
}

fn validate_smtp_body(body: &SmtpSettingsBody) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    if body.host.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "invalid_host",
                message: "host must not be empty".into(),
            }),
        ));
    }
    if body.port == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "invalid_port",
                message: "port must be greater than 0".into(),
            }),
        ));
    }
    if !body.from_address.contains('@') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "invalid_from_address",
                message: "from_address must contain '@'".into(),
            }),
        ));
    }
    Ok(())
}

/// POST `/api/admin/smtp` — persist the SMTP block into `config.json` and,
/// optionally, a fresh password into `smtp_secret.json`. Returns 204.
pub async fn post_smtp(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SmtpSettingsBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    validate_smtp_body(&body)?;

    if let Some(ref pw) = body.password {
        if !pw.is_empty() {
            let data_dir = data_dir_from_state(&state).ok_or((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "no_data_dir",
                    message: "server has no resolvable data directory".into(),
                }),
            ))?;
            SmtpSecretStore::new(&data_dir).save(pw).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorBody {
                        code: "save_failed",
                        message: e.to_string(),
                    }),
                )
            })?;
        }
    }

    let path = require_config_path(&state)?;
    crate::config::write_mutation(&path, |value| {
        let obj = value
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("config.json root is not a JSON object"))?;
        let auth = obj
            .entry("dashboard_auth".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let auth_obj = auth
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("dashboard_auth is not an object"))?;
        let smtp = auth_obj
            .entry("smtp".to_string())
            .or_insert_with(|| serde_json::json!({}));
        let smtp_obj = smtp
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("dashboard_auth.smtp is not an object"))?;
        smtp_obj.insert("host".into(), serde_json::json!(body.host));
        smtp_obj.insert("port".into(), serde_json::json!(body.port));
        smtp_obj.insert("username".into(), serde_json::json!(body.username));
        smtp_obj.insert("from_address".into(), serde_json::json!(body.from_address));
        // Backfill `password_env` when it is missing so lettre can still find
        // a fallback env var if `smtp_secret.json` is ever removed.
        smtp_obj
            .entry("password_env".to_string())
            .or_insert_with(|| serde_json::json!("SMTP_PASSWORD"));
        Ok(())
    })
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;

    // Refresh the in-memory SMTP config so the test endpoint and subsequent OTP
    // sends pick up the new values without a server restart.
    if let Some(ref auth_mgr) = state.auth_manager {
        auth_mgr
            .set_smtp_config(Some(crate::otp::SmtpConfig {
                host: body.host.clone(),
                port: body.port,
                username: body.username.clone(),
                password_env: "SMTP_PASSWORD".to_string(),
                from_address: body.from_address.clone(),
            }))
            .await;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct EmailTokenBody {
    pub to: String,
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct EmailTokenResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// POST `/api/admin/token/email` — email the freshly rotated admin token to
/// the given address. Requires SMTP to already be configured (typically by
/// the CLI installer before the dashboard came up).
pub async fn post_token_email(
    State(state): State<Arc<AppState>>,
    Json(body): Json<EmailTokenBody>,
) -> Json<EmailTokenResult> {
    if !body.to.contains('@') {
        return Json(EmailTokenResult {
            ok: false,
            message: None,
            error: Some("recipient address must contain '@'".into()),
        });
    }
    if body.token.trim().is_empty() {
        return Json(EmailTokenResult {
            ok: false,
            message: None,
            error: Some("token must not be empty".into()),
        });
    }
    let Some(ref auth_mgr) = state.auth_manager else {
        return Json(EmailTokenResult {
            ok: false,
            message: None,
            error: Some("SMTP is not configured on the server".into()),
        });
    };
    let escaped = html_escape(&body.token);
    let html = format!(
        "<p>Your new octos admin token:</p>\
         <pre style=\"font-family:monospace;font-size:14px;padding:12px;background:#f4f4f4;border-radius:6px;word-break:break-all\">{}</pre>\
         <p style=\"color:#666;font-size:12px\">Save this somewhere safe — it won't be shown in the dashboard again.</p>",
        escaped
    );
    match auth_mgr
        .send_html_email(&body.to, "Your octos admin token", &html)
        .await
    {
        Ok(true) => Json(EmailTokenResult {
            ok: true,
            message: Some(format!("token emailed to {}", body.to)),
            error: None,
        }),
        Ok(false) => Json(EmailTokenResult {
            ok: false,
            message: None,
            error: Some("SMTP is not configured on the server".into()),
        }),
        Err(e) => Json(EmailTokenResult {
            ok: false,
            message: None,
            error: Some(e.to_string()),
        }),
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// POST `/api/admin/smtp/test` — send a diagnostic email to the caller.
pub async fn post_smtp_test(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SmtpTestBody>,
) -> Json<SmtpTestResult> {
    if !body.to.contains('@') {
        return Json(SmtpTestResult {
            ok: false,
            message: None,
            error: Some("recipient address must contain '@'".into()),
        });
    }
    let Some(ref auth_mgr) = state.auth_manager else {
        return Json(SmtpTestResult {
            ok: false,
            message: None,
            error: Some(
                "SMTP is not configured on the server (no dashboard_auth.smtp block)".into(),
            ),
        });
    };
    match auth_mgr
        .send_html_email(
            &body.to,
            "octos SMTP test",
            "<p>Your octos SMTP settings are working.</p>",
        )
        .await
    {
        Ok(true) => Json(SmtpTestResult {
            ok: true,
            message: Some(format!("test email sent to {}", body.to)),
            error: None,
        }),
        Ok(false) => Json(SmtpTestResult {
            ok: false,
            message: None,
            error: Some("SMTP not configured on the server".into()),
        }),
        Err(e) => Json(SmtpTestResult {
            ok: false,
            message: None,
            error: Some(e.to_string()),
        }),
    }
}

// ------------------------- Deployment mode -------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct DeploymentModeBody {
    pub mode: String,
}

#[derive(Debug, Serialize)]
pub struct DeploymentModeDetection {
    pub detected: String,
}

fn valid_mode(m: &str) -> bool {
    matches!(m, "local" | "tenant" | "cloud")
}

/// GET `/api/admin/deployment-mode` — read the current mode from `config.json`.
pub async fn get_deployment_mode(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DeploymentModeBody>, (StatusCode, Json<ErrorBody>)> {
    let path = require_config_path(&state)?;
    let mode = if path.exists() {
        let body = std::fs::read_to_string(&path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "read_failed",
                    message: e.to_string(),
                }),
            )
        })?;
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    code: "parse_failed",
                    message: e.to_string(),
                }),
            )
        })?;
        value
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string()
    } else {
        "local".to_string()
    };
    Ok(Json(DeploymentModeBody { mode }))
}

/// POST `/api/admin/deployment-mode` — persist the mode into `config.json`.
pub async fn post_deployment_mode(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DeploymentModeBody>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    if !valid_mode(&body.mode) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                code: "invalid_mode",
                message: "mode must be one of: local, tenant, cloud".into(),
            }),
        ));
    }
    let path = require_config_path(&state)?;
    let mode = body.mode.clone();
    crate::config::write_mutation(&path, move |value| {
        let obj = value
            .as_object_mut()
            .ok_or_else(|| eyre::eyre!("config.json root is not a JSON object"))?;
        obj.insert("mode".into(), serde_json::json!(mode));
        Ok(())
    })
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                code: "save_failed",
                message: e.to_string(),
            }),
        )
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET `/api/admin/deployment-mode/detect` — heuristic detection.
pub async fn get_deployment_mode_detect(
    State(state): State<Arc<AppState>>,
) -> Json<DeploymentModeDetection> {
    let detected = if std::env::var("TUNNEL_DOMAIN").is_ok() {
        "cloud"
    } else {
        let frpc_exists = data_dir_from_state(&state)
            .map(|dir| dir.join("frpc.toml").exists())
            .unwrap_or(false);
        if frpc_exists { "tenant" } else { "local" }
    };
    Json(DeploymentModeDetection {
        detected: detected.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_token_store::AdminTokenStore;
    use crate::api::AppState;
    use crate::setup_state_store::SetupStateStore;

    fn state_with_store(dir: &std::path::Path) -> Arc<AppState> {
        Arc::new(AppState {
            admin_token_store: Arc::new(AdminTokenStore::new(dir)),
            setup_state_store: Arc::new(SetupStateStore::new(dir)),
            ..AppState::empty_for_tests()
        })
    }

    #[tokio::test]
    async fn token_status_is_false_before_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let Json(status) = token_status(State(state)).await;
        assert!(!status.rotated);
    }

    #[tokio::test]
    async fn token_status_is_true_after_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext(
                "Already-Rotated-Token-1234567890-ABCDE",
            ))
            .unwrap();
        let state = state_with_store(dir.path());
        let Json(status) = token_status(State(state)).await;
        assert!(status.rotated);
    }

    #[tokio::test]
    async fn rotate_token_persists_and_returns_204() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let body = RotateBody {
            new_token: "Strong-New-Token-1234567890-ABCDEFGHI".into(),
        };
        let status = rotate_token(State(state.clone()), Json(body))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let record = state.admin_token_store.load().unwrap().unwrap();
        assert!(record.verify("Strong-New-Token-1234567890-ABCDEFGHI"));
    }

    #[tokio::test]
    async fn rotate_token_rejects_short_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let body = RotateBody {
            new_token: "TooShort-1Aa".into(),
        };
        let err = rotate_token(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "weak_token");
    }

    #[tokio::test]
    async fn rotate_token_rejects_low_entropy_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        // 32 chars but only lowercase + digit = 2 classes
        let body = RotateBody {
            new_token: "abcdefghijklmnopqrstuvwxyz012345".into(),
        };
        let err = rotate_token(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "weak_token");
    }

    #[tokio::test]
    async fn get_setup_state_returns_default_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let Json(s) = get_setup_state(State(state)).await.unwrap();
        assert!(s.wizard_completed_at.is_none());
        assert!(!s.wizard_skipped);
        assert_eq!(s.wizard_last_step_reached, 0);
    }

    #[tokio::test]
    async fn post_setup_step_persists_last_step() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let status = post_setup_step(State(state.clone()), Json(StepBody { step: 3 }))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let loaded = state.setup_state_store.load().unwrap();
        assert_eq!(loaded.wizard_last_step_reached, 3);
    }

    #[tokio::test]
    async fn post_setup_step_accepts_boundary_values() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        for step in [0u32, 5u32] {
            let status = post_setup_step(State(state.clone()), Json(StepBody { step }))
                .await
                .unwrap();
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(
                state
                    .setup_state_store
                    .load()
                    .unwrap()
                    .wizard_last_step_reached,
                step
            );
        }
    }

    #[tokio::test]
    async fn post_setup_step_rejects_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let err = post_setup_step(State(state), Json(StepBody { step: 6 }))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "invalid_step");
    }

    #[tokio::test]
    async fn post_setup_complete_marks_completed_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let status = post_setup_complete(State(state.clone())).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let s = state.setup_state_store.load().unwrap();
        assert!(s.wizard_completed_at.is_some());
        assert!(!s.wizard_skipped);
    }

    #[tokio::test]
    async fn post_setup_skip_marks_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());
        let status = post_setup_skip(State(state.clone())).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        let s = state.setup_state_store.load().unwrap();
        assert!(s.wizard_completed_at.is_some());
        assert!(s.wizard_skipped);
    }

    #[tokio::test]
    async fn get_setup_state_round_trips_via_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_store(dir.path());

        post_setup_step(State(state.clone()), Json(StepBody { step: 4 }))
            .await
            .unwrap();
        post_setup_complete(State(state.clone())).await.unwrap();

        let Json(s) = get_setup_state(State(state)).await.unwrap();
        assert_eq!(s.wizard_last_step_reached, 4);
        assert!(s.wizard_completed_at.is_some());
        assert!(!s.wizard_skipped);
    }

    #[tokio::test]
    async fn rotate_token_conflicts_when_already_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext(
                "Already-Rotated-Token-1234567890-ABCDE",
            ))
            .unwrap();
        let state = state_with_store(dir.path());
        let body = RotateBody {
            new_token: "Another-Strong-Token-1234567890-ABCDEFGH".into(),
        };
        let err = rotate_token(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert_eq!(err.1.code, "already_rotated");
    }

    // ----- SMTP endpoints -----

    fn smtp_state(dir: &std::path::Path) -> Arc<AppState> {
        Arc::new(AppState {
            admin_token_store: Arc::new(AdminTokenStore::new(dir)),
            setup_state_store: Arc::new(SetupStateStore::new(dir)),
            config_path: Some(dir.join("config.json")),
            ..AppState::empty_for_tests()
        })
    }

    #[tokio::test]
    async fn get_smtp_returns_defaults_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let Json(s) = get_smtp(State(state)).await.unwrap();
        assert_eq!(s.host, "");
        assert_eq!(s.port, 465);
        assert_eq!(s.username, "");
        assert_eq!(s.from_address, "");
        assert!(!s.password_configured);
    }

    #[tokio::test]
    async fn post_smtp_writes_settings_and_password() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let body = SmtpSettingsBody {
            host: "smtp.example.com".into(),
            port: 587,
            username: "octos".into(),
            from_address: "noreply@example.com".into(),
            password: Some("hunter2".into()),
        };
        let status = post_smtp(State(state.clone()), Json(body)).await.unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);

        let Json(s) = get_smtp(State(state)).await.unwrap();
        assert_eq!(s.host, "smtp.example.com");
        assert_eq!(s.port, 587);
        assert_eq!(s.username, "octos");
        assert_eq!(s.from_address, "noreply@example.com");
        assert!(s.password_configured);

        let pw = SmtpSecretStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(pw, "hunter2");

        // password_env should be backfilled when missing.
        let raw = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed["dashboard_auth"]["smtp"]["password_env"],
            "SMTP_PASSWORD"
        );
    }

    #[tokio::test]
    async fn post_smtp_leaves_password_alone_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        // Pre-populate the password store.
        SmtpSecretStore::new(dir.path())
            .save("pre-existing")
            .unwrap();

        let body = SmtpSettingsBody {
            host: "smtp.example.com".into(),
            port: 587,
            username: "octos".into(),
            from_address: "noreply@example.com".into(),
            password: None,
        };
        post_smtp(State(state.clone()), Json(body)).await.unwrap();

        let pw = SmtpSecretStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(pw, "pre-existing");

        let body2 = SmtpSettingsBody {
            host: "smtp.example.com".into(),
            port: 587,
            username: "octos".into(),
            from_address: "noreply@example.com".into(),
            password: Some(String::new()),
        };
        post_smtp(State(state), Json(body2)).await.unwrap();
        let pw = SmtpSecretStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(pw, "pre-existing");
    }

    #[tokio::test]
    async fn post_smtp_rejects_invalid_host() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let body = SmtpSettingsBody {
            host: "".into(),
            port: 465,
            username: "u".into(),
            from_address: "a@b".into(),
            password: None,
        };
        let err = post_smtp(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "invalid_host");
    }

    #[tokio::test]
    async fn post_smtp_rejects_zero_port() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let body = SmtpSettingsBody {
            host: "smtp.example.com".into(),
            port: 0,
            username: "u".into(),
            from_address: "a@b".into(),
            password: None,
        };
        let err = post_smtp(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "invalid_port");
    }

    #[tokio::test]
    async fn post_smtp_rejects_from_without_at_sign() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let body = SmtpSettingsBody {
            host: "smtp.example.com".into(),
            port: 465,
            username: "u".into(),
            from_address: "not-an-email".into(),
            password: None,
        };
        let err = post_smtp(State(state), Json(body)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "invalid_from_address");
    }

    #[tokio::test]
    async fn post_smtp_preserves_unrelated_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "mode":"tenant",
                "some_other":{"keep":"me"},
                "dashboard_auth":{
                    "smtp":{"password_env":"CUSTOM_PW_ENV","host":"old"},
                    "session_expiry_hours":48
                }
            }"#,
        )
        .unwrap();
        let state = smtp_state(dir.path());

        let body = SmtpSettingsBody {
            host: "smtp.new".into(),
            port: 587,
            username: "u".into(),
            from_address: "a@b.com".into(),
            password: None,
        };
        post_smtp(State(state), Json(body)).await.unwrap();

        let raw = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["mode"], "tenant");
        assert_eq!(parsed["some_other"]["keep"], "me");
        assert_eq!(parsed["dashboard_auth"]["session_expiry_hours"], 48);
        assert_eq!(parsed["dashboard_auth"]["smtp"]["host"], "smtp.new");
        // password_env should remain untouched (backcompat).
        assert_eq!(
            parsed["dashboard_auth"]["smtp"]["password_env"],
            "CUSTOM_PW_ENV"
        );
    }

    #[tokio::test]
    async fn post_smtp_test_rejects_invalid_recipient() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let Json(result) =
            post_smtp_test(State(state), Json(SmtpTestBody { to: "bad".into() })).await;
        assert!(!result.ok);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn post_smtp_test_reports_missing_auth_manager() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let Json(result) = post_smtp_test(
            State(state),
            Json(SmtpTestBody {
                to: "x@example.com".into(),
            }),
        )
        .await;
        assert!(!result.ok);
        assert!(result.error.is_some());
    }

    // ----- Deployment-mode endpoints -----

    #[tokio::test]
    async fn get_deployment_mode_defaults_to_local() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let Json(body) = get_deployment_mode(State(state)).await.unwrap();
        assert_eq!(body.mode, "local");
    }

    #[tokio::test]
    async fn post_deployment_mode_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        for mode in ["local", "tenant", "cloud"] {
            let status = post_deployment_mode(
                State(state.clone()),
                Json(DeploymentModeBody {
                    mode: mode.to_string(),
                }),
            )
            .await
            .unwrap();
            assert_eq!(status, StatusCode::NO_CONTENT);
            let Json(body) = get_deployment_mode(State(state.clone())).await.unwrap();
            assert_eq!(body.mode, mode);
        }
    }

    #[tokio::test]
    async fn post_deployment_mode_rejects_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let state = smtp_state(dir.path());
        let err = post_deployment_mode(
            State(state),
            Json(DeploymentModeBody {
                mode: "mainframe".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.code, "invalid_mode");
    }

    #[tokio::test]
    async fn detect_deployment_mode_returns_tenant_when_frpc_exists() {
        // TUNNEL_DOMAIN must not be set for this test.
        // SAFETY: test-only manipulation; Rust test runners may run tests in
        // parallel, but this env var isn't consulted elsewhere in this file.
        let prev = std::env::var("TUNNEL_DOMAIN").ok();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TUNNEL_DOMAIN");
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("frpc.toml"), b"[common]\n").unwrap();
        let state = smtp_state(dir.path());
        let Json(body) = get_deployment_mode_detect(State(state)).await;
        assert_eq!(body.detected, "tenant");

        // Restore.
        if let Some(v) = prev {
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var("TUNNEL_DOMAIN", v);
            }
        }
    }
}
