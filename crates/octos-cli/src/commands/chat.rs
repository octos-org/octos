//! Chat command: interactive multi-turn conversation with an agent.

#[cfg(feature = "api")]
use std::io::IsTerminal;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "api")]
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Args, ValueEnum};
use colored::Colorize;
use eyre::{Result, WrapErr, eyre};
#[cfg(feature = "api")]
use octos_agent::{ToolApprovalDecision, ToolApprovalRequest, ToolApprovalRequester};
#[cfg(feature = "api")]
use octos_agent::{UserQuestionOutcome, UserQuestionRequest, UserQuestionRequester};
#[cfg(feature = "api")]
use octos_core::ui_protocol::UserQuestionAnswer;
use octos_llm::{EmbeddingProvider, LlmProvider, OpenAIEmbedder};
#[cfg(feature = "api")]
use rustyline::DefaultEditor;

use super::Executable;
use crate::config::Config;
pub use octos_services::provider_factory::create_provider_with_api_type;

#[cfg(feature = "api")]
mod oup;

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

    /// Maximum LLM-loop iterations per message. 0 (default) means unlimited;
    /// cancellation, idle detection, tool timeouts and convergence checkpoints
    /// remain active.
    #[arg(long, default_value = "0")]
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
    /// (`coding`, `swarm`), a user-dir id under
    /// `~/.octos/profiles/<id>/`, or an explicit path to a profile
    /// JSON/TOML file.
    ///
    /// If the id names a stored serve/onboarding profile (one created by
    /// `octos serve` or octoscode, saved as `~/.octos/profiles/<id>.json`),
    /// its LLM provider/model, route, and API key (`env_vars`) are reused too —
    /// so you don't re-enter a model or key that a profile already holds.
    /// `--config`, `--provider`, and `--model` still override.
    ///
    /// Defaults to `coding`: four common tools plus fixed discover/run
    /// gateways. Optional authorized tools remain accessible through discovery.
    /// Profiles customize authorization and model/agent setup.
    #[arg(long)]
    pub profile: Option<String>,

    /// Enable the GOAL tools (`goal_create` / `goal_get` / `goal_update`) in
    /// this chat session. A goal is a durable objective + token budget that
    /// SURVIVES across `octos chat` invocations: state lives in the profile's
    /// supervisor store, so a later `octos chat --goals` in the same profile
    /// re-reads the same goal. Off by default — the tool surface is unchanged
    /// unless you ask for it.
    ///
    /// Peers are opt-in on top of this — see `--peers`.
    #[arg(long)]
    pub goals: bool,

    /// Enable the PEER tools (`peer_handoff` / `peer_list` / `peer_respond`)
    /// and host staged peers IN THIS PROCESS.
    ///
    /// `peer_handoff` stages a peer under `<data_dir>/peers/<slug>` and chat
    /// immediately opens a `peer-<slug>` session for it, running its `brief.md`
    /// as the peer's first turn. A peer that hits a tool approval or an
    /// `ask_user_question` does NOT prompt this terminal — the terminal belongs
    /// to you, the master. It PARKS, `peer_list` reports it as
    /// `awaiting_input`, and you answer it with `peer_respond`.
    ///
    /// Requires `--goals`: a peer is a unit of work under a goal, and the
    /// handoff auto-binds to the session's active goal.
    #[arg(long, requires = "goals")]
    pub peers: bool,

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

    /// Reasoning effort for thinking models: `none`, `low`, `medium`, `high`,
    /// or `max` (claude/codex parity). `none` disables reasoning where
    /// supported. Overrides the config
    /// `gateway.reasoning_effort`; non-thinking models ignore it, and
    /// providers without a distinct `max` tier clamp it to `high`.
    #[arg(long, value_enum)]
    pub effort: Option<ChatEffort>,

    /// Do NOT persist this run as an episode (ephemeral). Mirrors
    /// `claude --no-session-persistence`: by default a completed turn is
    /// saved to the episode store for future recall; this skips that write.
    /// Chat history and context sidecars use a temporary runtime directory.
    /// Shared profile memory, tools and skills remain available; explicit
    /// memory, file, cron or goal writes are not disabled by this flag.
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
/// (`none`/`low`/`medium`/`high`/`max`), so a `config.cli.chat.effort` round-trips
/// losslessly — matching [`ChatSandboxMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatEffort {
    None,
    Low,
    Medium,
    High,
    Max,
}

