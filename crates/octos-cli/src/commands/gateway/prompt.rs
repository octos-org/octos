//! System prompt construction for gateway mode.

use std::path::Path;

use octos_agent::SkillsLoader;

use crate::persona_service::PersonaService;

const BUNDLED_PROMPT: &str = include_str!("../../prompts/gateway_default.txt");

pub const SLASH_COMMANDS_SEGMENT_NAME: &str = "slash_commands";
const SLASH_COMMANDS_HEADER: &str = "## Slash Commands";
const MAX_CLIENT_COMMANDS: usize = 64;
const MAX_CLIENT_COMMAND_LEN: usize = 32;
/// Gateway commands serve intercepts as unavailable (`api::ws_slash`) that act
/// on gateway per-actor state (adaptive router, queue mode, session reset):
/// no client can honor them, so declarations of them are rejected.
pub const SERVER_STATE_COMMANDS: &[&str] = &["adaptive", "router", "queue", "reset"];
/// Gateway commands serve also intercepts, but that a client can handle
/// locally without reaching the server (octoscode implements both), so
/// declarations of them are accepted.
pub const CLIENT_HANDLED_COMMANDS: &[&str] = &["status", "thinking"];

/// Build the system prompt with bootstrap files, memory context, and skills.
///
/// `max_inject_tokens` caps the injected memory block (long-term memory +
/// daily notes + bank summary combined); use
/// [`crate::config::MemoryConfig::effective_max_inject_tokens`] to resolve it
/// from config.
// These are the cohesive set of bootstrap inputs a system prompt is composed
// from (persona base, data/project dirs, memory + skills sources, and the two
// resolved memory knobs); they don't group into a smaller, meaningful sub-type,
// so the arg count is expected here.
#[allow(clippy::too_many_arguments)]
/// The base system prompt split at the memory slot. Memory is injected as
/// a NAMED per-agent prompt segment BETWEEN these halves (the pre-refactor
/// order was `…bootstrap/soul → memory → skills/tool prefs`; keeping the
/// slot preserves prompt precedence — persisted user memory must not
/// override the skill/tool guidance that always followed it).
#[derive(Debug, Clone, Default)]
pub struct GatewayPromptParts {
    /// Everything before the memory slot (base, date, platform, persona,
    /// bootstrap files, soul).
    pub pre_memory: String,
    /// Everything after it (active skills, skills summary, tool prefs).
    pub post_memory: String,
    /// Only the bundled base may be projected by tool availability. Operator
    /// prompts and appended workspace/persona/skill instructions stay verbatim.
    pub uses_bundled_base: bool,
}

impl GatewayPromptParts {
    /// Project once when constructing the session, after tool policy is bound.
    /// Never rebuild this prefix per model request or based on recent tool use.
    pub fn pre_memory_for_registry(&self, registry: &octos_agent::ToolRegistry) -> String {
        self.project_available_rules(|name| registry.is_tool_model_reachable(name))
    }

    fn project_available_rules(&self, available: impl Fn(&str) -> bool) -> String {
        if !self.uses_bundled_base {
            return self.pre_memory.clone();
        }
        let Some(suffix) = self.pre_memory.strip_prefix(BUNDLED_PROMPT) else {
            // A later operator override replaced the base.
            return self.pre_memory.clone();
        };
        let research_available = [
            "search",
            "deep_crawl",
            "web_search",
            "web_fetch",
            "news_fetch",
            "get_weather",
            "get_time",
            "podcast_generate",
            "podcast_voices",
            "voice_synthesize",
        ]
        .iter()
        .any(|name| available(name));
        let mut projected = String::new();
        for (index, section) in BUNDLED_PROMPT.split("\n## ").enumerate() {
            if index == 0 {
                projected.push_str(section);
                continue;
            }
            let heading = section.lines().next().unwrap_or_default();
            let keep = match heading {
                "Research & Search Rules" => research_available,
                "Rich Card Rendering (capable clients)" => available("send_app_card"),
                "Pipelines" => available("run_pipeline"),
                "Background Tasks" => [
                    "spawn",
                    "check_background_tasks",
                    "check_workspace_contract",
                ]
                .iter()
                .any(|name| available(name)),
                "Scheduled Tasks (Cron)" => available("cron"),
                "Other Rules" => available("message") || available("save_memory"),
                _ => true, // Keep core rules and future sections by default.
            };
            if keep {
                projected.push_str("\n## ");
                projected.push_str(section);
            }
        }
        projected.push_str(suffix);
        projected
    }

    /// The joined prompt WITHOUT a memory block — for read-only consumers
    /// (length logging, tests) that don't build agents.
    pub fn joined(&self) -> String {
        let mut out = self.pre_memory.clone();
        out.push_str(&self.post_memory);
        out
    }
}

/// Remove the `## Slash Commands` section (up to the next `## ` heading) from
/// `prompt`. Those commands are handled by bus channels only; a prompt
/// without the section — e.g. an operator override — comes back unchanged.
pub fn strip_slash_commands(prompt: &str) -> String {
    let Some(start) = prompt
        .match_indices(SLASH_COMMANDS_HEADER)
        .map(|(index, _)| index)
        .find(|&index| index == 0 || prompt[..index].ends_with('\n'))
    else {
        return prompt.to_string();
    };
    let body_start = start + SLASH_COMMANDS_HEADER.len();
    let end = prompt[body_start..]
        .find("\n## ")
        .map_or(prompt.len(), |offset| body_start + offset + 1);
    format!("{}{}", &prompt[..start], &prompt[end..])
}

