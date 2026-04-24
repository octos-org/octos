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
//! The built-in `coding` profile captures today's no-flag default verbatim
//! so `octos chat` keeps producing byte-for-byte the same runtime behaviour
//! as before. Alternate profiles (e.g. `swarm`) layer on top of `coding`
//! through explicit allow-list changes and expanded agent sets.
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
//! Today's built-in profiles are `coding` (the default) and `swarm` (an
//! allow-list extension that enables multi-worker swarm coordination tools).
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
///   what the built-in `coding` profile uses so behaviour parity with the
///   pre-M8.3 default path is guaranteed.
/// - `allow_list` — only the named tools survive. Names may reference
///   [`crate::tools::policy::ToolGroupInfo`] groups via `group:*` strings.
/// - `deny_list` — every tool survives except the named ones. Useful for
///   profiles that strip a single capability (e.g. drop `web_fetch` from
///   an otherwise-default set).
///
/// `spawn_only` tools are *never* filtered out regardless of mode — they
/// carry background-execution wiring that the runtime depends on.
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
    arg.starts_with('/') || arg.starts_with("./") || arg.starts_with("~/") || arg.starts_with("../")
}

fn expand_tilde(arg: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/") {
        if let Some(home_dir) = home {
            return home_dir.join(rest);
        }
    }
    PathBuf::from(arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_minimum_valid_profile() {
        // Only `name` and `version` are required. Every other field must
        // default cleanly so profile authors can skip sections they are
        // not customizing.
        let json = r#"{"name": "tiny", "version": 1}"#;
        let def = ProfileDefinition::from_json_str(json).expect("parse");
        assert_eq!(def.name, "tiny");
        assert_eq!(def.version, 1);
        assert!(def.description.is_none());
        assert!(matches!(def.tools, ProfileTools::Default));
        assert!(def.mcp_servers.is_empty());
        assert_eq!(def.permissions, PermissionMode::Default);
        assert!(def.compaction_policy.is_none());
        assert!(def.model_preferences.is_none());
        assert!(def.system_prompt_template.is_none());
        assert!(def.agents.is_empty());
    }

    #[test]
    fn should_parse_full_profile_with_all_fields() {
        // Exercise every optional section in a single manifest so the
        // round-trip and defaulting paths are both covered.
        let json = r#"{
            "name": "full",
            "version": 1,
            "description": "kitchen-sink profile",
            "tools": {"mode": "allow_list", "tools": ["shell", "group:fs"]},
            "mcp_servers": [{"id": "jiuwenclaw"}],
            "permissions": "restricted",
            "compaction_policy": {
                "token_budget": 8000,
                "preflight_threshold": 12000,
                "tiers": {"tier_1": 2000, "tier_2": 4000}
            },
            "model_preferences": {
                "default": "anthropic/claude-sonnet-4",
                "fast": "anthropic/claude-haiku",
                "strong": "openai/gpt-5"
            },
            "system_prompt_template": "prompts/coder.md",
            "agents": ["research-worker", "repo-editor"]
        }"#;

        let def = ProfileDefinition::from_json_str(json).expect("parse");
        assert_eq!(def.name, "full");
        assert_eq!(def.description.as_deref(), Some("kitchen-sink profile"));
        match &def.tools {
            ProfileTools::AllowList { tools } => {
                assert_eq!(tools, &vec!["shell".to_string(), "group:fs".to_string()]);
            }
            other => panic!("expected AllowList, got {other:?}"),
        }
        assert_eq!(def.mcp_servers.len(), 1);
        assert_eq!(def.mcp_servers[0].id, "jiuwenclaw");
        assert_eq!(def.permissions, PermissionMode::Restricted);
        let compaction = def.compaction_policy.as_ref().expect("compaction present");
        assert_eq!(compaction.token_budget, Some(8000));
        assert_eq!(compaction.preflight_threshold, Some(12000));
        assert_eq!(compaction.tiers.get("tier_1"), Some(&2000));
        let prefs = def
            .model_preferences
            .as_ref()
            .expect("model preferences present");
        assert_eq!(prefs.default.as_deref(), Some("anthropic/claude-sonnet-4"));
        assert_eq!(prefs.fast.as_deref(), Some("anthropic/claude-haiku"));
        assert_eq!(prefs.strong.as_deref(), Some("openai/gpt-5"));
        assert_eq!(
            def.system_prompt_template.as_deref(),
            Some(Path::new("prompts/coder.md"))
        );
        assert_eq!(def.agents, vec!["research-worker", "repo-editor"]);
    }

    #[test]
    fn should_reject_profile_with_version_mismatch() {
        let json = r#"{"name": "future", "version": 42}"#;
        let err = ProfileDefinition::from_json_str(json).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("version") && msg.contains("42"),
            "expected version error, got {msg}",
        );
    }

    #[test]
    fn should_round_trip_profile_through_json() {
        let original = ProfileDefinition {
            name: "rt".to_string(),
            version: 1,
            description: Some("round-trip".to_string()),
            tools: ProfileTools::DenyList {
                tools: vec!["web_fetch".to_string()],
            },
            mcp_servers: vec![McpServerRef {
                id: "hermes".to_string(),
            }],
            permissions: PermissionMode::Default,
            compaction_policy: Some(ProfileCompactionPolicy {
                token_budget: Some(2048),
                preflight_threshold: None,
                tiers: HashMap::new(),
            }),
            model_preferences: None,
            system_prompt_template: None,
            agents: vec!["repo-editor".to_string()],
        };

        let text = serde_json::to_string(&original).expect("serialize");
        let round = ProfileDefinition::from_json_str(&text).expect("deserialize");
        assert_eq!(round, original);
    }

    #[test]
    fn should_accept_profile_with_unknown_fields_for_forward_compat() {
        // A v2 producer may introduce new fields. The v1 parser must
        // ignore them rather than fail, so the CLI keeps working while
        // the schema evolves.
        let json = r#"{
            "name": "future-proof",
            "version": 1,
            "tools": {"mode": "default"},
            "new_v2_field": {"nested": true},
            "another_extra": 99
        }"#;
        let def = ProfileDefinition::from_json_str(json).expect("parse");
        assert_eq!(def.name, "future-proof");
        assert!(matches!(def.tools, ProfileTools::Default));
    }

    #[test]
    fn should_resolve_profile_name_to_builtin() {
        let coding = ProfileDefinition::builtin("coding").expect("coding builtin");
        assert_eq!(coding.name, "coding");
        assert_eq!(coding.version, 1);
        // `coding` must declare the default tool filter so behaviour
        // parity with the pre-M8.3 no-flag path is preserved.
        assert!(matches!(coding.tools, ProfileTools::Default));

        let swarm = ProfileDefinition::builtin("swarm").expect("swarm builtin");
        assert_eq!(swarm.name, "swarm");
        // Unknown names produce `None` so the load() caller can fall
        // through to a typed error.
        assert!(ProfileDefinition::builtin("does-not-exist").is_none());
    }

    #[test]
    fn should_resolve_profile_path_to_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("custom.json");
        std::fs::write(
            &path,
            r#"{"name": "custom", "version": 1, "description": "from disk"}"#,
        )
        .expect("write");
        let path_str = path.to_string_lossy().to_string();

        let (def, source) =
            ProfileDefinition::load_with_home(&path_str, None).expect("load from path");
        assert_eq!(def.name, "custom");
        assert_eq!(def.description.as_deref(), Some("from disk"));
        assert_eq!(source, ProfileSource::ExplicitPath);
    }

    #[test]
    fn should_resolve_profile_name_via_user_dir() {
        // Place a profile.json under `<home>/.octos/profiles/<name>/` and
        // confirm load() picks it up with source=UserDir.
        let fake_home = tempfile::tempdir().expect("tempdir");
        let profiles_dir = fake_home.path().join(".octos/profiles/alpha");
        std::fs::create_dir_all(&profiles_dir).expect("mkdirs");
        std::fs::write(
            profiles_dir.join("profile.json"),
            r#"{"name": "alpha", "version": 1}"#,
        )
        .expect("write");

        let (def, source) = ProfileDefinition::load_with_home("alpha", Some(fake_home.path()))
            .expect("load from user dir");
        assert_eq!(def.name, "alpha");
        assert_eq!(source, ProfileSource::UserDir);
    }

    #[test]
    fn should_resolve_builtin_when_user_dir_missing() {
        let fake_home = tempfile::tempdir().expect("tempdir");
        // No user-dir override; coding must resolve via the built-in
        // fallback with source=Builtin.
        let (def, source) = ProfileDefinition::load_with_home("coding", Some(fake_home.path()))
            .expect("load builtin");
        assert_eq!(def.name, "coding");
        assert_eq!(source, ProfileSource::Builtin);
    }

    #[test]
    fn should_reject_unknown_profile_name() {
        let fake_home = tempfile::tempdir().expect("tempdir");
        let err = ProfileDefinition::load_with_home("no-such-profile", Some(fake_home.path()))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no-such-profile"));
    }

    #[test]
    fn should_load_builtin_coding_profile_without_error() {
        let coding = ProfileDefinition::builtin("coding").expect("coding");
        coding.validate().expect("valid");
        // The default profile must not declare a tool allow/deny list —
        // otherwise the registry would be filtered and behaviour parity
        // with the pre-M8.3 no-flag path would regress.
        assert!(matches!(coding.tools, ProfileTools::Default));
        // Today's coding default carries no compaction or permission
        // override — those live at the workspace / app-state level.
        assert!(coding.compaction_policy.is_none());
        assert_eq!(coding.permissions, PermissionMode::Default);
        // Agents preloaded match the M8.2 built-in set so spawn() can
        // resolve them by id.
        assert!(coding.agents.contains(&"research-worker".to_string()));
        assert!(coding.agents.contains(&"repo-editor".to_string()));
    }

    #[test]
    fn should_load_builtin_swarm_profile_without_error() {
        let swarm = ProfileDefinition::builtin("swarm").expect("swarm");
        swarm.validate().expect("valid");
        // Swarm must declare an allow list so the registry keeps its
        // swarm-only tools reachable while normal workers stay denied.
        match &swarm.tools {
            ProfileTools::AllowList { tools } => {
                assert!(tools.contains(&"send_to_agent".to_string()));
                assert!(tools.contains(&"cancel_task".to_string()));
                assert!(tools.contains(&"relaunch_task".to_string()));
            }
            other => panic!("swarm must declare an allow list, got {other:?}"),
        }
        assert!(!swarm.agents.is_empty());
    }

    #[test]
    fn looks_like_path_classifies_arguments_correctly() {
        // Explicit paths start with /, ./, ~/, or ../; everything else is
        // treated as a profile name for user-dir / builtin lookup.
        assert!(looks_like_path("/etc/profile.json"));
        assert!(looks_like_path("./local.toml"));
        assert!(looks_like_path("~/my-profile.json"));
        assert!(looks_like_path("../shared.json"));
        assert!(!looks_like_path("coding"));
        assert!(!looks_like_path("swarm"));
    }

    #[test]
    fn expand_tilde_resolves_against_home() {
        let home = Path::new("/opt/octos-home");
        assert_eq!(
            expand_tilde("~/profiles/foo.json", Some(home)),
            PathBuf::from("/opt/octos-home/profiles/foo.json"),
        );
        // Without a home directory the tilde stays literal.
        assert_eq!(
            expand_tilde("~/profiles/foo.json", None),
            PathBuf::from("~/profiles/foo.json"),
        );
    }

    #[test]
    fn should_reject_profile_with_empty_name() {
        let json = r#"{"name": "   ", "version": 1}"#;
        let err = ProfileDefinition::from_json_str(json).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("name"), "expected name error, got {msg}");
    }

    #[test]
    fn should_parse_profile_tools_variants() {
        let default_json = r#"{"mode": "default"}"#;
        let allow_json = r#"{"mode": "allow_list", "tools": ["shell"]}"#;
        let deny_json = r#"{"mode": "deny_list", "tools": ["web_fetch"]}"#;
        let d: ProfileTools = serde_json::from_str(default_json).expect("default");
        let a: ProfileTools = serde_json::from_str(allow_json).expect("allow");
        let de: ProfileTools = serde_json::from_str(deny_json).expect("deny");
        assert!(matches!(d, ProfileTools::Default));
        match a {
            ProfileTools::AllowList { tools } => assert_eq!(tools, vec!["shell".to_string()]),
            _ => panic!("expected allow_list"),
        }
        match de {
            ProfileTools::DenyList { tools } => assert_eq!(tools, vec!["web_fetch".to_string()]),
            _ => panic!("expected deny_list"),
        }
    }
}
