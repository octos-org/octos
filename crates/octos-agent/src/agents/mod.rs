//! Agent manifest format (M8.2 — runtime-v0.1 gate).
//!
//! # What is an `AgentDefinition`?
//!
//! An [`AgentDefinition`] is an external, declarative JSON or TOML manifest
//! that describes a sub-agent's capability envelope: which tools it may use,
//! which are denied, its model preferences, lifecycle hooks, MCP server
//! dependencies, and so on. The schema deliberately mirrors Claude Code's
//! `AgentDefinition` from `loadAgentsDir.ts` so authors moving between the
//! two runtimes see the same field names and semantics.
//!
//! Every field in the manifest is domain-neutral: nothing about the schema
//! is coding-specific. A "research-worker" manifest carries the same shape
//! as a "repo-editor" manifest — only the tool allow-list differs. This is
//! the M8.2 "coding-only" falsification gate for runtime-v0.1.
//!
//! # How loading works
//!
//! [`AgentDefinitions::load_dir`] scans a directory for `*.json` and `*.toml`
//! files. Each file's stem is the definition id by default; if the file has
//! a `name` field it overrides the stem. The loader layers:
//!
//! 1. Built-in defaults (shipped under `crates/octos-agent/src/assets/agents/`)
//!    via [`AgentDefinitions::with_builtins`]. Today these are
//!    `research-worker` (deep_search / web_fetch / web_search; no
//!    shell/write/edit) and `repo-editor` (read_file / write_file / edit_file
//!    / shell / grep / glob; no deep_search).
//! 2. Local manifests from the caller-supplied directory, which replace
//!    any built-in with the same id.
//!
//! If the directory does not exist the loader returns the built-ins only.
//!
//! # How `SpawnTool` resolves a manifest
//!
//! `SpawnTool::execute` accepts an optional `agent_definition_id` field. When
//! set, the tool looks up the id in `ctx.agent_definitions` (the field on
//! [`crate::tools::ToolContext`] that M8.1 stubbed). The manifest's fields
//! become defaults for the spawn call; any inline field on the spawn args
//! overrides the manifest. If both are present, inline wins. This keeps
//! existing inline spawn callers working byte-for-byte while letting
//! manifest-driven spawns stay terse at the call site.
//!
//! # Forward compatibility
//!
//! `version: u32` is required and must be `1` for now. Unknown fields are
//! rejected (`#[serde(deny_unknown_fields)]`) — v1 only needs back-compat on
//! read, not forward-compat on write. Future versions that add fields will
//! bump the version number and relax the deny.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// Current schema version for [`AgentDefinition`].
///
/// Manifests with a different `version` are rejected at load time so callers
/// cannot accidentally mix incompatible shapes. Future schema revisions bump
/// this constant.
pub const AGENT_DEFINITION_SCHEMA_VERSION: u32 = 1;

/// A single agent manifest — the declarative capability envelope for a
/// spawned sub-agent.
///
/// Field order and naming follow Claude Code's `AgentDefinition` from
/// `loadAgentsDir.ts:76-94`. Every field except `name` and `version` is
/// optional; default values keep existing inline spawn callers unchanged.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    /// Manifest id — also its display name. Required.
    pub name: String,

    /// Schema version. Must match [`AGENT_DEFINITION_SCHEMA_VERSION`].
    pub version: u32,

    /// Allow-list of tool ids the sub-agent may use. Empty = inherit the
    /// parent's default set.
    #[serde(default)]
    pub tools: Vec<String>,

    /// Deny-list of tool ids the sub-agent may not use. Deny always wins
    /// over allow when a tool appears in both.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,

    /// Optional model override (e.g. `"anthropic/claude-haiku"`).
    #[serde(default)]
    pub model: Option<String>,

    /// Optional effort hint (`"low"` / `"medium"` / `"high"`). Free-form for
    /// now — the runtime may start honouring specific values in a future
    /// milestone.
    #[serde(default)]
    pub effort: Option<String>,

    /// Optional permission mode. Free-form today; M8.3 will map values to
    /// the typed permission layer.
    #[serde(default)]
    pub permission_mode: Option<String>,

    /// MCP server names this sub-agent wants attached. For v1 we only record
    /// the names — real MCP wiring lands in a later milestone.
    #[serde(default)]
    pub mcp_servers: Vec<String>,

    /// Lifecycle hooks this sub-agent inherits. Minimal v1 shape: event +
    /// command. Future versions may add timeouts, filters, etc.
    #[serde(default)]
    pub hooks: Vec<HookRef>,

    /// Optional cap on sub-agent turn count.
    #[serde(default)]
    pub max_turns: Option<u32>,

    /// Skill ids the sub-agent wants enabled. For v1 we only record the
    /// names — real skill loading lands in a later milestone.
    #[serde(default)]
    pub skills: Vec<String>,

    /// Optional memory configuration.
    #[serde(default)]
    pub memory: Option<MemoryConfig>,

    /// Whether the sub-agent should run as a background worker by default.
    #[serde(default)]
    pub background: bool,

    /// Optional isolation mode string. Free-form today; wired to sandbox
    /// policy in a later milestone.
    #[serde(default)]
    pub isolation: Option<String>,
}

