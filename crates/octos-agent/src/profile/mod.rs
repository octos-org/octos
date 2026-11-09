//! Profile system (M8.3 — runtime-v0.1 close-out).
//!
//! # What is a `ProfileDefinition`?
//!
//! A [`ProfileDefinition`] is a single, declarative manifest that describes
//! how the agent runtime should bootstrap: which tools are available, which
//! [`crate::agents::AgentDefinition`] sub-agents are preloaded, which MCP
//! servers are wired, how compaction is tiered, and which models are
//! preferred. Before M8.3 every one of these settings was wired implicitly
//! across a dozen startup sites; afterwards, a single profile declaration
//! consolidates the envelope.
//!
//! The built-in `coding` profile uses the standard fixed dispatcher. Four
//! common tools and two gateways are advertised; optional authorized tools
//! remain available through discovery. Custom profiles can still narrow tool
//! permissions and configure preloaded agents.
//!
//! # Forward compatibility
//!
//! Unlike [`crate::agents::AgentDefinition`] (which uses
//! `#[serde(deny_unknown_fields)]`) this schema is **forward-compatible**:
//! a v1 client MUST accept a v2 manifest that carries extra fields so the
//! CLI does not immediately break when a newer config arrives on the host
//! via config-sync or a mounted volume. The `version` field still acts as
//! a hard gate — a v2 profile on a v1 client produces a version-mismatch
//! error *before* the extra fields are considered.
//!
//! # Resolution order
//!
//! [`ProfileDefinition::load`] accepts either a name or a path:
//!
//! 1. If the argument starts with `/`, `./`, or `~/` it is treated as a
//!    filesystem path. The file is loaded directly.
//! 2. Otherwise the argument is a profile id. The loader first checks
//!    `~/.octos/profiles/<id>/profile.{toml,json}`.
//! 3. Finally the loader falls back to the crate-shipped built-in registry
//!    (JSON files under `crates/octos-agent/src/assets/profiles/`).
//!
//! Built-in profiles are `coding` and `swarm`. Tool presentation is shared;
//! profiles customize authorization and agent setup, not legacy tool surfaces.
//!
//! # Applied vs recorded settings
//!
//! M8.3 deliberately scopes its behaviour to "schema + loader + tool
//! filter". Some profile fields are populated today but *recorded, not
//! enforced* until a follow-up milestone wires them in:
//!
//! - `compaction_policy` — the tier overrides are parsed and exposed via
//!   [`ProfileDefinition::compaction_policy`], but the runtime still uses
//!   the workspace compaction runner from M6.3. M8.5's tiered runner is
//!   where the profile override becomes active.
//! - `model_preferences` — parsed and exposed, but the provider chain does
//!   not yet consult them. A future milestone wires the preferences into
//!   the adaptive router's lane-scoring input.
//! - `mcp_servers` — only the ids are captured. Actual server config
//!   resolution is a follow-up milestone. `coding` and `swarm` ship with
//!   an empty list so no behaviour change falls out of this.
//!
//! The `permissions` stub also lands in a minimal form (default /
//! restricted) so M8.4 can extend it without schema churn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::tools::ToolRegistry;

/// Current profile schema version. Manifests whose `version` differs from
/// this constant are rejected at load time.
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

/// Crate-shipped profiles available as a built-in fallback after the
/// user-config search. Ordered (name, raw JSON text).
const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("coding", include_str!("../assets/profiles/coding.json")),
    ("swarm", include_str!("../assets/profiles/swarm.json")),
];

/// The source a resolved profile was loaded from. Used by the CLI resolver
/// to emit an informative `profile resolved: ...` log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileSource {
    /// Explicit `--profile <path>` pointing at a file on disk.
    ExplicitPath,
    /// Named profile found in `~/.octos/profiles/<name>/profile.{toml,json}`.
    UserDir,
    /// Named profile that fell back to the crate-shipped built-in set.
    Builtin,
}

