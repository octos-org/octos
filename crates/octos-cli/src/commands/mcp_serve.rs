//! M7.2 — `octos mcp-serve` subcommand.
//!
//! Exposes octos as an MCP server so outer orchestrators can invoke it
//! as a sub-agent. See [`octos_agent::mcp_server`] for the transport and
//! session-level tool semantics.
//!
//! # Transports
//!
//! - `stdio` (default): JSON-RPC over stdin/stdout. Parent-trust auth.
//! - `http`: MCP Streamable HTTP served by the rmcp SDK. Requires a bearer
//!   token supplied via the `OCTOS_MCP_SERVER_TOKEN` environment variable.
//!
//! # Session dispatch (M7.2a)
//!
//! The [`RealSessionDispatch`] implementation wires outer MCP calls into
//! the existing [`Agent`](octos_agent::Agent) loop. Every `run_octos_session`
//! MCP invocation:
//!
//! 1. Loads [`ProfileConfig`](crate::profiles::ProfileConfig)-style config
//!    from disk (when present) and builds the LLM provider via the same
//!    factory chat/gateway use.
//! 2. Marks the session `Running` on the supplied
//!    [`SessionLifecycleObserver`](octos_agent::mcp_server::SessionLifecycleObserver).
//! 3. Constructs a single-shot [`Agent`](octos_agent::Agent) and runs the
//!    supplied prompt as a [`Task`](octos_core::Task) — the same code path
//!    the local chat command uses, including workspace-contract enforcement.
//! 4. Marks the session `Verifying`, resolves the contract artifact (either
//!    the caller-supplied `expected_artifact` or the workspace contract's
//!    primary artifact), then transitions to `Ready`/`Failed` with the
//!    aggregate outcome.
//!
//! Failures carry a typed prefix (`config_error:`, `llm_error:`,
//! `contract_failed:`, `artifact_missing:`, `session_failed:`) so outer
//! orchestrators can branch on category without scraping English text.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clap::{Args, ValueEnum};
use eyre::{Result, WrapErr};
use octos_agent::mcp_server::{
    McpServer, McpServerError, McpSessionDispatch, McpSessionOutcome, OCTOS_MCP_SERVER_TOKEN_ENV,
    SessionLifecycleObserver,
};
use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
use octos_agent::validators::{
    ValidatorInvocation, ValidatorPhase, ValidatorRunner, run_workspace_validators,
};
use octos_agent::{Agent, AgentConfig, HarnessEvent, SandboxConfig, ToolRegistry, create_sandbox};
use octos_core::{AgentId, Task, TaskContext, TaskKind};
use octos_llm::LlmProvider;
use octos_memory::EpisodeStore;
use serde_json::{Value, json};

use super::Executable;
use crate::config::Config;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTransport {
    Stdio,
    Http,
}

/// Run octos as an MCP server for outer orchestrators.
#[derive(Debug, Args)]
pub struct McpServeCommand {
    /// Transport to bind. `stdio` (default) uses parent-trust auth; `http`
    /// requires a bearer token via `OCTOS_MCP_SERVER_TOKEN`.
    #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
    pub transport: McpTransport,

    /// Bind address for the HTTP transport. Defaults to 127.0.0.1:0 (ephemeral).
    #[arg(long, default_value = "127.0.0.1:4033")]
    pub bind: SocketAddr,

    /// Working directory (defaults to current directory).
    #[arg(short, long)]
    pub cwd: Option<PathBuf>,

    /// Data directory for episodes, memory, sessions (defaults to $OCTOS_HOME or ~/.octos).
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    /// Path to config file.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

impl Executable for McpServeCommand {
    fn execute(self) -> Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(8 * 1024 * 1024)
            .build()
            .wrap_err("failed to create tokio runtime")?
            .block_on(self.run_async())
    }
}

