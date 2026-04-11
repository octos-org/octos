//! Gateway command: run as a persistent messaging daemon.

mod account_handler;
mod adapters;
mod gateway_runtime;
#[cfg(feature = "matrix")]
mod matrix_integration;
mod message_preprocessing;
mod profile_factory;
mod prompt;
pub(crate) mod session_ui;
mod skills_handler;

use std::path::PathBuf;

use clap::Args;
use eyre::{Result, WrapErr};
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use tracing::warn;

use super::Executable;

// Re-exports used by submodules (prompt, gateway_runtime)
#[cfg(feature = "matrix")]
use matrix_integration::*;
pub(crate) use prompt::build_system_prompt;

// Types used by tests via `use super::*`
#[cfg(test)]
use {
    crate::session_actor::SnapshotToolRegistryFactory,
    octos_agent::{AgentConfig, ToolRegistry},
    octos_bus::{ActiveSessionStore, ChannelManager, CronService, SessionManager},
    profile_factory::ProfileActorFactoryBuilder,
    std::sync::Arc,
    std::sync::atomic::{AtomicBool, AtomicUsize},
};

/// Run as a persistent gateway daemon.
#[derive(Debug, Args)]
pub struct GatewayCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long, conflicts_with = "profile")]
    pub config: Option<PathBuf>,

    /// Path to a profile JSON file (used by managed gateways).
    #[arg(long, conflicts_with = "config")]
    pub profile: Option<PathBuf>,

    /// Override WhatsApp bridge URL (used by managed gateways).
    #[arg(long, hide = true)]
    pub bridge_url: Option<String>,

    /// Internal: managed WeChat bridge WebSocket URL.
    #[arg(long, hide = true)]
    pub wechat_bridge_url: Option<String>,

    /// Override Feishu webhook port (used by managed gateways).
    #[arg(long, hide = true)]
    pub feishu_port: Option<u16>,

    /// Override API channel port (used by managed gateways).
    #[arg(long, hide = true)]
    pub api_port: Option<u16>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Maximum agent iterations per message (default: 50).
    #[arg(long)]
    pub max_iterations: Option<u32>,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,

    /// Path to parent profile JSON (sub-accounts inherit provider config).
    #[arg(long, hide = true)]
    pub parent_profile: Option<PathBuf>,

    /// Octos home directory for ProfileStore access (used by managed gateways).
    #[arg(long, hide = true)]
    pub octos_home: Option<PathBuf>,
}

fn resolve_dispatch_profile_id(
    current_gateway_profile_id: Option<&str>,
    target_profile_id: Option<&str>,
    profile_store: Option<&crate::profiles::ProfileStore>,
) -> Result<Option<String>> {
    let Some(profile_id) = target_profile_id.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    if current_gateway_profile_id.is_some_and(|current| current == profile_id) {
        return Ok(Some(profile_id.to_string()));
    }

    let Some(store) = profile_store else {
        warn!(
            profile_id = %profile_id,
            "profile store unavailable; routing target profile to main profile"
        );
        return Ok(None);
    };

    match store.get(profile_id) {
        Ok(Some(_)) => Ok(Some(profile_id.to_string())),
        Ok(None) => {
            warn!(
                profile_id = %profile_id,
                "target profile not found; routing message to main profile"
            );
            Ok(None)
        }
        Err(error) => {
            warn!(
                profile_id = %profile_id,
                %error,
                "failed to load target profile; routing message to main profile"
            );
            Ok(None)
        }
    }
}

pub(crate) fn build_profiled_session_key(
    profile_id: Option<&str>,
    channel: &str,
    chat_id: &str,
    topic: &str,
) -> SessionKey {
    let effective_profile_id = profile_id.unwrap_or(MAIN_PROFILE_ID);
    SessionKey::with_profile_topic(effective_profile_id, channel, chat_id, topic)
}

impl Executable for GatewayCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl GatewayCommand {
    async fn run_async(self) -> Result<()> {
        let runtime = gateway_runtime::GatewayRuntime::init(self).await?;
        runtime.run().await
    }
}

#[cfg(all(test, feature = "matrix"))]
mod tests {
    use super::*;
    use chrono::Utc;
    use octos_agent::ToolConfigStore;
    use octos_bus::BotManager;
    use octos_memory::{EpisodeStore, MemoryStore};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tokio::sync::{Mutex, RwLock, mpsc};