impl From<ChatEffort> for octos_llm::ReasoningEffort {
    fn from(effort: ChatEffort) -> Self {
        match effort {
            ChatEffort::None => octos_llm::ReasoningEffort::Disabled,
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
#[cfg(feature = "api")]
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
#[cfg(feature = "api")]
pub(crate) fn detach_route_on_provider_override(config: &mut Config, cli_provider: Option<&str>) {
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

/// Exit commands.
#[cfg(feature = "api")]
const EXIT_COMMANDS: &[&str] = &["exit", "quit", "/exit", "/quit", ":q"];

/// Serializes ALL interactive stdin prompts (approvals AND user questions):
/// if two prompt-raising tools run in the same turn, their stdin prints/reads
/// must not interleave — otherwise a single `y` or a picked number could land
/// on whichever request won the stdin race rather than the one the user
/// meant. One module-level lock shared by both requesters — a function-local
/// `static` would be a *distinct* mutex per function and not serialize an
/// approval against a question (codex review).
#[cfg(feature = "api")]
static CHAT_PROMPT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the user answered at the CLI approval prompt. `ApproveSession`
/// mirrors the TUI's `s` action / the serve `approval_scope: "session"`
/// (`ApprovalScopeKind::ApproveForSession`): every later approval-gated
/// request in this chat process auto-resolves without prompting.
#[cfg(feature = "api")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliApprovalAnswer {
    ApproveOnce,
    ApproveSession,
    Deny,
}

/// Parse the `[y/s/N]` answer line. Empty / unrecognized input denies —
/// same fail-closed default as the old `[y/N]` prompt.
#[cfg(feature = "api")]
fn parse_cli_approval_answer(line: &str) -> CliApprovalAnswer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => CliApprovalAnswer::ApproveOnce,
        "s" | "session" => CliApprovalAnswer::ApproveSession,
        _ => CliApprovalAnswer::Deny,
    }
}

#[cfg(feature = "api")]
#[derive(Default)]
struct CliApprovalRequester {
    /// Set once the user answers `s` — the CLI-chat equivalent of the serve
    /// scope table's `(ApproveForSession, MatchKey::Session)` entry, which
    /// auto-resolves every subsequent approval in the session. Scope lifetime
    /// is this chat process (serve evicts its entry on session close).
    session_approved: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "api")]