impl McpServeCommand {
    async fn run_async(self) -> Result<()> {
        let cwd = match self.cwd.clone() {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };
        let ctx = super::resolve_command_context(self.data_dir.clone())?;
        let data_dir = ctx.data_dir.clone();

        // Load config — same precedence as chat/gateway: project-local
        // `.octos/config.json` wins over the resolved config_home. Missing
        // config is fine for stdio mode; the LLM factory will fail later
        // if no provider is configured.
        let config = if let Some(ref config_path) = self.config {
            Config::from_file(config_path)?
        } else {
            Config::load_with_context(&cwd, &ctx)?
        };

        // Capture the sandbox policy before `config` is moved into the LLM
        // factory. The per-session tool registry uses it to confine
        // shell/exec/file tools to the workspace (see the security note on
        // `RealSessionDispatch`).
        let sandbox = config.sandbox.clone();
        let factory = AgentLlmFactory::from_config(config)
            .wrap_err("failed to build LLM factory from config")?;
        let dispatch_config = SessionDispatchConfig {
            cwd: cwd.clone(),
            data_dir: data_dir.clone(),
            max_iterations: 20,
            sandbox,
        };
        let dispatch: Arc<dyn McpSessionDispatch> =
            Arc::new(RealSessionDispatch::new(dispatch_config, factory));
        let supervisor = Arc::new(TaskSupervisor::new());
        let server = McpServer::new(dispatch, supervisor);

        // Install a lightweight event sink that forwards typed harness events
        // to the tracing subsystem. Operators can pipe logs into the same
        // harness audit tooling the rest of the runtime uses.
        server
            .set_event_sink(|event: HarnessEvent| {
                tracing::info!(target: "mcp_serve_audit", ?event, "mcp-serve audit");
            })
            .await;

        match self.transport {
            McpTransport::Stdio => {
                tracing::info!("octos mcp-serve: stdio transport");
                server.serve_stdio().await
            }
            McpTransport::Http => {
                let token = std::env::var(OCTOS_MCP_SERVER_TOKEN_ENV).map_err(|_| {
                    eyre::eyre!("{OCTOS_MCP_SERVER_TOKEN_ENV} must be set for the http transport")
                })?;
                if token.trim().is_empty() {
                    eyre::bail!("{OCTOS_MCP_SERVER_TOKEN_ENV} must not be empty");
                }
                tracing::info!(
                    bind = %self.bind,
                    "octos mcp-serve: http transport (bearer token required)",
                );
                serve_http(server, self.bind, token).await
            }
        }
    }
}

/// Maximum accepted HTTP request body for the MCP transport (1 MiB), restoring
/// the explicit cap the hand-rolled endpoint enforced. The rmcp tower service
/// otherwise buffers the whole body, so without this a bearer-holding client
/// could exhaust memory with an arbitrarily large or chunked POST.
#[cfg(feature = "api")]
const MAX_HTTP_BODY_BYTES: usize = 1_048_576;

/// rmcp tower service (from octos-agent) behind a bearer-token gate. Extracted
/// from [`serve_http`] so the gate can be exercised by an integration test
/// without binding a well-known port.
///
/// Returns the [`CancellationToken`](tokio_util::sync::CancellationToken) that
/// tears down live SSE sessions; [`serve_http`] cancels it on shutdown.
#[cfg(feature = "api")]
fn mcp_http_router(
    server: McpServer,
    token: String,
    allow_non_loopback: bool,
) -> (axum::Router, tokio_util::sync::CancellationToken) {
    use std::sync::Arc;

    use axum::Router;
    use axum::extract::{Request, State};
    use axum::http::{StatusCode, header::AUTHORIZATION};
    use axum::middleware::{self, Next};
    use axum::response::{IntoResponse, Response};
    use tower_http::limit::RequestBodyLimit;

    /// Reject any request whose bearer token is missing or wrong before it
    /// reaches the MCP service, using the same parser + constant-time compare
    /// the hand-rolled transport used.
    async fn bearer_gate(
        State(token): State<Arc<String>>,
        request: Request,
        next: Next,
    ) -> Response {
        let provided = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        match octos_agent::mcp_server::parse_bearer_token(provided) {
            Some(candidate) if octos_agent::mcp_server::constant_time_eq(&candidate, &token) => {
                next.run(request).await
            }
            _ => (StatusCode::UNAUTHORIZED, "authentication required").into_response(),
        }
    }

    let (service, cancel) = server.streamable_http_service(allow_non_loopback);
    // Bound the request body on the fallback service (inner) so rmcp never
    // buffers an unbounded POST, and keep the bearer gate as the sole router
    // layer (outermost) so unauthenticated requests are still rejected first.
    let limited = RequestBodyLimit::new(service, MAX_HTTP_BODY_BYTES);
    let router = Router::new()
        .fallback_service(limited)
        .layer(middleware::from_fn_with_state(Arc::new(token), bearer_gate));
    (router, cancel)
}

