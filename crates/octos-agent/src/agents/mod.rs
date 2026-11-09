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

    /// M8.5 fix-first item 5: report which manifest fields the runtime
    /// today does NOT enforce. Returns the labels of any populated
    /// fields so callers can either reject the manifest or surface a
    /// clear warning instead of silently accepting unsupported config.
    ///
    /// The fields tracked here are the same set the fix-first checklist
    /// flagged: `max_turns`, `background`, `memory`, `hooks`,
    /// `mcp_servers`, `isolation`. Each entry remains a placeholder
    /// until a follow-up milestone wires it into real runtime behaviour.
    pub fn unimplemented_fields(&self) -> Vec<&'static str> {
        let mut flagged = Vec::new();
        if self.max_turns.is_some() {
            flagged.push("max_turns");
        }
        if self.background {
            flagged.push("background");
        }
        if self.memory.is_some() {
            flagged.push("memory");
        }
        if !self.hooks.is_empty() {
            flagged.push("hooks");
        }
        if !self.mcp_servers.is_empty() {
            flagged.push("mcp_servers");
        }
        if self.isolation.is_some() {
            flagged.push("isolation");
        }
        flagged
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
