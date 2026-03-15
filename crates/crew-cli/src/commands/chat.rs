//! Chat command: interactive multi-turn conversation with an agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::Args;
use colored::Colorize;
use crew_agent::{Agent, AgentConfig, ConsoleReporter, HookExecutor, ToolRegistry};
use crew_core::{AgentId, Message, MessageRole};
use crew_llm::{
    AdaptiveConfig, AdaptiveRouter, EmbeddingProvider, LlmProvider, OpenAIEmbedder, ProviderChain,
    RetryProvider,
};
use crew_memory::{EpisodeStore, MemoryStore};
use eyre::{Result, WrapErr};
use rustyline::DefaultEditor;

use super::Executable;
use crate::config::Config;

/// Interactive multi-turn chat with an agent.
#[derive(Debug, Args)]
pub struct ChatCommand {
    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $CREW_HOME or ~/.crew).
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

        // Load config
        let config = if let Some(config_path) = &self.config {
            Config::from_file(config_path)?
        } else {
            Config::load(&cwd)?
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
            .unwrap_or_else(|| "anthropic".to_string());

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
                Arc::new(AdaptiveRouter::new(providers, adaptive_config))
            } else {
                Arc::new(ProviderChain::new(providers))
            }
        };

        // Resolve data directory (--data-dir > $CREW_HOME > ~/.crew)
        let data_dir = super::resolve_data_dir(self.data_dir)?;
        let memory = Arc::new(
            EpisodeStore::open(&data_dir)
                .await
                .wrap_err("failed to open episode store")?,
        );

        // Create tool registry (with sandbox if configured)
        let sandbox = crew_agent::create_sandbox(&config.sandbox);
        let mut tools = ToolRegistry::with_builtins_and_sandbox(&cwd, sandbox);

        // Open tool config store for user-customizable tool defaults
        let tool_config = std::sync::Arc::new(
            crew_agent::ToolConfigStore::open(&data_dir)
                .await
                .wrap_err("failed to open tool config store")?,
        );
        tools.inject_tool_config(tool_config.clone());

        // Override browser tool with configured timeout if set
        if let Some(gw) = &config.gateway {
            if let Some(secs) = gw.browser_timeout_secs {
                tools.register(
                    crew_agent::BrowserTool::with_timeout(std::time::Duration::from_secs(secs))
                        .with_config(tool_config.clone()),
                );
            }
        }

        // Register spawn tool for sync sub-agent support in chat mode.
        // Background mode won't deliver results (dummy channel), but sync mode works fine.
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(1);
        let worker_prompt = super::load_prompt("worker", crew_agent::DEFAULT_WORKER_PROMPT);
        tools.register(
            crew_agent::SpawnTool::new(llm.clone(), memory.clone(), cwd.clone(), spawn_tx)
                .with_worker_prompt(worker_prompt),
        );

        // Register research synthesis tool (map-reduce over deep_search source files)
        tools.register(crew_agent::SynthesizeResearchTool::new(
            llm.clone(),
            data_dir.clone(),
        ));

        // Create memory store and register memory bank tools
        let memory_store = Arc::new(
            MemoryStore::open(&data_dir)
                .await
                .wrap_err("failed to open memory store")?,
        );
        tools.register(crew_agent::RecallMemoryTool::new(memory_store.clone()));
        tools.register(crew_agent::SaveMemoryTool::new(memory_store.clone()));

        // Register MCP tools
        if !config.mcp_servers.is_empty() {
            match crew_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: MCP initialization failed: {e}"),
            }
        }

        // Bootstrap bundled app-skill binaries (deep_search, deep_crawl, etc.)
        // Must happen BEFORE plugin loading so PluginLoader picks them up.
        let project_dir = cwd.join(".crew");
        let n = crew_agent::bootstrap::bootstrap_bundled_skills(&project_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} app-skills");
        }
        let n = crew_agent::bootstrap::bootstrap_platform_skills(&project_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} platform skills");
        }

        // Load plugins (includes app-skills from .crew/skills/)
        let plugin_dirs = Config::plugin_dirs(&cwd);
        if !plugin_dirs.is_empty() {
            if let Err(e) = crew_agent::PluginLoader::load_into(&mut tools, &plugin_dirs, &[]) {
                eprintln!("Warning: plugin loading failed: {e}");
            }
        }

        // Pipeline tool (DOT-based multi-step workflows, with plugin access)
        let pipeline_tool = crew_pipeline::RunPipelineTool::new(
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
        let mut agent = Agent::new(AgentId::new("chat"), llm, tools, memory)
            .with_config(agent_config)
            .with_reporter(reporter)
            .with_shutdown(shutdown.clone());

        // Load bootstrap files (AGENTS.md, SOUL.md, etc.) from project .crew/ directory
        let project_dir = cwd.join(".crew");
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

        if !config.hooks.is_empty() {
            agent = agent.with_hooks(Arc::new(HookExecutor::new(config.hooks.clone())));
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
        println!("{}", "crew-rs chat".cyan().bold());
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

/// Find the matching provider-specific tool policy for the active model.
/// Checks model ID first (e.g. "claude-sonnet-4-20250514"), then provider name (e.g. "gemini").
pub(crate) fn resolve_provider_policy(
    config: &Config,
    provider_name: &str,
    model_id: &str,
) -> Option<crew_agent::ToolPolicy> {
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
    let entry = crew_llm::registry::lookup(name).ok_or_else(|| {
        eyre::eyre!(
            "unknown provider: {name}. Valid: {}",
            crew_llm::registry::all_names().join(", ")
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
            crew_llm::anthropic::AnthropicProvider::new(&key, &m).with_base_url(&url);
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(crew_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    // If api_type is "responses", use OpenAI Responses API directly.
    // This forces the Responses API even for models not auto-detected.
    if api_type == Some("responses") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for responses api_type"))?;
        let m = model.unwrap_or_else(|| entry.default_model.unwrap_or("gpt-4o").into());
        let mut provider = crew_llm::openai_responses::OpenAIResponsesProvider::new(&key, &m);
        if let Some(url) = base_url {
            provider = provider.with_base_url(&url);
        }
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(crew_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    let params = crew_llm::registry::CreateParams {
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
