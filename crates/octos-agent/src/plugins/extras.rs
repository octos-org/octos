//! Resolve skill manifest extras (MCP servers, hooks, prompt fragments) into
//! runtime-ready config types.

use std::collections::HashMap;
use std::path::Path;

use tracing::warn;

use crate::hooks::HookConfig;
use crate::mcp::McpServerConfig;

use super::manifest::{PluginManifest, SkillDiscovery, SkillHookDef, SkillMcpServer};

/// One-paragraph generic preamble pushed once per `resolve_extras` invocation
/// (before any plugin cards). Tells the LLM the skill_dir is read-accessible
/// via the existing file tools and to treat the skill source as the source
/// of truth rather than guessing from the card's summary.
///
/// PR-F replaced PR-C/D's per-hint curation with this generic instruction
/// after observing (a) hand-curated trigger phrases didn't survive doc
/// renames, and (b) the hints implicitly narrowed LLM curiosity vs. the
/// "you have the filesystem, go look" model that Claude Code's skill loader
/// uses successfully.
pub const SKILL_EXPLORATION_PREAMBLE: &str = "\
Active plugin skill directories are listed below as cards. Each `skill_dir` \
is read-accessible via `read_file`, `glob`, `list_dir`, `grep`. When you need \
a format schema, a worked example, the available styles/templates, or any \
other detail beyond the card's summary, READ the relevant files (SKILL.md, \
docs/*, examples/*, schemas, etc.) before guessing. The code and examples in \
skill_dir are the source of truth; the card is just a pointer.";

/// Resolved extras from a skill manifest, ready to merge into agent config.
#[derive(Debug, Default)]
pub struct SkillExtras {
    pub mcp_servers: Vec<McpServerConfig>,
    pub hooks: Vec<HookConfig>,
    pub prompt_fragments: Vec<String>,
    /// Tool names that should run in background automatically.
    pub spawn_only_tools: Vec<String>,
    /// Custom messages per spawn_only tool.
    pub spawn_only_messages: std::collections::HashMap<String, String>,
    /// RFC-1 (issue #1290): dispatcher entries collected from manifests
    /// that declare `make_type`. Each entry carries the content_type
    /// label, the target tool name to dispatch to, and the
    /// human-readable description. The loader feeds this list into the
    /// `mofa_make` registration site so the LLM-visible enum is built
    /// from the actually-discovered skills (per-profile shadowing
    /// preserved).
    pub make_type_entries: Vec<crate::tools::MakeTypeEntry>,
}