/// Serve the MCP Streamable HTTP transport on `bind`, gating every request
/// behind the bearer token. The rmcp tower service (built in octos-agent) is
/// mounted in axum here; bearer auth is an axum middleware in front of it.
/// Requires the `api` feature (axum); the canonical install includes it.
#[cfg(feature = "api")]
async fn serve_http(server: McpServer, bind: SocketAddr, token: String) -> Result<()> {
    // A loopback bind keeps rmcp's DNS-rebinding Host guard. An explicit
    // non-loopback bind opts into cross-host exposure, so disable the guard —
    // the bearer token is then the sole authenticator.
    let allow_non_loopback = !bind.ip().is_loopback();
    if allow_non_loopback {
        tracing::warn!(
            %bind,
            "octos mcp-serve http bound to a non-loopback address; the DNS-rebinding Host \
             guard is disabled and the bearer token is the only authenticator"
        );
    }

    let (app, cancel) = mcp_http_router(server, token, allow_non_loopback);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .wrap_err_with(|| format!("failed to bind MCP server on {bind}"))?;
    let addr = listener.local_addr().unwrap_or(bind);
    tracing::info!(%addr, "octos mcp-serve http bound (streamable HTTP, bearer required)");

    // Serve until Ctrl+C / SIGTERM. On shutdown, cancel the rmcp service so live
    // SSE sessions terminate immediately — otherwise axum's graceful drain would
    // block on those long-lived streams.
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        })
        .await
        .wrap_err("mcp-serve http server error")?;
    Ok(())
}

/// Builds without the `api` feature lack axum, so the HTTP transport is
/// unavailable; stdio still works.
#[cfg(not(feature = "api"))]
async fn serve_http(_server: McpServer, _bind: SocketAddr, _token: String) -> Result<()> {
    eyre::bail!(
        "the http transport for `octos mcp-serve` requires building octos with the `api` \
         feature (the canonical install includes it); use `--transport stdio` otherwise"
    )
}

// ---- M7.2a: real session dispatch ----

/// Runtime configuration for the session-level dispatch.
#[derive(Debug, Clone)]
pub struct SessionDispatchConfig {
    /// Working directory passed to every spawned [`Agent`]. This is the
    /// workspace root enforced by the workspace-contract checks.
    pub cwd: PathBuf,
    /// Data directory used for episode/memory persistence.
    pub data_dir: PathBuf,
    /// Maximum number of tool-call iterations per MCP session.
    pub max_iterations: u32,
    /// Sandbox policy applied to the per-session tool registry. Each MCP call
    /// runs agent tool calls (shell/exec/file) on behalf of an outer
    /// orchestrator that is only parent-trusted (stdio) or bearer-token
    /// authenticated (http) — never fully trusted. Confining shell/exec to the
    /// workspace via the OS sandbox is what stops `run_octos_session` from
    /// reading, writing, or executing outside `cwd`. Defaults to
    /// [`SandboxMode::Auto`](octos_agent::SandboxMode) via
    /// [`SandboxConfig::default`].
    pub sandbox: SandboxConfig,
}