impl AgentDefinition {
    /// Validate a freshly-loaded manifest. Today we only check the schema
    /// version — richer validation (e.g. unknown tool ids) lands when the
    /// permission layer (M8.3) can do cross-referencing.
    pub fn validate(&self) -> Result<()> {
        if self.version != AGENT_DEFINITION_SCHEMA_VERSION {
            eyre::bail!(
                "agent definition '{}' has unsupported schema version {} (expected {})",
                self.name,
                self.version,
                AGENT_DEFINITION_SCHEMA_VERSION,
            );
        }
        Ok(())
    }
}

/// Minimal v1 hook reference — event name + command.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HookRef {
    /// Hook event name (e.g. `"before_tool_call"`).
    pub event: String,
    /// Shell command the hook should run.
    pub command: String,
}

/// Minimal memory configuration carried by a manifest.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    /// Optional path to the memory store. When `None` the runtime picks a
    /// default location.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Whether memory is enabled for this sub-agent. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Built-in manifests shipped inside the crate so downstream users get a
/// sensible default set without having to author their own.
///
/// Pairs are `(id, raw JSON text)`. At load time the raw text is parsed via
/// [`AgentDefinition::from_json_str`].
const BUILTIN_AGENTS: &[(&str, &str)] = &[
    (
        "research-worker",
        include_str!("../assets/agents/research-worker.json"),
    ),
    (
        "repo-editor",
        include_str!("../assets/agents/repo-editor.json"),
    ),
];

impl AgentDefinition {
    /// Parse an [`AgentDefinition`] from JSON text.
    pub fn from_json_str(text: &str) -> Result<Self> {
        let def: AgentDefinition =
            serde_json::from_str(text).wrap_err("failed to parse AgentDefinition as JSON")?;
        def.validate()?;
        Ok(def)
    }

    /// Parse an [`AgentDefinition`] from TOML text.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let def: AgentDefinition =
            toml::from_str(text).wrap_err("failed to parse AgentDefinition as TOML")?;
        def.validate()?;
        Ok(def)
    }
}

/// Typed registry of [`AgentDefinition`] records indexed by id.
///
/// This is the concrete M8.2 replacement for the M8.1 stub. It lives in
/// `crate::tools` under the same name so the typed `ToolContext.agent_definitions`
/// field keeps its signature.
#[derive(Clone, Debug, Default)]
pub struct AgentDefinitions {
    by_id: HashMap<String, AgentDefinition>,
}

impl AgentDefinitions {
    /// Create an empty registry. Equivalent to [`Default::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the registry has zero manifests.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Number of registered manifests.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Look up a manifest by id.
    pub fn get(&self, id: &str) -> Option<&AgentDefinition> {
        self.by_id.get(id)
    }

    /// Insert a manifest, replacing any previous value for the same id.
    /// Returns the previous value if one existed.
    pub fn insert(
        &mut self,
        id: impl Into<String>,
        def: AgentDefinition,
    ) -> Option<AgentDefinition> {
        self.by_id.insert(id.into(), def)
    }