/// How the profile narrows the tool registry. Mirrors the three modes
/// called out in the issue scope:
///
/// - `default` — no filter; the registry passes through untouched. This is
///   what the built-in `coding` profile uses so behaviour parity with
///   the pre-M8.3 default path stays reachable.
/// - `allow_list` — only the named tools survive. Names may reference
///   [`crate::tools::policy::ToolGroupInfo`] groups via `group:*` strings.
/// - `deny_list` — every tool survives except the named ones. Useful for
///   profiles that strip a single capability (e.g. drop `web_fetch` from
///   an otherwise-default set).
///
/// Host-installed `spawn_only` wiring is normally retained regardless of mode.
/// Optional built-in observers opt out of that retention and are filtered.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProfileTools {
    /// Pass-through filter — registry is not narrowed.
    #[default]
    Default,
    /// Explicit whitelist. Only listed tools (or groups) are kept.
    AllowList {
        /// Tool names or `group:<id>` references to keep.
        #[serde(default)]
        tools: Vec<String>,
    },
    /// Inverse whitelist. Every registered tool except the listed ones is
    /// kept. Groups are expanded through the same mechanism as allow lists.
    DenyList {
        /// Tool names or `group:<id>` references to drop.
        #[serde(default)]
        tools: Vec<String>,
    },
}

impl ProfileTools {
    /// Whether a tool named `tool_name` would survive this filter.
    ///
    /// Mirrors [`crate::tools::ToolRegistry::filter_by_profile`]'s
    /// name-matching exactly — `group:<id>` expansion, `<prefix>*`
    /// wildcards, exact names, and the empty-allow-list pass-through —
    /// but deliberately WITHOUT the legacy spawn_only carve-out. A background
    /// tool that preserves its registration cannot be evicted by the
    /// filter, so bootstrap sites (chat/acp `run_pipeline`) consult this
    /// predicate FIRST and skip registration when the profile excludes
    /// the tool.
    pub fn allows(&self, tool_name: &str) -> bool {
        use crate::tools::policy::entry_matches;
        match self {
            Self::Default => true,
            Self::AllowList { tools } => {
                // Empty allow lists are treated as pass-through by
                // `filter_by_profile` (with a warning); agree with it so
                // the bootstrap gate never drops a tool the filter keeps.
                tools.is_empty() || tools.iter().any(|entry| entry_matches(entry, tool_name))
            }
            Self::DenyList { tools } => !tools.iter().any(|entry| entry_matches(entry, tool_name)),
        }
    }
}

/// Reference to an MCP server that the profile wants attached. For M8.3 we
/// only capture the `id`; resolution to a concrete config happens in a
/// follow-up milestone. Extra fields are tolerated (forward-compat).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct McpServerRef {
    /// Id of a server config declared elsewhere in the profile dir / config.
    pub id: String,
}

/// Coarse permission tier. The `default` variant mirrors today's
/// allow-everything behaviour — M8.4 will add richer per-tool rules by
/// extending this enum (adding variants is backward-compatible because we
/// do not use `deny_unknown_fields` on the containing struct).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Today's behaviour — every registered tool is executable.
    #[default]
    Default,
    /// Placeholder tier for hardened environments. Carries no runtime
    /// effect yet; M8.4 will map it to a concrete per-tool rule set.
    Restricted,
}

/// Profile-level override for the M8.5 tiered compaction runner.
///
/// Today this struct is *recorded only* — the runtime keeps using the
/// workspace compaction policy from M6.3. Once the M8.5 runner is wired,
/// the fields become live tier overrides.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProfileCompactionPolicy {
    /// Optional target token budget for the final compacted conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u32>,
    /// Optional trigger threshold (turns or tokens, interpreted by the
    /// runner). `None` leaves the runner default in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_threshold: Option<u32>,
    /// Optional tiers map (tier-id -> token budget). Free-form today;
    /// M8.5 will define the tier id vocabulary.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tiers: HashMap<String, u32>,
}

/// Model-name hints consulted by the provider chain. Today these are
/// recorded but not enforced — a follow-up milestone wires them into
/// adaptive routing.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelPreferences {
    /// Default model id (e.g. `"anthropic/claude-sonnet-4"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Low-latency model id (cheap, fast). May be used for background
    /// worker dispatch in a follow-up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
    /// Highest-capability model id. Reserved for orchestrator turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strong: Option<String>,
}