impl SessionDispatchConfig {
    /// Build the sandbox backend for this session's tool registry. Auto mode
    /// resolves to `sandbox-exec` on macOS / `bwrap` on Linux and falls back
    /// to `NoSandbox` only when no backend is available (matching chat and
    /// gateway).
    fn sandbox_backend(&self) -> Box<dyn octos_agent::Sandbox> {
        create_sandbox(&self.sandbox)
    }
}

/// Factory that yields a ready-to-use LLM provider for each session.
///
/// Production builds build a provider from a [`Config`] file (matching the
/// chat/gateway code path). Tests inject a scripted provider so they never
/// need network access.
pub struct AgentLlmFactory {
    inner: AgentLlmFactoryKind,
}

enum AgentLlmFactoryKind {
    /// A provider that can be cloned (behind an Arc) for each session.
    Shared(Arc<dyn LlmProvider>),
    /// A lazy factory that reads config on demand. Used by the production
    /// `McpServeCommand::run_async` path.
    Config(Box<Config>),
}

impl AgentLlmFactory {
    /// Build from a loaded config — used by the production path.
    pub fn from_config(config: Config) -> Result<Self> {
        Ok(Self {
            inner: AgentLlmFactoryKind::Config(Box::new(config)),
        })
    }

    /// Build from a preconstructed provider — used by integration tests.
    pub fn scripted(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            inner: AgentLlmFactoryKind::Shared(provider),
        }
    }

    fn build_provider(&self) -> Result<Arc<dyn LlmProvider>, McpServerError> {
        match &self.inner {
            AgentLlmFactoryKind::Shared(p) => Ok(p.clone()),
            AgentLlmFactoryKind::Config(config) => {
                let provider_name = config
                    .provider
                    .clone()
                    .or_else(|| {
                        config
                            .model
                            .as_deref()
                            .and_then(crate::config::detect_provider)
                            .map(String::from)
                    })
                    .ok_or_else(|| {
                        McpServerError::SessionFailed(
                            "config_error: no LLM provider configured (set provider or model in config.json)".into(),
                        )
                    })?;
                let model = config.model.clone();
                let base_url = config.base_url.clone();
                super::chat::create_provider(&provider_name, config, model, base_url)
                    .map_err(|err| McpServerError::SessionFailed(format!("config_error: {err}")))
            }
        }
    }
}

/// Session dispatch that wires MCP calls into the real agent loop.
///
/// Each `run_session` call:
///
/// * Emits `Running` on the supplied observer.
/// * Builds a fresh [`Agent`] sharing the process-level LLM factory but with
///   per-call episode/memory state so sessions do not alias.
/// * Runs the supplied prompt as a [`Task`], which exercises the full
///   build-messages → call-llm → tool-use → end-turn loop.
/// * Emits `Verifying`, resolves the contract artifact (either the
///   `expected_artifact` field from the MCP input or the workspace contract's
///   primary artifact), and transitions to `Ready`/`Failed`.
///
/// # Sandboxing
///
/// The per-session [`ToolRegistry`] is built with
/// [`ToolRegistry::with_builtins_and_sandbox`] using the
/// [`SessionDispatchConfig::sandbox`] policy (default
/// [`SandboxMode::Auto`](octos_agent::SandboxMode)). This confines
/// `shell`/`exec_command`/`bash` and the file tools to the workspace `cwd`,
/// exactly as `octos chat`/`octos gateway` do. Without it the outer MCP caller
/// — which is only parent-trusted (stdio) or bearer-authenticated (http) — can
/// drive `run_octos_session` to read, write, and execute anywhere the octos
/// process can reach.
pub struct RealSessionDispatch {
    config: SessionDispatchConfig,
    factory: AgentLlmFactory,
}

