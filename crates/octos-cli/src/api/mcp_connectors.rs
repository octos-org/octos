//! Profile-scoped MCP connector management (UPCR-2026-062, PR1).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use octos_agent::McpServerConfig;
use octos_core::{SessionKey, ui_protocol::RpcError};
use serde_json::{Value, json};

use crate::api::admin_audit::record_admin_action;
use crate::api::ui_protocol_transport::{
    commit_profile_llm_runtime_transition, mcp_status_list_result, raw_scoped_llm_profile_id,
    validate_session_scope,
};
use crate::api::{AppState, router::AuthIdentity};
use crate::profiles::{ProfileStore, UserProfile};

pub const MCP_MAX_CONNECTORS_PER_PROFILE: usize = 32;
pub const MCP_RECONNECT_MIN_INTERVAL: Duration = Duration::from_secs(5);

fn mutation_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn reconnect_times() -> &'static std::sync::Mutex<HashMap<String, Instant>> {
    static TIMES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Instant>>> =
        std::sync::OnceLock::new();
    TIMES.get_or_init(Default::default)
}

fn required_string(value: &Value, key: &str) -> Result<String, RpcError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| RpcError::invalid_params(format!("{key} is required")))
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn session_id(params: &Value) -> Result<SessionKey, RpcError> {
    Ok(SessionKey(required_string(params, "session_id")?))
}

fn connector_id(params: &Value) -> Result<String, RpcError> {
    required_string(params, "connector_id")
}

fn string_array(value: Option<&Value>, key: &str) -> Result<Vec<String>, RpcError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| RpcError::invalid_params(format!("{key} must contain strings")))
            })
            .collect(),
        _ => Err(RpcError::invalid_params(format!(
            "{key} must be an array of strings"
        ))),
    }
}

fn concurrency_class(value: Option<&Value>) -> Result<Option<String>, RpcError> {
    let Some(value) = value.and_then(Value::as_str) else {
        return Ok(None);
    };
    match value {
        "safe" | "exclusive" => Ok(Some(value.to_string())),
        _ => Err(RpcError::invalid_params(
            "concurrency_class must be safe or exclusive",
        )),
    }
}