/// Resolve manifest extras against the skill directory.
///
/// - MCP: resolves relative commands and `./`/`../` arg elements against
///   `skill_dir`, looks up env var names from the process environment.
/// - Hooks: parses event strings into `HookEvent`, resolves relative command paths.
/// - Discovery: renders a short 5-line skill card (PR-F) preceded by a
///   generic exploration preamble emitted once per call.
/// - Prompts: expands glob patterns against `skill_dir`, reads `.md` files.
///
/// PR-E note: the legacy "auto-inject the full SKILL.md body for spawn_only
/// plugins" code path has been removed. The discovery card + preamble are
/// the structured replacement; an explicit `prompts.include` glob is the
/// escape hatch for skills that still want a prompt fragment embedded.
///
/// PR-F note: only one preamble survives per session (multiple plugins all
/// push the same constant string; the loader's merge step folds duplicates
/// before serialising into the system prompt). The card itself collapsed
/// to 5 lines (name / purpose / tools / skill_dir).
pub fn resolve_extras(manifest: &PluginManifest, skill_dir: &Path) -> SkillExtras {
    let mut extras = SkillExtras::default();

    for srv in &manifest.mcp_servers {
        extras.mcp_servers.push(resolve_mcp_server(srv, skill_dir));
    }

    for hook_def in &manifest.hooks {
        match resolve_hook(hook_def, skill_dir) {
            Some(hook) => extras.hooks.push(hook),
            None => {
                warn!(
                    event = %hook_def.event,
                    skill = %manifest.name,
                    "unknown hook event, skipping"
                );
            }
        }
    }

    // PR-F: emit the generic exploration preamble + the 5-line card.
    // Per-hint curation from PR-C/D is gone — the preamble tells the
    // LLM to explore the skill_dir directly via the already-allowlisted
    // file tools.
    //
    // `resolve_extras` runs once per plugin; every discovery-bearing
    // plugin pushes the same preamble constant. Downstream merge code
    // (`PluginLoadResult::merge_extras`) is responsible for keeping only
    // the first preamble occurrence so the final system prompt has it
    // exactly once. See `extras_emits_exploration_preamble_once_per_session`.
    if let Some(discovery) = &manifest.discovery {
        extras
            .prompt_fragments
            .push(SKILL_EXPLORATION_PREAMBLE.to_string());
        extras
            .prompt_fragments
            .push(render_skill_card(manifest, discovery, skill_dir));
    }

    if let Some(prompts) = &manifest.prompts {
        for pattern in &prompts.include {
            let full_pattern = skill_dir.join(pattern);
            match glob::glob(&full_pattern.to_string_lossy()) {
                Ok(paths) => {
                    for entry in paths.flatten() {
                        match std::fs::read_to_string(&entry) {
                            Ok(content) => extras.prompt_fragments.push(content),
                            Err(e) => {
                                warn!(
                                    path = %entry.display(),
                                    error = %e,
                                    "failed to read prompt fragment"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        pattern = %pattern,
                        error = %e,
                        "invalid prompt glob pattern"
                    );
                }
            }
        }
    }

    extras
}

/// Render a 5-line skill card from manifest discovery (PR-F).
///
/// Output is a fixed-shape text block, suitable for concatenating into
/// the system prompt alongside the generic exploration preamble. The
/// `if you need:` section from PR-C/D is gone; the preamble tells the
/// LLM the skill_dir is read-accessible and to look there instead of
/// trusting the card.
///
/// Shape (4 newline-separated lines; the final `skill_dir` line has no
/// trailing newline so successive fragments don't accumulate blanks):
/// ```text
/// - name: <plugin_name>
///   purpose: <discovery.summary OR "(no summary)">
///   tools: <comma-separated tool names>
///   skill_dir: <absolute path>
/// ```
fn render_skill_card(
    manifest: &PluginManifest,
    discovery: &SkillDiscovery,
    skill_dir: &Path,
) -> String {
    let purpose = discovery.summary.as_deref().unwrap_or("(no summary)");
    let tools = manifest
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let mut card = String::with_capacity(256);
    card.push_str(&format!("- name: {}\n", manifest.name));
    card.push_str(&format!("  purpose: {purpose}\n"));
    card.push_str(&format!("  tools: {tools}\n"));
    card.push_str(&format!("  skill_dir: {}", skill_dir.display()));
    card
}

/// Resolve a `./`/`../`-prefixed relative path against the skill dir;
/// anything else (bare commands like "node", flags, absolute paths, plain
/// arguments) passes through verbatim.
fn resolve_skill_path(arg: &str, skill_dir: &Path) -> String {
    let p = Path::new(arg);
    if p.is_relative() && (arg.starts_with("./") || arg.starts_with("../")) {
        skill_dir.join(p).to_string_lossy().into_owned()
    } else {
        arg.to_owned()
    }
}

/// Convert a skill MCP server declaration into a runtime `McpServerConfig`.
fn resolve_mcp_server(srv: &SkillMcpServer, skill_dir: &Path) -> McpServerConfig {
    // Resolve relative command paths against skill dir; bare commands (e.g. "node") left for PATH.
    let command = srv
        .command
        .as_ref()
        .map(|cmd| resolve_skill_path(cmd, skill_dir));

    // `connect_stdio` sets no `current_dir`, so the server process runs with
    // the daemon's cwd — never the skill dir. Every `./`/`../` arg element
    // must resolve here too; resolving only `command` leaves a script path
    // like `["node", "./server.js"]` pointing at the wrong root.
    let args = srv
        .args
        .iter()
        .map(|arg| resolve_skill_path(arg, skill_dir))
        .collect();

    // Resolve env var NAMES to actual values from the process environment.
    let mut env = HashMap::new();
    for name in &srv.env {
        if let Ok(val) = std::env::var(name) {
            env.insert(name.clone(), val);
        }
    }

    McpServerConfig {
        id: None,
        name: None,
        enabled: true,
        allowlist_entry_id: None,
        command,
        args,
        env,
        url: srv.url.clone(),
        headers: srv.headers.clone(),
        // Skill-bundled MCP servers use stdio/static-HTTP only; OAuth servers
        // are operator-configured (they require an interactive `octos mcp login`).
        oauth: false,
        scopes: Vec::new(),
        // Skill-bundled MCP servers fall through to the wrapper's
        // server default (`Safe` — read-only common case). A skill
        // that bundles a mutating MCP server should plumb a per-server
        // hint through `SkillMcpServer` (a future extension) and have
        // it copied into this field; until then, the bundled servers
        // run in the parallel-friendly path.
        concurrency_class: None,
    }
}

/// Parse a skill hook definition into a runtime `HookConfig`.
/// Returns `None` if the event string is unrecognized.
fn resolve_hook(def: &SkillHookDef, skill_dir: &Path) -> Option<HookConfig> {
    use crate::hooks::HookEvent;

    let event = match def.event.as_str() {
        "user_prompt_submit" => HookEvent::UserPromptSubmit,
        "before_tool_call" => HookEvent::BeforeToolCall,
        "after_tool_call" => HookEvent::AfterToolCall,
        "before_llm_call" => HookEvent::BeforeLlmCall,
        "after_llm_call" => HookEvent::AfterLlmCall,
        "on_resume" => HookEvent::OnResume,
        "on_turn_end" => HookEvent::OnTurnEnd,
        "before_spawn_verify" => HookEvent::BeforeSpawnVerify,
        "on_spawn_verify" => HookEvent::OnSpawnVerify,
        "on_spawn_complete" => HookEvent::OnSpawnComplete,
        "on_spawn_failure" => HookEvent::OnSpawnFailure,
        _ => return None,
    };

    // Resolve every `./`/`../` element against the skill dir; bare commands
    // (e.g. "python3") are left for PATH. The hook process's cwd is the
    // payload's workspace root when the payload carries one
    // (after_tool_call / user_prompt_submit), otherwise the daemon's cwd —
    // never the skill dir — so a script argument like
    // `["python3", "./hooks/x.py"]` must resolve here too; resolving only
    // argv[0] leaves it pointing at the wrong root.
    let command: Vec<String> = def
        .command
        .iter()
        .map(|arg| {
            let p = Path::new(arg);
            if p.is_relative() && (arg.starts_with("./") || arg.starts_with("../")) {
                skill_dir.join(p).to_string_lossy().into_owned()
            } else {
                arg.clone()
            }
        })
        .collect();

    Some(HookConfig {
        event,
        command,
        timeout_ms: def.timeout_ms,
        tool_filter: def.tool_filter.clone(),
        path_filter: Vec::new(),
        requires_bin: None,
    })
}