impl RealSessionDispatch {
    /// Production constructor.
    pub fn new(config: SessionDispatchConfig, factory: AgentLlmFactory) -> Self {
        Self { config, factory }
    }

    /// Alias used by integration tests for visibility.
    pub fn new_for_test(config: SessionDispatchConfig, factory: AgentLlmFactory) -> Self {
        Self::new(config, factory)
    }
}

#[async_trait]
impl McpSessionDispatch for RealSessionDispatch {
    async fn run_session(
        &self,
        contract: &str,
        input: &Value,
        observer: &dyn SessionLifecycleObserver,
    ) -> Result<McpSessionOutcome, McpServerError> {
        observer.mark_state(TaskLifecycleState::Running);

        // Parse MCP input — prompt is required, `expected_artifact` and
        // `artifact_name` are optional hints that override workspace-
        // contract resolution. Everything else is ignored so callers can
        // forward contract-specific payloads without tripping the parser.
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Run the {contract} contract"));
        let expected_artifact = input
            .get("expected_artifact")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let artifact_name = input
            .get("artifact_name")
            .and_then(Value::as_str)
            .unwrap_or("primary");

        // Build the LLM provider and the per-session agent.
        let llm = self.factory.build_provider()?;
        let memory = Arc::new(
            EpisodeStore::open(&self.config.data_dir)
                .await
                .map_err(|err| {
                    McpServerError::SessionFailed(format!(
                        "config_error: open episode store: {err}"
                    ))
                })?,
        );
        // Confine shell/exec/file tools to the workspace. `with_builtins`
        // alone installs `NoSandbox`, which let an outer MCP caller drive
        // `shell`/`exec_command` to read, write, or execute anywhere the octos
        // process could (the M7.2 session-dispatch RCE). The OS sandbox
        // restricts writes to `cwd`.
        let tools = Arc::new(ToolRegistry::with_builtins_and_sandbox(
            &self.config.cwd,
            self.config.sandbox_backend(),
        ));
        let agent_config = AgentConfig {
            max_iterations: self.config.max_iterations,
            // Skip episode persistence — MCP sessions are short-lived and
            // the outer orchestrator owns durability. Writing episodes from
            // every MCP call would also bloat the memory store.
            save_episodes: false,
            ..Default::default()
        };
        let mut agent = Agent::new_shared(
            AgentId::new("mcp-serve"),
            llm.clone(),
            tools.clone(),
            memory,
        )
        .with_config(agent_config);

        // Review A F-004: propagate the workspace policy's declarative
        // compaction contract onto the MCP-served child Agent. Parity with
        // the local chat + session-actor wiring — the MCP-served child
        // session must honour the same preflight-token budget and preserved
        // artifacts declared in workspace_policy.toml.
        if let Ok(Some(workspace_policy)) = octos_agent::read_workspace_policy(&self.config.cwd) {
            if let Some(compaction_policy) = workspace_policy.compaction.clone() {
                use octos_agent::compaction::CompactionRunner;
                use octos_agent::workspace_policy::CompactionSummarizerKind;
                let runner = match compaction_policy.summarizer {
                    CompactionSummarizerKind::LlmIterative => {
                        CompactionRunner::with_provider(compaction_policy, llm.clone())
                    }
                    CompactionSummarizerKind::Extractive => {
                        CompactionRunner::new(compaction_policy)
                    }
                }
                .with_workspace_policy(&workspace_policy);
                agent = agent
                    .with_compaction_runner(Arc::new(runner))
                    .with_compaction_workspace(workspace_policy);
            }
        }

        // Run the task — this is the same code path the local chat command
        // uses. Any LLM/tool errors surface as `eyre::Report`.
        let task = Task::new(
            TaskKind::Custom {
                name: contract.to_string(),
                params: input.clone(),
            },
            TaskContext {
                working_dir: self.config.cwd.clone(),
                working_memory: vec![octos_core::Message {
                    role: octos_core::MessageRole::User,
                    content: prompt,
                    media: vec![],
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    client_message_id: None,
                    thread_id: None,
                    timestamp: chrono::Utc::now(),
                }],
                ..Default::default()
            },
        );

        let task_result = match agent.run_task(&task).await {
            Ok(result) => result,
            Err(err) => {
                observer.mark_state(TaskLifecycleState::Failed);
                return Ok(McpSessionOutcome {
                    final_state: TaskLifecycleState::Failed,
                    artifact_path: None,
                    artifact_content: None,
                    validator_results: Vec::new(),
                    cost: token_usage_to_value(&octos_core::TokenUsage::default()),
                    error: Some(format!("llm_error: {err}")),
                });
            }
        };

        observer.mark_state(TaskLifecycleState::Verifying);

        // Run completion-phase workspace validators when a policy is
        // defined. Results are surfaced via `validator_results` so MCP
        // callers see the same typed outcomes the local spawn pipeline
        // records in its ledger. Missing policy → empty vec (not an error).
        let validator_results = run_completion_validators(&self.config.cwd, contract, &tools).await;

        // Resolve the contract artifact. Precedence:
        //   1. Explicit `expected_artifact` from the MCP input.
        //   2. Workspace contract artifact entry (`artifact_name`).
        //   3. First file in `files_to_send`.
        let artifact_path = resolve_artifact_path(
            &self.config.cwd,
            expected_artifact.as_deref(),
            artifact_name,
            &task_result.files_to_send,
        );

        let cost = token_usage_to_value(&task_result.token_usage);

        // Any required validator failure blocks terminal success — mirrors
        // the local `enforce_spawn_task_contract` path.
        let required_validator_failed = validator_results.iter().any(|value| {
            let required = value
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pass");
            required && status != "pass"
        });
        if required_validator_failed {
            observer.mark_state(TaskLifecycleState::Failed);
            return Ok(McpSessionOutcome {
                final_state: TaskLifecycleState::Failed,
                artifact_path: None,
                artifact_content: None,
                validator_results,
                cost,
                error: Some(
                    "contract_failed: required completion-phase validator failed; hint: inspect the validator_results entries with status != pass before delivering the artifact".into(),
                ),
            });
        }

        match artifact_path {
            Some(path) if path.exists() => {
                // Populate artifact_content only for small text-like files so
                // we do not blow up the MCP response. Callers that need
                // binary bytes should read `artifact_path` themselves.
                let artifact_content = read_small_text_artifact(&path);
                observer.mark_state(TaskLifecycleState::Ready);
                Ok(McpSessionOutcome {
                    final_state: TaskLifecycleState::Ready,
                    artifact_path: Some(path.display().to_string()),
                    artifact_content,
                    validator_results,
                    cost,
                    error: None,
                })
            }
            Some(path) => {
                observer.mark_state(TaskLifecycleState::Failed);
                Ok(McpSessionOutcome {
                    final_state: TaskLifecycleState::Failed,
                    artifact_path: None,
                    artifact_content: None,
                    validator_results,
                    cost,
                    error: Some(format!(
                        "artifact_missing: expected artifact at '{}' but the file is not present; hint: ensure the agent writes the contract artifact before returning",
                        path.display()
                    )),
                })
            }
            None => {
                // No artifact path could be resolved. If the task itself
                // succeeded we still treat this as a contract violation —
                // MCP callers expect a concrete deliverable.
                observer.mark_state(TaskLifecycleState::Failed);
                let hint = if task_result.success {
                    "contract_failed: agent finished without declaring an artifact_path; hint: pass `expected_artifact` in the MCP input or declare the contract artifact in the workspace policy"
                } else {
                    "session_failed: agent loop halted before producing an artifact; hint: check the agent max_iterations budget and LLM provider errors"
                };
                Ok(McpSessionOutcome {
                    final_state: TaskLifecycleState::Failed,
                    artifact_path: None,
                    artifact_content: None,
                    validator_results,
                    cost,
                    error: Some(hint.to_string()),
                })
            }
        }
    }
}