    fn make_profile(id: &str, system_prompt: Option<&str>) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            enabled: false,
            data_dir: None,
            parent_id: None,
            config: crate::profiles::ProfileConfig {
                gateway: crate::profiles::GatewaySettings {
                    system_prompt: system_prompt.map(str::to_string),
                    ..Default::default()
                },
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_child_bot_from_admin_parent_gets_normal_tooling() {
        let dir = tempfile::TempDir::new().unwrap();
        let project_dir = dir.path().join("octos-home");
        std::fs::create_dir_all(&project_dir).unwrap();
        let _ = octos_agent::bootstrap::bootstrap_bundled_skills(&project_dir);

        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());

        let mut parent = make_profile("botfather", Some("admin parent"));
        parent.config.provider = Some("openai".into());
        parent.config.model = Some("gpt-4o-mini".into());
        parent.config.api_key_env = Some("OPENAI_API_KEY".into());
        parent.config.fallback_models = vec![crate::profiles::FallbackModelConfig {
            provider: "openai".into(),
            model: Some("gpt-4o".into()),
            api_key_env: Some("OPENAI_API_KEY".into()),
            ..Default::default()
        }];
        parent.config.admin_mode = true;
        store.save(&parent).unwrap();

        let mut child = make_profile("botfather--researcher", Some("child prompt"));
        child.parent_id = Some(parent.id.clone());
        store.save(&child).unwrap();

        let base_data_dir = dir.path().join("data");
        std::fs::create_dir_all(&base_data_dir).unwrap();
        let tool_config = Arc::new(ToolConfigStore::open(&base_data_dir).await.unwrap());
        let memory = Arc::new(EpisodeStore::open(&base_data_dir).await.unwrap());
        let memory_store = Arc::new(MemoryStore::open(&base_data_dir).await.unwrap());
        let session_mgr = Arc::new(Mutex::new(SessionManager::open(&base_data_dir).unwrap()));
        let active_sessions = Arc::new(RwLock::new(
            ActiveSessionStore::open(&base_data_dir).unwrap(),
        ));
        let pending_messages: crate::session_actor::PendingMessages =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (out_tx, _out_rx) = mpsc::channel(4);
        let (spawn_inbound_tx, _spawn_inbound_rx) = mpsc::channel(4);
        let (cron_in_tx, _cron_in_rx) = mpsc::channel(1);
        let cron_service = Arc::new(CronService::new(base_data_dir.join("cron"), cron_in_tx));

        let builder = ProfileActorFactoryBuilder {
            profile_store: store,
            project_dir: project_dir.clone(),
            tool_config,
            memory,
            memory_store,
            agent_config: AgentConfig::default(),
            session_mgr,
            out_tx,
            spawn_inbound_tx,
            cron_service,
            tool_registry_factory: Arc::new(SnapshotToolRegistryFactory::new(ToolRegistry::new())),
            pipeline_factory: None,
            max_history: Arc::new(AtomicUsize::new(50)),
            session_timeout_secs: octos_agent::DEFAULT_SESSION_TIMEOUT_SECS,
            shutdown: Arc::new(AtomicBool::new(false)),
            cwd: project_dir.clone(),
            provider_policy: None,
            worker_prompt: None,
            provider_router: None,
            active_sessions,
            pending_messages,
            queue_mode: crate::config::QueueMode::Followup,
            plugin_prompt_fragments: vec![],
            no_retry: false,
            sandbox_config: octos_agent::SandboxConfig::default(),
        };

        let factory = builder.build("botfather--researcher").await.unwrap();
        let registry = factory.tool_registry_factory.create_base_registry();
        let expected_data_dir = dir
            .path()
            .join("profiles")
            .join("botfather--researcher")
            .join("data");

        assert!(
            registry.get("web_search").is_some(),
            "child bot should expose normal-mode web_search"
        );
        assert!(
            registry.get("deep_search").is_some(),
            "child bot should expose bundled deep_search skill"
        );
        assert!(
            registry.get("synthesize_research").is_some(),
            "child bot should expose research synthesis tooling"
        );
        assert!(
            factory.pipeline_factory.is_some(),
            "child bot should build its own pipeline factory instead of inheriting admin-only None"
        );
        assert!(
            factory.provider_router.is_some(),
            "child bot should build a provider router for fallback-aware spawn/pipeline"
        );
        assert_eq!(
            factory.data_dir, expected_data_dir,
            "child bot should use its own data dir for sessions/status"
        );
    }

