//! Gateway command: run as a persistent messaging daemon.

mod account_handler;
mod adapters;
mod gateway_runtime;
#[cfg(feature = "matrix")]
mod matrix_integration;
mod message_preprocessing;
pub(crate) mod profile_factory;
pub mod prompt;
pub(crate) mod session_ui;
mod skills_handler;

use std::path::PathBuf;

use clap::Args;
use eyre::{Result, WrapErr};
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use tracing::warn;

use super::Executable;

// Imported for the test module (via `use super::*`); unused in non-test builds
#[cfg(all(test, feature = "matrix"))]
use matrix_integration::*;
pub(crate) use prompt::build_system_prompt;

// Types used by tests via `use super::*`
#[cfg(all(test, feature = "matrix"))]
use {
    crate::session_actor::SnapshotToolRegistryFactory,
    octos_agent::{AgentConfig, ToolRegistry},
    octos_bus::{ActiveSessionStore, ChannelManager, CronService, SessionManager},
    profile_factory::ProfileActorFactoryBuilder,
    std::sync::Arc,
    std::sync::atomic::{AtomicBool, AtomicUsize},
};

/// Run as a persistent gateway daemon.
///
/// `Serialize`/`Deserialize` back the layered startup config (see
/// [`crate::config_layer`]): non-explicit fields fall back to
/// `config.cli.gateway`.
#[derive(Debug, Args, serde::Serialize, serde::Deserialize, PartialEq)]
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
        return Ok(current_gateway_profile_id.map(str::to_string));
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
            .build()
            .wrap_err("failed to create tokio runtime")?;
        let result = runtime.block_on(self.run_async());

        // The CLI channel uses Tokio stdin, whose blocking read can keep the
        // runtime teardown attached to the terminal after a clean shutdown.
        // `serve` uses the same pattern for long-lived background tasks.
        if result.is_ok() {
            std::process::exit(0);
        }
        result
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

    fn test_cron_service(dir: &std::path::Path) -> Arc<CronService> {
        let (cron_in_tx, _cron_in_rx) = mpsc::channel(1);
        Arc::new(CronService::new(dir.join("cron"), cron_in_tx))
    }

    fn make_profile(id: &str, system_prompt: Option<&str>) -> crate::profiles::UserProfile {
        crate::profiles::UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            public_subdomain: None,
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

        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());

        let mut parent = make_profile("botfather", Some("admin parent"));
        parent.config.llm = Some(crate::profiles::LlmProfileConfig {
            primary: Some(crate::profiles::LlmModelSelectionConfig {
                family_id: Some("openai".into()),
                model_id: Some("gpt-4o-mini".into()),
                route: Some(crate::profiles::LlmRouteConfig {
                    api_key_env: Some("OPENAI_API_KEY".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            fallbacks: vec![crate::profiles::LlmModelSelectionConfig {
                family_id: Some("openai".into()),
                model_id: Some("gpt-4o".into()),
                route: Some(crate::profiles::LlmRouteConfig {
                    api_key_env: Some("OPENAI_API_KEY".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
        });
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
            // Gap 4.1 BLOCKER 1: in this admin-parent test `--octos-home` is
            // effectively the same dir, so mirror `project_dir`.
            effective_octos_home: project_dir.clone(),
            tool_config,
            memory,
            recall: Arc::new(
                octos_memory::RecallStore::open(
                    &project_dir,
                    octos_memory::RecallConfig::default(),
                )
                .unwrap(),
            ),
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
            task_query_store: crate::session_actor::SessionTaskQueryStore::default(),
            subagent_output_router: Arc::new(octos_agent::SubAgentOutputRouter::new(
                base_data_dir.join("subagent-out"),
            )),
            host_plugins: Default::default(),
            host_memory: None,
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
            registry.get("search").is_some(),
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
        assert!(
            settings.mention_only,
            "mention-only gating is safe-by-default (true) when unset"
        );
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
    fn matrix_channel_settings_mention_only_opt_out() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_AS_TOKEN: "as-token",
            MATRIX_SETTING_HS_TOKEN: "hs-token",
            MATRIX_SETTING_MENTION_ONLY: false,
        }));

        let settings = MatrixChannelSettings::from_entry(&entry).unwrap();

        assert!(
            !settings.mention_only,
            "operator can disable mention-only gating"
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
    fn matrix_defaults_to_appservice_mode() {
        let entry = matrix_entry(serde_json::json!({}));
        assert!(!matrix_is_user_mode(&entry));
    }

    #[test]
    fn matrix_user_mode_detected_case_insensitive() {
        let entry = matrix_entry(serde_json::json!({ MATRIX_SETTING_MODE: "User" }));
        assert!(matrix_is_user_mode(&entry));
    }

    #[test]
    fn matrix_user_settings_accept_access_token() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_HOMESERVER: "https://matrix.org",
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            MATRIX_SETTING_ROOMS: ["!a:matrix.org", "!b:matrix.org"],
        }));

        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(settings.homeserver, "https://matrix.org");
        assert_eq!(settings.access_token.as_deref(), Some("syt_token"));
        assert!(settings.password.is_none());
        assert_eq!(settings.rooms, vec!["!a:matrix.org", "!b:matrix.org"]);
        assert_eq!(settings.auto_join, octos_bus::MatrixAutoJoin::Off);
        assert_eq!(
            settings.group_policy,
            octos_bus::MatrixGroupPolicy::Allowlist
        );
        assert!(settings.require_mention);
    }

    #[test]
    fn matrix_user_settings_copy_allowed_senders() {
        let entry = crate::config::ChannelEntry {
            channel_type: MATRIX_CHANNEL_TYPE.to_string(),
            allowed_senders: vec!["@alice:matrix.org".into(), "@bob:matrix.org".into()],
            settings: serde_json::json!({
                MATRIX_SETTING_MODE: MATRIX_MODE_USER,
                MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            }),
        };

        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(
            settings.allowed_senders,
            vec![
                "@alice:matrix.org".to_string(),
                "@bob:matrix.org".to_string()
            ]
        );
    }

    #[test]
    fn matrix_user_settings_accept_openclaw_style_policy_keys() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            "autoJoin": "allowlist",
            "autoJoinAllowlist": ["!ops:matrix.org", "#support:matrix.org"],
            "groupPolicy": "open",
            "requireMention": false,
        }));

        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(settings.auto_join, octos_bus::MatrixAutoJoin::Allowlist);
        assert_eq!(
            settings.auto_join_allowlist,
            vec![
                "!ops:matrix.org".to_string(),
                "#support:matrix.org".to_string()
            ]
        );
        assert_eq!(settings.group_policy, octos_bus::MatrixGroupPolicy::Open);
        assert!(!settings.require_mention);
    }

    #[test]
    fn matrix_user_settings_accept_password_login() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_USER_ID: "@bot:matrix.org",
            MATRIX_SETTING_PASSWORD: "secret",
            MATRIX_SETTING_DEVICE_NAME: "octos-gw",
        }));

        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();

        assert_eq!(settings.user_id.as_deref(), Some("@bot:matrix.org"));
        assert_eq!(settings.password.as_deref(), Some("secret"));
        assert_eq!(settings.device_name.as_deref(), Some("octos-gw"));
        assert!(settings.access_token.is_none());
    }

    #[test]
    fn matrix_user_settings_default_mention_policy_strict_and_parse_open() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
        }));
        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();
        assert_eq!(
            settings.mention_policy,
            octos_bus::MatrixMentionPolicy::Strict
        );

        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            MATRIX_SETTING_MENTION_POLICY: "open",
        }));
        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();
        assert_eq!(
            settings.mention_policy,
            octos_bus::MatrixMentionPolicy::Open
        );

        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            MATRIX_SETTING_MENTION_POLICY_CAMEL: "open",
        }));
        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();
        assert_eq!(
            settings.mention_policy,
            octos_bus::MatrixMentionPolicy::Open
        );
    }

    #[test]
    fn matrix_user_settings_warn_on_unrecognized_mention_policy() {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            MATRIX_SETTING_MENTION_POLICY: "stric",
        }));
        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();
        assert_eq!(
            settings.mention_policy,
            octos_bus::MatrixMentionPolicy::Strict
        );
        let logs = capture.contents();
        assert!(
            logs.contains("unrecognized matrix mention_policy"),
            "a typo must warn, got logs: {logs}"
        );
        assert!(logs.contains("stric"), "warn must echo the value: {logs}");

        // Legitimate values parse silently.
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
            MATRIX_SETTING_MENTION_POLICY: "Strict",
        }));
        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();
        assert_eq!(
            settings.mention_policy,
            octos_bus::MatrixMentionPolicy::Strict
        );
        assert_eq!(
            capture
                .contents()
                .matches("unrecognized matrix mention_policy")
                .count(),
            1,
            "recognized values must not warn"
        );
    }

    /// Captures `tracing` output so warn-on-misconfig behaviour can be
    /// asserted.
    #[derive(Clone, Default)]
    struct LogCapture {
        buf: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl LogCapture {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn matrix_user_settings_require_credentials() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_USER_ID: "@bot:matrix.org",
        }));

        let err = MatrixUserChannelSettings::from_entry(&entry).unwrap_err();
        assert!(err.to_string().contains(MATRIX_USER_MISSING_AUTH_ERROR));
    }

    #[test]
    fn test_gateway_registers_matrix_user_channel() {
        let entry = matrix_entry(serde_json::json!({
            MATRIX_SETTING_MODE: MATRIX_MODE_USER,
            MATRIX_SETTING_ACCESS_TOKEN: "syt_token",
        }));
        let settings = MatrixUserChannelSettings::from_entry(&entry).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut channel_mgr = ChannelManager::new();
        let tmp = tempfile::TempDir::new().unwrap();

        let _ = register_matrix_user_channel(&mut channel_mgr, &settings, &shutdown, tmp.path(), 0);

        assert!(channel_mgr.get_channel(MATRIX_CHANNEL_TYPE).is_some());
    }

    #[test]
    fn test_dispatch_unknown_profile_falls_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = crate::profiles::ProfileStore::open_unified(dir.path()).unwrap();
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
        let store = crate::profiles::ProfileStore::open_unified(dir.path()).unwrap();
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

    #[test]
    fn test_dispatch_without_target_uses_current_gateway_profile() {
        let resolved = resolve_dispatch_profile_id(Some("dspfac--newsbot"), None, None).unwrap();

        assert_eq!(resolved.as_deref(), Some("dspfac--newsbot"));
    }

    #[test]
    fn test_dispatch_without_target_keeps_main_when_gateway_unscoped() {
        let resolved = resolve_dispatch_profile_id(None, None, None).unwrap();

        assert_eq!(resolved, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_delete_bot_keeps_route_when_profile_delete_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
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
                mention_only: true,
                mode: String::new(),
                user_id: String::new(),
                access_token: String::new(),
                password: String::new(),
                device_name: String::new(),
                rooms: vec![],
                auto_join: "off".to_string(),
                auto_join_allowlist: vec![],
                group_policy: "allowlist".to_string(),
                require_mention: true,
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
            cron_service: test_cron_service(dir.path()),
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
        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
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
                mention_only: true,
                mode: String::new(),
                user_id: String::new(),
                access_token: String::new(),
                password: String::new(),
                device_name: String::new(),
                rooms: vec![],
                auto_join: "off".to_string(),
                auto_join_allowlist: vec![],
                group_policy: "allowlist".to_string(),
                require_mention: true,
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
            cron_service: test_cron_service(dir.path()),
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
        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
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
                mention_only: true,
                mode: String::new(),
                user_id: String::new(),
                access_token: String::new(),
                password: String::new(),
                device_name: String::new(),
                rooms: vec![],
                auto_join: "off".to_string(),
                auto_join_allowlist: vec![],
                group_policy: "allowlist".to_string(),
                require_mention: true,
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
            cron_service: test_cron_service(dir.path()),
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

    // #2702: deleting a bot must release the keychain items the deleted
    // bot profile's markers point at; a bare account a surviving profile
    // still references stays (the remove-key shared-account contract, #2261).
    #[tokio::test]
    async fn test_delete_bot_releases_keychain_items_but_keeps_shared_ones() {
        use crate::auth::keychain;

        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
        let secrets = tempfile::tempdir().unwrap();
        let _secrets_root = keychain::test_override_secrets_root(secrets.path().to_path_buf());

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
                mention_only: true,
                mode: String::new(),
                user_id: String::new(),
                access_token: String::new(),
                password: String::new(),
                device_name: String::new(),
                rooms: vec![],
                auto_join: "off".to_string(),
                auto_join_allowlist: vec![],
                group_policy: "allowlist".to_string(),
                require_mention: true,
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        sub.config.env_vars.insert(
            "VERTEX_SA_JSON".into(),
            keychain::marker_for(&keychain::scoped_account(
                "VERTEX_SA_JSON",
                "botfather--weatherbot",
            )),
        );
        store.save(&sub).unwrap();

        let mut sibling = make_profile("botfather--clockbot", None);
        sibling.parent_id = Some(parent.id.clone());
        sibling
            .config
            .env_vars
            .insert("ZAI_API_KEY".into(), keychain::marker_for("ZAI_API_KEY"));
        store.save(&sibling).unwrap();

        keychain::set_secret(
            &keychain::scoped_account("VERTEX_SA_JSON", "botfather--weatherbot"),
            "sa-json",
        )
        .unwrap();
        keychain::set_secret("ZAI_API_KEY", "shared-key").unwrap();

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

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
            cron_service: test_cron_service(dir.path()),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@alice:localhost")
            .await;

        assert!(result.is_ok(), "owner delete should succeed: {result:?}");
        assert_eq!(
            channel
                .bot_router()
                .route("@bot_weatherbot:localhost")
                .await,
            None
        );
        assert!(
            store.get(&sub.id).unwrap().is_none(),
            "the bot profile itself must be gone"
        );
        assert_eq!(
            keychain::get_secret(&keychain::scoped_account(
                "VERTEX_SA_JSON",
                "botfather--weatherbot"
            ))
            .unwrap(),
            None,
            "the deleted bot's scoped keychain item must be released"
        );
        assert_eq!(
            keychain::get_secret("ZAI_API_KEY").unwrap().as_deref(),
            Some("shared-key"),
            "a bare keychain account a sibling bot still references must be kept"
        );
    }

    // The release must happen only once deletion is final: the
    // unregister-failure path restores the profile, and a restored profile
    // must never come back pointing at a released keychain item.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_delete_bot_failure_path_keeps_the_keychain_items() {
        use crate::auth::keychain;

        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::profiles::ProfileStore::open_unified(dir.path()).unwrap());
        let secrets = tempfile::tempdir().unwrap();
        let _secrets_root = keychain::test_override_secrets_root(secrets.path().to_path_buf());

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
                mention_only: true,
                mode: String::new(),
                user_id: String::new(),
                access_token: String::new(),
                password: String::new(),
                device_name: String::new(),
                rooms: vec![],
                auto_join: "off".to_string(),
                auto_join_allowlist: vec![],
                group_policy: "allowlist".to_string(),
                require_mention: true,
            });
        store.save(&parent).unwrap();

        let mut sub = make_profile("botfather--weatherbot", None);
        sub.parent_id = Some(parent.id.clone());
        sub.config.env_vars.insert(
            "VERTEX_SA_JSON".into(),
            keychain::marker_for(&keychain::scoped_account(
                "VERTEX_SA_JSON",
                "botfather--weatherbot",
            )),
        );
        store.save(&sub).unwrap();

        let scoped = keychain::scoped_account("VERTEX_SA_JSON", "botfather--weatherbot");
        keychain::set_secret(&scoped, "sa-json").unwrap();

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

        // A read-only data dir makes the router persist inside
        // unregister_bot fail (it needs to create a temp file), sending
        // delete_bot down its profile-restore path; the profile row itself
        // unlinks inside dir/profiles, whose permissions are untouched (the
        // room-leave round-trip is best-effort and never fails).
        let original_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        let manager = GatewayBotManager {
            store: store.clone(),
            channel: channel.clone(),
            parent_profile_id: parent.id.clone(),
            cron_service: test_cron_service(dir.path()),
        };

        let result = manager
            .delete_bot("@bot_weatherbot:localhost", "@alice:localhost")
            .await;

        let mut restore = std::fs::metadata(dir.path()).unwrap().permissions();
        restore.set_mode(original_mode);
        std::fs::set_permissions(dir.path(), restore).unwrap();

        assert!(
            result.is_err(),
            "the failing router persist must fail the delete: {result:?}"
        );
        assert!(
            store.get(&sub.id).unwrap().is_some(),
            "the profile must be restored when the bot cannot be unregistered"
        );
        assert_eq!(
            keychain::get_secret(&scoped).unwrap().as_deref(),
            Some("sa-json"),
            "a restored profile's keychain item must never be released by the failed deletion"
        );
    }
}
