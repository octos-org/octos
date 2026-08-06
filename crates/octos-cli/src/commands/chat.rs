//! Chat command: interactive multi-turn conversation with an agent.

use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Args, ValueEnum};
use colored::Colorize;
use eyre::{Result, WrapErr, eyre};
use octos_agent::compaction::CompactionRunner;
use octos_agent::{
    Agent, AgentConfig, CompactionSummarizerKind, ConsoleReporter, ConversationResponse,
    HookExecutor, ProgressReporter, SilentReporter, ToolApprovalDecision, ToolApprovalRequest,
    ToolApprovalRequester, ToolRegistry, UserQuestionOutcome, UserQuestionRequest,
    UserQuestionRequester, read_workspace_policy,
};
use octos_core::ui_protocol::UserQuestionAnswer;
use octos_core::{AgentId, Message, MessageRole, SessionScope};
use octos_llm::{
    AdaptiveConfig, AdaptiveRouter, EmbeddingProvider, LlmProvider, OpenAIEmbedder, ProviderChain,
    RetryProvider,
};
use octos_memory::{EpisodeStore, MemoryStore};
use rustyline::DefaultEditor;

use super::Executable;
use crate::config::Config;

/// Interactive multi-turn chat with an agent.
///
/// `Serialize`/`Deserialize` back the layered startup config (see
/// [`crate::config_layer`]): non-explicit fields fall back to
/// `config.cli.chat`.
#[derive(Debug, Args, serde::Serialize, serde::Deserialize, PartialEq)]
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

    /// API wire protocol to speak to `--base-url` (overrides config's
    /// `api_type`): `anthropic`, `openai`, or `responses`. Use this for a
    /// custom endpoint that speaks a known protocol (e.g. a z.ai/GLM Anthropic
    /// endpoint) instead of overloading `--provider` with a vendor name.
    #[arg(long = "api-type", visible_alias = "api-style")]
    pub api_type: Option<String>,

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

    /// Emit the result as a single JSON object on stdout (logs + UI go to
    /// stderr); intended for scripting / one-shot `--message` use. Requires
    /// `--message`: interactive `--json` is rejected (a REPL cannot keep
    /// stdout pure). On any runtime error a `{"error": "..."}` object is
    /// printed to stdout and the process exits non-zero, so stdout is always
    /// parseable — for a valid invocation. (Argument errors and `--help` are
    /// handled by the arg parser and follow normal CLI conventions: usage/help
    /// text and a non-zero exit, not a JSON object.)
    #[arg(long)]
    pub json: bool,

    /// Runtime profile to apply at startup (M8.3). Accepts a built-in name
    /// (`coding`, `coding-full`, `swarm`), a user-dir id under
    /// `~/.octos/profiles/<id>/`, or an explicit path to a profile
    /// JSON/TOML file.
    ///
    /// If the id names a stored serve/onboarding profile (one created by
    /// `octos serve` or octoscode, saved as `~/.octos/profiles/<id>.json`),
    /// its LLM provider/model, route, and API key (`env_vars`) are reused too —
    /// so you don't re-enter a model or key that a profile already holds.
    /// `--config`, `--provider`, and `--model` still override.
    ///
    /// Defaults to `coding`, the lean core-coding tool surface (files,
    /// shell, search, memory, spawn, user questions). Use `coding-full`
    /// for the unfiltered pre-lean tool set (web, research, pipelines,
    /// bundled skills).
    #[arg(long)]
    pub profile: Option<String>,

    /// FULL AUTONOMY ("yolo"): bypass all approvals AND the sandbox — the
    /// agent can edit any file and run any command with network access,
    /// without asking. Equivalent to `--sandbox danger-full-access`. Only
    /// safe on a local single-user box you trust; risks data loss.
    ///
    /// `octos chat` is inherently local single-user, so this resolves through
    /// the solo runtime mode. The `--yolo` alias is hidden for brevity.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        visible_alias = "yolo",
        default_value_t = false
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Sandbox / filesystem reach (codex parity). One of `read-only`,
    /// `workspace-write` (default), or `danger-full-access`. Mutually
    /// exclusive with a non-danger combination of `--yolo`.
    #[arg(long, value_enum)]
    pub sandbox: Option<ChatSandboxMode>,

    /// When to ask for command approval (codex parity). `ask` (default)
    /// prompts for risky commands; `never` fails them closed at the tool
    /// boundary instead of prompting.
    #[arg(long, value_enum)]
    pub ask_for_approval: Option<ChatApprovalMode>,

    /// Reasoning effort for thinking models: `low`, `medium`, `high`, or
    /// `max` (claude/codex parity). Overrides the config
    /// `gateway.reasoning_effort`; non-thinking models ignore it, and
    /// providers without a distinct `max` tier clamp it to `high`.
    #[arg(long, value_enum)]
    pub effort: Option<ChatEffort>,

    /// Do NOT persist this run as an episode (ephemeral). Mirrors
    /// `claude --no-session-persistence`: by default a completed turn is
    /// saved to the episode store for future recall; this skips that write.
    ///
    /// This also lets many `octos chat` agents run CONCURRENTLY against one
    /// `--data-dir` (hence one shared `--profile`): a normal run takes an
    /// exclusive lock on the data dir's episode DB, but an ephemeral run falls
    /// back to an in-memory episode handle when the lock is already held,
    /// instead of failing. Pass this to fan out parallel agents on a single
    /// profile — e.g. review agents over one repo, or edit agents each in
    /// their own `--cwd`.
    #[arg(long)]
    pub no_session_persistence: bool,

    /// One-shot PROMPT (positional): `octos chat "…"` runs a single turn and
    /// exits, matching `claude -p "…"`. Sugar for `--message`; supplying the
    /// prompt both positionally and via `--message` is an error.
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,
}

/// `--sandbox` choices, mirroring codex's sandbox modes and octos's
/// [`PermissionProfile`](octos_agent::PermissionProfile).
///
/// `rename_all = "kebab-case"` makes the serde encoding (`"workspace-write"`,
/// …) identical to clap's `ValueEnum` possible-value names, so the config
/// layering round-trips a `config.cli.chat.sandbox` value losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatSandboxMode {
    /// Read-only workspace access; write/edit tools fail.
    ReadOnly,
    /// Read/write inside the workspace (default).
    WorkspaceWrite,
    /// No sandbox, host filesystem, network on, approvals never ("yolo").
    DangerFullAccess,
}

/// Host-managed paths used by a chat session.
///
/// `--sandbox read-only` constrains both agent tools and Octos's own startup
/// work: bundled-skill bootstrap and spawned-worker deliverables must not
/// create `cwd/.octos`. Existing project state remains readable for local
/// plugins and bootstrap prompt files.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatRuntimePaths {
    /// Existing workspace-local state, read-only in a read-only session.
    project_dir: PathBuf,
    /// Root for host-generated bundled skills.
    bootstrap_dir: PathBuf,
    /// Per-chat root for spawn deliverables.
    spawn_deliverable_dir: PathBuf,
}

fn resolve_chat_runtime_paths(
    cwd: &std::path::Path,
    data_dir: &std::path::Path,
    permissions: octos_agent::EffectivePermissions,
    run_id: &str,
) -> ChatRuntimePaths {
    let project_dir = cwd.join(".octos");
    if permissions.file_access.allows_write() {
        return ChatRuntimePaths {
            bootstrap_dir: project_dir.clone(),
            spawn_deliverable_dir: project_dir.join("spawn-deliverables"),
            project_dir,
        };
    }

    // Bootstrap performs direct filesystem writes. Keep every read-only chat
    // in a private runtime subtree so concurrent sessions cannot race over
    // generated bundled skills or see each other's plugin state.
    let run_dir = data_dir.join("runtime").join("runs").join(run_id);
    ChatRuntimePaths {
        project_dir,
        bootstrap_dir: run_dir.join("bootstrap"),
        spawn_deliverable_dir: run_dir.join("spawn-deliverables"),
    }
}

/// `--ask-for-approval` choices, mirroring codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatApprovalMode {
    /// Prompt for approval on risky commands (default).
    Ask,
    /// Never prompt; risky commands fail closed at the tool boundary.
    Never,
}

/// `--effort` choices, mirroring claude/codex reasoning-effort tiers. Maps
/// 1:1 to [`octos_llm::ReasoningEffort`]. For these single-word variants
/// clap's default `ValueEnum` naming and serde's `kebab-case` agree
/// (`low`/`medium`/`high`/`max`), so a `config.cli.chat.effort` round-trips
/// losslessly — matching [`ChatSandboxMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatEffort {
    Low,
    Medium,
    High,
    Max,
}

impl From<ChatEffort> for octos_llm::ReasoningEffort {
    fn from(effort: ChatEffort) -> Self {
        match effort {
            ChatEffort::Low => octos_llm::ReasoningEffort::Low,
            ChatEffort::Medium => octos_llm::ReasoningEffort::Medium,
            ChatEffort::High => octos_llm::ReasoningEffort::High,
            ChatEffort::Max => octos_llm::ReasoningEffort::Max,
        }
    }
}

/// `claude -p "…"` parity: fold a positional PROMPT into `--message`. The two
/// are two spellings of the same one-shot prompt, so supplying both is
/// contradictory — fail closed. Returns the single effective one-shot message,
/// or `None` for interactive mode (neither given).
fn reconcile_one_shot_prompt(
    message: Option<String>,
    prompt: Option<String>,
) -> Result<Option<String>> {
    match (message, prompt) {
        (Some(_), Some(_)) => Err(eyre!(
            "provide the prompt positionally OR via --message/-m, not both"
        )),
        (Some(message), None) => Ok(Some(message)),
        (None, prompt) => Ok(prompt),
    }
}

/// Apply the "explicit `--provider` detaches the inherited route" rule.
///
/// When the CLI names a provider that DIFFERS from the one already resolved
/// into `config` (typically inherited from a `--profile`), the sibling route
/// fields carried by that config — `base_url`, `api_key_env`, `api_type` — are
/// cleared so the *new* provider's defaults (or an explicit `--base-url` /
/// `--api-type` / key env) apply instead of a stale, mismatched route. Without
/// this, `octos chat --profile p --provider anthropic` keeps `p`'s openai
/// key-env + endpoint + wire protocol and builds an incoherent client
/// (anthropic name talking to openai's URL with openai's key). Naming the SAME
/// provider — or naming none, or having no inherited provider to detach from —
/// leaves the route untouched.
///
/// The model is deliberately NOT cleared: some providers mark the model as
/// required (`create_provider_with_api_type` bails when it is missing), so
/// blanking it would turn a provider swap into a hard error. Callers complete a
/// cross-provider switch by also passing `--model`.
fn detach_route_on_provider_override(config: &mut Config, cli_provider: Option<&str>) {
    let detaches = matches!(
        (cli_provider, config.provider.as_deref()),
        (Some(cli), Some(cfg)) if cli != cfg
    );
    if detaches {
        config.base_url = None;
        config.api_key_env = None;
        config.api_type = None;
    }
}

/// Resolve the chat session's [`EffectivePermissions`] from the CLI flags.
///
/// yolo GAP #3 — codex parity. Precedence and conflict rules:
///   * `--yolo` (a.k.a. `--dangerously-bypass-approvals-and-sandbox`) and
///     `--sandbox danger-full-access` both select the dangerous profile.
///   * `--yolo` combined with an explicit NON-danger `--sandbox` is
///     contradictory and errors (fail closed).
///   * `--ask-for-approval` overrides the approval policy, EXCEPT it may not
///     contradict the dangerous profile (which is always `never`).
///
/// `octos chat` is inherently local single-user, so the dangerous profile is
/// resolved through [`RuntimeMode::Solo`](octos_agent::RuntimeMode) — a
/// legitimate use of solo here, unlike the multi-tenant `serve` path.
pub fn resolve_chat_permissions(
    yolo: bool,
    sandbox: Option<ChatSandboxMode>,
    approval: Option<ChatApprovalMode>,
) -> Result<octos_agent::EffectivePermissions> {
    use octos_agent::{ApprovalPolicy, EffectivePermissions, PermissionProfile, RuntimeMode};

    // Determine the requested profile, folding `--yolo` and `--sandbox`
    // together and rejecting contradictions.
    let profile = match (yolo, sandbox) {
        // `--yolo` alone, or `--yolo`/`--sandbox danger-full-access` agreeing.
        (true, None) | (true, Some(ChatSandboxMode::DangerFullAccess)) => {
            PermissionProfile::DangerFullAccess
        }
        // `--yolo` with a non-danger sandbox is contradictory.
        (true, Some(other)) => {
            return Err(eyre!(
                "--dangerously-bypass-approvals-and-sandbox (--yolo) conflicts with \
                 --sandbox {:?}: yolo implies danger-full-access",
                other
            ));
        }
        (false, Some(ChatSandboxMode::ReadOnly)) => PermissionProfile::ReadOnly,
        (false, Some(ChatSandboxMode::WorkspaceWrite)) | (false, None) => {
            PermissionProfile::WorkspaceWrite
        }
        (false, Some(ChatSandboxMode::DangerFullAccess)) => PermissionProfile::DangerFullAccess,
    };

    // `octos chat` is local single-user; the dangerous profile is legitimately
    // resolved via Solo. `for_runtime` still centralizes the danger gate.
    let mut permissions = EffectivePermissions::for_runtime(profile, RuntimeMode::Solo)
        .map_err(|err| eyre!("{err}"))?;

    // Apply an explicit `--ask-for-approval` override, guarding against a
    // contradiction with the always-`never` dangerous profile.
    if let Some(approval) = approval {
        let requested = match approval {
            ChatApprovalMode::Ask => ApprovalPolicy::Ask,
            ChatApprovalMode::Never => ApprovalPolicy::Never,
        };
        if permissions.is_dangerous() && requested != ApprovalPolicy::Never {
            return Err(eyre!(
                "--ask-for-approval ask conflicts with danger-full-access, \
                 which never asks for approval"
            ));
        }
        permissions = permissions.with_approval_policy(requested);
    }

    Ok(permissions)
}

