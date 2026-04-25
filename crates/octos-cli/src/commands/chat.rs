//! Chat command: interactive multi-turn conversation with an agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Args;
use colored::Colorize;
use eyre::{Result, WrapErr};
use octos_agent::{Agent, AgentConfig, ConsoleReporter, HookExecutor, ToolRegistry};
use octos_core::{AgentId, Message, MessageRole};
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, EmbeddingProvider, LlmProvider, OpenAIEmbedder, ProviderChain,
    RetryProvider,
};
use octos_memory::{EpisodeStore, MemoryStore};
use rustyline::DefaultEditor;

use super::Executable;
use crate::config::Config;

/// Interactive multi-turn chat with an agent.
#[derive(Debug, Args)]
pub struct ChatCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// LLM provider to use (overrides config).
    #[arg(long)]
    pub provider: Option<String>,

    /// Model to use (overrides config).
    #[arg(long)]
    pub model: Option<String>,

    /// Custom base URL for the API endpoint (overrides config).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Maximum tool-call iterations per message (default: 20).
    #[arg(long, default_value = "20")]
    pub max_iterations: u32,

    /// Verbose output (show tool outputs).
    #[arg(short, long)]
    pub verbose: bool,

    /// Disable automatic retry on transient errors.
    #[arg(long)]
    pub no_retry: bool,

    /// Send a single message and exit (non-interactive mode).
    #[arg(short, long)]
    pub message: Option<String>,

    /// Runtime profile to apply at startup (M8.3). Accepts a built-in name
    /// (`coding`, `swarm`), a user-dir id under `~/.octos/profiles/<id>/`,
    /// or an explicit path to a profile JSON/TOML file.
    ///
    /// Defaults to `coding` which preserves today's no-flag behaviour
    /// byte-for-byte.
    #[arg(long)]
    pub profile: Option<String>,
}

/// Exit commands.
const EXIT_COMMANDS: &[&str] = &["exit", "quit", "/exit", "/quit", ":q"];