fn token_usage_to_value(usage: &octos_core::TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
    })
}

/// Run completion-phase workspace validators and return their typed outcomes
/// as JSON. No policy or no completion validators → empty vec. Failures
/// during individual validators show up as `status != pass` entries in the
/// returned array — the dispatch blocks terminal success on required misses.
async fn run_completion_validators(
    workspace_root: &std::path::Path,
    contract: &str,
    tools: &Arc<ToolRegistry>,
) -> Vec<Value> {
    let Ok(Some(policy)) = octos_agent::read_workspace_policy(workspace_root) else {
        return Vec::new();
    };
    if policy.validation.validators.is_empty() {
        return Vec::new();
    }
    let runner = ValidatorRunner::new(tools.clone(), workspace_root);
    let invocation = ValidatorInvocation {
        phase: ValidatorPhase::Completion,
        workspace_root: workspace_root.to_path_buf(),
        repo_label: format!("mcp-serve/{contract}"),
        input_args: None,
        tool_output: None,
        spawn_only_files: Vec::new(),
    };
    let outcomes = run_workspace_validators(
        &runner,
        &invocation,
        &policy.validation.validators,
        Some(ValidatorPhase::Completion),
    )
    .await;
    outcomes
        .into_iter()
        .map(|outcome| {
            serde_json::to_value(outcome).unwrap_or_else(
                |_| serde_json::json!({"status": "error", "reason": "serialize failed"}),
            )
        })
        .collect()
}