/// Bind every loaded [`octos_agent::plugins::PluginTool`] in `tools` to the
/// resolved chat working directory.
///
/// yolo GAP #4: unlike `octos serve` (whose `SessionRuntime::bootstrap`
/// calls `rebind_plugin_work_dirs`), `octos chat` loads plugins with
/// `work_dir: None` and never rebinds them. `PluginTool::execute` derives
/// its `current_dir`/`OCTOS_WORK_DIR` from `ctx.session_scope` ONLY when its
/// own `work_dir` is unset — and under a Host-scope (`--yolo`) session the
/// scope is deliberately omitted (so host-reaching file tools keep host
/// access). With BOTH `work_dir: None` and `session_scope: None`, plugins run
/// in the process LAUNCH directory instead of the requested `--cwd`, breaking
/// relative plugin inputs/outputs.
///
/// Binding the work_dir at registration (via the registry's existing
/// [`ToolRegistry::rebind_plugin_work_dirs`] path) fixes the Host case while
/// staying a no-op for the Workspace case — there `work_dir` == `cwd` ==
/// `scope.workspace()`, and `execute` prefers `work_dir` first, so the
/// resolved directory is unchanged. Only plugin working directory is
/// affected; the Host-scope decision for FILE tools is untouched.
fn bind_chat_plugin_work_dirs(tools: &mut ToolRegistry, cwd: &std::path::Path) {
    tools.rebind_plugin_work_dirs(cwd);
}

/// Exit commands.
const EXIT_COMMANDS: &[&str] = &["exit", "quit", "/exit", "/quit", ":q"];

/// Serializes ALL interactive stdin prompts (approvals AND user questions):
/// if two prompt-raising tools run in the same turn, their stdin prints/reads
/// must not interleave — otherwise a single `y` or a picked number could land
/// on whichever request won the stdin race rather than the one the user
/// meant. One module-level lock shared by both requesters — a function-local
/// `static` would be a *distinct* mutex per function and not serialize an
/// approval against a question (codex review).
static CHAT_PROMPT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the user answered at the CLI approval prompt. `ApproveSession`
/// mirrors the TUI's `s` action / the serve `approval_scope: "session"`
/// (`ApprovalScopeKind::ApproveForSession`): every later approval-gated
/// request in this chat process auto-resolves without prompting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliApprovalAnswer {
    ApproveOnce,
    ApproveSession,
    Deny,
}

/// Parse the `[y/s/N]` answer line. Empty / unrecognized input denies —
/// same fail-closed default as the old `[y/N]` prompt.
fn parse_cli_approval_answer(line: &str) -> CliApprovalAnswer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => CliApprovalAnswer::ApproveOnce,
        "s" | "session" => CliApprovalAnswer::ApproveSession,
        _ => CliApprovalAnswer::Deny,
    }
}

#[derive(Default)]
struct CliApprovalRequester {
    /// Set once the user answers `s` — the CLI-chat equivalent of the serve
    /// scope table's `(ApproveForSession, MatchKey::Session)` entry, which
    /// auto-resolves every subsequent approval in the session. Scope lifetime
    /// is this chat process (serve evicts its entry on session close).
    session_approved: std::sync::atomic::AtomicBool,
}

impl CliApprovalRequester {
    fn session_approved(&self) -> bool {
        self.session_approved
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl ToolApprovalRequester for CliApprovalRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        // Fast path: a prior `s` answer auto-resolves without prompting
        // (mirrors serve's `approval_auto_resolved`). Print a note so the
        // grant stays visible instead of commands silently running.
        if self.session_approved() {
            eprintln!(
                "{} {}",
                "Auto-approved (session scope):".dimmed(),
                request
                    .command
                    .as_deref()
                    .unwrap_or(&request.title)
                    .dimmed()
            );
            return ToolApprovalDecision::Approve;
        }
        let _guard = CHAT_PROMPT_LOCK.lock().await;
        // Re-check after acquiring the lock: a parallel tool batch can queue
        // two prompts; if the first answer was `s`, the second must
        // auto-resolve instead of prompting again.
        if self.session_approved() {
            eprintln!(
                "{} {}",
                "Auto-approved (session scope):".dimmed(),
                request
                    .command
                    .as_deref()
                    .unwrap_or(&request.title)
                    .dimmed()
            );
            return ToolApprovalDecision::Approve;
        }
        let answer = tokio::task::spawn_blocking(move || prompt_for_cli_approval(request))
            .await
            .unwrap_or(CliApprovalAnswer::Deny);
        match answer {
            CliApprovalAnswer::ApproveOnce => ToolApprovalDecision::Approve,
            CliApprovalAnswer::ApproveSession => {
                self.session_approved
                    .store(true, std::sync::atomic::Ordering::Release);
                ToolApprovalDecision::Approve
            }
            CliApprovalAnswer::Deny => ToolApprovalDecision::Deny,
        }
    }
}

fn prompt_for_cli_approval(request: ToolApprovalRequest) -> CliApprovalAnswer {
    eprintln!();
    eprintln!("{}", "Approval required".yellow().bold());
    eprintln!("{}", request.title.bold());
    eprintln!("{}", request.body);
    if let Some(cwd) = request.cwd.as_deref() {
        eprintln!("cwd: {cwd}");
    }

    if !io::stdin().is_terminal() {
        eprintln!("No interactive terminal available; denying request.");
        return CliApprovalAnswer::Deny;
    }

    eprint!("Approve? [y]es once / [s]ession / [N]o ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    match io::stdin().read_line(&mut answer) {
        Ok(_) => parse_cli_approval_answer(&answer),
        Err(_) => CliApprovalAnswer::Deny,
    }
}

struct CliUserQuestionRequester;

#[async_trait::async_trait]
impl UserQuestionRequester for CliUserQuestionRequester {
    async fn request_user_question(&self, request: UserQuestionRequest) -> UserQuestionOutcome {
        // Shared CHAT_PROMPT_LOCK: an approval and a question in the same
        // turn must not interleave their stdin reads.
        let _guard = CHAT_PROMPT_LOCK.lock().await;
        tokio::task::spawn_blocking(move || prompt_for_cli_user_question(request))
            .await
            .unwrap_or(UserQuestionOutcome::Cancelled)
    }
}

fn prompt_for_cli_user_question(request: UserQuestionRequest) -> UserQuestionOutcome {
    // No terminal to prompt on → let the tool degrade to its structured
    // fallback (the model re-asks in plain text) rather than block forever.
    if !io::stdin().is_terminal() {
        return UserQuestionOutcome::Unsupported;
    }

    eprintln!();
    eprintln!("{}", "Agent needs your input".cyan().bold());
    if !request.title.is_empty() {
        eprintln!("{}", request.title.bold());
    }
    if !request.body.is_empty() {
        eprintln!("{}", request.body);
    }

    let total = request.questions.len();
    let mut answers: Vec<UserQuestionAnswer> = Vec::with_capacity(total);

    for (qi, question) in request.questions.iter().enumerate() {
        eprintln!();
        if total > 1 {
            eprintln!("{}", format!("Question {} of {}", qi + 1, total).dimmed());
        }
        eprintln!("{}", question.question.bold());

        // Numbered options, then an "Other" row when free text is offered.
        for (oi, opt) in question.options.iter().enumerate() {
            if opt.description.is_empty() {
                eprintln!("  {}. {}", oi + 1, opt.label);
            } else {
                eprintln!("  {}. {} — {}", oi + 1, opt.label, opt.description.dimmed());
            }
        }
        let other_index = question.options.len() + 1;
        if question.allow_free_text {
            eprintln!("  {other_index}. {}", "Other (type your own)".dimmed());
        }

        if question.multi_select {
            eprint!("Choose one or more, comma-separated [1]: ");
        } else {
            eprint!("Choose [1]: ");
        }
        let _ = io::stderr().flush();

        let mut line = String::new();
        if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            // EOF (Ctrl-D) — abandon the whole question.
            return UserQuestionOutcome::Cancelled;
        }
        let (selected_labels, other_picked) = parse_question_selection(question, &line);

        // Read the free-text answer only when "Other" was chosen.
        let mut free_text: Option<String> = None;
        if other_picked {
            eprint!("Your answer: ");
            let _ = io::stderr().flush();
            let mut ft = String::new();
            if io::stdin().read_line(&mut ft).unwrap_or(0) == 0 {
                return UserQuestionOutcome::Cancelled;
            }
            let ft = ft.trim().to_string();
            if !ft.is_empty() {
                free_text = Some(ft);
            }
        }

        answers.push(UserQuestionAnswer {
            selected_labels,
            free_text,
        });
    }

    UserQuestionOutcome::Answered(answers)
}

/// Parse the numbered-selection `line` for one question into (option labels,
/// was-"Other"-picked). Empty input defaults to option 1. Out-of-range and
/// non-numeric tokens are dropped; a single-select question keeps only the
/// first valid pick. The "Other" row is index `options.len() + 1` and is only
/// honoured when the question allows free text. Pure so it can be unit-tested
/// without stdin.
fn parse_question_selection(
    question: &octos_core::ui_protocol::UserQuestion,
    line: &str,
) -> (Vec<String>, bool) {
    let other_index = question.options.len() + 1;
    // The "Other" row is only selectable when the question allows free text.
    let max_index = if question.allow_free_text {
        other_index
    } else {
        question.options.len()
    };
    let trimmed = line.trim();
    let mut picks: Vec<usize> = trimmed
        .split(',')
        .filter_map(|tok| tok.trim().parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= max_index)
        .collect();
    // Empty or all-invalid input falls back to the first option (the "[1]"
    // default shown in the prompt) so a stray keystroke still yields an answer.
    if picks.is_empty() {
        picks.push(1);
    }
    if !question.multi_select {
        picks.truncate(1);
    }

    let mut selected_labels = Vec::new();
    let mut other_picked = false;
    for n in picks {
        // `n == other_index` is only reachable when free text is allowed
        // (`max_index` excludes it otherwise), so it always means "Other".
        if n == other_index {
            other_picked = true;
        } else if let Some(opt) = question.options.get(n - 1) {
            selected_labels.push(opt.label.clone());
        }
    }
    (selected_labels, other_picked)
}

async fn with_chat_approval<F, T>(
    approval_requester: Arc<dyn ToolApprovalRequester>,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    // Scope BOTH the approval and the user-question requesters for the turn,
    // so `ask_user_question` renders the interactive numbered prompt instead
    // of degrading to Unsupported (the model re-asking in plain text).
    let uq_requester: Arc<dyn UserQuestionRequester> = Arc::new(CliUserQuestionRequester);
    octos_agent::tools::USER_QUESTION_CTX
        .scope(
            uq_requester,
            octos_agent::tools::TOOL_APPROVAL_CTX.scope(approval_requester, future),
        )
        .await
}

async fn process_chat_turn(
    agent: &Agent,
    input: &str,
    history: &[Message],
    approval_requester: Arc<dyn ToolApprovalRequester>,
) -> Result<ConversationResponse> {
    with_chat_approval(
        approval_requester,
        agent.process_message(input, history, vec![]),
    )
    .await
}