impl Executable for ChatCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl ChatCommand {
    async fn run_async(self) -> Result<()> {
        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        // Resolve data directory (--data-dir > $OCTOS_HOME > ~/.octos)
        let data_dir = super::resolve_data_dir(self.data_dir)?;

        // Load config
        let config = if let Some(config_path) = &self.config {
            Config::from_file(config_path)?
        } else {
            Config::load(&cwd, &data_dir)?
        };

        let model = self.model.or(config.model.clone());
        let base_url = self.base_url.or(config.base_url.clone());
        let provider_name = self
            .provider
            .or(config.provider.clone())
            .or_else(|| {
                model
                    .as_deref()
                    .and_then(crate::config::detect_provider)
                    .map(String::from)
            })
            .ok_or_else(|| {
                eyre::eyre!(
                    "no LLM provider configured. Run `octos init` or set provider in config.json"
                )
            })?;

        // Create LLM provider (with optional failover chain)
        let base_provider: Arc<dyn LlmProvider> =
            create_provider(&provider_name, &config, model, base_url)?;
        let model_id = base_provider.model_id().to_string();

        let llm: Arc<dyn LlmProvider> = if self.no_retry {
            base_provider
        } else if config.fallback_models.is_empty() {
            Arc::new(RetryProvider::new(base_provider))
        } else {
            let mut providers: Vec<Arc<dyn LlmProvider>> =
                vec![Arc::new(RetryProvider::new(base_provider))];
            for fb in &config.fallback_models {
                let fb_config = if fb.api_key_env.is_some() {
                    let mut c = config.clone();
                    c.api_key_env = fb.api_key_env.clone();
                    c
                } else {
                    config.clone()
                };
                match create_provider_with_api_type(
                    &fb.provider,
                    &fb_config,
                    fb.model.clone(),
                    fb.base_url.clone(),
                    fb.api_type.as_deref(),
                ) {
                    Ok(p) => providers.push(Arc::new(RetryProvider::new(p))),
                    Err(e) => {
                        tracing::warn!(provider = %fb.provider, error = %e, "skipping fallback provider");
                    }
                }
            }
            // Auto-enable adaptive routing when multiple providers exist
            if providers.len() > 1 {
                let adaptive_config = config
                    .adaptive_routing
                    .as_ref()
                    .map(AdaptiveConfig::from)
                    .unwrap_or_default();
                tracing::info!("adaptive routing enabled ({} providers)", providers.len());
                Arc::new(AdaptiveRouter::new(providers, &[], adaptive_config))
            } else {
                Arc::new(ProviderChain::new(providers))
            }
        };

        let memory = Arc::new(
            EpisodeStore::open(&data_dir)
                .await
                .wrap_err("failed to open episode store")?,
        );

        // Resolve the runtime profile (M8.3). Order:
        //   1. --profile CLI arg, if present;
        //   2. `~/.octos/profile` symlink, if it exists (points at a
        //      profile name or dir);
        //   3. fallback to the built-in `coding` profile.
        // The resolved profile's tool filter is applied after the full
        // registry has been assembled, preserving the existing bootstrap
        // path (plugins, MCP, pipelines etc. all register first).
        let (profile, profile_source_label) = resolve_profile(&self.profile)?;
        tracing::info!(
            "profile resolved: name={} source={}",
            profile.name,
            profile_source_label
        );

        // Create tool registry (with sandbox if configured)
        let sandbox = octos_agent::create_sandbox(&config.sandbox);
        let mut tools = ToolRegistry::with_builtins_and_sandbox(&cwd, sandbox);

        // Open tool config store for user-customizable tool defaults
        let tool_config = std::sync::Arc::new(
            octos_agent::ToolConfigStore::open(&data_dir)
                .await
                .wrap_err("failed to open tool config store")?,
        );
        tools.inject_tool_config(tool_config.clone());

        // Override browser tool with configured timeout if set
        if let Some(gw) = &config.gateway {
            if let Some(secs) = gw.browser_timeout_secs {
                tools.register(
                    octos_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                        .with_config(tool_config.clone()),
                );
            }
        }

        // Register spawn tool for sync sub-agent support in chat mode.
        // Background mode won't deliver results (dummy channel), but sync mode works fine.
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(1);
        let worker_prompt = super::load_prompt("worker", octos_agent::DEFAULT_WORKER_PROMPT);
        tools.register(
            octos_agent::SpawnTool::new(llm.clone(), memory.clone(), cwd.clone(), spawn_tx)
                .with_worker_prompt(worker_prompt),
        );

        // Register research synthesis tool (map-reduce over deep_search source files)
        tools.register(octos_agent::SynthesizeResearchTool::new(
            llm.clone(),
            data_dir.clone(),
        ));

        // Create memory store and register memory bank tools
        let memory_store = Arc::new(
            MemoryStore::open(&data_dir)
                .await
                .wrap_err("failed to open memory store")?,
        );
        tools.register(octos_agent::RecallMemoryTool::new(memory_store.clone()));
        tools.register(octos_agent::SaveMemoryTool::new(memory_store.clone()));

        // Register MCP tools
        if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: MCP initialization failed: {e}"),
            }
        }

        // Bootstrap bundled app-skill binaries (deep_search, deep_crawl, etc.)
        // Must happen BEFORE plugin loading so PluginLoader picks them up.
        let project_dir = cwd.join(".octos");
        let n = octos_agent::bootstrap::bootstrap_bundled_skills(&project_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} app-skills");
        }
        let n = octos_agent::bootstrap::bootstrap_platform_skills(&project_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} platform skills");
        }

        // Load plugins (includes app-skills from .octos/skills/)
        let plugin_dirs = Config::plugin_dirs_from_project(&cwd.join(".octos"));
        let mut plugin_result = octos_agent::PluginLoadResult::default();
        if !plugin_dirs.is_empty() {
            match octos_agent::PluginLoader::load_into(&mut tools, &plugin_dirs, &[]) {
                Ok(result) => plugin_result = result,
                Err(e) => eprintln!("Warning: plugin loading failed: {e}"),
            }
        }

        // Start MCP servers declared in skill manifests
        if !plugin_result.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: skill MCP initialization failed: {e}"),
            }
        }

        // Pipeline tool (DOT-based multi-step workflows, with plugin access)
        let pipeline_tool = octos_pipeline::RunPipelineTool::new(
            llm.clone(),
            memory.clone(),
            cwd.clone(),
            data_dir.clone(),
        )
        .with_provider_policy(tools.provider_policy().cloned())
        .with_plugin_dirs(plugin_dirs);
        tools.register(pipeline_tool);

        // Apply tool policy from config
        if let Some(ref policy) = config.tool_policy {
            tools.apply_policy(policy);
        }

        // Apply context-based tag filter
        if !config.context_filter.is_empty() {
            tools.set_context_filter(config.context_filter.clone());
        }

        // Apply provider-specific tool policy
        if let Some(policy) = resolve_provider_policy(&config, &provider_name, &model_id) {
            tools.set_provider_policy(policy);
        }

        // M8.3: narrow the tool registry through the resolved profile.
        // Runs AFTER every other filter so profile narrowing is the final
        // envelope and `spawn_only` tools are still preserved.
        profile.apply_to_registry(&mut tools);

        // Set up Ctrl+C handler
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                shutdown_clone.store(true, Ordering::Release);
            }
        });

        // Create agent
        let reporter = Arc::new(ConsoleReporter::new().with_verbose(self.verbose));
        let agent_config = AgentConfig {
            max_iterations: self.max_iterations,
            save_episodes: true,
            chat_max_tokens: config.gateway.as_ref().and_then(|g| g.max_output_tokens),
            ..Default::default()
        };
        // M8.2: load sub-agent manifests from `<cwd>/agents/` layered on
        // top of the crate-shipped built-ins (research-worker, repo-editor).
        // Missing dirs fall back to built-ins only.
        let agents_dir = cwd.join("agents");
        let agent_definitions = match octos_agent::agents::AgentDefinitions::load_dir(&agents_dir) {
            Ok(defs) => Arc::new(defs),
            Err(err) => {
                eprintln!(
                    "Warning: failed to load agent manifests from {}: {err}",
                    agents_dir.display()
                );
                Arc::new(octos_agent::agents::AgentDefinitions::with_builtins())
            }
        };

        // M8 fix-first item 8 (gap 4a): hard-validate the resolved profile's
        // referenced agent ids against the loaded `AgentDefinitions`
        // registry before bootstrapping. The validator helper has been
        // present since M8.5 but bootstrap never invoked it; an unknown
        // agent id silently let `spawn` succeed with a missing manifest.
        // Bootstrap is the right place to fail fast on this.
        profile
            .validate_against_registry(&agent_definitions)
            .wrap_err("profile references missing agent_definition ids")?;

        // M8.3: share the resolved profile with the Agent so downstream
        // code can introspect the envelope. The tool filter has already
        // been applied above.
        let profile_arc = Arc::new(profile);

        // M8 fix-first item 8 (gap 1): the M8.4 FileStateCache helper
        // exists and is consumed by file tools, but bootstrap never built
        // an instance for the real chat agent. Construct one here so
        // foreground reads short-circuit on unchanged files and the
        // hand-off from `seed_from_replacement_refs` (M8.6) lands in a
        // live cache.
        let file_state_cache = Arc::new(octos_agent::FileStateCache::new());

        // M8 fix-first item 8 (gap 2): wire the M8.7 SubAgentOutputRouter
        // and AgentSummaryGenerator into the real chat agent. Without
        // this the spawn_only background branch silently skips disk
        // routing and the periodic summary watcher.
        let subagent_output_root = data_dir.join("subagent-outputs");
        let subagent_output_router =
            Arc::new(octos_agent::SubAgentOutputRouter::new(subagent_output_root));
        // Dereference the Arc<TaskSupervisor> the registry hands back so
        // `AgentSummaryGenerator::new` (which takes `TaskSupervisor` by
        // value, leveraging its Clone impl that shares the inner state)
        // gets a handle aliasing the same supervisor the registry uses.
        let supervisor_for_summary = (*tools.supervisor()).clone();
        let subagent_summary_generator = Arc::new(octos_agent::AgentSummaryGenerator::new(
            llm.clone(),
            subagent_output_router.clone(),
            supervisor_for_summary,
        ));

        let mut agent = Agent::new(AgentId::new("chat"), llm, tools, memory)
            .with_config(agent_config)
            .with_reporter(reporter)
            .with_shutdown(shutdown.clone())
            .with_agent_definitions(agent_definitions)
            .with_profile(profile_arc.clone())
            .with_file_state_cache(file_state_cache)
            .with_subagent_output_router(subagent_output_router)
            .with_subagent_summary_generator(subagent_summary_generator);

        // M8.3: if the profile declares a system_prompt_template, try to
        // read it relative to `~/.octos/profiles/<name>/`. The path is a
        // hint — missing files are a warning, not an error, so profiles
        // referring to templates that ship separately keep working.
        if let Some(template_rel) = profile_arc.system_prompt_template.as_ref() {
            if let Some(prompt_text) =
                super::load_profile_prompt_template(&profile_arc.name, template_rel)
            {
                agent.set_system_prompt(prompt_text);
            }
        }

        // Load bootstrap files (AGENTS.md, SOUL.md, etc.) from project .octos/ directory
        let project_dir = cwd.join(".octos");
        let bootstrap = super::load_bootstrap_files(&project_dir);
        if !bootstrap.is_empty() {
            agent.append_system_prompt(&bootstrap);
        }

        // Inject memory context (long-term + daily notes)
        let memory_ctx = memory_store.get_memory_context().await;
        if !memory_ctx.is_empty() {
            agent.append_system_prompt(&memory_ctx);
        }

        // Inject memory bank summary (entity abstracts)
        let bank_summary = memory_store.get_bank_summary().await;
        if !bank_summary.is_empty() {
            agent.append_system_prompt(&bank_summary);
        }

        // Inject skill prompt fragments
        for fragment in &plugin_result.prompt_fragments {
            agent.append_system_prompt(fragment);
        }

        // Merge config hooks with skill-declared hooks
        let mut all_hooks = config.hooks.clone();
        all_hooks.extend(plugin_result.hooks);
        if !all_hooks.is_empty() {
            agent = agent.with_hooks(Arc::new(HookExecutor::new(all_hooks)));
        }

        if let Some(embedder) = create_embedder(&config) {
            agent = agent.with_embedder(embedder);
        }

        // Single-message mode: send one message and exit
        if let Some(msg) = self.message {
            let response = agent.process_message(&msg, &[], vec![]).await?;
            if !response.streamed {
                println!("{}", response.content);
            }
            return Ok(());
        }

        // Set up readline
        let history_dir = data_dir.join("history");
        std::fs::create_dir_all(&history_dir).ok();
        let history_path = history_dir.join("chat_history");

        let mut rl = DefaultEditor::new().wrap_err("failed to initialize readline")?;
        let _ = rl.load_history(&history_path);

        // Banner
        println!("{}", "octos chat".cyan().bold());
        println!("{}", "(type /exit or Ctrl+C to quit)".dimmed());
        println!();

        // Conversation history
        let mut history: Vec<Message> = Vec::new();

        // Interactive loop — readline is blocking so we run it on a separate thread.
        loop {
            if shutdown.load(Ordering::Acquire) {
                break;
            }

            // Spawn blocking readline on a separate thread
            let (line_tx, line_rx) = tokio::sync::oneshot::channel();
            let mut rl_moved = rl;
            let readline_handle = tokio::task::spawn_blocking(move || {
                let result = rl_moved.readline("you> ");
                let _ = line_tx.send(result);
                rl_moved
            });

            // Wait for user input
            let readline_result = line_rx
                .await
                .unwrap_or(Err(rustyline::error::ReadlineError::Eof));

            // Recover the Editor from the blocking thread
            rl = readline_handle.await.unwrap_or_else(|_| {
                rustyline::DefaultEditor::new().expect("failed to create editor")
            });

            let line = match readline_result {
                Ok(line) => line,
                Err(
                    rustyline::error::ReadlineError::Interrupted
                    | rustyline::error::ReadlineError::Eof,
                ) => {
                    break;
                }
                Err(e) => {
                    eprintln!("Input error: {e}");
                    break;
                }
            };

            let input = line.trim();
            if input.is_empty() {
                continue;
            }

            rl.add_history_entry(input).ok();

            if EXIT_COMMANDS.contains(&input.to_lowercase().as_str()) {
                break;
            }

            // Handle /config command
            if input == "/config" || input.starts_with("/config ") {
                let args = input.strip_prefix("/config").unwrap_or("").trim();
                let response = tool_config.handle_config_command(args).await;
                println!("{response}");
                continue;
            }

            // Process message
            let response = agent.process_message(input, &history, vec![]).await?;

            // Append to history
            history.push(Message {
                role: MessageRole::User,
                content: input.to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            });
            history.push(Message {
                role: MessageRole::Assistant,
                content: response.content.clone(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            });

            // Print response (skip if already streamed to console)
            if !response.streamed {
                println!();
                println!("{}: {}", "assistant".blue().bold(), response.content);
            }
            println!();
        }

        // Save history
        let _ = rl.save_history(&history_path);
        println!("{}", "Goodbye!".dimmed());

        Ok(())
    }
}