fn resolve_artifact_path(
    cwd: &std::path::Path,
    expected: Option<&std::path::Path>,
    artifact_name: &str,
    files_to_send: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(path) = expected {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return Some(absolute);
    }
    if let Ok(Some(policy)) = octos_agent::read_workspace_policy(cwd) {
        if let Some(pattern) = policy.artifacts.entries.get(artifact_name) {
            let candidate = if std::path::Path::new(pattern).is_absolute() {
                PathBuf::from(pattern)
            } else {
                cwd.join(pattern)
            };
            // Only return a direct file match — globs are deliberately left
            // to the workspace-contract enforcement path. This keeps the
            // MCP dispatch cheap and dependency-free.
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    files_to_send.iter().find(|p| p.is_file()).cloned()
}

fn read_small_text_artifact(path: &std::path::Path) -> Option<String> {
    const MAX_INLINE_BYTES: u64 = 64 * 1024;
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_INLINE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_transport_parses() {
        let cmd: McpServeCommand = McpServeCommand {
            transport: McpTransport::Http,
            bind: "127.0.0.1:4033".parse().unwrap(),
            cwd: None,
            data_dir: None,
            config: None,
        };
        assert!(matches!(cmd.transport, McpTransport::Http));
    }

    #[test]
    fn sandbox_backend_honors_config_and_defaults_to_auto() {
        let mk = |sandbox: SandboxConfig| SessionDispatchConfig {
            cwd: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            max_iterations: 4,
            sandbox,
        };

        // A disabled policy must produce a pass-through backend, proving the
        // dispatch reads the configured policy rather than hardcoding one.
        let disabled = mk(SandboxConfig {
            enabled: false,
            ..Default::default()
        });
        let program = disabled
            .sandbox_backend()
            .wrap_command("true", std::path::Path::new("."))
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        assert!(
            program == "sh" || program == "cmd",
            "disabled sandbox must pass through, got {program:?}"
        );

        // The default policy is enabled + Auto — never NoSandbox by omission,
        // which was the M7.2 dispatch RCE (`with_builtins` hardcoded NoSandbox).
        let default = mk(SandboxConfig::default());
        assert!(default.sandbox.enabled);
        assert_eq!(default.sandbox.mode, octos_agent::SandboxMode::Auto);
    }

    #[test]
    fn resolve_artifact_prefers_expected_path_when_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("out.bin");
        std::fs::write(&expected, b"x").unwrap();
        let resolved = resolve_artifact_path(dir.path(), Some(&expected), "primary", &[]).unwrap();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_artifact_falls_back_to_files_to_send_when_no_expected() {
        let dir = tempfile::tempdir().unwrap();
        let delivered = dir.path().join("delivered.txt");
        std::fs::write(&delivered, b"hello").unwrap();
        let resolved = resolve_artifact_path(
            dir.path(),
            None,
            "primary",
            std::slice::from_ref(&delivered),
        );
        assert_eq!(resolved.as_ref(), Some(&delivered));
    }

    #[test]
    fn token_usage_to_value_includes_all_counters() {
        let usage = octos_core::TokenUsage {
            input_tokens: 12,
            output_tokens: 7,
            reasoning_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
        };
        let value = token_usage_to_value(&usage);
        assert_eq!(value["input_tokens"], 12);
        assert_eq!(value["output_tokens"], 7);
        assert_eq!(value["reasoning_tokens"], 3);
        assert_eq!(value["cache_read_tokens"], 2);
        assert_eq!(value["cache_write_tokens"], 1);
    }

    #[test]
    fn read_small_text_artifact_returns_none_for_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // Write 128 KiB — above the 64 KiB inline ceiling.
        let payload = vec![b'A'; 128 * 1024];
        std::fs::write(&path, payload).unwrap();
        assert!(read_small_text_artifact(&path).is_none());
    }

    #[test]
    fn read_small_text_artifact_inlines_small_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(read_small_text_artifact(&path).as_deref(), Some("hello"));
    }
}