/// A single profile manifest.
///
/// Field layout and naming mirrors the runtime plan's "profile envelope".
/// Fields marked *recorded only* land without runtime enforcement in M8.3
/// and are picked up by a follow-up milestone.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProfileDefinition {
    /// Profile id. Also its display name when rendered in logs.
    pub name: String,
    /// Schema version. Must equal [`PROFILE_SCHEMA_VERSION`]. Mismatched
    /// versions produce an error so a forward-compatible client cannot
    /// accidentally swallow schema churn.
    pub version: u32,
    /// Free-text description for humans reading the profile file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tool filter policy. Defaults to [`ProfileTools::Default`] so the
    /// registry is left untouched.
    #[serde(default)]
    pub tools: ProfileTools,
    /// MCP servers to attach. Only ids are captured today.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerRef>,
    /// Coarse permission tier. Defaults to [`PermissionMode::Default`].
    #[serde(default)]
    pub permissions: PermissionMode,
    /// Optional override for the M8.5 tiered compaction runner. Recorded
    /// only today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_policy: Option<ProfileCompactionPolicy>,
    /// Optional model preferences. Recorded only today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preferences: Option<ModelPreferences>,
    /// Path within the profile dir to a system-prompt template file.
    /// Resolved against the profile's parent directory at load time; left
    /// as-is in the struct so tests and callers can inspect the raw hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_template: Option<PathBuf>,
    /// Ids of [`crate::agents::AgentDefinition`] manifests to preload when
    /// this profile is activated. Consumes the M8.2 registry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
}

impl Default for ProfileDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: PROFILE_SCHEMA_VERSION,
            description: None,
            tools: ProfileTools::default(),
            mcp_servers: Vec::new(),
            permissions: PermissionMode::default(),
            compaction_policy: None,
            model_preferences: None,
            system_prompt_template: None,
            agents: Vec::new(),
        }
    }
}

impl ProfileDefinition {
    /// Validate a freshly-deserialized profile. Today only the schema
    /// version is enforced — additional cross-field validation lands as
    /// the permission / compaction wiring comes online.
    pub fn validate(&self) -> Result<()> {
        if self.version != PROFILE_SCHEMA_VERSION {
            eyre::bail!(
                "profile '{}' has unsupported schema version {} (expected {})",
                self.name,
                self.version,
                PROFILE_SCHEMA_VERSION,
            );
        }
        if self.name.trim().is_empty() {
            eyre::bail!("profile manifest is missing a non-empty `name` field");
        }
        Ok(())
    }

    /// M8.5 fix-first item 5: cross-validate `profile.agents` against an
    /// `AgentDefinitions` registry. Returns the list of unknown ids so
    /// the caller can either reject the profile or warn the operator.
    /// Empty result means every referenced manifest exists.
    pub fn unknown_agent_ids(&self, registry: &crate::agents::AgentDefinitions) -> Vec<String> {
        self.agents
            .iter()
            .filter(|id| registry.get(id).is_none())
            .cloned()
            .collect()
    }

    /// M8.5 fix-first item 5: hard-validate `profile.agents` against a
    /// registry. Returns an error listing the missing ids when any are
    /// unknown. This is the call sites use when they need an
    /// authoritative profile/manifest envelope (e.g. M9 control-plane).
    pub fn validate_against_registry(
        &self,
        registry: &crate::agents::AgentDefinitions,
    ) -> Result<()> {
        let missing = self.unknown_agent_ids(registry);
        if !missing.is_empty() {
            eyre::bail!(
                "profile '{}' references agent_definition ids not present in the registry: {:?} \
                 (available: {:?})",
                self.name,
                missing,
                registry.ids().collect::<Vec<_>>(),
            );
        }
        Ok(())
    }

    /// Parse a profile from JSON text. Validates on success.
    pub fn from_json_str(text: &str) -> Result<Self> {
        let def: Self =
            serde_json::from_str(text).wrap_err("failed to parse ProfileDefinition as JSON")?;
        def.validate()?;
        Ok(def)
    }

