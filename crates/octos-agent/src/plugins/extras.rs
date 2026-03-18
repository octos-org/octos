//! Resolve skill manifest extras (MCP servers, hooks, prompt fragments) into
//! runtime-ready config types.

use std::collections::HashMap;
use std::path::Path;

use tracing::warn;

use crate::hooks::HookConfig;
use crate::mcp::McpServerConfig;

use super::manifest::{PluginManifest, SkillHookDef, SkillMcpServer};

/// Resolved extras from a skill manifest, ready to merge into agent config.
#[derive(Debug, Default)]
pub struct SkillExtras {
    pub mcp_servers: Vec<McpServerConfig>,
    pub hooks: Vec<HookConfig>,
    pub prompt_fragments: Vec<String>,
}

/// Resolve manifest extras against the skill directory.
///
/// - MCP: resolves relative commands against `skill_dir`, looks up env var names
///   from the process environment.
/// - Hooks: parses event strings into `HookEvent`, resolves relative command paths.
/// - Prompts: expands glob patterns against `skill_dir`, reads `.md` files.
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
}