impl CliApprovalRequester {
    fn session_approved(&self) -> bool {
        self.session_approved
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl ToolApprovalRequester for CliApprovalRequester {
    async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        // Fast path: a prior `s` answer auto-resolves without prompting
        // (mirrors serve's `approval_auto_resolved`). Print a note so the
        // grant stays visible instead of commands silently running. A
        // once-only approval (UPCR-2026-035) is never auto-resolved.
        let once_only = request.once_only;
        if self.session_approved() && !once_only {
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
        if self.session_approved() && !once_only {
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
            // A once-only approval answers this call and records no session
            // scope.
            CliApprovalAnswer::ApproveSession if once_only => ToolApprovalDecision::Approve,
            CliApprovalAnswer::ApproveSession => {
                self.session_approved
                    .store(true, std::sync::atomic::Ordering::Release);
                ToolApprovalDecision::Approve
            }
            CliApprovalAnswer::Deny => ToolApprovalDecision::Deny,
        }
    }
}

#[cfg(feature = "api")]
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

#[cfg(feature = "api")]
struct CliUserQuestionRequester;

#[cfg(feature = "api")]
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

#[cfg(feature = "api")]
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
#[cfg(feature = "api")]
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

/// The goal tools `octos chat --goals` registers. Also the exact set added to
/// an allow-list profile surface so `filter_by_profile` keeps them.
#[cfg(feature = "api")]
const CHAT_GOAL_TOOLS: &[&str] = &["goal_get", "goal_create", "goal_update"];

/// The peer tools `octos chat --peers` registers, and the exact set added to an
/// allow-list profile surface.
///
/// Preserve the lean default surface. The OUP backend also implements gather,
/// close and follow-up input for profiles that explicitly allow those tools.
#[cfg(feature = "api")]
const CHAT_PEER_TOOLS: &[&str] = &["peer_handoff", "peer_list", "peer_respond"];

/// Add `wanted` to an ALLOW-LIST profile surface, in place, without duplicates.
///
/// Chat's default `coding` profile is an allow list, so REGISTERING a tool is
/// not enough — `filter_by_profile` drops anything the list does not name and
/// the model never sees it (observed live in Phase 1: the tool count was
/// identical with and without `--goals`). A deny list and the pass-through
/// `Default` mode need no change (none of these names appear in either), and an
/// EMPTY allow list is already pass-through, so both are left alone.
#[cfg(feature = "api")]
fn widen_allow_list(surface: &mut octos_agent::profile::ProfileTools, wanted: &[&str]) {
    if let octos_agent::profile::ProfileTools::AllowList { tools } = surface {
        if !tools.is_empty() {
            for name in wanted {
                if !tools.iter().any(|entry| entry == name) {
                    tools.push((*name).to_owned());
                }
            }
        }
    }
}

/// Stable goal session key for `octos chat --goals`.
///
/// Goal tools and the OUP dispatcher share this durable session identity.
/// Minting the SAME key on every run is precisely what
/// makes a chat goal outlive the process. Scoped by profile so two profiles
/// don't share one goal; the `cli` segment keeps it from colliding with a
/// `serve` wire session (`<profile>:local:<name>` / `<profile>:api:<name>`).
#[cfg(feature = "api")]
fn chat_goal_session_key(profile_id: &str) -> String {
    format!("{profile_id}:cli:chat")
}

/// Machine-readable result envelope for `octos chat --json --message`.
///
/// Text, answering model and token usage come from OUP's terminal and
/// canonical persisted answer, never an inferred completion summary.
#[cfg(feature = "api")]
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
}

#[cfg(feature = "api")]
impl ChatJsonResult {
    /// Serialize to a single-line JSON string (nice for piping). This fixed
    /// string/number-only struct cannot realistically fail to serialize; if it
    /// ever did, fall back to a valid JSON error object so stdout stays
    /// parseable.
    fn to_json_line(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize chat result\"}".to_string())
    }
}

/// Generic failures keep the existing one-object error shape. A failed OUP
/// terminal additionally exposes its typed code, exact cumulative usage, and
/// actual nonempty partial answer. It is never a successful result envelope.
fn json_error_value(error: &eyre::Report) -> serde_json::Value {
    let value = serde_json::json!({ "error": error.to_string() });
    #[cfg(feature = "api")]
    if let Some(failure) = error.downcast_ref::<crate::commands::oup_session::OupTurnFailure>() {
        let mut value = value;
        if let Some(terminal_error) = &failure.terminal_error {
            value["code"] = serde_json::json!(terminal_error.code);
        }
        value["usage"] = serde_json::json!(failure.partial.usage);
        if !failure.partial.text.trim().is_empty() {
            value["partial"] = serde_json::json!({
                "text": failure.partial.text,
                "model": failure.partial.model,
            });
        }
        return value;
    }
    value
}

fn print_json_error(error: &eyre::Report) {
    let obj = json_error_value(error);
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
                print_json_error(&error);
                std::process::exit(1);
            }
            Err(error) => Err(error),
        }
    }
}

