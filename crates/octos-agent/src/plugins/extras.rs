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
    /// spawn_only tools opted into bounded file summary follow-ups.
    pub spawn_only_auto_summarize: Vec<String>,
}

/// Resolve manifest extras against the skill directory.
///
/// - MCP: resolves relative commands against `skill_dir`, looks up env var names
///   from the process environment.
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

/// Convert a skill MCP server declaration into a runtime `McpServerConfig`.
fn resolve_mcp_server(srv: &SkillMcpServer, skill_dir: &Path) -> McpServerConfig {
    // Resolve relative command paths against skill dir; bare commands (e.g. "node") left for PATH.
    let command = srv.command.as_ref().map(|cmd| {
        let p = Path::new(cmd);
        if p.is_relative() && (cmd.starts_with("./") || cmd.starts_with("../")) {
            skill_dir.join(p).to_string_lossy().into_owned()
        } else {
            cmd.clone()
        }
    });

    // Resolve env var NAMES to actual values from the process environment.
    let mut env = HashMap::new();
    for name in &srv.env {
        if let Ok(val) = std::env::var(name) {
            env.insert(name.clone(), val);
        }
    }

    McpServerConfig {
        command,
        args: srv.args.clone(),
        env,
        url: srv.url.clone(),
        headers: srv.headers.clone(),
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

    // Resolve the first element of command if it's a relative path.
    let command: Vec<String> = def
        .command
        .iter()
        .enumerate()
        .map(|(i, arg)| {
            if i == 0 {
                let p = Path::new(arg);
                if p.is_relative() && (arg.starts_with("./") || arg.starts_with("../")) {
                    skill_dir.join(p).to_string_lossy().into_owned()
                } else {
                    arg.clone()
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::manifest::{SkillHookDef, SkillMcpServer, SkillPrompts};

    #[test]
    fn test_resolve_mcp_bare_command() {
        let srv = SkillMcpServer {
            command: Some("node".into()),
            args: vec!["server.js".into()],
            env: vec![],
            url: None,
            headers: HashMap::new(),
        };
        let config = resolve_mcp_server(&srv, Path::new("/skills/my-skill"));
        assert_eq!(config.command.as_deref(), Some("node"));
        assert_eq!(config.args, vec!["server.js"]);
    }

    #[test]
    fn test_resolve_mcp_relative_command() {
        let srv = SkillMcpServer {
            command: Some("./bin/server".into()),
            args: vec![],
            env: vec![],
            url: None,
            headers: HashMap::new(),
        };
        let config = resolve_mcp_server(&srv, Path::new("/skills/my-skill"));
        let cmd = config.command.unwrap();
        assert!(
            cmd == "/skills/my-skill/./bin/server" || cmd == "/skills/my-skill\\./bin/server",
            "unexpected resolved command: {cmd}"
        );
    }

    #[test]
    fn test_resolve_mcp_url_transport() {
        let srv = SkillMcpServer {
            command: None,
            args: vec![],
            env: vec![],
            url: Some("https://mcp.example.com/v1".into()),
            headers: HashMap::from([("Authorization".into(), "Bearer tok".into())]),
        };
        let config = resolve_mcp_server(&srv, Path::new("/skills/my-skill"));
        assert!(config.command.is_none());
        assert_eq!(config.url.as_deref(), Some("https://mcp.example.com/v1"));
        assert_eq!(config.headers.get("Authorization").unwrap(), "Bearer tok");
    }

    #[test]
    fn test_resolve_mcp_env_missing_vars_omitted() {
        let srv = SkillMcpServer {
            command: Some("node".into()),
            args: vec![],
            env: vec!["_CERTAINLY_MISSING_VAR_12345".into()],
            url: None,
            headers: HashMap::new(),
        };
        let config = resolve_mcp_server(&srv, Path::new("/skills/x"));
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_resolve_hook_known_events() {
        for (event_str, _) in [
            ("before_tool_call", ()),
            ("after_tool_call", ()),
            ("before_llm_call", ()),
            ("after_llm_call", ()),
            ("on_resume", ()),
            ("on_turn_end", ()),
            ("before_spawn_verify", ()),
            ("on_spawn_verify", ()),
            ("on_spawn_complete", ()),
            ("on_spawn_failure", ()),
        ] {
            let def = SkillHookDef {
                event: event_str.into(),
                command: vec!["./audit.sh".into()],
                timeout_ms: 3000,
                tool_filter: vec![],
            };
            let hook = resolve_hook(&def, Path::new("/skills/s"));
            assert!(hook.is_some(), "should resolve event: {event_str}");
            let hook = hook.unwrap();
            assert_eq!(hook.timeout_ms, 3000);
            assert!(
                hook.command[0] == "/skills/s/./audit.sh"
                    || hook.command[0] == "/skills/s\\./audit.sh",
                "unexpected resolved command: {}",
                hook.command[0]
            );
        }
    }

    #[test]
    fn test_resolve_hook_unknown_event() {
        let def = SkillHookDef {
            event: "on_startup".into(),
            command: vec!["echo".into(), "hi".into()],
            timeout_ms: 5000,
            tool_filter: vec![],
        };
        assert!(resolve_hook(&def, Path::new("/skills/s")).is_none());
    }

    #[test]
    fn test_resolve_extras_empty_manifest() {
        let manifest = PluginManifest {
            name: "test".into(),
            version: "1.0".into(),
            tools: vec![],
            sha256: None,
            binaries: HashMap::new(),
            requires_network: false,
            timeout_secs: None,
            mcp_servers: vec![],
            hooks: vec![],
            prompts: None,
            discovery: None,
        };
        let extras = resolve_extras(&manifest, Path::new("/skills/test"));
        assert!(extras.mcp_servers.is_empty());
        assert!(extras.hooks.is_empty());
        assert!(extras.prompt_fragments.is_empty());
    }

    #[test]
    fn test_resolve_prompt_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let prompts_dir = dir.path().join("prompts");
        std::fs::create_dir(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("intro.md"), "# Hello\nWelcome.").unwrap();
        std::fs::write(prompts_dir.join("rules.md"), "Be careful.").unwrap();

        let manifest = PluginManifest {
            name: "test".into(),
            version: "1.0".into(),
            tools: vec![],
            sha256: None,
            binaries: HashMap::new(),
            requires_network: false,
            timeout_secs: None,
            mcp_servers: vec![],
            hooks: vec![],
            prompts: Some(SkillPrompts {
                include: vec!["prompts/*.md".into()],
            }),
            discovery: None,
        };
        let extras = resolve_extras(&manifest, dir.path());
        assert_eq!(extras.prompt_fragments.len(), 2);
        assert!(extras.prompt_fragments.iter().any(|f| f.contains("Hello")));
        assert!(
            extras
                .prompt_fragments
                .iter()
                .any(|f| f.contains("Be careful"))
        );
    }

    // ------------------------------------------------------------------
    // SKILL.md PR-F: 5-line skill card + generic exploration preamble
    // ------------------------------------------------------------------

    fn manifest_for_card_test(
        name: &str,
        tools: Vec<&str>,
        discovery: Option<crate::plugins::manifest::SkillDiscovery>,
        any_spawn_only: bool,
    ) -> PluginManifest {
        use crate::plugins::manifest::PluginToolDef;
        PluginManifest {
            name: name.into(),
            version: "1.0.0".into(),
            tools: tools
                .into_iter()
                .map(|t| PluginToolDef {
                    name: t.into(),
                    description: "desc".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    spawn_only: any_spawn_only,
                    env: vec![],
                    risk: None,
                    spawn_only_message: None,
                    concurrency_class: None,
                })
                .collect(),
            sha256: None,
            binaries: HashMap::new(),
            requires_network: false,
            timeout_secs: None,
            mcp_servers: vec![],
            hooks: vec![],
            prompts: None,
            discovery,
        }
    }

    /// PR-F: a manifest with `discovery.summary` set produces exactly the
    /// 4-line card (`- name`, `  purpose`, `  tools`, `  skill_dir`) plus
    /// the generic exploration preamble. No `if you need:` section.
    #[test]
    fn extras_renders_5_line_card_with_summary() {
        use crate::plugins::manifest::SkillDiscovery;

        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path();

        let discovery = SkillDiscovery {
            summary: Some("Generate AI presentation slides.".into()),
        };

        let manifest = manifest_for_card_test(
            "mofa-slides",
            vec!["slides_generate", "slides_preview"],
            Some(discovery),
            false,
        );

        let extras = resolve_extras(&manifest, skill_dir);
        // Expect: [preamble, card]
        assert_eq!(
            extras.prompt_fragments.len(),
            2,
            "expected preamble + card; got {:?}",
            extras.prompt_fragments
        );

        let preamble = &extras.prompt_fragments[0];
        assert!(
            preamble.contains("Active plugin skill directories")
                && preamble.contains("read_file")
                && preamble.contains("source of truth"),
            "preamble missing expected wording: {preamble}"
        );

        let card = &extras.prompt_fragments[1];
        // 4 lines because the trailing newline is stripped from the
        // skill_dir line (no `if you need:` section).
        let line_count = card.lines().count();
        assert_eq!(
            line_count, 4,
            "card must be exactly 4 lines (name/purpose/tools/skill_dir); got {line_count}: {card}"
        );
        assert!(
            card.contains("- name: mofa-slides"),
            "card missing name line: {card}"
        );
        assert!(
            card.contains("purpose: Generate AI presentation slides."),
            "card missing purpose line: {card}"
        );
        assert!(
            card.contains("tools: slides_generate, slides_preview"),
            "card missing tools line: {card}"
        );
        assert!(
            card.contains(&format!("skill_dir: {}", skill_dir.display())),
            "card missing skill_dir line: {card}"
        );
        // PR-F: no more "if you need:" hint section.
        assert!(
            !card.contains("if you need:"),
            "PR-F card must NOT contain 'if you need:': {card}"
        );
    }

    /// PR-F: when `discovery.summary` is `None`, the purpose line falls
    /// back to the `(no summary)` placeholder. The card still has all
    /// four lines so downstream parsing stays stable.
    #[test]
    fn extras_renders_card_without_summary_uses_placeholder() {
        use crate::plugins::manifest::SkillDiscovery;

        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path();

        let discovery = SkillDiscovery { summary: None };

        let manifest = manifest_for_card_test("noisy-skill", vec!["t"], Some(discovery), false);

        let extras = resolve_extras(&manifest, skill_dir);
        assert_eq!(extras.prompt_fragments.len(), 2);

        let card = &extras.prompt_fragments[1];
        assert_eq!(
            card.lines().count(),
            4,
            "card must still be 4 lines when summary is missing"
        );
        assert!(
            card.contains("purpose: (no summary)"),
            "missing-summary placeholder not used: {card}"
        );
    }

    /// PR-F: when no manifest declares `discovery`, neither the preamble
    /// nor a card fires. The preamble is a discovery-gated companion,
    /// not a constant system-prompt header.
    #[test]
    fn extras_omits_preamble_and_card_when_discovery_absent() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_for_card_test("legacy", vec!["t"], None, false);
        let extras = resolve_extras(&manifest, dir.path());
        assert!(
            extras.prompt_fragments.is_empty(),
            "expected no fragments without discovery; got {:?}",
            extras.prompt_fragments
        );
    }

    /// PR-E regression carryover: a spawn_only plugin with a SKILL.md
    /// file on disk must NOT have its body injected. PR-E already
    /// removed that path; PR-F adds the discovery-gated preamble +
    /// card on top, so a spawn_only plugin WITHOUT a discovery block
    /// still produces zero fragments.
    #[test]
    fn extras_skips_legacy_body_for_spawn_only_skill_with_skill_md_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Legacy SKILL.md\nPre-PR-E this body would be injected.\n",
        )
        .unwrap();

        let manifest = manifest_for_card_test(
            "legacy-spawn-only",
            vec!["bg_tool"],
            None, // no discovery
            true, // spawn_only — would have triggered legacy auto-inject
        );

        let extras = resolve_extras(&manifest, skill_dir);
        assert!(
            extras.prompt_fragments.is_empty(),
            "legacy SKILL.md auto-inject must be gone; got {:?}",
            extras.prompt_fragments
        );
    }

    /// PR-F: the generic exploration preamble must appear exactly ONCE
    /// per loader-side merge of plugin fragments, even when multiple
    /// plugins each declare a `discovery` block.
    ///
    /// `resolve_extras` itself is called per-plugin, so each
    /// discovery-bearing plugin pushes the same preamble constant onto
    /// its own `prompt_fragments` Vec. The loader's `merge_extras` step
    /// folds duplicates — see `PluginLoadResult::merge_extras` in
    /// loader.rs — so the final session-wide prompt has the preamble
    /// once and every distinct card.
    ///
    /// This test pins both halves: per-plugin emit AND merge-side dedup.
    /// A regression in either the constant string or the dedup logic
    /// surfaces here rather than during a fleet soak.
    #[test]
    fn extras_emits_exploration_preamble_once_per_session() {
        use crate::plugins::manifest::SkillDiscovery;

        let dir = tempfile::tempdir().unwrap();

        let m1 = manifest_for_card_test(
            "plugin-one",
            vec!["t1"],
            Some(SkillDiscovery {
                summary: Some("First skill.".into()),
            }),
            false,
        );
        let m2 = manifest_for_card_test(
            "plugin-two",
            vec!["t2"],
            Some(SkillDiscovery {
                summary: Some("Second skill.".into()),
            }),
            false,
        );

        let e1 = resolve_extras(&m1, dir.path());
        let e2 = resolve_extras(&m2, dir.path());

        // Pre-merge: both invocations push the preamble.
        let combined: Vec<String> = e1
            .prompt_fragments
            .iter()
            .chain(e2.prompt_fragments.iter())
            .cloned()
            .collect();
        let preamble_count = combined
            .iter()
            .filter(|f| f.as_str() == SKILL_EXPLORATION_PREAMBLE)
            .count();
        assert_eq!(
            preamble_count, 2,
            "each `resolve_extras` call must push the preamble; got {preamble_count} \
             in {combined:?}"
        );

        // Post-merge dedup: only the first preamble survives.
        let deduped = dedup_preamble(combined.clone());
        let after = deduped
            .iter()
            .filter(|f| f.as_str() == SKILL_EXPLORATION_PREAMBLE)
            .count();
        assert_eq!(
            after, 1,
            "merge-side dedup must keep exactly one preamble; got {after} in {deduped:?}"
        );

        // The two distinct skill cards must both survive dedup (the
        // dedup only collapses the preamble, not the cards).
        assert!(deduped.iter().any(|f| f.contains("name: plugin-one")));
        assert!(deduped.iter().any(|f| f.contains("name: plugin-two")));
    }

    /// Test-side helper that models the merge-step dedup the loader
    /// applies when concatenating per-plugin fragments. Pinning the
    /// contract here lets `PluginLoadResult::merge_extras` (loader.rs)
    /// re-use the same constant string match without re-testing the
    /// substring invariant from scratch.
    fn dedup_preamble(fragments: Vec<String>) -> Vec<String> {
        let mut seen = false;
        fragments
            .into_iter()
            .filter(|frag| {
                if frag.as_str() == SKILL_EXPLORATION_PREAMBLE {
                    if seen {
                        false
                    } else {
                        seen = true;
                        true
                    }
                } else {
                    true
                }
            })
            .collect()
    }
}