/// The slash commands the server accepts out of those a client declared on
/// `session/open`, each as `/name`. Names are validated (alphanumeric, `-`,
/// `_`), deduplicated and capped, since they land in the system prompt;
/// gateway-only commands are dropped.
pub fn accepted_client_commands(commands: &[String]) -> Vec<String> {
    let mut names: Vec<&str> = Vec::new();
    for command in commands {
        let name = command.trim().trim_start_matches('/');
        let valid = !name.is_empty()
            && name.len() <= MAX_CLIENT_COMMAND_LEN
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && !SERVER_STATE_COMMANDS
                .iter()
                .any(|blocked| name.eq_ignore_ascii_case(blocked));
        if valid && !names.contains(&name) {
            names.push(name);
        }
        if names.len() == MAX_CLIENT_COMMANDS {
            break;
        }
    }
    names.iter().map(|name| format!("/{name}")).collect()
}

/// Render the slash commands a client declared on `session/open`, filtered
/// by [`accepted_client_commands`]. Nothing valid renders as an empty section.
pub fn render_client_commands(commands: &[String]) -> String {
    let names = accepted_client_commands(commands);
    if names.is_empty() {
        return String::new();
    }
    let list = names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{SLASH_COMMANDS_HEADER}\n\n\
         The user's client handles these slash commands itself (no LLM round-trip): {list}.\n\n\
         When the user asks about something one of them covers, point them to it. \
         Do not suggest slash commands that are not listed here."
    )
}

pub async fn build_system_prompt(
    base: Option<&str>,
    data_dir: &Path,
    project_dir: &Path,
    skills_loader: &SkillsLoader,
    tool_config: &octos_agent::ToolConfigStore,
) -> GatewayPromptParts {
    let compiled = BUNDLED_PROMPT;
    let runtime = super::super::load_prompt("gateway", compiled);
    let mut prompt = base.unwrap_or(&runtime).to_string();

    // Inject current date so the model knows "今年" = which year
    let today = chrono::Local::now().format("%Y-%m-%d");
    prompt.push_str(&format!("\n\nCurrent date: {today}"));

    // Inject platform guidance so the model doesn't emit Unix-only shell
    // commands on Windows hosts.
    #[cfg(windows)]
    {
        prompt.push_str(
            "\n\n## Runtime Platform\n\n\
             Current host OS: Windows.\n\
             If you use the shell tool, write Windows cmd.exe-compatible commands only.\n\
             Do NOT use Unix-only commands like `ps`, `grep`, `head`, `rm`, `ls`, `cat`, `which`, or `bash`.\n\
             Prefer built-in tools (`glob`, `grep`, `list_dir`, `read_file`, `deep_search`, `deep_crawl`, `web_search`, `web_fetch`) over shell whenever possible.\n\
             If a task depends on a tool or binary that is not available on this host, say so explicitly and do not retry via shell.",
        );
    }

    // Inject dynamically generated persona (from persona.md) if available
    if let Some(persona) = PersonaService::read_persona(data_dir) {
        prompt.push_str("\n\n## Communication Style\n\n");
        prompt.push_str(&persona);
    }

    // Append bootstrap files (AGENTS.md, SOUL.md, USER.md, etc.)
    let bootstrap = super::super::load_bootstrap_files(project_dir);
    if !bootstrap.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&bootstrap);
    }

    // Per-user soul override (takes precedence over shared SOUL.md)
    if let Some(user_soul) = crate::soul_service::read_soul(data_dir) {
        prompt.push_str("\n\n## Soul\n\n");
        prompt.push_str(&user_soul);
    }

    // ---- memory slot ----------------------------------------------------
    // Memory is NOT inlined here anymore: every model call flows through a
    // per-session `octos_agent::Agent`, which owns the memory as a named
    // prompt segment refreshed at each turn start (chat.rs pattern —
    // fingerprint stat per turn). Inlining it in this base String froze it
    // at build time. The split preserves the slot's POSITION.
    let pre_memory = std::mem::take(&mut prompt);

    // Append always-on skills
    if let Ok(always_names) = skills_loader.get_always_skills().await {
        if !always_names.is_empty() {
            if let Ok(skills_content) = skills_loader.load_skills_for_context(&always_names).await {
                if !skills_content.is_empty() {
                    prompt.push_str("\n\n## Active Skills\n\n");
                    prompt.push_str(&skills_content);
                }
            }
        }
    }

    // Append skills summary
    if let Ok(summary) = skills_loader.build_skills_summary().await {
        if !summary.is_empty() {
            prompt.push_str("\n\n## Available Skills\n\n");
            prompt.push_str(&summary);
        }
    }

    // Append tool preferences summary
    let config_summary = tool_config.summary().await;
    if !config_summary.is_empty() {
        prompt.push_str("\n\n## Tool Preferences\n\n");
        prompt.push_str(&config_summary);
    }

    GatewayPromptParts {
        pre_memory,
        post_memory: prompt,
        uses_bundled_base: base.is_none() && runtime == compiled,
    }
}

/// Extract a string value from channel settings JSON, with a default fallback.
#[cfg(any(
    feature = "telegram",
    feature = "discord",
    feature = "dingtalk",
    feature = "slack",
    feature = "whatsapp",
    feature = "email",
    feature = "feishu",
    feature = "twilio",
    feature = "wecom",
    feature = "wecom-bot",
    feature = "line",
    feature = "matrix",
    feature = "qq-bot",
    feature = "wechat"
))]
pub fn settings_str(settings: &serde_json::Value, key: &str, default: &str) -> String {
    settings
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}