/// M8.3 — resolve the runtime profile for an `octos chat` invocation.
///
/// Resolution order:
///
/// 1. `--profile <name_or_path>` CLI arg, if present.
/// 2. `~/.octos/profile` symlink, if it exists (its target is treated as a
///    profile name or path using the same rules as the CLI arg).
/// 3. Built-in `coding` profile — the behaviour-parity fallback.
///
/// Returns the resolved [`octos_agent::profile::ProfileDefinition`] plus a
/// human-readable source label (`cli`, `symlink`, or `default`) suitable for
/// inclusion in the `profile resolved: ...` log line.
pub(crate) fn resolve_profile(
    cli_arg: &Option<String>,
) -> Result<(octos_agent::profile::ProfileDefinition, &'static str)> {
    use octos_agent::profile::ProfileDefinition;

    if let Some(arg) = cli_arg.as_deref() {
        let (def, _) = ProfileDefinition::load(arg)
            .wrap_err_with(|| format!("failed to load profile '{arg}'"))?;
        return Ok((def, "cli"));
    }

    // `~/.octos/profile` symlink (or plain file containing a profile name).
    // A symlink target can be either a path (dereferences normally through
    // filesystem APIs, which `load` will then detect as a path arg) or a
    // simple profile name if the link points at a directory under
    // `~/.octos/profiles/`.
    if let Some(home) = dirs::home_dir() {
        let pointer = home.join(".octos/profile");
        if pointer.symlink_metadata().is_ok() {
            // Plain symlink: dereference and feed the target into `load`.
            if let Ok(target) = std::fs::read_link(&pointer) {
                let target_str = target.to_string_lossy().to_string();
                if let Ok((def, _)) = ProfileDefinition::load(&target_str) {
                    return Ok((def, "symlink"));
                }
                tracing::warn!(
                    target = %target.display(),
                    "~/.octos/profile symlink target could not be resolved; falling back to default"
                );
            } else if let Ok(text) = std::fs::read_to_string(&pointer) {
                // Regular file: treat its first non-empty line as a profile name.
                let name = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                if !name.is_empty() {
                    if let Ok((def, _)) = ProfileDefinition::load(name) {
                        return Ok((def, "symlink"));
                    }
                }
            }
        }
    }

    let (def, _) = octos_agent::profile::ProfileDefinition::load("coding")
        .wrap_err("failed to load built-in coding profile")?;
    Ok((def, "default"))
}