/// Machine-readable result envelope for `octos chat --json --message`.
///
/// Serialized as a single JSON object — the ONLY thing `--json` writes to
/// stdout on success. Deliberately limited to the fields the completed
/// [`ConversationResponse`] reports as ground truth (final text, answering
/// model, token usage). Turn telemetry — a real terminal stop reason, the loop
/// iteration count, the executed tool-call count — is intentionally omitted:
/// `ConversationResponse` does not carry those, and deriving them from the
/// message list is wrong in edge cases (budget / max-token cutoffs mislabel the
/// stop reason; the loop detector inserts synthetic tool-result rows that
/// overstate tool calls). Surfacing them correctly needs an octos-agent change
/// to populate them at every turn return site — deferred to a follow-up.
#[derive(Debug, serde::Serialize)]
struct ChatJsonResult {
    /// Final assistant text for this turn.
    text: String,
    /// Model that actually produced the answer (e.g. `glm-5.2`). Taken from the
    /// final reply's provider provenance, so adaptive failover to a fallback
    /// lane is reported honestly.
    model: String,
    /// Prompt tokens consumed across the turn.
    input_tokens: u32,
    /// Completion tokens produced across the turn.
    output_tokens: u32,
    // NOTE: intentionally limited to fields `ConversationResponse` reports as
    // ground truth. Turn telemetry (a real terminal `stop_reason`, the loop
    // iteration count, executed tool-call count) is deferred to a follow-up:
    // `ConversationResponse` does not carry those, and deriving them from
    // `messages` is wrong in edge cases (budget/max-token cutoffs mislabel the
    // stop reason; the loop detector inserts synthetic tool-result rows that
    // overstate tool calls). Surfacing them correctly needs an octos-agent
    // change to populate them at every turn return site.
}

impl ChatJsonResult {
    /// Build the envelope from a completed turn. `fallback_model` is the
    /// configured model id, used only when the reply carries no provider
    /// provenance.
    fn from_response(response: &ConversationResponse, fallback_model: &str) -> Self {
        // Prefer the provenance of the FINAL reply: adaptive failover can answer
        // from a different lane than the primary provider, and the caller should
        // see the model that actually produced the text.
        let model = response
            .provider_metadata
            .as_ref()
            .map(|m| m.model.clone())
            .unwrap_or_else(|| fallback_model.to_string());
        Self {
            text: response.content.clone(),
            model,
            input_tokens: response.token_usage.input_tokens,
            output_tokens: response.token_usage.output_tokens,
        }
    }

    /// Serialize to a single-line JSON string (nice for piping). This fixed
    /// string/number-only struct cannot realistically fail to serialize; if it
    /// ever did, fall back to a valid JSON error object so stdout stays
    /// parseable.
    fn to_json_line(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize chat result\"}".to_string())
    }
}

/// Emit `{"error": "<message>"}` on stdout (so a `--json` caller can always
/// parse stdout) and flush. Used on the `--json` error paths before a
/// non-zero exit; `serde_json` handles all string escaping.
fn print_json_error(message: &str) {
    let obj = serde_json::json!({ "error": message });
    println!("{obj}");
    let _ = io::stdout().flush();
}

impl Executable for ChatCommand {
    fn execute(self) -> Result<()> {
        // Capture before `self` is moved: in `--json` mode ANY failure —
        // provider/config bootstrap or the turn itself — must surface as a
        // `{"error": ...}` object on stdout with a non-zero exit, so a caller
        // can always parse stdout instead of hitting an empty stream + an eyre
        // report on stderr.
        let json = self.json;
        // Build the runtime and run the turn inside one fallible block so that
        // EVERY failure — including a runtime-build failure — is caught by the
        // `--json` arm below rather than escaping as a bare `?` before it.
        let outcome = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(8 * 1024 * 1024) // 8MB stack for deep agent futures
                .build()
                .wrap_err("failed to create tokio runtime")?;
            runtime.block_on(self.run_async())
        })();
        match outcome {
            Ok(()) => Ok(()),
            Err(error) if json => {
                print_json_error(&error.to_string());
                std::process::exit(1);
            }
            Err(error) => Err(error),
        }
    }
}

