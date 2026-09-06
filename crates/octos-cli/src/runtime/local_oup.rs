//! Local frontend bootstrap for the canonical OUP dispatcher.
//!
//! No model loop, transcript vector or compaction implementation lives here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use octos_agent::profile::ProfileDefinition;
use octos_llm::LlmProvider;

use super::{BootstrapRole, ProfileRuntime};
use crate::api::AppState;
use crate::config::Config;
use crate::profiles::{ProfileConfig, UserProfile};

pub(crate) struct LocalOupOptions {
    pub config: Config,
    pub profile: UserProfile,
    pub data_dir: PathBuf,
    pub config_home: PathBuf,
    pub no_retry: bool,
    pub provider: Option<Arc<dyn LlmProvider>>,
    pub tool_profile: Option<ProfileDefinition>,
    pub save_episodes: bool,
}

/// Metadata for an ambient-config invocation. The resolved Config remains
/// authoritative; this only supplies profile identity and profile-only knobs.
pub(crate) fn local_profile(id: &str, config: &Config) -> UserProfile {
    let mut settings = ProfileConfig {
        env_vars: config.env_vars.clone(),
        sandbox: config.sandbox.clone(),
        approval_policy: config.approval_policy.clone(),
        hooks: config.hooks.clone(),
        memory: config.memory.clone(),
        ..Default::default()
    };
    if let Some(gateway) = &config.gateway {
        settings.gateway.system_prompt = gateway.system_prompt.clone();
        settings.gateway.browser_timeout_secs = gateway.browser_timeout_secs;
        settings.gateway.max_output_tokens = gateway.max_output_tokens;
    }
    settings.gateway.max_iterations = config.max_iterations;
    UserProfile {
        id: id.to_owned(),
        name: id.to_owned(),
        enabled: false,
        public_subdomain: None,
        parent_id: None,
        data_dir: None,
        config: settings,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

pub(crate) async fn bootstrap(options: LocalOupOptions) -> Result<Arc<AppState>> {
    let profile_data_dir = options.data_dir.clone();
    bootstrap_with_profile_root(options, &profile_data_dir, false).await
}

/// An ephemeral frontend keeps profile context and explicitly invoked shared
/// tools, but writes its transcript and OUP/session sidecars only to its
/// temporary runtime root. No databases are copied or reopened under aliases.
pub(crate) async fn bootstrap_ephemeral(
    options: LocalOupOptions,
    profile_data_dir: &Path,
) -> Result<Arc<AppState>> {
    bootstrap_with_profile_root(options, profile_data_dir, true).await
}

async fn bootstrap_with_profile_root(
    options: LocalOupOptions,
    profile_data_dir: &Path,
    ephemeral: bool,
) -> Result<Arc<AppState>> {
    std::fs::create_dir_all(&options.data_dir).wrap_err("create local OUP runtime directory")?;
    let mut runtime = ProfileRuntime::bootstrap_resolved(
        &options.profile,
        profile_data_dir,
        Some(&options.config_home),
        if ephemeral {
            BootstrapRole::Gateway
        } else {
            BootstrapRole::Serve
        },
        options.config,
        None,
        options.no_retry,
        options.provider,
    )
    .await?;
    {
        let runtime = Arc::get_mut(&mut runtime)
            .ok_or_else(|| eyre::eyre!("local profile published before tool policy binding"))?;
        runtime.session_store_root = ephemeral.then(|| options.data_dir.clone());
        runtime.session_defaults = Some(octos_agent::AgentConfig {
            max_iterations: super::turn_policy::max_iterations(
                runtime.max_iterations,
                super::turn_policy::TurnIntent::Interactive,
            ),
            save_episodes: options.save_episodes && !ephemeral,
            ..super::session::configured_agent_defaults(runtime)
        });
    }
    if let Some(tool_profile) = options.tool_profile {
        let runtime = Arc::get_mut(&mut runtime)
            .ok_or_else(|| eyre::eyre!("local profile published before tool policy binding"))?;
        let mut tools = runtime.tool_specs.snapshot_excluding(&[]);
        // A profile template is trusted operator configuration, like its
        // existing CLI counterpart; workspace instructions are loaded later
        // by OUP against the actual opened session cwd.
        if let Some(template) = &tool_profile.system_prompt_template {
            if let Some(template) =
                crate::commands::load_profile_prompt_template(&tool_profile.name, template)
            {
                runtime.prompt_parts.pre_memory = template;
                runtime.system_prompt = runtime.prompt_parts.joined();
            }
        }
        runtime.agent_profile = Some(Arc::new(tool_profile));
        runtime.apply_tool_envelope(&mut tools);
        runtime.tool_specs = Arc::new(tools);
    }
    let mut state = AppState::without_services(&options.data_dir);
    state.sessions = Some(Arc::new(tokio::sync::Mutex::new(
        octos_bus::SessionManager::open(&options.data_dir)
            .wrap_err("open local OUP session store")?,
    )));
    // This state is reachable only through an in-process pipe. No network
    // listener or login endpoint is exposed by local frontend bootstrap.
    state.solo_login_enabled = true;
    state.default_network_denied = !runtime.default_sandbox.allow_network;
    state.task_query_store = Some(crate::session_actor::SessionTaskQueryStore::default());
    state.profiles.insert(runtime.profile_id.clone(), runtime);
    Ok(Arc::new(state))
}

pub(crate) fn resolve_stored_profile(
    id: Option<&str>,
    data_dir: &Path,
) -> Result<Option<UserProfile>> {
    let Some(id) = id.filter(|id| !id.contains('/') && !id.contains(std::path::MAIN_SEPARATOR))
    else {
        return Ok(None);
    };
    let store = crate::profiles::ProfileStore::open_unified(data_dir)?;
    Ok(store
        .get(id)?
        .map(|profile| store.resolve_runtime_profile(&profile)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::oup_session::{OupFrontend, OupSession};
    use octos_core::ui_protocol::{UiCommand, UiNotification};

    #[derive(Default)]
    struct ContextModel(std::sync::Mutex<Vec<Vec<octos_core::Message>>>);

    #[async_trait::async_trait]
    impl LlmProvider for ContextModel {
        async fn chat(
            &self,
            messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            self.0.lock().unwrap().push(messages.to_vec());
            Ok(octos_llm::ChatResponse {
                content: Some("ephemeral-fixture-answer".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: octos_llm::StopReason::EndTurn,
                usage: octos_llm::TokenUsage::default(),
                provider_index: None,
            })
        }

        fn provider_name(&self) -> &str {
            "local"
        }

        fn model_id(&self) -> &str {
            "ephemeral-fixture"
        }
    }

    struct Frontend;

    #[async_trait::async_trait]
    impl OupFrontend for Frontend {
        async fn event(&self, _event: UiNotification) -> Result<Option<UiCommand>> {
            Ok(None)
        }
    }

    fn options(data_dir: &Path, config_home: &Path, model: Arc<ContextModel>) -> LocalOupOptions {
        let config = Config {
            provider: Some("local".into()),
            model: Some("ephemeral-fixture".into()),
            memory: Some(crate::config::MemoryConfig {
                refresh: Some(crate::config::MemoryRefreshConfig {
                    enabled: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        LocalOupOptions {
            profile: local_profile("ephemeral-fixture", &config),
            config,
            data_dir: data_dir.to_owned(),
            config_home: config_home.to_owned(),
            no_retry: true,
            provider: Some(model),
            tool_profile: None,
            save_episodes: false,
        }
    }

    fn transcript_files(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    found.extend(transcript_files(&path));
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    found.push(path);
                }
            }
        }
        found
    }

    #[tokio::test]
    async fn local_oup_preserves_model_defaults_and_gateway_sampling() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut options = options(data.path(), home.path(), Arc::new(ContextModel::default()));
        options.config.model_temperature = Some(0.3);
        options.config.model_top_p = Some(0.8);
        options.config.model_reasoning_effort = Some(octos_llm::ReasoningEffort::High);
        let gateway = options.config.gateway.get_or_insert_with(Default::default);
        gateway.llm_temperature = Some(0.7);
        gateway.reasoning_effort = Some(octos_llm::ReasoningEffort::Low);
        gateway.max_output_tokens = Some(1234);
        gateway.llm_sampling_params = Some(serde_json::Map::from_iter([
            ("top_p".into(), serde_json::json!(0.95)),
            ("repeat_penalty".into(), serde_json::json!(1.1)),
        ]));
        let state = bootstrap(options).await.unwrap();
        let profile = &state.profiles["ephemeral-fixture"];
        let config = profile.session_defaults.as_ref().unwrap();
        assert_eq!(config.chat_temperature, Some(0.3));
        assert_eq!(config.chat_max_tokens, Some(1234));
        assert_eq!(
            config.reasoning_effort,
            Some(octos_llm::ReasoningEffort::High)
        );
        let sampling = config.chat_sampling_params.as_ref().unwrap();
        assert_eq!(sampling["top_p"], serde_json::json!(0.8_f32));
        assert_eq!(sampling["repeat_penalty"], serde_json::json!(1.1));
    }

    #[tokio::test]
    async fn should_preserve_shared_profile_context_in_ephemeral_oup_turn() {
        let home = tempfile::tempdir().unwrap();
        let shared = home.path().join("profiles/ephemeral-fixture/data");
        let transient = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let memory = octos_memory::MemoryStore::open(&shared).await.unwrap();
        memory
            .write_long_term("SHARED-MEMORY-CONTEXT")
            .await
            .unwrap();
        let config = octos_agent::ToolConfigStore::open(&shared).await.unwrap();
        config
            .set(
                "read_file",
                "fixture",
                serde_json::json!("SHARED-TOOL-CONFIG"),
            )
            .await
            .unwrap();
        let skill = shared.join("skills/ephemeral-context-fixture");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("manifest.json"),
            r#"{
            "name":"ephemeral-context-fixture", "version":"1.0.0", "tools":[],
            "prompts":{"include":["SKILL.md"]}
        }"#,
        )
        .unwrap();
        std::fs::write(skill.join("SKILL.md"), "SHARED-PROFILE-SKILL-CONTEXT").unwrap();
        let model = Arc::new(ContextModel::default());
        let state = bootstrap_ephemeral(
            options(transient.path(), home.path(), model.clone()),
            &shared,
        )
        .await
        .unwrap();
        let profile = &state.profiles["ephemeral-fixture"];

        assert_eq!(
            profile.memory_store.read_long_term().await.unwrap(),
            "SHARED-MEMORY-CONTEXT"
        );
        assert_eq!(
            profile.tool_config.get("read_file", "fixture").await,
            Some(serde_json::json!("SHARED-TOOL-CONFIG"))
        );
        assert_eq!(
            profile.skills_dir.as_deref(),
            Some(shared.join("skills").as_path())
        );
        assert!(!profile.session_defaults.as_ref().unwrap().save_episodes);
        for sessions_in_cwd in [false, true] {
            for hint in [None, Some(workspace.path())] {
                assert_eq!(
                    crate::runtime::session::resolve_sessions_root_from_hint(
                        profile,
                        hint,
                        sessions_in_cwd,
                    ),
                    transient.path(),
                    "cache identity must use the same temporary root as the session store"
                );
            }
        }

        let session = OupSession::open(
            state.clone(),
            octos_core::SessionKey::with_profile("ephemeral-fixture", "cli", "context"),
            workspace.path(),
            octos_agent::EffectivePermissions::workspace_write(),
        )
        .await
        .unwrap();
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            session.turn(
                "ephemeral-private-turn-marker",
                None,
                &std::sync::atomic::AtomicBool::new(false),
                &Frontend,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reply.text, "ephemeral-fixture-answer");
        assert!(
            session
                .hydrate()
                .await
                .unwrap()
                .messages
                .unwrap()
                .iter()
                .any(|message| message.content == "ephemeral-fixture-answer")
        );
        session.close().await.unwrap();
        assert!(
            profile
                .memory
                .find_relevant(workspace.path(), "ephemeral-private-turn-marker", 10)
                .await
                .unwrap()
                .is_empty(),
            "the ephemeral turn must not save an episode"
        );
        let prompt = {
            let seen = model.0.lock().unwrap();
            seen.first()
                .unwrap()
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        for marker in [
            "SHARED-MEMORY-CONTEXT",
            "SHARED-TOOL-CONFIG",
            "SHARED-PROFILE-SKILL-CONTEXT",
        ] {
            assert!(
                prompt.contains(marker),
                "missing actual model context: {marker}"
            );
        }
        assert!(
            transcript_files(transient.path()).iter().any(|path| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .contains("ephemeral-private-turn-marker")
            }),
            "canonical OUP transcript must exist only in the temporary runtime"
        );
        assert!(
            transcript_files(&shared).is_empty(),
            "no shared-profile transcript writes"
        );
        assert!(
            transcript_files(workspace.path()).is_empty(),
            "no workspace transcript writes"
        );

        let rebuilt = profile.rebuild_plugin_layer().await.unwrap();
        assert_eq!(rebuilt.data_dir, shared);
        assert_eq!(
            rebuilt.session_store_root.as_deref(),
            Some(transient.path())
        );
        assert!(
            rebuilt
                .system_prompt
                .contains("SHARED-PROFILE-SKILL-CONTEXT")
        );

        // Even an explicit per-cwd storage request cannot override ephemeral ownership.
        let scoped = crate::runtime::SessionRuntime::bootstrap_in_cwd(
            profile,
            octos_core::SessionKey::with_profile("ephemeral-fixture", "cli", "cwd"),
            Some(workspace.path().to_owned()),
            true,
        )
        .await
        .unwrap();
        assert_eq!(scoped.sessions_root, transient.path());
        assert!(!workspace.path().join(".octos").exists());
    }

    #[tokio::test]
    async fn should_degrade_only_ephemeral_episode_store_on_shared_lock_contention() {
        let home = tempfile::tempdir().unwrap();
        let shared = home.path().join("profiles/ephemeral-fixture/data");
        let transient = tempfile::tempdir().unwrap();
        let _owner = octos_memory::EpisodeStore::open(&shared).await.unwrap();
        let memory = octos_memory::MemoryStore::open(&shared).await.unwrap();
        memory
            .write_long_term("LOCKED-SHARED-CONTEXT")
            .await
            .unwrap();
        let model = Arc::new(ContextModel::default());
        assert!(
            bootstrap(options(&shared, home.path(), model.clone()))
                .await
                .is_err(),
            "ordinary local bootstrap retains strict episode ownership"
        );
        let state = bootstrap_ephemeral(options(transient.path(), home.path(), model), &shared)
            .await
            .expect("ephemeral companion tolerates the shared redb owner");
        let profile = &state.profiles["ephemeral-fixture"];
        assert!(profile.memory.is_degraded());
        assert_eq!(
            profile.memory_store.read_long_term().await.unwrap(),
            "LOCKED-SHARED-CONTEXT"
        );
        assert!(!profile.session_defaults.as_ref().unwrap().save_episodes);
        assert!(
            !transient.path().join("episodes.redb").exists(),
            "do not replace shared recall with a fresh database"
        );
    }

    #[tokio::test]
    async fn should_keep_default_local_session_storage_unchanged() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = bootstrap(options(
            data.path(),
            data.path(),
            Arc::new(ContextModel::default()),
        ))
        .await
        .unwrap();
        let profile = &state.profiles["ephemeral-fixture"];
        assert!(profile.session_store_root.is_none());
        assert!(!profile.memory.is_degraded());
        assert_eq!(
            crate::runtime::session::resolve_sessions_root_from_hint(
                profile,
                Some(workspace.path()),
                false,
            ),
            data.path()
        );
        assert_eq!(
            crate::runtime::session::resolve_sessions_root_from_hint(
                profile,
                Some(workspace.path()),
                true,
            ),
            crate::runtime::session::project_sessions_root(
                &workspace.path().canonicalize().unwrap(),
                "ephemeral-fixture",
            )
        );
    }
}