/// Find the matching provider-specific tool policy for the active model.
/// Checks model ID first (e.g. "claude-sonnet-4-20250514"), then provider name (e.g. "gemini").
pub(crate) fn resolve_provider_policy(
    config: &Config,
    provider_name: &str,
    model_id: &str,
) -> Option<octos_agent::ToolPolicy> {
    if config.tool_policy_by_provider.is_empty() {
        return None;
    }
    // Exact model ID match first
    if let Some(policy) = config.tool_policy_by_provider.get(model_id) {
        return Some(policy.clone());
    }
    // Provider name match
    if let Some(policy) = config.tool_policy_by_provider.get(provider_name) {
        return Some(policy.clone());
    }
    None
}

/// Create an embedding provider from config, if configured.
pub(crate) fn create_embedder(config: &Config) -> Option<Arc<dyn EmbeddingProvider>> {
    let cfg = config.embedding.as_ref()?;
    let key = config.get_api_key(&cfg.provider).ok()?;
    let mut e = OpenAIEmbedder::new(key);
    if let Some(ref url) = cfg.base_url {
        e = e.with_base_url(url);
    }
    Some(Arc::new(e))
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_provider_policy_model_id_match() {
        let json = r#"{
            "tool_policy_by_provider": {
                "gemini": {"deny": ["diff_edit"]},
                "claude-sonnet-4-20250514": {"allow": ["shell"]}
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let policy =
            resolve_provider_policy(&config, "anthropic", "claude-sonnet-4-20250514").unwrap();
        assert!(policy.is_allowed("shell"));
        assert!(!policy.is_allowed("read_file"));
    }

    #[test]
    fn test_resolve_provider_policy_provider_fallback() {
        let json = r#"{
            "tool_policy_by_provider": {
                "gemini": {"deny": ["diff_edit"]}
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        let policy = resolve_provider_policy(&config, "gemini", "gemini-2.0-flash").unwrap();
        assert!(!policy.is_allowed("diff_edit"));
        assert!(policy.is_allowed("shell"));
    }

    #[test]
    fn test_resolve_provider_policy_none() {
        let config = Config::default();
        assert!(
            resolve_provider_policy(&config, "anthropic", "claude-sonnet-4-20250514").is_none()
        );
    }
}

/// Create an LLM provider from name and config.
///
/// When `api_type` is `Some("anthropic")` (from config or sub-provider),
/// the Anthropic Messages API protocol is used regardless of provider name.
pub(crate) fn create_provider(
    name: &str,
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
) -> Result<Arc<dyn LlmProvider>> {
    let provider =
        create_provider_with_api_type(name, config, model, base_url, config.api_type.as_deref())?;
    println!("{}: {}", "Model".green(), provider.model_id());
    Ok(provider)
}

/// Inner factory that accepts an explicit `api_type` override.
///
/// Does NOT print to stdout — callers that want a log line should print
/// after calling this function.
pub(crate) fn create_provider_with_api_type(
    name: &str,
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    let entry = octos_llm::registry::lookup(name).ok_or_else(|| {
        eyre::eyre!(
            "unknown provider: {name}. Valid: {}",
            octos_llm::registry::all_names().join(", ")
        )
    })?;

    // Resolve API key via config (auth store → env var).
    let api_key = if entry.requires_api_key {
        Some(config.get_api_key(entry.name)?)
    } else {
        config.get_api_key(entry.name).ok()
    };

    if entry.requires_model && model.is_none() {
        eyre::bail!("{} provider requires --model to be specified", name);
    }
    if entry.requires_base_url && base_url.is_none() {
        eyre::bail!("{} provider requires --base-url to be specified", name);
    }

    // Extract timeout overrides from gateway config (if any).
    let llm_timeout_secs = config.gateway.as_ref().and_then(|g| g.llm_timeout_secs);
    let llm_connect_timeout_secs = config
        .gateway
        .as_ref()
        .and_then(|g| g.llm_connect_timeout_secs);

    // If api_type is "anthropic", bypass registry and use AnthropicProvider directly.
    // This allows any provider to use the Anthropic Messages API protocol.
    if api_type == Some("anthropic") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for anthropic api_type"))?;
        let m = model.unwrap_or_else(|| {
            entry
                .default_model
                .unwrap_or("claude-sonnet-4-20250514")
                .into()
        });
        let url = base_url.unwrap_or_else(|| {
            entry
                .default_base_url
                .unwrap_or("https://api.anthropic.com")
                .into()
        });
        let mut provider =
            octos_llm::anthropic::AnthropicProvider::new(&key, &m).with_base_url(&url);
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    // If api_type is "responses", use OpenAI Responses API directly.
    // This forces the Responses API even for models not auto-detected.
    if api_type == Some("responses") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for responses api_type"))?;
        let m = model.unwrap_or_else(|| entry.default_model.unwrap_or("gpt-4o").into());
        let mut provider = octos_llm::openai_responses::OpenAIResponsesProvider::new(&key, &m);
        if let Some(url) = base_url {
            provider = provider.with_base_url(&url);
        }
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    let params = octos_llm::registry::CreateParams {
        api_key,
        model,
        base_url,
        model_hints: config.model_hints.clone(),
        llm_timeout_secs,
        llm_connect_timeout_secs,
    };

    let provider = (entry.create)(params)?;
    Ok(provider)
}