    fn matrix_entry(settings: serde_json::Value) -> crate::config::ChannelEntry {
        crate::config::ChannelEntry {
            channel_type: MATRIX_CHANNEL_TYPE.to_string(),
            allowed_senders: Vec::new(),
            settings,
        }
    }

    #[test]
    fn matrix_channel_settings_use_defaults() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
        }));

        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(settings.homeserver, MATRIX_DEFAULT_HOMESERVER);
        assert_eq!(settings.server_name, MATRIX_DEFAULT_SERVER_NAME);
        assert_eq!(settings.sender_localpart, MATRIX_DEFAULT_SENDER_LOCALPART);
        assert_eq!(settings.user_prefix, MATRIX_DEFAULT_USER_PREFIX);
        assert_eq!(settings.port, MATRIX_DEFAULT_PORT);
        assert!(settings.allowed_senders.is_empty());
    }

    #[test]
    fn matrix_channel_settings_copy_allowed_senders() {
        let entry = crate::config::ChannelEntry {
            channel_type: MATRIX_CHANNEL_TYPE.to_string(),
            allowed_senders: vec!["@alice:localhost".into(), "@bob:localhost".into()],
            settings: serde_json::json!({
                MATRIX_SETTING_AS_TOKEN: "as-token",
                MATRIX_SETTING_HS_TOKEN: "hs-token",
            }),
        };

        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(
            settings.allowed_senders,
            vec!["@alice:localhost".to_string(), "@bob:localhost".to_string()]
        );
    }

    #[test]
    fn matrix_channel_settings_require_tokens() {
        let entry = matrix_entry(serde_json::json!({}));

        let err = MatrixChannelSettings::from_entry(&entry).unwrap_err();

        assert!(err.to_string().contains(MATRIX_MISSING_TOKENS_ERROR));
    }

    #[test]
    fn matrix_channel_settings_reject_out_of_range_port() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
            "port": 70000,
        }));

        let err = MatrixChannelSettings::from_entry(&entry).unwrap_err();

        assert!(err.to_string().contains("port"));
    }

    #[test]
    fn test_gateway_registers_matrix_channel() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
        }));
        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let data_dir = tempfile::TempDir::new().unwrap();
        let mut channel_mgr = ChannelManager::new();
        let mut matrix_channel = None;

        let channel = register_matrix_channel(
            &mut channel_mgr,
            &mut matrix_channel,
            &settings,
            &shutdown,
            data_dir.path(),
        );

        assert!(channel_mgr.get_channel(MATRIX_CHANNEL_TYPE).is_some());
        assert!(matrix_channel.is_some());
        assert!(Arc::ptr_eq(
            &channel,
            matrix_channel
                .as_ref()
                .expect("matrix channel should be cached")
        ));
    }

    #[test]
    fn test_dispatch_unknown_profile_falls_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::profiles::ProfileStore::open(dir.path()).unwrap();
        store
            .save(&make_profile("weather", Some("weather prompt")))
            .unwrap();

        let resolved =
            resolve_dispatch_profile_id(Some("weather"), Some("missing-profile"), Some(&store))
                .unwrap();

        assert_eq!(resolved, None);
    }

    #[test]
    fn test_dispatch_known_profile_keeps_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::profiles::ProfileStore::open(dir.path()).unwrap();
        store
            .save(&make_profile("weather", Some("weather prompt")))
            .unwrap();

        let resolved =
            resolve_dispatch_profile_id(Some("botfather"), Some("weather"), Some(&store)).unwrap();

        assert_eq!(resolved.as_deref(), Some("weather"));
    }

    #[test]
    fn test_dispatch_current_gateway_profile_keeps_target_without_lookup() {
        let resolved =
            resolve_dispatch_profile_id(Some("dspfac--newsbot"), Some("dspfac--newsbot"), None)
                .unwrap();

        assert_eq!(resolved.as_deref(), Some("dspfac--newsbot"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_delete_bot_keeps_route_when_profile_delete_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());
        let mut parent = make_profile("botfather", None);
        parent
            .config
            .channels
            .push(crate::profiles::ChannelCredentials::Matrix {
                homeserver: "http://localhost:6167".to_string(),
                as_token: "as-token".to_string(),
                hs_token: "hs-token".to_string(),
                server_name: "localhost".to_string(),
                sender_localpart: "bot".to_string(),
                user_prefix: "bot_".to_string(),
                port: MATRIX_DEFAULT_PORT,
                allowed_senders: vec![],
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        store.save(&sub).unwrap();

        let channel = Arc::new(
            octos_bus::MatrixChannel::new(
                "http://localhost:6167",
                "as-token",
                "hs-token",
                "localhost",
                "bot",
                "bot_",
                6166,
                Arc::new(AtomicBool::new(false)),
            )
            .with_bot_router(dir.path()),
        );
        channel
            .bot_router()
            .register_entry(
                "@bot_weatherbot:localhost",
                &sub.id,
                "@alice:localhost",
                octos_bus::BotVisibility::Private,
            )
            .await
            .unwrap();

        let profiles_dir = dir.path().join("profiles");
        let original_mode = std::fs::metadata(&profiles_dir)
            .unwrap()
            .permissions()
            .mode();
        let mut perms = std::fs::metadata(&profiles_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&profiles_dir, perms).unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@alice:localhost")
            .await;

        let mut restore = std::fs::metadata(&profiles_dir).unwrap().permissions();
        restore.set_mode(original_mode);
        std::fs::set_permissions(&profiles_dir, restore).unwrap();

        assert!(
            result.is_err(),
            "delete should fail when profile cannot be removed"
        );
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            Some(sub.id.clone()),
            "route should remain registered when profile deletion fails"
        );
    }

    #[tokio::test]
    async fn test_delete_bot_rejects_non_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());
        let mut parent = make_profile("botfather", None);
        parent
            .config
            .channels
            .push(crate::profiles::ChannelCredentials::Matrix {
                homeserver: "http://localhost:6167".to_string(),
                as_token: "as-token".to_string(),
                hs_token: "hs-token".to_string(),
                server_name: "localhost".to_string(),
                sender_localpart: "bot".to_string(),
                user_prefix: "bot_".to_string(),
                port: MATRIX_DEFAULT_PORT,
                allowed_senders: vec![],
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        store.save(&sub).unwrap();

        let channel = Arc::new(
            octos_bus::MatrixChannel::new(
                "http://localhost:6167",
                "as-token",
                "hs-token",
                "localhost",
                "bot",
                "bot_",
                6166,
                Arc::new(AtomicBool::new(false)),
            )
            .with_bot_router(dir.path()),
        );
        channel
            .bot_router()
            .register_entry(
                "@bot_weatherbot:localhost",
                &sub.id,
                "@alice:localhost",
                octos_bus::BotVisibility::Public,
            )
            .await
            .unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@mallory:localhost")
            .await;

        let err = result.expect_err("non-owner delete should fail");
        assert!(
            err.to_string().contains("only delete bots you created"),
            "unexpected error: {err}"
        );
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            Some(sub.id.clone())
        );
    }

    #[tokio::test]
    async fn test_delete_bot_allows_operator_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open(dir.path()).unwrap());
        let mut parent = make_profile("botfather", None);
        parent
            .config
            .channels
            .push(crate::profiles::ChannelCredentials::Matrix {
                homeserver: "http://localhost:6167".to_string(),
                as_token: "as-token".to_string(),
                hs_token: "hs-token".to_string(),
                server_name: "localhost".to_string(),
                sender_localpart: "bot".to_string(),
                user_prefix: "bot_".to_string(),
                port: MATRIX_DEFAULT_PORT,
                allowed_senders: vec!["@admin:localhost".to_string()],
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        store.save(&sub).unwrap();

        let channel = Arc::new(
            octos_bus::MatrixChannel::new(
                "http://localhost:6167",
                "as-token",
                "hs-token",
                "localhost",
                "bot",
                "bot_",
                6166,
                Arc::new(AtomicBool::new(false)),
            )
            .with_admin_allowed_senders(vec!["@admin:localhost".to_string()])
            .with_bot_router(dir.path()),
        );
        channel
            .bot_router()
            .register_entry(
                "@bot_weatherbot:localhost",
                &sub.id,
                "@alice:localhost",
                octos_bus::BotVisibility::Private,
            )
            .await
            .unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@admin:localhost")
            .await;

        assert!(
            result.is_ok(),
            "operator override should succeed: {result:?}"
        );
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            None
        );
    }
}