fn validate_http_url(url: &str) -> Result<(), RpcError> {
    let parsed = url::Url::parse(url)
        .map_err(|_| RpcError::invalid_params("url must be a valid MCP HTTP endpoint"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(RpcError::invalid_params(
            "url must be an http(s) MCP endpoint",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RpcError::invalid_params(
            "URL userinfo is not accepted; use OAuth instead",
        ));
    }
    if octos_agent::tools::ssrf::is_private_host(parsed.host_str().unwrap_or_default()) {
        return Err(RpcError::invalid_params(
            "private or loopback MCP endpoints are refused",
        ));
    }
    Ok(())
}

fn connector_summary(config: &McpServerConfig) -> Value {
    json!({
        "id": config.connector_id(),
        "name": config.display_name(),
        "transport": config.transport_name(),
        "enabled": config.enabled,
        "url": config.url.as_deref().map(|url| octos_core::secret_redaction::redact_secrets_in_text(url).into_owned()),
        "allowlist_entry_id": config.allowlist_entry_id,
        "oauth": config.oauth,
        "scopes": config.scopes,
        "concurrency_class": config.concurrency_class,
    })
}

fn failure_summary(error: &RpcError) -> Value {
    json!({
        "result": "error",
        "kind": error.data.as_ref().and_then(|data| data.get("kind")).cloned().unwrap_or(Value::Null),
    })
}

fn audit_result(
    state: &AppState,
    identity: Option<&AuthIdentity>,
    action: &str,
    target_id: String,
    before: Option<Value>,
    result: Result<Value, RpcError>,
) -> Result<Value, RpcError> {
    let after = match &result {
        Ok(value) => Some(value.clone()),
        Err(error) => Some(failure_summary(error)),
    };
    record_admin_action(state, identity, action, target_id, before, after).map_err(|error| {
        RpcError::internal_error(format!("failed to record MCP audit: {error}"))
    })?;
    result
}

struct MutationContext {
    profile_id: String,
    store: Arc<ProfileStore>,
    profile: UserProfile,
}

fn mutation_context(
    state: &Arc<AppState>,
    session_id: &SessionKey,
    connection_profile_id: Option<&str>,
) -> Result<MutationContext, RpcError> {
    let profile_id = raw_scoped_llm_profile_id(None, Some(session_id), connection_profile_id)?;
    validate_session_scope(session_id, Some(&profile_id), connection_profile_id)?;
    let store = state
        .profile_store
        .clone()
        .ok_or_else(|| RpcError::internal_error("profile store unavailable"))?;
    let profile = store
        .get(&profile_id)
        .map_err(|error| RpcError::internal_error(format!("failed to read profile: {error}")))?
        .ok_or_else(|| RpcError::not_found("profile", &profile_id))?;
    Ok(MutationContext {
        profile_id,
        store,
        profile,
    })
}

fn find_connector<'a>(profile: &'a UserProfile, id: &str) -> Result<usize, RpcError> {
    profile
        .config
        .mcp_servers
        .iter()
        .position(|config| config.connector_id() == id)
        .ok_or_else(|| RpcError::not_found("mcp_connector", id))
}

fn assign_legacy_ids(profile: &mut UserProfile) {
    let mut used: std::collections::HashSet<String> = profile
        .config
        .mcp_servers
        .iter()
        .filter_map(|config| config.id.clone())
        .collect();
    for config in &mut profile.config.mcp_servers {
        if config.id.is_none() {
            let mut id = format!("mcp-{}", uuid::Uuid::new_v4().simple());
            while !used.insert(id.clone()) {
                id = format!("mcp-{}", uuid::Uuid::new_v4().simple());
            }
            config.id = Some(id);
        }
    }
}

fn stdio_config(
    state: &AppState,
    entry_id: &str,
    id: String,
    name: String,
    scopes: Vec<String>,
    concurrency: Option<String>,
) -> Result<McpServerConfig, RpcError> {
    let entry = state
        .mcp_stdio_allowlist
        .iter()
        .find(|entry| entry.id == entry_id)
        .ok_or_else(|| {
            RpcError::permission_denied("stdio connector is not in the deployment allowlist")
                .with_data(json!({"kind": "stdio_not_allowlisted", "entry_id": entry_id}))
        })?;
    if !std::path::Path::new(&entry.command).is_absolute() {
        return Err(RpcError::invalid_params(
            "allowlist command must be an absolute executable path",
        ));
    }
    Ok(McpServerConfig {
        id: Some(id),
        name: Some(name),
        enabled: true,
        allowlist_entry_id: Some(entry.id.clone()),
        command: Some(entry.command.clone()),
        args: entry.args.clone(),
        env: entry.env.clone(),
        url: None,
        headers: HashMap::new(),
        oauth: false,
        scopes,
        concurrency_class: Some(concurrency.unwrap_or_else(|| "exclusive".into())),
    })
}

fn http_config(
    id: String,
    name: String,
    url: String,
    scopes: Vec<String>,
    concurrency: Option<String>,
) -> Result<McpServerConfig, RpcError> {
    validate_http_url(&url)?;
    Ok(McpServerConfig {
        id: Some(id),
        name: Some(name),
        enabled: true,
        allowlist_entry_id: None,
        command: None,
        args: Vec::new(),
        env: HashMap::new(),
        url: Some(url),
        headers: HashMap::new(),
        oauth: false,
        scopes,
        concurrency_class: concurrency,
    })
}

async fn finish_mutation(
    state: &Arc<AppState>,
    context: &MutationContext,
    profile: UserProfile,
    session_id: &SessionKey,
    connector: Option<Value>,
    extra: Value,
) -> Result<Value, RpcError> {
    context
        .store
        .save_with_merge(&mut profile.clone())
        .map_err(|error| RpcError::internal_error(format!("failed to save profile: {error}")))?;
    let transition = commit_profile_llm_runtime_transition(
        state,
        &context.profile_id,
        Some(profile.updated_at.to_rfc3339()),
    )
    .await;
    let status = mcp_status_list_result(state, session_id, Some(&context.profile_id));
    let mut result = json!({
        "status": status["servers"].as_array().and_then(|servers| {
            connector.as_ref().and_then(|connector| {
                let id = connector.get("id").and_then(Value::as_str);
                servers.iter().find(|server| server.get("id").and_then(Value::as_str) == id).cloned()
            })
        }),
        "runtime": transition.wire_status(),
    });
    if let Some(connector) = connector {
        result["connector"] = connector;
    }
    if let Some(object) = extra.as_object() {
        for (key, value) in object {
            result[key] = value.clone();
        }
    }
    Ok(result)
}

pub async fn create(
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    params: Value,
) -> Result<Value, RpcError> {
    let session_id = session_id(&params)?;
    let context = mutation_context(state, &session_id, connection_profile_id)?;
    let target = format!("mcp:{}:create", context.profile_id);
    let result = async {
        let _guard = mutation_lock().lock().await;
        let mut profile = context.profile.clone();
        assign_legacy_ids(&mut profile);
        if profile.config.mcp_servers.len() >= MCP_MAX_CONNECTORS_PER_PROFILE {
            return Err(
                RpcError::invalid_params("profile connector limit reached").with_data(
                    json!({"kind": "connector_limit", "limit": MCP_MAX_CONNECTORS_PER_PROFILE}),
                ),
            );
        }
        let input = params
            .get("connector")
            .ok_or_else(|| RpcError::invalid_params("connector is required"))?;
        let transport = required_string(input, "transport")?;
        let name = required_string(input, "name")?;
        let scopes = string_array(input.get("scopes"), "scopes")?;
        let concurrency = concurrency_class(input.get("concurrency_class"))?;
        let id = format!("mcp-{}", uuid::Uuid::new_v4().simple());
        let config = match transport.as_str() {
            "stdio" => stdio_config(
                state,
                &required_string(input, "allowlist_entry_id")?,
                id,
                name,
                scopes,
                concurrency,
            )?,
            "http" => http_config(
                id,
                name,
                required_string(input, "url")?,
                scopes,
                concurrency,
            )?,
            _ => return Err(RpcError::invalid_params("transport must be stdio or http")),
        };
        let summary = connector_summary(&config);
        profile.config.mcp_servers.push(config);
        profile.updated_at = chrono::Utc::now();
        finish_mutation(
            state,
            &context,
            profile,
            &session_id,
            Some(summary),
            json!({"created": true}),
        )
        .await
    }
    .await;
    audit_result(
        state,
        identity,
        "mcp.connector.create",
        target,
        None,
        result,
    )
}

pub async fn update(
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    params: Value,
) -> Result<Value, RpcError> {
    let session_id = session_id(&params)?;
    let id = connector_id(&params)?;
    let context = mutation_context(state, &session_id, connection_profile_id)?;
    let target = format!("mcp:{}:{id}", context.profile_id);
    let result = async {
        let _guard = mutation_lock().lock().await;
        let mut profile = context.profile.clone();
        assign_legacy_ids(&mut profile);
        let index = find_connector(&profile, &id)?;
        let before = connector_summary(&profile.config.mcp_servers[index]);
        let patch = params
            .get("patch")
            .ok_or_else(|| RpcError::invalid_params("patch is required"))?;
        let mut config = profile.config.mcp_servers[index].clone();
        if let Some(name) = optional_string(patch, "name") {
            config.name = Some(name);
        }
        if let Some(scopes) = patch.get("scopes") {
            config.scopes = string_array(Some(scopes), "scopes")?;
        }
        if let Some(concurrency) = patch.get("concurrency_class") {
            config.concurrency_class = concurrency_class(Some(concurrency))?;
        }
        if config.url.is_some() {
            if let Some(url) = optional_string(patch, "url") {
                validate_http_url(&url)?;
                config.url = Some(url);
            }
        } else if let Some(entry_id) = optional_string(patch, "allowlist_entry_id") {
            config = stdio_config(
                state,
                &entry_id,
                config.connector_id(),
                config.display_name().to_string(),
                config.scopes.clone(),
                config.concurrency_class.clone(),
            )?;
            config.enabled = profile.config.mcp_servers[index].enabled;
        }
        let summary = connector_summary(&config);
        profile.config.mcp_servers[index] = config;
        profile.updated_at = chrono::Utc::now();
        finish_mutation(
            state,
            &context,
            profile,
            &session_id,
            Some(summary),
            json!({"updated": true, "before": before}),
        )
        .await
    }
    .await;
    audit_result(
        state,
        identity,
        "mcp.connector.update",
        target,
        None,
        result,
    )
}

pub async fn delete(
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    params: Value,
) -> Result<Value, RpcError> {
    let session_id = session_id(&params)?;
    let id = connector_id(&params)?;
    let context = mutation_context(state, &session_id, connection_profile_id)?;
    let target = format!("mcp:{}:{id}", context.profile_id);
    let result = async {
        if params.get("revoke_oauth").and_then(Value::as_bool) == Some(true) {
            return Err(RpcError::invalid_params(
                "OAuth revoke requires mcp.oauth.v1",
            ));
        }
        let _guard = mutation_lock().lock().await;
        let mut profile = context.profile.clone();
        assign_legacy_ids(&mut profile);
        let before = profile
            .config
            .mcp_servers
            .iter()
            .find(|config| config.connector_id() == id)
            .map(connector_summary);
        profile
            .config
            .mcp_servers
            .retain(|config| config.connector_id() != id);
        profile.updated_at = chrono::Utc::now();
        finish_mutation(
            state,
            &context,
            profile,
            &session_id,
            None,
            json!({"deleted": true, "before": before}),
        )
        .await
    }
    .await;
    audit_result(
        state,
        identity,
        "mcp.connector.delete",
        target,
        None,
        result,
    )
}

pub async fn set_enabled(
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    params: Value,
    enabled: bool,
) -> Result<Value, RpcError> {
    let session_id = session_id(&params)?;
    let id = connector_id(&params)?;
    let context = mutation_context(state, &session_id, connection_profile_id)?;
    let target = format!("mcp:{}:{id}", context.profile_id);
    let result = async {
        let _guard = mutation_lock().lock().await;
        let mut profile = context.profile.clone();
        assign_legacy_ids(&mut profile);
        let index = find_connector(&profile, &id)?;
        profile.config.mcp_servers[index].enabled = enabled;
        let summary = connector_summary(&profile.config.mcp_servers[index]);
        profile.updated_at = chrono::Utc::now();
        finish_mutation(
            state,
            &context,
            profile,
            &session_id,
            Some(summary),
            json!({"enabled": enabled}),
        )
        .await
    }
    .await;
    audit_result(
        state,
        identity,
        "mcp.connector.set_enabled",
        target,
        None,
        result,
    )
}

pub async fn reconnect(
    state: &Arc<AppState>,
    identity: Option<&AuthIdentity>,
    connection_profile_id: Option<&str>,
    params: Value,
) -> Result<Value, RpcError> {
    let session_id = session_id(&params)?;
    let id = connector_id(&params)?;
    let context = mutation_context(state, &session_id, connection_profile_id)?;
    let target = format!("mcp:{}:{id}", context.profile_id);
    let result = async {
        let key = format!("{}:{id}", context.profile_id);
        {
            let mut times = reconnect_times().lock().unwrap_or_else(|error| error.into_inner());
            if times.get(&key).is_some_and(|last| last.elapsed() < MCP_RECONNECT_MIN_INTERVAL) {
                return Err(RpcError::invalid_params("reconnect rate limit")
                    .with_data(json!({"kind": "reconnect_rate_limited", "retry_after_seconds": MCP_RECONNECT_MIN_INTERVAL.as_secs()})));
            }
            times.insert(key, Instant::now());
        }
        let _guard = mutation_lock().lock().await;
        let mut profile = context.profile.clone();
        assign_legacy_ids(&mut profile);
        let index = find_connector(&profile, &id)?;
        if !profile.config.mcp_servers[index].enabled {
            return Err(RpcError::invalid_params("disabled connector cannot reconnect")
                .with_data(json!({"kind": "connector_disabled"})));
        }
        let summary = connector_summary(&profile.config.mcp_servers[index]);
        profile.updated_at = chrono::Utc::now();
        finish_mutation(state, &context, profile, &session_id, Some(summary), json!({"reconnected": true})).await
    }
    .await;
    audit_result(
        state,
        identity,
        "mcp.connector.reconnect",
        target,
        None,
        result,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_summary_never_contains_stdio_args_or_http_headers() {
        let config = McpServerConfig {
            id: Some("mcp-1".into()),
            name: Some("secret-test".into()),
            enabled: true,
            allowlist_entry_id: Some("entry".into()),
            command: Some("/bin/echo".into()),
            args: vec!["secret-arg".into()],
            env: HashMap::from([("TOKEN".into(), "secret-env".into())]),
            url: None,
            headers: HashMap::from([("Authorization".into(), "secret-header".into())]),
            oauth: false,
            scopes: vec![],
            concurrency_class: Some("exclusive".into()),
        };
        let rendered = connector_summary(&config).to_string();
        assert!(!rendered.contains("secret-arg"));
        assert!(!rendered.contains("secret-env"));
        assert!(!rendered.contains("secret-header"));
    }

    #[test]
    fn http_url_rejects_userinfo_and_private_hosts() {
        assert!(validate_http_url("https://user:pass@example.com/mcp").is_err());
        assert!(validate_http_url("http://127.0.0.1:8080/mcp").is_err());
        assert!(validate_http_url("https://example.com/mcp").is_ok());
    }

    fn test_profile(id: &str) -> UserProfile {
        UserProfile {
            id: id.into(),
            name: id.into(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: crate::profiles::ProfileConfig::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn test_state() -> (tempfile::TempDir, Arc<AppState>, Arc<ProfileStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ProfileStore::open_unified(dir.path()).unwrap());
        let state = Arc::new(AppState {
            profile_store: Some(store.clone()),
            ..AppState::empty_for_tests()
        });
        (dir, state, store)
    }

    fn identity() -> AuthIdentity {
        AuthIdentity::User {
            id: "tenant".into(),
            role: crate::user_store::UserRole::User,
        }
    }

    #[tokio::test]
    async fn http_connector_lifecycle_is_profile_scoped_and_idempotent() {
        let (_dir, state, store) = test_state();
        store.save(&test_profile("tenant")).unwrap();
        let identity = identity();
        let created = create(
            &state,
            Some(&identity),
            Some("tenant"),
            json!({
                "session_id": "tenant:api:demo",
                "connector": {"transport": "http", "name": "Example", "url": "https://example.com/mcp"}
            }),
        )
        .await
        .unwrap();
        let id = created["connector"]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("mcp-"));
        let disabled = set_enabled(
            &state,
            Some(&identity),
            Some("tenant"),
            json!({"session_id": "tenant:api:demo", "connector_id": id.clone(), "enabled": false}),
            false,
        )
        .await
        .unwrap();
        assert_eq!(disabled["connector"]["enabled"], false);
        for _ in 0..2 {
            let deleted = delete(
                &state,
                Some(&identity),
                Some("tenant"),
                json!({"session_id": "tenant:api:demo", "connector_id": id.clone()}),
            )
            .await
            .unwrap();
            assert_eq!(deleted["deleted"], true);
        }
        assert!(
            store
                .get("tenant")
                .unwrap()
                .unwrap()
                .config
                .mcp_servers
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cross_profile_mutation_is_refused_before_persistence() {
        let (_dir, state, store) = test_state();
        store.save(&test_profile("tenant")).unwrap();
        store.save(&test_profile("other")).unwrap();
        let error = create(
            &state,
            Some(&identity()),
            Some("tenant"),
            json!({
                "session_id": "other:api:demo",
                "connector": {"transport": "http", "name": "Bad", "url": "https://example.com/mcp"}
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.data.unwrap()["kind"], "auth_scope_violation");
        assert!(
            store
                .get("other")
                .unwrap()
                .unwrap()
                .config
                .mcp_servers
                .is_empty()
        );
    }

    #[tokio::test]
    async fn stdio_create_requires_operator_allowlist_entry() {
        let (_dir, _state, store) = test_state();
        store.save(&test_profile("tenant")).unwrap();
        let state = Arc::new(AppState {
            profile_store: Some(store.clone()),
            mcp_stdio_allowlist: vec![crate::config::McpStdioAllowlistEntry {
                id: "echo".into(),
                command: "/bin/echo".into(),
                args: vec!["fixed".into()],
                env: HashMap::from([("SECRET".into(), "do-not-echo".into())]),
                concurrency_class: None,
            }],
            ..AppState::empty_for_tests()
        });
        let created = create(
            &state,
            Some(&identity()),
            Some("tenant"),
            json!({
                "session_id": "tenant:api:demo",
                "connector": {"transport": "stdio", "name": "Echo", "allowlist_entry_id": "echo"}
            }),
        )
        .await
        .unwrap();
        assert_eq!(created["connector"]["concurrency_class"], "exclusive");
        assert!(!created.to_string().contains("do-not-echo"));
        let error = create(
            &state,
            Some(&identity()),
            Some("tenant"),
            json!({
                "session_id": "tenant:api:demo",
                "connector": {"transport": "stdio", "name": "Bad", "allowlist_entry_id": "missing"}
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.data.unwrap()["kind"], "stdio_not_allowlisted");
    }
}