impl ChatCommand {
    async fn run_async(mut self) -> Result<()> {
        // `claude -p "…"` parity: fold a positional PROMPT into `--message` up
        // front so every downstream `self.message` check (the `--json` guard
        // just below and the one-shot dispatch) sees a single source of truth.
        self.message = reconcile_one_shot_prompt(self.message.take(), self.prompt.take())?;

        // `--json` is a one-shot, machine-readable mode: exactly one JSON
        // object on stdout, and it requires `--message`. An interactive REPL
        // cannot keep stdout pure (the readline prompt and per-turn framing
        // would interleave with the JSON), so reject `--json` without
        // `--message` up front — emitted as a JSON error so even this failure
        // is parseable on stdout — before doing any provider/agent bootstrap.
        if self.json && self.message.is_none() {
            print_json_error("--json requires --message (interactive --json is not supported)");
            std::process::exit(2);
        }

        let cwd = match self.cwd {
            Some(p) => p,
            None => std::env::current_dir().wrap_err("failed to get current directory")?,
        };

        // Resolve the canonical config context (data_dir, config_home,
        // auth_home, is_default) once and run migrations.
        let ctx = super::resolve_command_context(self.data_dir)?;
        let data_dir = ctx.data_dir.clone();

        // Load config. Precedence: an explicit `--config` wins; otherwise a
        // stored serve/onboarding profile named by `--profile <id>` supplies its
        // LLM provider/model + route + `env_vars` API key (so `octos chat
        // --profile <id>` reuses an octoscode / `serve` profile without a
        // separate flat config or a duplicated key); otherwise the ambient
        // config context.
        let serve_profile_config = load_serve_profile_config(self.profile.as_deref(), &data_dir)?;
        let profile_is_serve = serve_profile_config.is_some();
        let mut config = if let Some(config_path) = &self.config {
            Config::from_file(config_path)?
        } else if let Some(profile_config) = serve_profile_config {
            profile_config
        } else {
            Config::load_with_context(&cwd, &ctx)?
        };

        // An explicit `--provider` that names a DIFFERENT provider than the
        // inherited one does a clean switch: detach the profile's route so the
        // new provider's defaults (or explicit `--base-url`/`--api-type`/key)
        // apply, instead of silently keeping the old provider's key-env +
        // endpoint. No-op when the provider matches or is unset.
        detach_route_on_provider_override(&mut config, self.provider.as_deref());

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

        // Create LLM provider (with optional failover chain). `--api-type`
        // (alias `--api-style`) overrides config's `api_type` so a custom
        // `--base-url` can pick the wire protocol without pretending to be a
        // vendor via `--provider`.
        let api_type = self.api_type.as_deref().or(config.api_type.as_deref());
        let base_provider: Arc<dyn LlmProvider> =
            create_provider_with_api_type(&provider_name, &config, model, base_url, api_type)?;
        // Status line goes to stderr; suppress it entirely in `--json` mode to
        // keep even stderr quiet for scripting.
        if !self.json {
            eprintln!("{}: {}", "Model".green(), base_provider.model_id());
        }
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

        // Episode store. Normally strict-open (the canonical single writer of
        // `episodes.redb`, which takes an exclusive process lock). But an
        // ephemeral run (`--no-session-persistence`) isn't going to save
        // episodes anyway, so it opens via `open_or_degraded`: if another
        // `octos chat` already holds the lock on this data dir, this run falls
        // back to an in-memory episode handle instead of failing. That lets
        // many ephemeral `octos chat` agents run concurrently against ONE
        // `--data-dir` (hence one shared `--profile`) — e.g. a fan-out of
        // review/edit agents — without the redb lock serializing them.
        let memory = Arc::new(if self.no_session_persistence {
            let store = EpisodeStore::open_or_degraded(&data_dir)
                .await
                .wrap_err("failed to open episode store")?;
            if store.is_degraded() {
                tracing::info!(
                    "episode store on this data dir is held by another process; \
                     running with an in-memory episode handle (no persistence/recall) \
                     — expected when running concurrent `octos chat` agents"
                );
            }
            store
        } else {
            EpisodeStore::open(&data_dir)
                .await
                .wrap_err("failed to open episode store")?
        });

        // Resolve the runtime profile (M8.3). Order:
        //   1. --profile CLI arg, if present;
        //   2. `~/.octos/profile` symlink, if it exists (points at a
        //      profile name or dir);
        //   3. fallback to the built-in `coding` profile.
        // The resolved profile's tool filter is applied after the full
        // registry has been assembled, preserving the existing bootstrap
        // path (plugins, MCP, pipelines etc. all register first).
        // Tool surface: normal runtime-profile resolution. A stored serve profile
        // (named by `--profile`, whose LLM config was loaded above) has no runtime
        // tool-surface definition, so fall back to the default `coding` surface
        // instead of erroring — gated on `profile_is_serve` so an actually-unknown
        // profile name still fails loudly.
        let (profile, profile_source_label) = match resolve_profile(&self.profile) {
            Ok(resolved) => resolved,
            Err(_) if profile_is_serve => {
                tracing::info!(
                    profile = self.profile.as_deref().unwrap_or_default(),
                    "serve profile has no runtime tool-surface definition; \
                     using the default `coding` tool surface"
                );
                (
                    octos_agent::profile::ProfileDefinition::load("coding")
                        .wrap_err("failed to load built-in coding profile")?
                        .0,
                    "coding (serve-profile default)",
                )
            }
            Err(err) => return Err(err),
        };
        tracing::info!(
            "profile resolved: name={} source={}",
            profile.name,
            profile_source_label
        );

        // yolo GAP #3: resolve the effective permissions from the CLI flags
        // (codex parity: --yolo / --sandbox / --ask-for-approval). `octos
        // chat` is inherently local single-user, so a dangerous profile
        // legitimately resolves through RuntimeMode::Solo.
        //
        // Guardrails PRESERVED in yolo (codex parity): hooks
        // `before_tool_call` deny still runs (wired further below); `ToolPolicy`
        // deny lists still apply (resolved per-provider below); SSRF +
        // `BLOCKED_ENV_VARS` are untouched (enforced inside the tools/sandbox
        // regardless of profile). yolo only relaxes the approval prompt, the
        // shell command policy (SafePolicy→AllowAll), the filesystem scope
        // (workspace→host), and the sandbox (off) — nothing else.
        let permissions = resolve_chat_permissions(
            self.dangerously_bypass_approvals_and_sandbox,
            self.sandbox,
            self.ask_for_approval,
        )?;
        let runtime_paths = resolve_chat_runtime_paths(
            &cwd,
            &data_dir,
            permissions,
            &uuid::Uuid::now_v7().to_string(),
        );
        if permissions.is_dangerous() {
            // Codex-style one-line RED warning on stderr.
            eprintln!(
                "{}",
                "⚠ full access — can edit any file and run commands with network, \
                 without approval; risk of data loss"
                    .red()
                    .bold()
            );
        }

        // Create tool registry under the resolved permissions. The sandbox is
        // derived from the config default with the permission profile applied
        // (a dangerous profile disables the sandbox and forces network on).
        let effective_sandbox_config = permissions.apply_to_sandbox(&config.sandbox);
        let sandbox = octos_agent::create_sandbox(&effective_sandbox_config);
        let mut tools = ToolRegistry::with_builtins_and_permissions(&cwd, sandbox, permissions);

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

        // Resolve the embedding provider ONCE and share the handle across
        // every consumer (spawn workers, pipeline workers, the chat agent)
        // so they agree on the exact same embed-on-save + hybrid-recall
        // behaviour — and the "pinning …" log fires once, not per site.
        let embedder = create_embedder(&config);

        // Register spawn tool for sync sub-agent support in chat mode.
        // Background mode won't deliver results (dummy channel), but sync mode works fine.
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::channel(1);
        let worker_prompt = super::load_prompt("worker", octos_agent::DEFAULT_WORKER_PROMPT);
        let mut spawn_tool =
            octos_agent::SpawnTool::new(llm.clone(), memory.clone(), cwd.clone(), spawn_tx)
                .with_worker_prompt(worker_prompt)
                .with_workspace_write_access(permissions.file_access.allows_write())
                // #1607 (codex-review follow-up): thread the same sandbox the
                // parent registry was built from so the spawn/agent_mcp child
                // completion path confines workspace-declared `Command`
                // validators instead of running them on the host.
                .with_sandbox(effective_sandbox_config.clone());
        if !permissions.file_access.allows_write() {
            spawn_tool =
                spawn_tool.with_deliverable_root(runtime_paths.spawn_deliverable_dir.clone());
        }
        if let Some(ref embedder) = embedder {
            // Workers save episodes by default; without the embedder those
            // episodes are stored vectorless and worker recall skips.
            spawn_tool = spawn_tool.with_embedder(embedder.clone());
        }
        tools.register(spawn_tool);

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
        tools.register(octos_agent::RecordMemoryUseTool::new(memory_store.clone()));
        let memory_refresh_enabled =
            crate::config::MemoryConfig::refresh_enabled(config.memory.as_ref());
        if memory_refresh_enabled {
            tools.register(octos_agent::MemoryNoteTool::new(memory_store.clone()));
        }

        // Register MCP tools
        if !config.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&config.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: MCP initialization failed: {e}"),
            }
        }

        // Bootstrap bundled app-skill binaries (deep_search, deep_crawl, etc.)
        // before plugin loading. In a read-only chat this targets the data-dir
        // runtime root; the workspace's existing `.octos` remains read-only.
        let n = octos_agent::bootstrap::bootstrap_bundled_skills(&runtime_paths.bootstrap_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} app-skills");
        }
        let n = octos_agent::bootstrap::bootstrap_platform_skills(&runtime_paths.bootstrap_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} platform skills");
        }
        // Gap 4.1: bundle generic pipelines (deep_research) into the
        // dedicated `<data_dir>/bundled-pipelines` dir so `run_pipeline` can
        // always discover them, independent of per-profile skill deployment.
        // The chat `RunPipelineTool` registers that dir as the LOWEST-
        // precedence search path via `with_bundled_pipelines_root(data_dir)`
        // (bootstrap-dir == search-dir); installed pipelines of the same name
        // always win.
        let n = octos_agent::bootstrap::bootstrap_bundled_pipelines(&data_dir);
        if n > 0 {
            eprintln!("Bootstrapped {n} bundled pipelines");
        }

        // Load existing project plugins plus bundled skills from the host
        // runtime root. Preserve project precedence so an operator-installed
        // skill still wins over an Octos bundled fallback.
        // Section B (codex review P1.1): honour `plugins.require_signed`
        // from the resolved Config so an operator who opts into strict
        // signing has it enforced on `octos chat` too.
        let mut plugin_dirs = Config::plugin_dirs_from_project(&runtime_paths.project_dir);
        for runtime_dir in Config::plugin_dirs_from_project(&runtime_paths.bootstrap_dir) {
            if !plugin_dirs.contains(&runtime_dir) {
                plugin_dirs.push(runtime_dir);
            }
        }
        let mut plugin_result = octos_agent::PluginLoadResult::default();
        if !plugin_dirs.is_empty() {
            match octos_agent::PluginLoader::load_into_with_options(
                &mut tools,
                &plugin_dirs,
                &[],
                octos_agent::PluginLoadOptions {
                    work_dir: None,
                    synthesis_config: None,
                    require_signed: config.plugins.require_signed,
                    verified_cache_dir: None,
                },
            ) {
                Ok(result) => plugin_result = result,
                Err(e) => eprintln!("Warning: plugin loading failed: {e}"),
            }
            // SPEC-VENDOR-NODE-V1 HTTP tool discovery — sync `load_into_with_options`
            // only handles static binary-protocol skills; this async pass walks the
            // same dirs for `tool_discovery: Http { base_url }` manifests and
            // registers their catalog-derived tools. Per @ymote's Finding 2 contract
            // (preserved in the post-merge review), an unreachable bridge or
            // unparseable catalog must hard-fail the boot rather than silently
            // register zero tools and let the operator find out at first LLM call.
            octos_agent::plugins::register_http_skills_on_startup(&mut tools, &plugin_dirs)
                .await
                .wrap_err("HTTP tool discovery failed at agent boot")?;
        }

        // Start MCP servers declared in skill manifests
        if !plugin_result.mcp_servers.is_empty() {
            match octos_agent::McpClient::start(&plugin_result.mcp_servers).await {
                Ok(client) => client.register_tools(&mut tools),
                Err(e) => eprintln!("Warning: skill MCP initialization failed: {e}"),
            }
        }

        // yolo GAP #2/#3: plugin tools are registered directly on `tools`
        // above (chat does not go through `rebind_cwd_with_permissions`), so
        // thread the session approval context into them here. Under `never` a
        // high-risk plugin fails closed; under danger-full-access it
        // auto-allows — matching the shell/coding tools built with the same
        // permissions.
        tools.apply_permissions_to_plugin_tools(permissions);

        // Pipeline tool (DOT-based multi-step workflows, with plugin access).
        // Section B (codex review follow-up): propagate
        // `plugins.require_signed` so pipeline workers enforce the same
        // gate as the main session.
        //
        // NEW-06 codex follow-up: also propagate the embedder so the
        // pipeline-spawned worker `Agent` instances inherit the same
        // hybrid scored + filtered episodic-memory recall the parent
        // chat agent gets at the `agent = agent.with_embedder(..)` line
        // below. Without this, `octos chat`'s pipeline workers fall back
        // to the unfiltered cwd-only path in `EpisodeStore::find_relevant`
        // and can pull in cross-domain episodes (the NEW-06 contamination
        // root cause). Construction is extracted into
        // [`build_run_pipeline_tool`] so the regression test in
        // `octos-pipeline/tests/embedder_propagation.rs` can pin the
        // wiring without instantiating the full chat command.
        // Lean-profile gate: `run_pipeline` is marked spawn_only, and
        // spawn_only tools survive `filter_by_profile` unconditionally —
        // so a profile can only exclude the pipeline engine by never
        // registering it. `ProfileTools::allows` mirrors the filter's
        // name-matching (groups / wildcards / exact names), which keeps
        // this gate and the registry narrowing below in agreement. The
        // lean `coding` default excludes it; `coding-full` (and any
        // profile without an explicit filter) keeps it.
        if profile.tools.allows("run_pipeline") {
            let pipeline_tool = build_run_pipeline_tool(
                llm.clone(),
                memory.clone(),
                cwd.clone(),
                data_dir.clone(),
                tools.provider_policy().cloned(),
                plugin_dirs.clone(),
                config.plugins.require_signed,
                embedder.clone(),
                // #1607: same sandbox `octos chat` builds for its shell/exec tools
                // (see `effective_sandbox_config` above), so pipeline command
                // validators run under the identical backend.
                effective_sandbox_config.clone(),
            );
            tools.register(pipeline_tool);
            tools.mark_spawn_only(
                "run_pipeline",
                Some(
                    "Pipeline started in background. The final result and any artifacts will be sent here when complete. You can keep chatting in the meantime."
                        .to_string(),
                ),
            );
        }

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

        // F-005: Build credential pool + content classifier at startup.
        // Absent config → `None` so the agent falls back to the legacy
        // single-credential flow and strong-only routing. Distinct
        // names (`_init` suffix) keep these out of the way of other
        // per-profile wiring that may land here later.
        let _credential_pool_init =
            super::build_credential_pool(config.credential_pool.as_ref(), &data_dir);
        let _content_classifier_init: Option<Arc<octos_llm::ContentClassifier>> = config
            .content_routing
            .as_ref()
            .filter(|cfg| cfg.enabled)
            .map(|cfg| Arc::new(octos_llm::ContentClassifier::new(cfg.clone())));

        // Create agent. In `--json` mode nothing may reach stdout except the
        // final result object, so silence the console reporter — its spinner,
        // `◆` answer preview, streamed chunks, and tool-progress lines all
        // print to stdout.
        let reporter: Arc<dyn ProgressReporter> = if self.json {
            Arc::new(SilentReporter)
        } else {
            Arc::new(ConsoleReporter::new().with_verbose(self.verbose))
        };
        let agent_config = AgentConfig {
            max_iterations: self.max_iterations,
            // `--no-session-persistence` (claude parity): skip the episode write.
            save_episodes: !self.no_session_persistence,
            chat_max_tokens: config.gateway.as_ref().and_then(|g| g.max_output_tokens),
            // `--effort` (claude/codex parity) overrides `gateway.reasoning_effort`.
            reasoning_effort: self
                .effort
                .map(octos_llm::ReasoningEffort::from)
                .or_else(|| config.gateway.as_ref().and_then(|g| g.reasoning_effort)),
            // #1774: opt-in post-edit formatting (rustfmt/prettier/black/gofmt).
            format_after_edit: config.format_after_edit,
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

        // Phase 1 of the SessionScope migration (PR #1198 follow-up):
        // construct the single filesystem contract for this solo
        // session and stash it on the agent. `cwd` may have come from
        // `--cwd` (potentially relative) or from `current_dir()`
        // (always absolute). Absolutize defensively so the
        // `SessionScope::solo` invariant holds without panicking on
        // user input. Phase 2 PRs will start reading this from tools,
        // pipelines, and plugins; today it is wired through but unused.
        //
        // Codex review note (Phase-1 LOW): surface a hard error when
        // `cwd` is relative AND `current_dir()` fails so the
        // `SessionScope::solo` `expect` below can never fire on a
        // relative path. Mirroring `current_dir()` failures up as
        // `wrap_err` is consistent with the existing fallback at the
        // top of `run_async` that constructed `cwd` the same way.
        let absolute_cwd: PathBuf = if cwd.is_absolute() {
            cwd.clone()
        } else {
            std::env::current_dir()
                .wrap_err("failed to absolutize --cwd: current_dir() unavailable")?
                .join(&cwd)
        };

        // yolo GAP #4: bind every loaded `PluginTool` to the resolved chat
        // cwd. Chat loads plugins with `work_dir: None` and does NOT go
        // through `SessionRuntime::rebind_plugin_work_dirs`, so under a
        // Host-scope (`--yolo`) session — where `session_scope` is left
        // `None` so file tools keep host reach — `PluginTool::execute`
        // would derive its `current_dir`/`OCTOS_WORK_DIR` from neither the
        // scope nor a work_dir and fall back to the process LAUNCH dir,
        // breaking relative plugin inputs/outputs under `--cwd`. Binding the
        // work_dir here fixes the Host case and is a no-op behavioural change
        // for Workspace scope (where `work_dir` == `scope.workspace()` ==
        // `absolute_cwd` already). See `bind_chat_plugin_work_dirs`.
        bind_chat_plugin_work_dirs(&mut tools, &absolute_cwd);

        // PR-A: thread the per-profile plugin install directories
        // through to the scope so `read_file` can reach the SKILL.md
        // content the agent's system prompt auto-injects.
        //
        // Codex round-2 BLOCKER 2 (PR #1327 review): SKIP dirs that
        // fail canonicalize (fail-closed). Keeping the raw path was a
        // fail-open vulnerability — a later symlink replacement
        // (`/tmp/missing -> /etc`) would canonicalise both sides to
        // `/etc` and allow reads as `InSkillDir`. The shared helper in
        // `octos-core` drops the entry and logs a warning per skip.
        let canonical_skill_dirs: Vec<PathBuf> =
            octos_core::canonicalize_skill_read_zones(&plugin_dirs);
        // yolo GAP #3: mirror the serve path (session.rs) — a
        // danger-full-access session resolves to `FilesystemScope::Host` and
        // the file tools deliberately keep their absolute-host reach. They
        // PREFER an attached `ctx.session_scope` over their `filesystem_scope`,
        // so attaching a solo scope here would silently re-fence a session the
        // operator explicitly opened up. Leave the scope unset under Host.
        let session_scope = if permissions.filesystem_scope.is_host() {
            tracing::debug!(
                "skipping SessionScope: --yolo/danger-full-access grants Host \
                 filesystem access — file tools must keep their host reach"
            );
            None
        } else {
            let base = SessionScope::solo(absolute_cwd.clone(), Vec::new()).expect(
                "solo CWD absolutized just above; SessionScope::solo's only invariant is absolute",
            );
            let scope = base
                .with_skill_read_zones(canonical_skill_dirs)
                .unwrap_or_else(|err| {
                    eprintln!(
                        "Warning: with_skill_read_zones rejected one or more plugin_dirs: {err}; \
                         continuing without skill_read_zones (read_file may not reach SKILL.md references)"
                    );
                    SessionScope::solo(absolute_cwd.clone(), Vec::new())
                        .expect("absolutized cwd still valid")
                });
            Some(Arc::new(scope))
        };

        let mut agent = Agent::new(AgentId::new("chat"), llm, tools, memory)
            .with_config(agent_config)
            .with_reporter(reporter)
            .with_shutdown(shutdown.clone())
            .with_agent_definitions(agent_definitions)
            .with_profile(profile_arc.clone())
            .with_file_state_cache(file_state_cache)
            .with_subagent_output_router(subagent_output_router)
            .with_subagent_summary_generator(subagent_summary_generator);
        if let Some(session_scope) = session_scope {
            agent = agent.with_session_scope(session_scope);
        }

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

        // Load operator-provided bootstrap files from the project state. The
        // runtime bootstrap root only holds generated bundled skills.
        let bootstrap = super::load_bootstrap_files(&runtime_paths.project_dir);
        if !bootstrap.is_empty() {
            agent.append_system_prompt(&bootstrap);
        }

        // Inject the token-capped memory block (long-term memory + daily
        // notes + bank summary; omissions are disclosed to the model) as a
        // replaceable named segment between bootstrap and skill fragments.
        // With memory refresh on, the capture policy rides the same segment
        // and a provider re-renders it when MEMORY.md changes on disk or
        // the date rolls over (one stat per turn otherwise).
        let max_inject =
            crate::config::MemoryConfig::effective_max_inject_tokens(config.memory.as_ref());
        let memory_ctx = memory_store.get_injectable_context(max_inject).await;
        agent.set_prompt_segment(
            octos_agent::MEMORY_SEGMENT_NAME,
            octos_agent::compose_memory_segment(&memory_ctx, memory_refresh_enabled),
        );
        if memory_refresh_enabled {
            agent.add_prompt_segment_provider(Arc::new(octos_agent::MemorySegmentProvider::new(
                memory_store.clone(),
                max_inject,
                true,
            )));
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

        // #1768: opt-in git-backed workspace snapshots before mutating
        // tools. Uses a SEPARATE git dir under `<data_dir>/snapshots/` —
        // the user's own repo/index is never touched. Silently unavailable
        // when no git binary is on PATH (`SnapshotManager::new` returns
        // `None` and logs once).
        if let Some(snapshot_cfg) = config.snapshots.as_ref().filter(|cfg| cfg.enabled) {
            if let Some(manager) = octos_agent::SnapshotManager::new(
                data_dir.join("snapshots"),
                cwd.clone(),
                snapshot_cfg.keep_last,
            ) {
                agent = agent.with_snapshot_manager(Arc::new(manager));
            }
        }

        if let Some(ref embedder) = embedder {
            agent = agent.with_embedder(embedder.clone());
        }

        // Harness M6.3/M6.4: wire the declarative compaction runner when the
        // workspace policy in the cwd declares a compaction block. Picks the
        // LLM-iterative summarizer when the policy asks for it; falls back to
        // extractive otherwise. No-op when the policy file is missing or
        // declares no compaction.
        match read_workspace_policy(&cwd) {
            Ok(Some(workspace_policy)) => {
                if let Some(compaction_policy) = workspace_policy.compaction.clone() {
                    let runner = match compaction_policy.summarizer {
                        CompactionSummarizerKind::LlmIterative => {
                            CompactionRunner::with_provider(compaction_policy, agent.llm_provider())
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
            Ok(None) => {}
            Err(error) => {
                eprintln!("Warning: failed to read workspace policy for compaction: {error}");
            }
        }

        let approval_requester: Arc<dyn ToolApprovalRequester> =
            Arc::new(CliApprovalRequester::default());

        // Single-message mode: send one message and exit.
        if let Some(msg) = self.message {
            let response =
                process_chat_turn(&agent, &msg, &[], Arc::clone(&approval_requester)).await?;
            if self.json {
                // JSON mode: emit exactly one machine-readable result object on
                // stdout (the reporter is silent and every status line goes to
                // stderr). A turn failure propagates via `?` and is rendered as
                // a JSON error at the `execute` boundary.
                println!(
                    "{}",
                    ChatJsonResult::from_response(&response, &model_id).to_json_line()
                );
                let _ = io::stdout().flush();
                return Ok(());
            }
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
            let response =
                match process_chat_turn(&agent, input, &history, Arc::clone(&approval_requester))
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("{}: {e}", "Error".red().bold());
                        continue;
                    }
                };

            // Append to history
            history.push(Message {
                role: MessageRole::User,
                content: input.to_string(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            });
            history.push(Message {
                role: MessageRole::Assistant,
                content: response.content.clone(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
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

/// Load the LLM config from a stored serve/onboarding profile so
/// `octos chat --profile <id>` can reuse an octoscode / `serve` profile's
/// provider, model, route (base URL + API type), API key (`config.env_vars`),
/// and fallbacks — without a separate flat config or a duplicated key.
///
/// Returns `Ok(None)` when no `--profile` is given, the arg is a path (a runtime
/// [`octos_agent::profile::ProfileDefinition`] file, left to [`resolve_profile`]),
/// or the id does not name a stored profile (e.g. a built-in runtime profile like
/// `coding`) — leaving the caller on its normal config path. An explicit
/// `--config` still takes precedence (handled by the caller), and CLI
/// `--provider`/`--model`/… continue to override the profile's values downstream.
///
/// [`ProfileStore::get`](crate::profiles::ProfileStore::get) is a lock-free JSON
/// read, so this is safe to call while a `serve` process holds the same data dir.
fn load_serve_profile_config(
    profile_arg: Option<&str>,
    data_dir: &std::path::Path,
) -> Result<Option<Config>> {
    let Some(id) = profile_arg else {
        return Ok(None);
    };
    // A path-form `--profile` names a runtime ProfileDefinition file, not a stored
    // serve-profile id; leave those to `resolve_profile`.
    if id.contains('/') || id.contains(std::path::MAIN_SEPARATOR) {
        return Ok(None);
    }
    let store = crate::profiles::ProfileStore::open_unified(data_dir)
        .wrap_err("failed to open profile store")?;
    let Some(profile) = store.get(id)? else {
        return Ok(None);
    };
    // Apply parent inheritance + global profile-defaults exactly like serve's
    // per-profile loop, then flatten `llm.primary` into the flat provider/model/
    // route fields the chat provider builder reads.
    let resolved = store.resolve_runtime_profile(&profile);
    let config = crate::profiles::config_from_profile(&resolved, None, None);
    tracing::info!(
        profile = id,
        provider = config.provider.as_deref().unwrap_or("<unset>"),
        model = config.model.as_deref().unwrap_or("<unset>"),
        "using LLM config from stored profile",
    );
    Ok(Some(config))
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

    // In-process llama.cpp GGUF provider (every platform, feature `embed-llama`).
    // `provider = "llamacpp"` + `model_path = "<file.gguf>"`; `dimensions`
    // truncates the output via Matryoshka (MRL). Unlike the MLX provider below
    // this is NOT Apple-only, and it runs any GGUF embedding model rather than
    // one hand-ported architecture — with a CPU backend that is a legitimate
    // choice, not a fallback.
    if cfg.provider.eq_ignore_ascii_case("llamacpp") || cfg.provider.eq_ignore_ascii_case("llama") {
        #[cfg(feature = "embed-llama")]
        {
            let path = cfg.model_path.as_deref().or(cfg.model.as_deref());
            let Some(path) = path else {
                tracing::error!(
                    "embedding.provider=\"llamacpp\" requires `model_path` (the .gguf file)"
                );
                return None;
            };
            // Offload everything when built with an accelerator; the CPU build
            // ignores this.
            let n_gpu_layers = if cfg!(any(
                feature = "embed-llama-metal",
                feature = "embed-llama-cuda"
            )) {
                99
            } else {
                0
            };
            match octos_embed_llama::LlamaEmbedder::from_model_file(path, n_gpu_layers) {
                Ok(mut e) => {
                    if let Some(d) = cfg.dimensions {
                        e = e.with_output_dim(d as usize);
                    }
                    tracing::info!(
                        model_path = %path,
                        dimension = e.dimension(),
                        n_gpu_layers,
                        "loaded in-process llama.cpp embedder"
                    );
                    return Some(Arc::new(e));
                }
                Err(err) => {
                    tracing::error!(%err, model_path = %path, "failed to load llama.cpp embedder");
                    return None;
                }
            }
        }
        #[cfg(not(feature = "embed-llama"))]
        {
            tracing::warn!(
                "embedding.provider=\"llamacpp\" needs a build with `--features embed-llama`; \
                 ignoring and disabling embeddings"
            );
            return None;
        }
    }

    // `provider = "mlx"` was the Apple-Silicon-only in-process backend. It has
    // been replaced by `"llamacpp"` above, which is cross-platform and measured
    // at parity or better. Fail LOUDLY rather than falling through to the
    // remote-provider path below, where "mlx" would be treated as an API
    // provider name and produce a baffling credential error.
    if cfg.provider.eq_ignore_ascii_case("mlx") {
        tracing::error!(
            "embedding.provider=\"mlx\" has been removed — use \"llamacpp\" with a \
             .gguf `model_path`. NOTE: the two backends' vectors are not \
             interchangeable (~0.96-0.99 cosine), so stored episode embeddings \
             must be regenerated after switching; until then recall degrades to \
             BM25-only for those episodes."
        );
        return None;
    }

    // `api_key_env` was declared on EmbeddingConfig but never honored —
    // it wins over the provider-default var name, resolving through the
    // SAME credential chain as every other key (auth store, env_vars +
    // keychain, process env), so `octos auth login` / config-stored keys
    // keep working (codex P2).
    let key = config
        .get_api_key_with_env(&cfg.provider, cfg.api_key_env.as_deref())
        .ok()?;
    let mut e = OpenAIEmbedder::new(key);
    if let Some(ref url) = cfg.base_url {
        e = e.with_base_url(url);
    } else if !cfg.provider.eq_ignore_ascii_case("openai") {
        // A non-openai provider without an explicit base_url falls back to
        // the registry's default endpoint — otherwise the request goes to
        // api.openai.com with the other provider's key/model (codex R8).
        if let Some(url) =
            octos_llm::registry::lookup(&cfg.provider).and_then(|e| e.default_base_url)
        {
            e = e.with_base_url(url);
        }
    }
    if let Some(ref model) = cfg.model {
        e = e.with_model(model);
    }
    if let Some(dimensions) = cfg.dimensions {
        if dimensions as usize != octos_memory::EPISODIC_INDEX_DIMENSION {
            tracing::warn!(
                dimensions,
                index = octos_memory::EPISODIC_INDEX_DIMENSION,
                "embedding.dimensions differs from the episodic index dimension — \
                 vectors will be dropped to BM25-only"
            );
        }
        e = e.with_dimensions(dimensions);
    } else if let Some(model) = cfg.model.as_deref() {
        // Auto-pin to the index dimension ONLY for families known to
        // accept the OpenAI-standard `dimensions` field (OpenAI 3-series;
        // DashScope text-embedding-v3/v4, natively 1024-d). Models that
        // reject the field (ada-002) keep the legacy request shape; for
        // families we can't classify, warn loudly instead of degrading
        // silently — the native size is unknown and non-1536 vectors are
        // dropped to BM25-only.
        // Exactly the families verified to accept `dimensions: 1536`:
        // OpenAI 3-series (native 1536/3072, truncation supported) and
        // DashScope text-embedding-v4 (64–2048, verified live). v3 caps
        // below 1536 and would error — it falls to the warn path.
        let supports_dimensions =
            model.starts_with("text-embedding-3") || model == "text-embedding-v4";
        if supports_dimensions {
            tracing::info!(
                model = %e.model(),
                pinned = octos_memory::EPISODIC_INDEX_DIMENSION,
                "pinning custom embedding model to the episodic index dimension"
            );
            e = e.with_dimensions(octos_memory::EPISODIC_INDEX_DIMENSION as u32);
        } else {
            tracing::warn!(
                model = %e.model(),
                index = octos_memory::EPISODIC_INDEX_DIMENSION,
                "custom embedding model without `dimensions`: native size unknown — \
                 vectors that are not index-sized will be dropped to BM25-only; set \
                 embedding.dimensions if the provider supports it"
            );
        }
    }
    Some(Arc::new(e))
}

/// Build the [`octos_pipeline::RunPipelineTool`] used by the chat command,
/// threading through the per-session policy / plugin dirs / signing gate /
/// embedder.
///
/// NEW-06 codex follow-up — extracted into a stand-alone function so the
/// regression test in `octos-pipeline/tests/embedder_propagation.rs` can
/// pin the embedder propagation without instantiating the entire chat
/// command (which depends on rustyline, hooks, profiles, MCP, etc.).
///
/// Keep the construction order byte-for-byte identical to the inline
/// path it replaced — the policy/plugin builders rely on insertion order
/// (`with_plugin_dirs` invalidates the plugin cache, etc.).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_run_pipeline_tool(
    llm: Arc<dyn LlmProvider>,
    memory: Arc<EpisodeStore>,
    cwd: PathBuf,
    data_dir: PathBuf,
    provider_policy: Option<octos_agent::ToolPolicy>,
    plugin_dirs: Vec<PathBuf>,
    plugin_require_signed: bool,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    // #1607: session sandbox forwarded onto the pipeline executor so its
    // terminal / per-node command validators run confined instead of on the
    // host.
    sandbox: octos_agent::SandboxConfig,
) -> octos_pipeline::RunPipelineTool {
    let mut pipeline_tool =
        octos_pipeline::RunPipelineTool::new(llm, memory, cwd, data_dir.clone())
            .with_provider_policy(provider_policy)
            .with_plugin_dirs(plugin_dirs)
            .with_plugin_require_signed(plugin_require_signed)
            // #1607: confine pipeline command validators to the session sandbox.
            .with_sandbox(sandbox)
            // Gap 4.1 BLOCKER 2/3: `octos chat` bootstraps the bundle into
            // `<data_dir>/bundled-pipelines` (see chat.rs above). Register that
            // exact dir as the LOWEST-precedence discovery path so the bundled
            // `deep_research` is discoverable (bootstrap-dir == search-dir) yet
            // any installed `deep_research.dot` in `<data_dir>/{pipelines,skills}`
            // still wins.
            .with_bundled_pipelines_root(data_dir);
    if let Some(embedder) = embedder {
        pipeline_tool = pipeline_tool.with_embedder(embedder);
    }
    pipeline_tool
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    /// A profile-derived config carrying a full openai route.
    fn openai_route_config() -> Config {
        Config {
            provider: Some("openai".into()),
            model: Some("gpt-4o".into()),
            base_url: Some("https://fake.example/v1".into()),
            api_key_env: Some("MYFAKE_PROFILE_KEY".into()),
            api_type: Some("openai".into()),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_provider_override_detaches_inherited_route() {
        // Switching to a DIFFERENT provider must drop the profile's route
        // siblings so the new provider's defaults / explicit flags apply —
        // otherwise `--profile p --provider anthropic` builds an incoherent
        // anthropic client still pointed at openai's key-env + endpoint.
        let mut config = openai_route_config();
        detach_route_on_provider_override(&mut config, Some("anthropic"));
        assert_eq!(config.base_url, None, "base_url must detach");
        assert_eq!(config.api_key_env, None, "api_key_env must detach");
        assert_eq!(config.api_type, None, "api_type must detach");
        // The model is intentionally left inherited (some providers require one;
        // blanking it would turn a swap into a hard error). Complete the switch
        // with `--model`.
        assert_eq!(config.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn same_provider_override_keeps_the_route() {
        // Re-naming the SAME provider is a no-op re-affirmation; keep the route.
        let mut config = openai_route_config();
        detach_route_on_provider_override(&mut config, Some("openai"));
        assert_eq!(config.base_url.as_deref(), Some("https://fake.example/v1"));
        assert_eq!(config.api_key_env.as_deref(), Some("MYFAKE_PROFILE_KEY"));
        assert_eq!(config.api_type.as_deref(), Some("openai"));
    }

    #[test]
    fn absent_cli_provider_keeps_the_route() {
        // No `--provider` at all — pure profile reuse — keeps the whole route.
        let mut config = openai_route_config();
        detach_route_on_provider_override(&mut config, None);
        assert_eq!(config.base_url.as_deref(), Some("https://fake.example/v1"));
        assert_eq!(config.api_key_env.as_deref(), Some("MYFAKE_PROFILE_KEY"));
        assert_eq!(config.api_type.as_deref(), Some("openai"));
    }

    #[test]
    fn provider_override_without_inherited_provider_keeps_route() {
        // No inherited provider identity to detach from (unusual ambient config
        // with a bare route): leave it alone rather than clobber it.
        let mut config = Config {
            provider: None,
            base_url: Some("https://amb.example/v1".into()),
            api_key_env: Some("AMBIENT_KEY".into()),
            ..Default::default()
        };
        detach_route_on_provider_override(&mut config, Some("anthropic"));
        assert_eq!(config.base_url.as_deref(), Some("https://amb.example/v1"));
        assert_eq!(config.api_key_env.as_deref(), Some("AMBIENT_KEY"));
    }

    #[test]
    fn read_only_runtime_paths_keep_host_state_outside_workspace() {
        let workspace = PathBuf::from("/tmp/workspace");
        let data_dir = PathBuf::from("/tmp/octos-data");
        let permissions =
            resolve_chat_permissions(false, Some(ChatSandboxMode::ReadOnly), None).unwrap();

        let paths = resolve_chat_runtime_paths(&workspace, &data_dir, permissions, "run-123");

        assert_eq!(paths.project_dir, workspace.join(".octos"));
        assert_eq!(
            paths.bootstrap_dir,
            data_dir
                .join("runtime")
                .join("runs")
                .join("run-123")
                .join("bootstrap")
        );
        assert_eq!(
            paths.spawn_deliverable_dir,
            data_dir
                .join("runtime")
                .join("runs")
                .join("run-123")
                .join("spawn-deliverables")
        );
    }

    #[test]
    fn read_only_runtime_paths_are_isolated_per_chat_run() {
        let workspace = PathBuf::from("/tmp/workspace");
        let data_dir = PathBuf::from("/tmp/octos-data");
        let permissions =
            resolve_chat_permissions(false, Some(ChatSandboxMode::ReadOnly), None).unwrap();

        let first = resolve_chat_runtime_paths(&workspace, &data_dir, permissions, "run-a");
        let second = resolve_chat_runtime_paths(&workspace, &data_dir, permissions, "run-b");

        assert_ne!(first.bootstrap_dir, second.bootstrap_dir);
        assert_ne!(first.spawn_deliverable_dir, second.spawn_deliverable_dir);
        assert!(first.bootstrap_dir.starts_with(&data_dir));
        assert!(second.bootstrap_dir.starts_with(&data_dir));
    }

    #[test]
    fn workspace_write_runtime_paths_preserve_project_state_locations() {
        let workspace = PathBuf::from("/tmp/workspace");
        let data_dir = PathBuf::from("/tmp/octos-data");
        let permissions =
            resolve_chat_permissions(false, Some(ChatSandboxMode::WorkspaceWrite), None).unwrap();

        let paths = resolve_chat_runtime_paths(&workspace, &data_dir, permissions, "run-123");

        assert_eq!(paths.project_dir, workspace.join(".octos"));
        assert_eq!(paths.bootstrap_dir, workspace.join(".octos"));
        assert_eq!(
            paths.spawn_deliverable_dir,
            workspace.join(".octos").join("spawn-deliverables")
        );
    }

    #[test]
    fn chat_profile_loads_llm_config_from_stored_serve_profile() {
        use crate::profiles::{
            LlmModelSelectionConfig, LlmProfileConfig, LlmRouteConfig, ProfileConfig, ProfileStore,
            UserProfile,
        };

        let dir = tempfile::tempdir().unwrap();
        let store = ProfileStore::open_unified(dir.path()).unwrap();
        let profile = UserProfile {
            id: "dev".to_string(),
            name: "Dev".to_string(),
            public_subdomain: None,
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                llm: Some(LlmProfileConfig {
                    primary: Some(LlmModelSelectionConfig {
                        family_id: Some("moonshot".to_string()),
                        model_id: Some("kimi-k2.5".to_string()),
                        route: Some(LlmRouteConfig {
                            base_url: Some("https://api.kimi.com/coding/v1".to_string()),
                            api_key_env: Some("KIMI_API_KEY".to_string()),
                            api_type: Some("openai".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    fallbacks: vec![],
                }),
                env_vars: [("KIMI_API_KEY".to_string(), "sk-from-profile".to_string())].into(),
                ..Default::default()
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.save(&profile).unwrap();

        // `--profile dev` names a stored serve profile → flattened LLM config,
        // including the API key carried in the profile's own `env_vars` (so the
        // chat run reuses the profile's model AND key with no separate setup).
        let config = load_serve_profile_config(Some("dev"), dir.path())
            .unwrap()
            .expect("stored profile should produce a config");
        assert_eq!(config.provider.as_deref(), Some("moonshot"));
        assert_eq!(config.model.as_deref(), Some("kimi-k2.5"));
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://api.kimi.com/coding/v1")
        );
        assert_eq!(config.api_type.as_deref(), Some("openai"));
        assert_eq!(config.api_key_env.as_deref(), Some("KIMI_API_KEY"));
        assert_eq!(
            config.env_vars.get("KIMI_API_KEY").map(String::as_str),
            Some("sk-from-profile")
        );

        // A built-in runtime-profile name, a path-form arg, and an absent arg all
        // fall through (Ok(None)) so the caller keeps its normal config path.
        assert!(
            load_serve_profile_config(Some("coding"), dir.path())
                .unwrap()
                .is_none()
        );
        assert!(
            load_serve_profile_config(Some("./some/path.json"), dir.path())
                .unwrap()
                .is_none()
        );
        assert!(
            load_serve_profile_config(None, dir.path())
                .unwrap()
                .is_none()
        );
    }

    // ---- yolo GAP #3: chat permission flags → EffectivePermissions ----

    use octos_agent::{ApprovalPolicy, PermissionProfile};

    #[test]
    fn should_yield_danger_full_access_when_yolo_flag_set() {
        // `--yolo` / `--dangerously-bypass-approvals-and-sandbox` maps onto
        // the codex "danger full access" profile: no approvals, no sandbox,
        // host filesystem, network on.
        let perms = resolve_chat_permissions(true, None, None)
            .expect("--yolo must resolve to danger_full_access");
        assert_eq!(
            perms.permission_profile,
            PermissionProfile::DangerFullAccess
        );
        assert_eq!(perms.approval_policy, ApprovalPolicy::Never);
        assert!(perms.is_dangerous());
    }

    #[test]
    fn should_yield_workspace_write_never_when_sandbox_and_approval_flags_set() {
        // Codex parity: `--sandbox workspace-write --ask-for-approval never`
        // yields exactly that pair (workspace-write profile, approvals never)
        // WITHOUT escalating to host/danger.
        let perms = resolve_chat_permissions(
            false,
            Some(ChatSandboxMode::WorkspaceWrite),
            Some(ChatApprovalMode::Never),
        )
        .expect("explicit sandbox + approval flags must resolve");
        assert_eq!(perms.permission_profile, PermissionProfile::WorkspaceWrite);
        assert_eq!(perms.approval_policy, ApprovalPolicy::Never);
        assert!(!perms.is_dangerous());
    }

    #[test]
    fn should_default_to_workspace_write_ask_when_no_flags() {
        let perms =
            resolve_chat_permissions(false, None, None).expect("no flags is the plain default");
        assert_eq!(perms.permission_profile, PermissionProfile::WorkspaceWrite);
        assert_eq!(perms.approval_policy, ApprovalPolicy::Ask);
    }

    #[test]
    fn should_map_sandbox_read_only_and_danger_variants() {
        let ro = resolve_chat_permissions(false, Some(ChatSandboxMode::ReadOnly), None).unwrap();
        assert_eq!(ro.permission_profile, PermissionProfile::ReadOnly);

        // `--sandbox danger-full-access` is the long-form of `--yolo`.
        let danger =
            resolve_chat_permissions(false, Some(ChatSandboxMode::DangerFullAccess), None).unwrap();
        assert_eq!(
            danger.permission_profile,
            PermissionProfile::DangerFullAccess
        );
        assert!(danger.is_dangerous());
    }

    #[test]
    fn should_reject_conflicting_yolo_and_non_danger_sandbox() {
        // `--yolo` plus an explicit non-danger `--sandbox` is contradictory;
        // fail closed rather than silently pick one.
        let err = resolve_chat_permissions(true, Some(ChatSandboxMode::ReadOnly), None)
            .expect_err("conflicting flags must error");
        assert!(
            err.to_string().contains("sandbox"),
            "error should explain the sandbox conflict; got: {err}"
        );
    }

    #[test]
    fn should_reject_approval_override_on_danger_sandbox() {
        // DangerFullAccess implies approvals=never; an explicit
        // `--ask-for-approval ask` alongside it is contradictory.
        let err = resolve_chat_permissions(
            false,
            Some(ChatSandboxMode::DangerFullAccess),
            Some(ChatApprovalMode::Ask),
        )
        .expect_err("ask-for-approval=ask cannot combine with danger-full-access");
        assert!(err.to_string().contains("approval"));
    }

    #[test]
    fn should_parse_yolo_alias_and_sandbox_flags_via_clap() {
        // Prove the clap wiring: the hidden `--yolo` alias, the long form,
        // and both value-enum flags parse into the expected fields.
        use clap::Parser;

        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            chat: ChatCommand,
        }

        let yolo = Wrap::parse_from(["prog", "--yolo"]).chat;
        assert!(yolo.dangerously_bypass_approvals_and_sandbox);
        let perms = resolve_chat_permissions(
            yolo.dangerously_bypass_approvals_and_sandbox,
            yolo.sandbox,
            yolo.ask_for_approval,
        )
        .unwrap();
        assert!(perms.is_dangerous());

        let long = Wrap::parse_from(["prog", "--dangerously-bypass-approvals-and-sandbox"]).chat;
        assert!(long.dangerously_bypass_approvals_and_sandbox);

        let explicit = Wrap::parse_from([
            "prog",
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "never",
        ])
        .chat;
        assert_eq!(explicit.sandbox, Some(ChatSandboxMode::WorkspaceWrite));
        assert_eq!(explicit.ask_for_approval, Some(ChatApprovalMode::Never));

        // Default: neither flag present.
        let bare = Wrap::parse_from(["prog"]).chat;
        assert!(!bare.dangerously_bypass_approvals_and_sandbox);
        assert_eq!(bare.sandbox, None);
        assert_eq!(bare.ask_for_approval, None);
    }

    #[test]
    fn should_map_chat_effort_to_reasoning_effort() {
        use octos_llm::ReasoningEffort;
        assert_eq!(ReasoningEffort::from(ChatEffort::Low), ReasoningEffort::Low);
        assert_eq!(
            ReasoningEffort::from(ChatEffort::Medium),
            ReasoningEffort::Medium
        );
        assert_eq!(
            ReasoningEffort::from(ChatEffort::High),
            ReasoningEffort::High
        );
        assert_eq!(ReasoningEffort::from(ChatEffort::Max), ReasoningEffort::Max);
    }

    #[test]
    fn should_reconcile_positional_prompt_with_message() {
        // Positional-only → used as the one-shot message.
        assert_eq!(
            reconcile_one_shot_prompt(None, Some("hi".into())).unwrap(),
            Some("hi".to_string())
        );
        // --message-only → passthrough.
        assert_eq!(
            reconcile_one_shot_prompt(Some("hi".into()), None).unwrap(),
            Some("hi".to_string())
        );
        // Neither → interactive (None).
        assert_eq!(reconcile_one_shot_prompt(None, None).unwrap(), None);
        // Both → contradictory, fail closed.
        let err = reconcile_one_shot_prompt(Some("a".into()), Some("b".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
    }

    #[test]
    fn should_parse_effort_no_persistence_and_positional_prompt_via_clap() {
        // `claude -p` parity: --effort, --no-session-persistence, and a bare
        // positional PROMPT all parse into the expected fields.
        use clap::Parser;

        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            chat: ChatCommand,
        }

        let full = Wrap::parse_from([
            "prog",
            "--effort",
            "max",
            "--no-session-persistence",
            "Review the diff",
        ])
        .chat;
        assert_eq!(full.effort, Some(ChatEffort::Max));
        assert!(full.no_session_persistence);
        // The positional prompt lands in `prompt`, distinct from `--message`.
        assert_eq!(full.prompt.as_deref(), Some("Review the diff"));
        assert_eq!(full.message, None);

        // Every effort tier parses (clap's default kebab/lower naming).
        for (arg, want) in [
            ("low", ChatEffort::Low),
            ("medium", ChatEffort::Medium),
            ("high", ChatEffort::High),
            ("max", ChatEffort::Max),
        ] {
            let c = Wrap::parse_from(["prog", "--effort", arg]).chat;
            assert_eq!(c.effort, Some(want));
        }

        // Defaults: no effort, persistence ON, no positional prompt.
        let bare = Wrap::parse_from(["prog"]).chat;
        assert_eq!(bare.effort, None);
        assert!(!bare.no_session_persistence);
        assert_eq!(bare.prompt, None);
    }

    #[test]
    fn should_parse_api_type_flag_and_its_api_style_alias() {
        // `--api-type` (and its `--api-style` alias) picks the wire protocol
        // for a custom `--base-url`, independent of the vendor `--provider`.
        use clap::Parser;

        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            chat: ChatCommand,
        }

        let via_type = Wrap::parse_from(["prog", "--api-type", "anthropic"]).chat;
        assert_eq!(via_type.api_type.as_deref(), Some("anthropic"));

        let via_alias = Wrap::parse_from(["prog", "--api-style", "openai"]).chat;
        assert_eq!(via_alias.api_type.as_deref(), Some("openai"));

        // Honest form: a real vendor name + an explicit protocol, no overload.
        let combined =
            Wrap::parse_from(["prog", "--provider", "zai", "--api-type", "anthropic"]).chat;
        assert_eq!(combined.provider.as_deref(), Some("zai"));
        assert_eq!(combined.api_type.as_deref(), Some("anthropic"));

        // Absent by default (falls back to config's api_type at runtime).
        assert_eq!(Wrap::parse_from(["prog"]).chat.api_type, None);
    }

    // ---- `--json` result envelope ----

    #[test]
    fn should_serialize_chat_json_result_with_expected_shape() {
        // The `--json` envelope is a single-line object with every documented
        // key, in declaration order, so an agent/script can parse it directly.
        let result = ChatJsonResult {
            text: "hello world".to_string(),
            model: "glm-5.2".to_string(),
            input_tokens: 4582,
            output_tokens: 7,
        };
        let json = serde_json::to_string(&result).expect("envelope must serialize");
        assert_eq!(
            json,
            r#"{"text":"hello world","model":"glm-5.2","input_tokens":4582,"output_tokens":7}"#
        );

        // And it parses back to the exact fields/values a caller reads.
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["text"], "hello world");
        assert_eq!(value["model"], "glm-5.2");
        assert_eq!(value["input_tokens"], 4582);
        assert_eq!(value["output_tokens"], 7);
    }

    #[test]
    fn should_build_envelope_from_conversation_response() {
        // The envelope carries ground-truth fields only: text + token usage pass
        // through, and `model` comes from the final reply's provider provenance
        // so adaptive failover is reported honestly — falling back to the
        // configured id only when the reply carries no provenance.
        let build = |provider_metadata: Option<octos_llm::ProviderMetadata>| ConversationResponse {
            content: "final answer".to_string(),
            reasoning_content: None,
            provider_metadata,
            token_usage: octos_core::TokenUsage {
                input_tokens: 100,
                output_tokens: 5,
                ..Default::default()
            },
            estimated_spend_usd: None,
            files_modified: vec![],
            files_to_send: vec![],
            streamed: false,
            messages: vec![],
            tool_results: vec![],
            synthesized_from_spawn_only: false,
            pending_approval: None,
        };

        // No provenance → fall back to the configured model id.
        let fallback = build(None);
        let envelope = ChatJsonResult::from_response(&fallback, "glm-5.2");
        assert_eq!(envelope.text, "final answer");
        assert_eq!(envelope.model, "glm-5.2");
        assert_eq!(envelope.input_tokens, 100);
        assert_eq!(envelope.output_tokens, 5);

        // Failover: the reply came from a different lane than the configured id
        // → report the model that actually answered, not the fallback.
        let routed = build(Some(octos_llm::ProviderMetadata::new(
            "zai", "glm-4.7", None,
        )));
        let envelope = ChatJsonResult::from_response(&routed, "glm-5.2");
        assert_eq!(envelope.model, "glm-4.7");
    }

    // ---- #1570: [y/s/N] approval prompt + numbered user-question prompt ----

    fn q(multi: bool, allow_free_text: bool) -> octos_core::ui_protocol::UserQuestion {
        use octos_core::ui_protocol::{UserQuestion, UserQuestionOption};
        UserQuestion {
            header: "H".into(),
            question: "Which?".into(),
            options: vec![
                UserQuestionOption {
                    label: "axum".into(),
                    description: String::new(),
                },
                UserQuestionOption {
                    label: "actix".into(),
                    description: String::new(),
                },
                UserQuestionOption {
                    label: "warp".into(),
                    description: String::new(),
                },
            ],
            multi_select: multi,
            allow_free_text,
        }
    }

    #[test]
    fn parse_approval_answer_maps_y_s_and_default_deny() {
        assert_eq!(
            parse_cli_approval_answer("y\n"),
            CliApprovalAnswer::ApproveOnce
        );
        assert_eq!(
            parse_cli_approval_answer("  YES "),
            CliApprovalAnswer::ApproveOnce
        );
        assert_eq!(
            parse_cli_approval_answer("s"),
            CliApprovalAnswer::ApproveSession
        );
        assert_eq!(
            parse_cli_approval_answer("Session\n"),
            CliApprovalAnswer::ApproveSession
        );
        // Fail-closed: empty and anything unrecognized deny.
        assert_eq!(parse_cli_approval_answer(""), CliApprovalAnswer::Deny);
        assert_eq!(parse_cli_approval_answer("n"), CliApprovalAnswer::Deny);
        assert_eq!(parse_cli_approval_answer("wat"), CliApprovalAnswer::Deny);
    }

    #[tokio::test]
    async fn session_scope_auto_resolves_later_requests_without_prompting() {
        // With the session flag set, request_approval must return Approve on
        // the fast path — before spawn_blocking would ever touch stdin (this
        // test has no TTY; a prompt would deny, failing the assert).
        let requester = CliApprovalRequester::default();
        requester
            .session_approved
            .store(true, std::sync::atomic::Ordering::Release);
        let request = ToolApprovalRequest {
            tool_id: "t1".into(),
            tool_name: "shell".into(),
            title: "Approve command".into(),
            body: "Run command: sudo echo hi".into(),
            command: Some("sudo echo hi".into()),
            cwd: None,
        };
        let decision = requester.request_approval(request).await;
        assert_eq!(decision, ToolApprovalDecision::Approve);
    }

    #[test]
    fn parse_selection_single_picks_the_numbered_option() {
        let (labels, other) = parse_question_selection(&q(false, true), "2");
        assert_eq!(labels, vec!["actix"]);
        assert!(!other);
    }

    #[test]
    fn parse_selection_empty_defaults_to_first_option() {
        let (labels, other) = parse_question_selection(&q(false, true), "  \n");
        assert_eq!(labels, vec!["axum"]);
        assert!(!other);
    }

    #[test]
    fn parse_selection_single_ignores_extra_picks() {
        // Single-select keeps only the first valid pick.
        let (labels, _) = parse_question_selection(&q(false, true), "3,1");
        assert_eq!(labels, vec!["warp"]);
    }

    #[test]
    fn parse_selection_multi_keeps_all_valid_and_drops_garbage() {
        let (labels, other) = parse_question_selection(&q(true, true), "1, 3, 9, x");
        assert_eq!(labels, vec!["axum", "warp"]); // 9 out-of-range, x non-numeric
        assert!(!other);
    }

    #[test]
    fn parse_selection_other_index_sets_free_text_flag() {
        // Other is options.len()+1 = 4 here.
        let (labels, other) = parse_question_selection(&q(false, true), "4");
        assert!(labels.is_empty());
        assert!(other);
    }

    #[test]
    fn parse_selection_other_ignored_when_free_text_disallowed() {
        // With free text off, index 4 is out of range → filtered → default(1).
        let (labels, other) = parse_question_selection(&q(false, false), "4");
        assert!(!other);
        assert_eq!(labels, vec!["axum"]); // empty picks after filter → default
    }

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

    #[test]
    fn chat_constructs_solo_session_scope_with_user_cwd() {
        // Phase 1 SessionScope migration (PR #1198 follow-up): the
        // chat entry point constructs a solo [`SessionScope`] from the
        // user-finalized `cwd` (or `current_dir()` fallback) and
        // attaches it to the per-session agent via
        // [`Agent::with_session_scope`]. This test mirrors the exact
        // construction the entry point performs so a regression that
        // drops the wiring (or accidentally rejects valid input by
        // mistakenly making the constructor fail) fails the suite
        // before it ships to fleet.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        // Mirror chat.rs's absolutize-then-build pattern. The entry
        // point propagates `current_dir()` failures via `wrap_err?`;
        // here in the test the cwd is already absolute (`tempdir`
        // returns an absolute path) so the relative branch is never
        // taken.
        let absolute_cwd: PathBuf = if cwd.is_absolute() {
            cwd.clone()
        } else {
            std::env::current_dir()
                .expect("current_dir() in tests")
                .join(&cwd)
        };
        let scope = SessionScope::solo(absolute_cwd, Vec::new())
            .expect("solo SessionScope construction must succeed for an absolute cwd");
        assert_eq!(scope.workspace(), cwd.as_path());
        assert_eq!(scope.root(), cwd.as_path());
        assert!(scope.shared_zones().is_empty());
    }

    #[test]
    fn chat_solo_session_scope_does_not_panic_on_relative_cwd_input() {
        // Defensive cover for chat.rs's absolutize branch — the
        // `--cwd relative` case must not propagate a relative path
        // into `SessionScope::solo`, which would `expect` on the
        // `RootNotAbsolute` invariant. The chat entry point now
        // bubbles `current_dir()` errors up via `wrap_err?` so the
        // branch only ever produces an absolute path or returns Err
        // before reaching the `SessionScope::solo` call site.
        let relative = PathBuf::from("some-subdir");
        let base = std::env::current_dir().expect("current_dir() in tests");
        let absolute_cwd: PathBuf = if relative.is_absolute() {
            relative.clone()
        } else {
            base.join(&relative)
        };
        assert!(
            absolute_cwd.is_absolute(),
            "current_dir().join(relative) must produce an absolute path"
        );
        SessionScope::solo(absolute_cwd, Vec::new())
            .expect("SessionScope::solo accepts the absolutized path");
    }

    /// NEW-06 codex follow-up — when [`build_run_pipeline_tool`] is
    /// given an embedder, the resulting [`octos_pipeline::RunPipelineTool`]
    /// must carry it through so pipeline workers spawned from `octos chat`
    /// inherit the contamination-safe hybrid memory recall path.
    ///
    /// Regression guard: if a future refactor drops the
    /// `.with_embedder(...)` call on the chat construction path the
    /// `embedder_for_test()` assertion below goes red.
    #[tokio::test]
    async fn build_run_pipeline_tool_propagates_embedder_when_present() {
        use async_trait::async_trait;
        use octos_llm::EmbeddingProvider;

        struct StubEmbedder;
        #[async_trait]
        impl EmbeddingProvider for StubEmbedder {
            async fn embed(&self, texts: &[&str]) -> eyre::Result<Vec<Vec<f32>>> {
                Ok(vec![vec![0.0]; texts.len()])
            }
            fn dimension(&self) -> usize {
                1
            }
        }

        struct MockLlm;
        #[async_trait]
        impl LlmProvider for MockLlm {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[octos_llm::ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<octos_llm::ChatResponse> {
                Ok(octos_llm::ChatResponse {
                    content: Some("ok".into()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: octos_llm::StopReason::EndTurn,
                    usage: octos_llm::TokenUsage::default(),
                    provider_index: None,
                })
            }
            fn provider_name(&self) -> &str {
                "mock"
            }
            fn model_id(&self) -> &str {
                "mock-1"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path()).await.unwrap());
        let llm = Arc::new(MockLlm) as Arc<dyn LlmProvider>;
        let embedder = Arc::new(StubEmbedder) as Arc<dyn EmbeddingProvider>;

        let tool = build_run_pipeline_tool(
            llm,
            memory,
            std::env::temp_dir(),
            std::env::temp_dir(),
            None,
            vec![],
            false,
            Some(embedder),
            octos_agent::SandboxConfig::default(),
        );

        assert!(
            tool.embedder_for_test().is_some(),
            "build_run_pipeline_tool must call `.with_embedder(..)` when \
             the caller supplies one — otherwise `octos chat` pipeline \
             workers fall back to the unfiltered cwd-only memory recall \
             path and re-introduce the NEW-06 contamination."
        );
    }

    /// NEW-06 codex follow-up — without an embedder argument the helper
    /// produces a tool that matches pre-fix behaviour byte-for-byte
    /// (`embedder_for_test()` returns `None`). Locks the legacy fall-through
    /// for callers that don't have an embedder configured.
    #[tokio::test]
    async fn build_run_pipeline_tool_defaults_to_no_embedder() {
        use async_trait::async_trait;

        struct MockLlm;
        #[async_trait]
        impl LlmProvider for MockLlm {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[octos_llm::ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<octos_llm::ChatResponse> {
                Ok(octos_llm::ChatResponse {
                    content: Some("ok".into()),
                    reasoning_content: None,
                    tool_calls: vec![],
                    stop_reason: octos_llm::StopReason::EndTurn,
                    usage: octos_llm::TokenUsage::default(),
                    provider_index: None,
                })
            }
            fn provider_name(&self) -> &str {
                "mock"
            }
            fn model_id(&self) -> &str {
                "mock-1"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path()).await.unwrap());
        let llm = Arc::new(MockLlm) as Arc<dyn LlmProvider>;

        let tool = build_run_pipeline_tool(
            llm,
            memory,
            std::env::temp_dir(),
            std::env::temp_dir(),
            None,
            vec![],
            false,
            None,
            octos_agent::SandboxConfig::default(),
        );

        assert!(
            tool.embedder_for_test().is_none(),
            "build_run_pipeline_tool with `embedder = None` must not \
             attach one — otherwise legacy callers that never configured \
             an embedder would observe a behaviour change."
        );
    }

    /// yolo GAP #4 (codex P2): a `PluginTool` loaded for a chat session with
    /// an explicit cwd must be BOUND to that cwd, so `PluginTool::execute`
    /// runs the plugin in `--cwd` even when the session scope is omitted
    /// (Host/yolo). Before the fix, chat loaded plugins with `work_dir: None`
    /// and never rebound them, so a Host-scope session (scope `None`) left the
    /// plugin's `work_dir` `None` → the plugin ran in the process launch dir.
    ///
    /// This test replicates chat's plugin-load path (loader `work_dir: None`,
    /// mirroring `run_async`), then applies the chat cwd-binding helper the
    /// yolo path uses, and asserts the loaded plugin's `work_dir` == the
    /// resolved cwd. Removing the `bind_chat_plugin_work_dirs` call makes this
    /// fail (RED), pinning the fix.
    #[cfg(unix)]
    #[test]
    fn should_bind_plugin_work_dir_to_cwd_for_host_scope_chat_session() {
        use octos_agent::plugins::PluginTool;
        use std::os::unix::fs::PermissionsExt;

        // Plugin fixture: manifest + executable (mirrors loader.rs tests).
        let root = tempfile::tempdir().expect("tempdir");
        let plugin_dir = root.path().join("demo-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();
        let manifest = r#"{
            "name": "demo-plugin",
            "version": "1.0",
            "tools": [{"name": "demo_tool", "description": "d", "input_schema": {"type": "object", "properties": {}}}]
        }"#;
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("demo-plugin");
        std::fs::write(&exec_path, b"#!/bin/sh\necho ok").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The resolved chat cwd (the `--cwd` the operator passed).
        let cwd = root.path().join("work-here");
        std::fs::create_dir(&cwd).unwrap();

        // Load exactly as chat's `run_async` does: `work_dir: None`.
        let mut tools = ToolRegistry::new();
        let result = octos_agent::PluginLoader::load_into_with_options(
            &mut tools,
            &[root.path().to_path_buf()],
            &[],
            octos_agent::PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: false,
                verified_cache_dir: None,
            },
        )
        .expect("plugin load");
        assert_eq!(result.tool_count, 1, "fixture must register one tool");

        // Sanity: straight off the loader (chat's pre-fix state) the tool is
        // UNBOUND — this is the gap that breaks Host-scope plugin cwd.
        let unbound = tools
            .get("demo_tool")
            .and_then(|t| {
                t.as_any()
                    .downcast_ref::<PluginTool>()
                    .map(|p| p.work_dir().is_none())
            })
            .expect("demo_tool is a PluginTool");
        assert!(unbound, "precondition: loader leaves plugin work_dir unset");

        // Apply the chat cwd-binding (the fix run_async performs before it
        // constructs the agent). This is the ONLY step under test.
        bind_chat_plugin_work_dirs(&mut tools, &cwd);

        let bound = tools
            .get("demo_tool")
            .and_then(|t| {
                t.as_any()
                    .downcast_ref::<PluginTool>()
                    .map(|p| p.work_dir().map(|d| d.to_path_buf()))
            })
            .expect("demo_tool is a PluginTool");
        assert_eq!(
            bound.as_deref(),
            Some(cwd.as_path()),
            "chat must bind the plugin work_dir to --cwd so Host-scope (yolo) \
             sessions run plugins in --cwd, not the process launch dir"
        );
    }

    #[cfg(unix)]
    struct AlwaysAskPolicy;

    #[cfg(unix)]
    impl octos_agent::policy::CommandPolicy for AlwaysAskPolicy {
        fn check(&self, _command: &str, _cwd: &std::path::Path) -> octos_agent::policy::Decision {
            octos_agent::policy::Decision::Ask
        }
    }

    #[cfg(unix)]
    struct RecordingApprovalRequester {
        decision: ToolApprovalDecision,
        requests: Arc<std::sync::Mutex<Vec<ToolApprovalRequest>>>,
    }

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl ToolApprovalRequester for RecordingApprovalRequester {
        async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
            self.requests.lock().expect("requests lock").push(request);
            self.decision
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chat_approval_scope_resumes_ask_gated_shell_tool_once() {
        use octos_agent::{ShellTool, Tool};

        let tmp = tempfile::tempdir().expect("tempdir");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requester: Arc<dyn ToolApprovalRequester> = Arc::new(RecordingApprovalRequester {
            decision: ToolApprovalDecision::Approve,
            requests: Arc::clone(&requests),
        });
        let tool = ShellTool::new(tmp.path()).with_policy(Arc::new(AlwaysAskPolicy));

        let result = with_chat_approval(
            requester,
            tool.execute(&serde_json::json!({"command": "printf approved"})),
        )
        .await
        .expect("shell execution should return");

        assert!(
            result.success,
            "approved command should run: {}",
            result.output
        );
        assert!(result.output.contains("approved"), "{}", result.output);
        let recorded = requests.lock().expect("requests lock");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].tool_name, "shell");
        assert_eq!(recorded[0].command.as_deref(), Some("printf approved"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chat_approval_scope_denies_ask_gated_shell_tool_once() {
        use octos_agent::{ShellTool, Tool};

        let tmp = tempfile::tempdir().expect("tempdir");
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requester: Arc<dyn ToolApprovalRequester> = Arc::new(RecordingApprovalRequester {
            decision: ToolApprovalDecision::Deny,
            requests: Arc::clone(&requests),
        });
        let tool = ShellTool::new(tmp.path()).with_policy(Arc::new(AlwaysAskPolicy));

        let result = with_chat_approval(
            requester,
            tool.execute(&serde_json::json!({"command": "printf denied"})),
        )
        .await
        .expect("shell execution should return");

        assert!(!result.success, "denied command must not run");
        assert!(!result.output.contains("denied\n"), "{}", result.output);
        assert!(result.output.contains("Command denied by user approval"));
        assert_eq!(requests.lock().expect("requests lock").len(), 1);
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
    eprintln!("{}: {}", "Model".green(), provider.model_id());
    Ok(provider)
}

/// Inner factory that accepts an explicit `api_type` override.
///
/// Does NOT print to stdout — callers that want a log line should print
/// after calling this function.
///
/// Exposed as `pub` (was `pub(crate)`) so the `octos-ffi` C-ABI crate can
/// reuse the exact provider-construction path (auth store → env_vars → env
/// key resolution, timeout overrides, anthropic/responses api_type bypasses)
/// instead of re-implementing a drift-prone parallel factory.
pub fn create_provider_with_api_type(
    name: &str,
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    if name == "custom" {
        return create_custom_provider(config, model, base_url, api_type);
    }

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

fn create_custom_provider(
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    let key = config.get_api_key("custom")?;
    let model = model.ok_or_else(|| eyre::eyre!("custom provider requires model"))?;
    let base_url = base_url.ok_or_else(|| eyre::eyre!("custom provider requires base_url"))?;

    // Extract timeout overrides from gateway config (if any).
    let llm_timeout_secs = config.gateway.as_ref().and_then(|g| g.llm_timeout_secs);
    let llm_connect_timeout_secs = config
        .gateway
        .as_ref()
        .and_then(|g| g.llm_connect_timeout_secs);

    match api_type.unwrap_or("openai") {
        "openai" => {
            let mut provider = octos_llm::openai::OpenAIProvider::new(key, model)
                .with_base_url(&base_url)
                .with_provider_label("custom");
            if let Some(t) = llm_timeout_secs {
                let c =
                    llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
                provider = provider.with_http_timeout(t, c);
            }
            Ok(Arc::new(provider))
        }
        "anthropic" => {
            let mut provider = octos_llm::anthropic::AnthropicProvider::new(key, model)
                .with_base_url(&base_url)
                .with_provider_label("custom");
            if let Some(t) = llm_timeout_secs {
                let c =
                    llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
                provider = provider.with_http_timeout(t, c);
            }
            Ok(Arc::new(provider))
        }
        other => eyre::bail!("unsupported custom api_type '{other}'; use openai or anthropic"),
    }
}

#[cfg(test)]
mod custom_provider_tests {
    use super::*;

    fn custom_config() -> Config {
        let mut config = Config {
            api_key_env: Some("CUSTOM_API_KEY".to_string()),
            ..Default::default()
        };
        config
            .env_vars
            .insert("CUSTOM_API_KEY".to_string(), "test-key".to_string());
        config
    }

    #[test]
    fn creates_custom_openai_compatible_provider() {
        let provider = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("llama-3.1-70b-instruct".to_string()),
            Some("http://127.0.0.1:11434/v1".to_string()),
            Some("openai"),
        )
        .unwrap();

        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(provider.model_id(), "llama-3.1-70b-instruct");
    }

    #[test]
    fn creates_custom_anthropic_compatible_provider() {
        let provider = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("claude-compatible".to_string()),
            Some("https://proxy.example.com/anthropic".to_string()),
            Some("anthropic"),
        )
        .unwrap();

        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(provider.model_id(), "claude-compatible");
    }

    #[test]
    fn rejects_custom_provider_without_base_url() {
        let result = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("llama".to_string()),
            None,
            Some("openai"),
        );

        match result {
            Ok(provider) => panic!(
                "expected missing base_url error, got provider {}",
                provider.provider_name()
            ),
            Err(err) => assert!(err.to_string().contains("requires base_url")),
        }
    }
}