impl ChatCommand {
    async fn run_async(self) -> Result<()> {
        #[cfg(feature = "api")]
        {
            self.run_oup().await
        }
        #[cfg(not(feature = "api"))]
        {
            eyre::bail!(
                "octos chat requires the OUP runtime; rebuild with default features or --features api"
            )
        }
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
#[cfg(feature = "api")]
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
#[cfg(feature = "api")]
pub(crate) fn load_serve_profile_config(
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
///
/// With the default build (feature `embed-llama`) and no `embedding` section,
/// the bundled in-process embedder is used: EmbeddingGemma-300M under
/// `<data_dir>/models/`, fetched on first use unless downloads are disabled
/// (`embedding.auto_download = false` / `OCTOS_NO_MODEL_DOWNLOAD`). Without
/// the model the runtime stays keyword-only.
pub(crate) fn create_embedder(config: &Config) -> Option<Arc<dyn EmbeddingProvider>> {
    let data_dir = octos_services::config_context::resolve_config_context(None).data_dir;
    create_embedder_in(config, &data_dir)
}

/// Async-runtime wrapper for provider construction. The bundled model is lazy,
/// but an explicit custom GGUF still needs eager native-dimension discovery;
/// keep that llama load/context creation off Tokio's async workers.
pub(crate) async fn create_embedder_async(config: &Config) -> Option<Arc<dyn EmbeddingProvider>> {
    let config = config.clone();
    match tokio::task::spawn_blocking(move || create_embedder(&config)).await {
        Ok(provider) => provider,
        Err(error) => {
            tracing::error!(%error, "failed to join embedding provider construction");
            None
        }
    }
}

/// [`create_embedder`] for an explicit data dir (where the default model lives).
pub(crate) fn create_embedder_in(
    config: &Config,
    data_dir: &std::path::Path,
) -> Option<Arc<dyn EmbeddingProvider>> {
    let bundled_default;
    let cfg = match config.embedding.as_ref() {
        Some(cfg) => cfg,
        None => {
            if !cfg!(feature = "embed-llama") {
                return None;
            }
            bundled_default = crate::config::EmbeddingConfig {
                provider: "llamacpp".to_string(),
                api_key_env: None,
                base_url: None,
                model: None,
                dimensions: None,
                model_path: None,
                auto_download: None,
            };
            &bundled_default
        }
    };

    // In-process llama.cpp GGUF provider (every platform, feature `embed-llama`).
    // `provider = "llamacpp"` + `model_path = "<file.gguf>"`; `dimensions`
    // truncates the output via Matryoshka (MRL). Unlike the MLX provider below
    // this is NOT Apple-only, and it runs any GGUF embedding model rather than
    // one hand-ported architecture — with a CPU backend that is a legitimate
    // choice, not a fallback.
    if cfg.provider.eq_ignore_ascii_case("llamacpp") || cfg.provider.eq_ignore_ascii_case("llama") {
        #[cfg(feature = "embed-llama")]
        {
            // Explicit path wins; otherwise the bundled default model,
            // provisioned under the data dir on first use.
            let configured = cfg.model_path.as_deref().or(cfg.model.as_deref());
            let resolved: String = match configured {
                Some(p) => p.to_string(),
                None => {
                    let download = crate::embed_model::downloads_allowed(cfg.auto_download);
                    let status = crate::embed_model::model_status(data_dir);
                    if status.complete {
                        status.path.to_string_lossy().into_owned()
                    } else if download {
                        // Session/profile bootstrap must not wait for the 334 MB
                        // model. Recall starts keyword-only and backfills when ready.
                        crate::embed_model::spawn_default_model_download(data_dir, true);
                        return Some(Arc::new(
                            crate::embed_model::DeferredEmbeddingProvider::new(
                                data_dir.to_path_buf(),
                            ),
                        ));
                    } else {
                        tracing::warn!(
                            path = %status.path.display(),
                            "default embedding model unavailable; memory search runs keyword-only"
                        );
                        return None;
                    }
                }
            };
            let path = resolved.as_str();
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
            // The bundled model has a known native width, so profile bootstrap
            // can size its index without loading 334 MiB of weights. Explicit
            // custom GGUFs retain eager construction because their native width
            // is not knowable from this config; both paths share cached weights.
            let mut e = if configured.is_none() {
                octos_embed_llama::LlamaEmbedder::from_model_file_lazy(
                    path,
                    n_gpu_layers,
                    crate::embed_model::DEFAULT_MODEL_NATIVE_DIM,
                )
            } else {
                match octos_embed_llama::LlamaEmbedder::from_model_file(path, n_gpu_layers) {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::error!(%err, model_path = %path, "failed to load llama.cpp embedder");
                        return None;
                    }
                }
            };
            if let Some(d) = cfg.dimensions {
                e = e.with_output_dim(d as usize);
            }
            tracing::info!(
                model_path = %path,
                dimension = e.dimension(),
                n_gpu_layers,
                initialized = e.is_initialized(),
                "configured in-process llama.cpp embedder"
            );
            return Some(Arc::new(e));
        }
        #[cfg(not(feature = "embed-llama"))]
        {
            let _ = data_dir;
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