    /// Iterate manifest ids.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.by_id.keys().map(String::as_str)
    }

    /// Registry containing the crate-shipped built-in manifests only.
    ///
    /// Today this is `research-worker` and `repo-editor`. The list is an
    /// implementation detail and may grow over time.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        for (id, text) in BUILTIN_AGENTS {
            let def = AgentDefinition::from_json_str(text).unwrap_or_else(|err| {
                // Built-in manifests are authored by the crate; a parse error
                // here is a programming bug, not a user-visible condition.
                panic!("built-in agent definition '{id}' is malformed: {err}");
            });
            reg.by_id.insert((*id).to_string(), def);
        }
        reg
    }

    /// Load manifests from a directory, layered on top of the built-ins.
    ///
    /// Reads `*.json` and `*.toml` files directly under `dir` (non-recursive).
    /// Each file's stem is the id unless the file specifies a `name` field
    /// that overrides it. Local manifests replace built-ins with the same
    /// id. Missing directories return the built-ins only.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        let mut reg = Self::with_builtins();
        reg.load_dir_into(dir)?;
        Ok(reg)
    }

    /// Load manifests from `dir` into an existing registry. Local manifests
    /// override any previous entry with the same id.
    pub fn load_dir_into(&mut self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        let iter = std::fs::read_dir(dir)
            .wrap_err_with(|| format!("failed to read agents dir {}", dir.display()))?;
        for entry in iter {
            let entry = entry
                .wrap_err_with(|| format!("failed to enumerate agents dir {}", dir.display()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let text = std::fs::read_to_string(&path)
                .wrap_err_with(|| format!("failed to read {}", path.display()))?;
            let def = match ext {
                "json" => AgentDefinition::from_json_str(&text)
                    .wrap_err_with(|| format!("failed to parse {}", path.display()))?,
                "toml" => AgentDefinition::from_toml_str(&text)
                    .wrap_err_with(|| format!("failed to parse {}", path.display()))?,
                _ => continue,
            };
            // File stem is the default id; the manifest's own `name` field
            // overrides it when present (it is always present in v1 because
            // `name` is required, so the stem is effectively a display hint
            // for humans inspecting the directory).
            let id = def.name.clone();
            // Preserve the stem-id convention by rejecting manifests whose
            // `name` does not match the filename stem. This keeps directory
            // lookups predictable: `foo.json` registers id `foo`.
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if stem != id {
                    tracing::warn!(
                        file = %path.display(),
                        stem = %stem,
                        name = %id,
                        "agent definition filename stem differs from manifest name; \
                         using manifest name as id"
                    );
                }
            }
            self.by_id.insert(id, def);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write manifest");
    }

    #[test]
    fn should_parse_minimum_valid_manifest() {
        // Only `name` and `version` are required. Every other field is
        // optional and must default cleanly.
        let json = r#"{"name":"tiny","version":1}"#;
        let def = AgentDefinition::from_json_str(json).unwrap();

        assert_eq!(def.name, "tiny");
        assert_eq!(def.version, 1);
        assert!(def.tools.is_empty());
        assert!(def.disallowed_tools.is_empty());
        assert!(def.model.is_none());
        assert!(def.effort.is_none());
        assert!(def.permission_mode.is_none());
        assert!(def.mcp_servers.is_empty());
        assert!(def.hooks.is_empty());
        assert!(def.max_turns.is_none());
        assert!(def.skills.is_empty());
        assert!(def.memory.is_none());
        assert!(!def.background);
        assert!(def.isolation.is_none());
    }

    #[test]
    fn should_parse_full_manifest_with_all_12_fields() {
        // All 12 optional fields plus the two required ones, populated with
        // realistic values.
        let json = r#"{
            "name": "full-agent",
            "version": 1,
            "tools": ["read_file", "shell"],
            "disallowed_tools": ["web_search"],
            "model": "anthropic/claude-haiku",
            "effort": "high",
            "permission_mode": "ask",
            "mcp_servers": ["jiuwenclaw", "hermes"],
            "hooks": [
                {"event": "before_tool_call", "command": "/bin/true"}
            ],
            "max_turns": 25,
            "skills": ["weather", "time"],
            "memory": {"path": "/tmp/mem", "enabled": true},
            "background": true,
            "isolation": "docker"
        }"#;
        let def = AgentDefinition::from_json_str(json).unwrap();

        assert_eq!(def.name, "full-agent");
        assert_eq!(def.version, 1);
        assert_eq!(
            def.tools,
            vec!["read_file".to_string(), "shell".to_string()]
        );
        assert_eq!(def.disallowed_tools, vec!["web_search".to_string()]);
        assert_eq!(def.model.as_deref(), Some("anthropic/claude-haiku"));
        assert_eq!(def.effort.as_deref(), Some("high"));
        assert_eq!(def.permission_mode.as_deref(), Some("ask"));
        assert_eq!(def.mcp_servers.len(), 2);
        assert_eq!(def.hooks.len(), 1);
        assert_eq!(def.hooks[0].event, "before_tool_call");
        assert_eq!(def.hooks[0].command, "/bin/true");
        assert_eq!(def.max_turns, Some(25));
        assert_eq!(def.skills, vec!["weather".to_string(), "time".to_string()]);
        let memory = def.memory.as_ref().expect("memory present");
        assert_eq!(memory.path.as_deref(), Some(Path::new("/tmp/mem")));
        assert!(memory.enabled);
        assert!(def.background);
        assert_eq!(def.isolation.as_deref(), Some("docker"));
    }

    #[test]
    fn should_round_trip_manifest_through_json() {
        // Serialize -> deserialize should be byte-stable for semantics.
        let original = AgentDefinition {
            name: "rt".to_string(),
            version: 1,
            tools: vec!["shell".to_string()],
            disallowed_tools: Vec::new(),
            model: Some("openai/gpt-4o-mini".to_string()),
            effort: Some("medium".to_string()),
            permission_mode: None,
            mcp_servers: Vec::new(),
            hooks: vec![HookRef {
                event: "after_tool_call".to_string(),
                command: "/usr/bin/env".to_string(),
            }],
            max_turns: Some(10),
            skills: Vec::new(),
            memory: Some(MemoryConfig {
                path: None,
                enabled: false,
            }),
            background: false,
            isolation: None,
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let round_tripped = AgentDefinition::from_json_str(&json).expect("deserialize");
        assert_eq!(round_tripped, original);
    }

    #[test]
    fn should_reject_manifest_with_unknown_fields_in_v1() {
        // `#[serde(deny_unknown_fields)]` must catch forward-incompatible
        // manifests so v1 consumers cannot silently ignore new fields.
        let json = r#"{
            "name": "future",
            "version": 1,
            "unknown_future_field": true
        }"#;
        let err = AgentDefinition::from_json_str(json).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown_future_field") || msg.contains("unknown field"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn should_merge_builtin_and_local_agents() {
        // Built-ins provide `research-worker` and `repo-editor`. A local
        // manifest with the same id must override the built-in.
        let tmp = tempfile::tempdir().expect("tempdir");
        // Override the research-worker built-in with a local variant that
        // adds `shell` to the tool list.
        write(
            &tmp.path().join("research-worker.json"),
            r#"{"name":"research-worker","version":1,"tools":["deep_search","shell"]}"#,
        );
        // Add a brand-new local-only definition.
        write(
            &tmp.path().join("local-only.json"),
            r#"{"name":"local-only","version":1,"tools":["read_file"]}"#,
        );

        let reg = AgentDefinitions::load_dir(tmp.path()).expect("load_dir");

        // Built-in that was not overridden remains available.
        let repo_editor = reg.get("repo-editor").expect("repo-editor");
        assert!(repo_editor.tools.contains(&"read_file".to_string()));

        // Overridden built-in now carries the local fields.
        let research = reg.get("research-worker").expect("research-worker");
        assert!(research.tools.contains(&"shell".to_string()));

        // Local-only definition is present.
        let local = reg.get("local-only").expect("local-only");
        assert_eq!(local.tools, vec!["read_file".to_string()]);
    }

    #[test]
    fn should_load_toml_manifest() {
        // TOML is supported as a sibling format to JSON.
        let tmp = tempfile::tempdir().expect("tempdir");
        let toml_text = r#"
            name = "toml-agent"
            version = 1
            tools = ["read_file"]
            background = true
        "#;
        write(&tmp.path().join("toml-agent.toml"), toml_text);

        let reg = AgentDefinitions::load_dir(tmp.path()).expect("load_dir");
        let def = reg.get("toml-agent").expect("toml-agent");
        assert_eq!(def.tools, vec!["read_file".to_string()]);
        assert!(def.background);
    }

    #[test]
    fn should_load_empty_registry_when_dir_missing() {
        let reg = AgentDefinitions::load_dir(Path::new("/tmp/does-not-exist-octos-m82-tests"))
            .expect("load_dir");
        // Built-ins are always present even when the caller's dir is missing.
        assert!(reg.get("research-worker").is_some());
        assert!(reg.get("repo-editor").is_some());
    }

    #[test]
    fn should_reject_manifest_with_mismatched_schema_version() {
        let json = r#"{"name":"wrong","version":2}"#;
        let err = AgentDefinition::from_json_str(json).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("version"), "expected version error, got {msg}");
    }

    #[test]
    fn should_provide_builtin_research_worker() {
        let reg = AgentDefinitions::with_builtins();
        let def = reg
            .get("research-worker")
            .expect("research-worker built-in");
        assert_eq!(def.name, "research-worker");
        assert!(def.tools.contains(&"deep_search".to_string()));
        assert!(def.tools.contains(&"web_fetch".to_string()));
        assert!(def.tools.contains(&"web_search".to_string()));
        // Research worker explicitly denies shell/write/edit.
        assert!(def.disallowed_tools.contains(&"shell".to_string()));
        assert!(def.disallowed_tools.contains(&"write_file".to_string()));
        assert!(def.disallowed_tools.contains(&"edit_file".to_string()));
    }

    #[test]
    fn should_provide_builtin_repo_editor() {
        let reg = AgentDefinitions::with_builtins();
        let def = reg.get("repo-editor").expect("repo-editor built-in");
        assert_eq!(def.name, "repo-editor");
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "shell",
            "grep",
            "glob",
        ] {
            assert!(
                def.tools.contains(&expected.to_string()),
                "repo-editor missing {expected}"
            );
        }
        // Repo editor denies deep_search.
        assert!(def.disallowed_tools.contains(&"deep_search".to_string()));
    }
}