#[cfg(all(test, feature = "api"))]
mod http_transport_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use octos_agent::mcp_server::{
        McpServer, McpServerError, McpSessionDispatch, McpSessionOutcome, SessionLifecycleObserver,
    };
    use octos_agent::task_supervisor::{TaskLifecycleState, TaskSupervisor};
    use serde_json::Value;

    use super::mcp_http_router;

    struct ReadyDispatch;

    #[async_trait]
    impl McpSessionDispatch for ReadyDispatch {
        async fn run_session(
            &self,
            _contract: &str,
            _input: &Value,
            observer: &dyn SessionLifecycleObserver,
        ) -> Result<McpSessionOutcome, McpServerError> {
            observer.mark_state(TaskLifecycleState::Ready);
            Ok(McpSessionOutcome {
                final_state: TaskLifecycleState::Ready,
                artifact_path: None,
                artifact_content: None,
                validator_results: vec![],
                cost: serde_json::json!({}),
                error: None,
            })
        }
    }

    /// Bind the Streamable HTTP router on an ephemeral loopback port and return
    /// its address. The server task is detached; it stops when the test's
    /// runtime is dropped.
    async fn spawn_http_server(token: &str) -> std::net::SocketAddr {
        let server = McpServer::new(Arc::new(ReadyDispatch), Arc::new(TaskSupervisor::new()));
        let (app, _cancel) = mcp_http_router(server, token.to_string(), false);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        addr
    }

    /// The bearer gate must reject missing/wrong tokens with 401 before the
    /// request reaches the rmcp service, and let a correct token through.
    #[tokio::test]
    async fn http_transport_gates_on_bearer_token() {
        let addr = spawn_http_server("super-secret").await;
        let url = format!("http://{addr}/mcp");
        let client = reqwest::Client::new();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

        // No Authorization header -> 401.
        let resp = client.post(&url).body(body).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Wrong token -> 401.
        let resp = client
            .post(&url)
            .header("Authorization", "Bearer nope")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Correct token passes the gate and reaches rmcp. rmcp may reject the
        // bare body with a 4xx for missing MCP headers/session, but never 401.
        let resp = client
            .post(&url)
            .header("Authorization", "Bearer super-secret")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "a correct bearer token must pass the gate"
        );
    }
}