    /// Parse a profile from TOML text. Validates on success.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let def: Self =
            toml::from_str(text).wrap_err("failed to parse ProfileDefinition as TOML")?;
        def.validate()?;
        Ok(def)
    }

    /// Parse from a file on disk, picking the format from the file
    /// extension (`.toml` -> TOML, everything else -> JSON).
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read profile at {}", path.display()))?;
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
        let def = if is_toml {
            Self::from_toml_str(&text)
        } else {
            Self::from_json_str(&text)
        }
        .wrap_err_with(|| format!("failed to parse profile at {}", path.display()))?;
        Ok(def)
    }

    /// Look up a named built-in profile from the crate-shipped registry.
    /// Returns `None` when the name is unknown.
    pub fn builtin(name: &str) -> Option<Self> {
        BUILTIN_PROFILES.iter().find_map(|(id, text)| {
            if *id == name {
                Some(
                    Self::from_json_str(text).unwrap_or_else(|err| {
                        panic!("built-in profile '{id}' is malformed: {err}")
                    }),
                )
            } else {
                None
            }
        })
    }

    /// List built-in profile ids. Useful for CLI help output and tests.
    pub fn builtin_ids() -> Vec<&'static str> {
        BUILTIN_PROFILES.iter().map(|(id, _)| *id).collect()
    }

    /// Resolve a profile from a name or path argument. See the module doc
    /// for the full resolution order. The returned tuple reports the
    /// source so the caller can log `profile resolved: ... source=...`.
    pub fn load(arg: &str) -> Result<(Self, ProfileSource)> {
        let home = dirs::home_dir();
        Self::load_with_home(arg, home.as_deref())
    }

    /// Variant of [`Self::load`] that takes an explicit home directory so
    /// unit tests can exercise the user-dir lookup without touching the
    /// real filesystem.
    pub fn load_with_home(arg: &str, home: Option<&Path>) -> Result<(Self, ProfileSource)> {
        if looks_like_path(arg) {
            let resolved = expand_tilde(arg, home);
            let def = Self::from_file(&resolved)?;
            return Ok((def, ProfileSource::ExplicitPath));
        }

        if let Some(home_dir) = home {
            let profile_dir = home_dir.join(".octos/profiles").join(arg);
            for candidate in ["profile.toml", "profile.json"] {
                let path = profile_dir.join(candidate);
                if path.exists() {
                    let def = Self::from_file(&path)?;
                    return Ok((def, ProfileSource::UserDir));
                }
            }
        }

        if let Some(def) = Self::builtin(arg) {
            return Ok((def, ProfileSource::Builtin));
        }

        eyre::bail!(
            "unknown profile '{arg}': not a file, no entry in ~/.octos/profiles/, and \
             not a built-in ({})",
            Self::builtin_ids().join(", "),
        )
    }

    /// Apply the tool filter declared by this profile to a freshly-built
    /// [`ToolRegistry`]. See [`ToolRegistry::filter_by_profile`] for the
    /// spawn-only carve-out.
    pub fn apply_to_registry(&self, registry: &mut ToolRegistry) {
        registry.filter_by_profile(&self.tools);
    }
}

fn looks_like_path(arg: &str) -> bool {
    arg.starts_with('/')
        || arg.starts_with("./")
        || arg.starts_with("~/")
        || arg.starts_with("../")
        // Windows absolute paths (`C:\…`, `C:/…`, verbatim `\\?\…`, UNC
        // `\\server\share`) match none of the Unix-style prefixes above, so a
        // real profile file passed by absolute path was misclassified as a
        // profile *name* and rejected as "unknown profile". `is_absolute()` is
        // platform-aware and a no-op on Unix (there it ⇔ a leading `/`).
        || Path::new(arg).is_absolute()
}

fn expand_tilde(arg: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/") {
        if let Some(home_dir) = home {
            return home_dir.join(rest);
        }
    }
    PathBuf::from(arg)
}
