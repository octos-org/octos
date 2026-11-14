//! Plugin loader: scans directories for plugins and registers their tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use octos_llm::vertex_auth::{ServiceAccount, TokenSource, VertexTokenProvider};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::agent::MAX_TOOL_TIMEOUT_SECS;
use crate::hooks::HookConfig;
use crate::mcp::McpServerConfig;
use crate::sandbox::BLOCKED_ENV_VARS;
use crate::tools::{MakeTypeEntry, Tool, ToolRegistry};

use super::extras::{SKILL_EXPLORATION_PREAMBLE, SkillExtras, resolve_extras};
use super::manifest::{
    ConcurrencyClassClassification, PluginManifest, PluginToolDef, SkillActionDef,
};
use super::source_runtime;
use super::tool::{PluginTool, SynthesisConfig};

const MAX_EXECUTABLE_SIZE: u64 = 100_000_000;
const GENERATIVE_SKILL_ENV_ALLOWLIST: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "GEMINI_API_KEY",
    "GEMINI_BASE_URL",
    "GOOGLE_API_KEY",
    "GOOGLE_BASE_URL",
    "DASHSCOPE_API_KEY",
    "DASHSCOPE_BASE_URL",
];

/// Aggregated result from loading plugins across directories.
#[derive(Debug, Default)]
pub struct PluginLoadResult {
    /// Number of tools registered into the `ToolRegistry`.
    pub tool_count: usize,
    /// Names of all tools registered by plugins.
    pub tool_names: Vec<String>,
    /// UI-callable actions whose bindings were validated against tools loaded
    /// from the same accepted plugin manifest.
    pub loaded_actions: Vec<LoadedSkillAction>,
    /// MCP server configs resolved from skill manifests.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Hook configs resolved from skill manifests.
    pub hooks: Vec<HookConfig>,
    /// Prompt fragments read from skill directories.
    pub prompt_fragments: Vec<String>,
    /// RFC-1 (issue #1290): dispatcher entries collected from manifests
    /// that declare `make_type`. The host (CLI / serve / chat) uses
    /// this list to register the `mofa_make` dispatcher AND hide the
    /// individual target tools from the LLM-visible spec list. Empty
    /// means no mofa-* skills were discovered — the dispatcher is not
    /// registered (avoids publishing an unusable tool).
    pub make_type_entries: Vec<MakeTypeEntry>,
    /// Individual plugins rejected during an otherwise best-effort load.
    /// Legacy startup callers continue with successfully loaded plugins;
    /// mutation-time rebuilds may fail closed on this structured record.
    pub plugin_errors: Vec<PluginLoadError>,
}

/// A plugin-specific failure observed by the aggregating loader.
#[derive(Debug, Clone)]
pub struct PluginLoadError {
    /// Directory of the rejected plugin.
    pub plugin_dir: PathBuf,
    /// Sanitized display message for diagnostics and strict callers.
    pub message: String,
}

/// A UI action trusted by the canonical plugin loader.
#[derive(Debug, Clone)]
pub struct LoadedSkillAction {
    /// Plugin that declared and owns the action.
    pub plugin_name: String,
    /// Skill directory recorded by the canonical loader.
    pub plugin_dir: PathBuf,
    /// Validated manifest action definition.
    pub definition: SkillActionDef,
    /// Successfully loaded tool owned by the same plugin.
    pub tool_name: String,
}

impl LoadedSkillAction {
    /// Whether the current registry still contains the plugin tool that this
    /// action was registered against. Registries may be rebound or extended
    /// after loading, so name presence alone is not sufficient.
    pub fn is_bound_to_registry(&self, registry: &ToolRegistry) -> bool {
        registry
            .get(&self.tool_name)
            .and_then(|tool| tool.as_any().downcast_ref::<PluginTool>())
            .is_some_and(|tool| tool.plugin_name() == self.plugin_name)
    }
}

struct LoadedPluginTool {
    tool: PluginTool,
    risk: Option<String>,
}

/// Optional knobs for plugin loading beyond `extra_env` and `work_dir`.
///
/// Add new fields here when introducing host→plugin config injection so the
/// existing `load_into` and `load_into_with_work_dir` signatures stay stable
/// for callers that don't need the new functionality.
#[derive(Debug, Default, Clone)]
pub struct PluginLoadOptions<'a> {
    /// Per-process working directory for plugin executions.
    pub work_dir: Option<&'a Path>,
    /// Synthesis LLM provider config injected into plugin args for tools that
    /// opt in via `x-octos-host-config-keys: ["synthesis_config"]`. Tools
    /// without the opt-in never receive this struct.
    pub synthesis_config: Option<SynthesisConfig>,
    /// Strict signature policy. When `true`, plugins without a declared
    /// `manifest.sha256` are REJECTED at load time (instead of the legacy
    /// "warn and proceed" path) AND every invocation re-hashes the verified
    /// executable bytes and compares against the load-time hash before
    /// spawning. When `false` (the default), the legacy permissive flow is
    /// preserved for backward compatibility.
    pub require_signed: bool,
    /// Override the directory used to store the verified-hash ledger.
    /// When `None`, the loader resolves to `~/.octos/cache/verified/` so
    /// the ledger lives outside the skill source tree (writing into the
    /// source dir taints ownership when the daemon runs as a different
    /// uid — see 2026-05 fleet skill-dir-root-ownership bug). Tests pass
    /// a tempdir here for isolation; production callers leave this `None`.
    ///
    /// IMPORTANT: only the hash ledger lives here. The plugin binary
    /// itself executes from its skill source directory so that the
    /// plugin's asset-resolution (which walks from `exe_parent` to find
    /// sibling assets like `<skill>/styles/`) keeps working. PR #1319's
    /// original revision additionally copied the binary bytes into this
    /// directory and ran from there, which broke asset-bearing plugins
    /// like `mofa-slides` (#1325). The hash file alone is sufficient to
    /// solve the skill-dir-ownership goal.
    pub verified_cache_dir: Option<PathBuf>,
}

impl PluginLoadResult {
    fn merge_extras(&mut self, extras: SkillExtras) {
        self.mcp_servers.extend(extras.mcp_servers);
        self.hooks.extend(extras.hooks);
        // PR-F: dedup the generic exploration preamble across plugins.
        // Each discovery-bearing plugin pushes the same constant string
        // through `resolve_extras`; keeping every copy would balloon the
        // system prompt with N identical paragraphs (`N` = number of
        // skills that ship a `discovery` block). The first occurrence
        // wins; later duplicates of the same exact string are dropped.
        // Non-preamble fragments (skill cards, prompts.include outputs)
        // are NOT deduped — distinct cards must survive.
        let mut have_preamble = self
            .prompt_fragments
            .iter()
            .any(|f| f.as_str() == SKILL_EXPLORATION_PREAMBLE);
        for frag in extras.prompt_fragments {
            if frag.as_str() == SKILL_EXPLORATION_PREAMBLE {
                if have_preamble {
                    continue;
                }
                have_preamble = true;
            }
            self.prompt_fragments.push(frag);
        }
        // RFC-1: collect dispatcher entries. Last-write-wins by
        // content_type so per-profile skills shadow global skills
        // (matches the existing "first occurrence wins" semantics
        // applied at the manifest layer above, then dedup by
        // content_type here).
        for entry in extras.make_type_entries {
            if let Some(existing) = self
                .make_type_entries
                .iter_mut()
                .find(|e| e.content_type == entry.content_type)
            {
                *existing = entry;
            } else {
                self.make_type_entries.push(entry);
            }
        }
    }
}

/// Scans plugin directories and registers discovered tools.
pub struct PluginLoader;

impl PluginLoader {
    /// Scan directories for plugins and register tools into the registry.
    ///
    /// Each plugin is a directory containing:
    /// - `manifest.json` — plugin metadata and tool definitions
    /// - An executable file (same name as directory, or `main`)
    ///
    /// `extra_env` is injected into plugin processes. Secret-like entries
    /// (API keys, passwords, tokens, secrets) are only injected when the tool
    /// manifest explicitly allowlists that environment variable.
    ///
    /// Returns a `PluginLoadResult` with tool count and any resolved extras
    /// (MCP servers, hooks, prompt fragments).
    pub fn load_into(
        registry: &mut ToolRegistry,
        dirs: &[PathBuf],
        extra_env: &[(String, String)],
    ) -> Result<PluginLoadResult> {
        Self::load_into_with_work_dir(registry, dirs, extra_env, None)
    }

    /// Like `load_into`, but sets a working directory for plugin processes.
    pub fn load_into_with_work_dir(
        registry: &mut ToolRegistry,
        dirs: &[PathBuf],
        extra_env: &[(String, String)],
        work_dir: Option<&Path>,
    ) -> Result<PluginLoadResult> {
        Self::load_into_with_options(
            registry,
            dirs,
            extra_env,
            PluginLoadOptions {
                work_dir,
                synthesis_config: None,
                require_signed: false,
                verified_cache_dir: None,
            },
        )
    }

    /// Full-featured loader that accepts arbitrary [`PluginLoadOptions`].
    ///
    /// New host-controlled config (e.g. `synthesis_config`) is plumbed
    /// through here so older `load_into` callers keep working without
    /// signature churn.
    pub fn load_into_with_options(
        registry: &mut ToolRegistry,
        dirs: &[PathBuf],
        extra_env: &[(String, String)],
        options: PluginLoadOptions<'_>,
    ) -> Result<PluginLoadResult> {
        Self::load_into_with_options_and_filter(registry, dirs, extra_env, options, None)
    }

    /// Like [`load_into_with_options`](Self::load_into_with_options), but applies
    /// a skill-layering (v1) selection filter: a plugin whose manifest id is
    /// not permitted by `skill_filter` is skipped entirely (no tools, hooks,
    /// MCP servers, or prompt fragments registered).
    ///
    /// `skill_filter == None` ⇒ no filtering: every discovered plugin loads
    /// exactly as before this feature existed (backwards-compatible). The
    /// filter matches on `DiscoveredPlugin.manifest.id`, which equals the
    /// skill's `SKILL.md` name and directory name.
    pub fn load_into_with_options_and_filter(
        registry: &mut ToolRegistry,
        dirs: &[PathBuf],
        extra_env: &[(String, String)],
        options: PluginLoadOptions<'_>,
        skill_filter: Option<&crate::skills::SkillFilter>,
    ) -> Result<PluginLoadResult> {
        let mut result = PluginLoadResult::default();

        // Delegate dir scanning + dedup to octos_plugin::discovery so the
        // legacy loader inherits "first occurrence wins" semantics. Without
        // this, a plugin id present in both `~/.octos/skills/` and the
        // per-profile `<data_dir>/skills/` would register twice — and
        // because `ToolRegistry::register` overwrites by tool name, the
        // *last* dir's plugin would silently shadow the earlier one. The
        // per-profile dir is typically appended last (see
        // `runtime/profile.rs::ProfileFactory`), so a stale per-profile
        // install would shadow a freshly-deployed global skill. We hit
        // this twice in 2026 (yangmi, douwentao) before consolidating.
        let mut sources: Vec<octos_plugin::PluginSource> = Vec::with_capacity(dirs.len());
        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            sources.push(octos_plugin::PluginSource {
                path: dir.clone(),
                origin: octos_plugin::PluginOrigin::User,
            });
        }
        let extra_env_map: std::collections::HashMap<String, String> = extra_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Status (Available / Unavailable) is intentionally ignored: the
        // legacy loader has never gated on `requires.bins` / `requires.env`
        // / `requires.os` at registration time — it surfaces failures
        // through actual invocation. Preserving that behaviour avoids
        // silently dropping skills on hosts where a probe disagrees with
        // reality. We may tighten this in a follow-up.
        let discovery = octos_plugin::discover_plugins_with_errors(&sources, &extra_env_map);
        result
            .plugin_errors
            .extend(discovery.errors.into_iter().map(|error| PluginLoadError {
                plugin_dir: error.plugin_dir,
                message: error.message,
            }));

        for plugin in discovery.plugins {
            // Skill layering v1: skip plugins whose skill id is disabled (or
            // not allow-listed) by the profile's inherited selection. Matched
            // on the manifest id (== SKILL.md name == skill dir name).
            if let Some(filter) = skill_filter {
                if !filter.allows(&plugin.manifest.id) {
                    info!(
                        skill = %plugin.manifest.id,
                        "skill layering: skipping plugin disabled by profile config"
                    );
                    continue;
                }
            }
            let path = plugin.path;
            // Re-parse via the agent-side manifest type below: octos_plugin's
            // PluginManifest is a structural subset and doesn't model
            // mcp_servers / hooks / prompts / spawn_only. Discovery has
            // already filtered for `manifest.json` presence, so we skip
            // re-checking and head straight into the rich load path.
            match Self::load_plugin_with_options_and_risks(&path, extra_env, options.clone()) {
                Ok((tools, extras, actions)) => {
                    let mut n = tools.len();
                    let spawn_only = extras.spawn_only_tools.clone();
                    for loaded in tools {
                        let tool = loaded.tool;
                        let name = tool.name().to_string();
                        // A plugin never takes a compiled-in tool's name:
                        // `register` would silently replace the built-in,
                        // and a name-based policy would then trust plugin
                        // code (UPCR-2026-036).
                        if registry.is_builtin_name(&name) {
                            warn!(
                                plugin = %path.display(),
                                tool = %name,
                                "plugin tool name collides with a built-in tool, skipping"
                            );
                            result.plugin_errors.push(PluginLoadError {
                                plugin_dir: path.clone(),
                                message: format!(
                                    "tool {name} collides with a built-in tool and was not registered"
                                ),
                            });
                            n -= 1;
                            continue;
                        }
                        let risk =
                            octos_core::ui_protocol::manifest_tool_risk(loaded.risk.as_deref());
                        octos_core::ui_protocol::register_tool_approval_risk(name.clone(), risk);
                        result.tool_names.push(name.clone());
                        registry.mark_as_plugin(&name);
                        registry.register(tool);
                    }
                    // Defer spawn_only tools so they're hidden from main session specs
                    // but still registered (available in spawn subagent registries).
                    if !spawn_only.is_empty() {
                        for name in &spawn_only {
                            let msg = extras.spawn_only_messages.get(name).cloned();
                            registry.mark_spawn_only(name, msg);
                        }
                        // Don't defer — tool stays visible to LLM.
                        // The execution loop auto-redirects calls to background spawn.
                        tracing::info!(
                            tools = %spawn_only.join(", "),
                            "registered spawn-only tools (auto-redirect to background)"
                        );
                    }
                    result.tool_count += n;
                    result.loaded_actions.extend(actions);
                    result.merge_extras(extras);
                }
                Err(e) => {
                    result.plugin_errors.push(PluginLoadError {
                        plugin_dir: path.clone(),
                        message: e.to_string(),
                    });
                    warn!(
                        plugin_dir = %path.display(),
                        error = %e,
                        "failed to load plugin, skipping"
                    );
                }
            }
        }

        // A later plugin can legally reuse a tool name, and ToolRegistry
        // resolves that collision by replacement. Drop any action whose
        // original owner is no longer the registered tool owner.
        result.loaded_actions.retain(|action| {
            let keep = action.is_bound_to_registry(registry);
            if !keep {
                warn!(
                    plugin = %action.plugin_name,
                    action = %action.definition.id,
                    tool = %action.tool_name,
                    "dropping action whose bound tool was replaced after plugin load"
                );
            }
            keep
        });

        // RFC-1 (issue #1290): after every per-plugin registration is
        // done, install the `mofa_make` dispatcher + its describe
        // companion and hide the individual target tools from the
        // LLM-visible spec list.
        //
        // Hiding uses `mark_internal_hidden` (not unregister) so the target
        // tools:
        //   - remain reachable via `registry.get(name)` — the dispatcher
        //     forwards to them by name, and legacy/internal callers
        //     (e.g. pre-existing test paths) can still invoke them.
        //   - are invisible to `specs()` — the LLM never sees them, so the
        //     LLM only ever reaches them through the `mofa_make` dispatcher.
        //
        // The LLM only sees `mofa_make` + `mofa_describe_content_type`,
        // never the individual `mofa_slides` / `mofa_cards` / ...
        // tools that the loader registered.
        if !result.make_type_entries.is_empty() {
            for entry in &result.make_type_entries {
                if registry.get(&entry.target_tool).is_some() {
                    registry.mark_internal_hidden(&entry.target_tool);
                } else {
                    warn!(
                        content_type = %entry.content_type,
                        target_tool = %entry.target_tool,
                        "mofa_make dispatcher target not in registry — \
                         skill load may have failed; dispatch will surface \
                         DISPATCHER_ERROR at invocation"
                    );
                }
            }
            // Mint dispatcher pair (returns None only when entries
            // is empty, ruled out by the outer guard).
            if let Some((dispatcher, describe)) =
                crate::tools::make_dispatcher_with_entries(result.make_type_entries.clone())
            {
                let dispatcher = std::sync::Arc::new(dispatcher);
                let describe = std::sync::Arc::new(describe);
                registry.register_arc(dispatcher.clone());
                registry.register_arc(describe.clone());
                // The dispatcher itself is spawn_only so the execution
                // loop intercepts it and runs the forward in a
                // background tokio task — matches the spawn_only
                // contract every individual mofa_* skill historically
                // had, and lets the chat UI anchor every long-running
                // generation to a single bubble.
                registry.mark_spawn_only(
                    "mofa_make",
                    Some(
                        "SUCCESS: content generation started in the background. \
                         The result will be delivered to the user automatically when ready. \
                         No further action needed for this request."
                            .into(),
                    ),
                );
                // `mofa_describe_content_type` is a synchronous catalog
                // query (no skill spawn), so it stays foreground.
                tracing::info!(
                    entries = result.make_type_entries.len(),
                    "registered mofa_make dispatcher (RFC-1)"
                );
                // The dispatcher's Weak<ToolRegistry> back-reference is
                // wired by `wire_mofa_make_registry_back_ref` AFTER
                // the host wraps the registry in `Arc`.
                result.tool_names.push("mofa_make".into());
                result.tool_names.push("mofa_describe_content_type".into());
            }
        }

        if result.tool_count > 0 {
            info!(tools = result.tool_count, "loaded plugin tools");
        }
        if !result.mcp_servers.is_empty() || !result.hooks.is_empty() {
            info!(
                mcp_servers = result.mcp_servers.len(),
                hooks = result.hooks.len(),
                prompt_fragments = result.prompt_fragments.len(),
                "loaded skill extras"
            );
        }

        Ok(result)
    }

    /// RFC-1: wire the dispatcher's registry back-reference after the
    /// host wraps the registry in `Arc`. The dispatcher's `execute`
    /// path needs a `Weak<ToolRegistry>` to look up forwarding targets.
    /// Without this call the dispatcher returns a `DISPATCHER_ERROR`
    /// on every invocation.
    ///
    /// Idempotent and silent on registries with no mofa-* skills —
    /// hosts can call it unconditionally after every load.
    pub fn wire_mofa_make_registry_back_ref(registry: &std::sync::Arc<ToolRegistry>) {
        let weak = std::sync::Arc::downgrade(registry);
        if let Some(arc) = registry.get("mofa_make") {
            if let Some(t) = arc.as_any().downcast_ref::<crate::tools::MofaMakeTool>() {
                t.set_registry(weak.clone());
            }
        }
        if let Some(arc) = registry.get("mofa_describe_content_type") {
            if let Some(t) = arc
                .as_any()
                .downcast_ref::<crate::tools::MofaDescribeContentTypeTool>()
            {
                t.set_registry(weak);
            }
        }
    }

    /// Load a single plugin directory and return its tools and extras.
    pub fn load_plugin(
        plugin_dir: &Path,
        extra_env: &[(String, String)],
    ) -> Result<(Vec<PluginTool>, SkillExtras)> {
        Self::load_plugin_with_work_dir(plugin_dir, extra_env, None)
    }

    /// Load a single plugin directory with an optional working directory.
    ///
    /// Returns `(tools, extras)`. If the manifest declares no tools but has
    /// extras (MCP servers, hooks, prompts), the executable search is skipped
    /// and an empty tool vec is returned alongside the resolved extras.
    pub fn load_plugin_with_work_dir(
        plugin_dir: &Path,
        extra_env: &[(String, String)],
        work_dir: Option<&Path>,
    ) -> Result<(Vec<PluginTool>, SkillExtras)> {
        Self::load_plugin_with_options(
            plugin_dir,
            extra_env,
            PluginLoadOptions {
                work_dir,
                synthesis_config: None,
                require_signed: false,
                verified_cache_dir: None,
            },
        )
    }

    /// Full-featured single-plugin loader that accepts arbitrary
    /// [`PluginLoadOptions`].
    pub fn load_plugin_with_options(
        plugin_dir: &Path,
        extra_env: &[(String, String)],
        options: PluginLoadOptions<'_>,
    ) -> Result<(Vec<PluginTool>, SkillExtras)> {
        let (tools, extras, _actions) =
            Self::load_plugin_with_options_and_risks(plugin_dir, extra_env, options)?;
        Ok((
            tools.into_iter().map(|loaded| loaded.tool).collect(),
            extras,
        ))
    }

    fn load_plugin_with_options_and_risks(
        plugin_dir: &Path,
        extra_env: &[(String, String)],
        options: PluginLoadOptions<'_>,
    ) -> Result<(Vec<LoadedPluginTool>, SkillExtras, Vec<LoadedSkillAction>)> {
        let work_dir = options.work_dir;
        let synthesis_config = options.synthesis_config;
        let require_signed = options.require_signed;
        let verified_cache_dir = options.verified_cache_dir.clone();
        // A dev skill (`octos skills dev`) runs from source that lives
        // elsewhere: refresh its manifest copy and launcher before reading
        // them. Unsigned source has no place under strict signing.
        if let Some(dev) = source_runtime::read_dev_marker(plugin_dir)? {
            if require_signed {
                eyre::bail!(
                    "plugin dir '{}' rejected: `plugins.require_signed` is enabled \
                     and a dev skill runs unsigned source",
                    plugin_dir.display(),
                );
            }
            source_runtime::sync_dev_skill(plugin_dir, &dev)?;
        }
        let manifest_path = plugin_dir.join("manifest.json");
        let content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| eyre::eyre!("no manifest.json: {e}"))?;
        // Section C (codex review round-5 P1.1): compute manifest digest at
        // load time so the pre-spawn re-hash gate can detect manifest
        // tampering between load and invocation. Strict mode propagates
        // this hash to `PluginTool` below; permissive mode discards it.
        let manifest_load_hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let manifest: PluginManifest = serde_json::from_str(&content)
            .map_err(|e| eyre::eyre!("invalid manifest.json: {e}"))?;
        validate_manifest_tool_schemas(&manifest)?;

        // Section B (codex review P1.2 + follow-up): under strict signing,
        // REJECT any extras-only manifest before we resolve extras or
        // install anything. The `manifest.sha256` field anchors executable
        // bytes — there is no canonical hash input for an extras-only
        // skill, and a manifest with empty `tools` would never see those
        // bytes hashed below because of the `tools.is_empty()` early
        // return. Under strict mode we refuse to mint trust for that code
        // path entirely; the operator must split executable + extras into
        // separate skills if they need a verifiable extras-only payload.
        if require_signed && manifest.tools.is_empty() && manifest.has_extras() {
            eyre::bail!(
                "plugin '{}' rejected: `plugins.require_signed` is enabled \
                 and extras-only skills (no tools) cannot anchor a verifiable \
                 hash. Split the executable + extras into separate skills.",
                manifest.name,
            );
        }
        // Section B (codex review P1.2): under strict signing, REJECT any
        // tools-bearing skill that omits `sha256`. MCP server commands and
        // lifecycle hooks resolved from `manifest.json` introduce executable
        // code paths the operator did not authorize via a hash. The check
        // runs BEFORE the executable search so we never read or write any
        // bytes for an unsigned plugin under strict mode.
        if require_signed && manifest.sha256.is_none() {
            eyre::bail!(
                "plugin '{}' rejected: `plugins.require_signed` is enabled \
                 and manifest.json has no `sha256` field",
                manifest.name,
            );
        }

        // Section B (codex review round-3 + round-4 P2 + P2-bis): the
        // current `manifest.sha256` semantics anchor only the executable
        // bytes — manifest-side declarations (MCP servers, lifecycle
        // hooks, prompt fragments, and the auto-injected SKILL.md for
        // spawn-only skills) are NOT covered by the digest. A malicious
        // patcher could edit `manifest.json` (or replace SKILL.md
        // contents alongside it) to add executable / prompt code paths
        // without invalidating the executable hash. The strict policy
        // must refuse to mint trust for those paths until the signed
        // material covers the manifest too.
        //
        // We therefore SKIP `resolve_extras` entirely under strict mode
        // so:
        //   1. no glob expansion / file reads against the skill dir
        //      (closing the load-time DoS surface flagged in round-4),
        //   2. no auto-injected SKILL.md for spawn-only skills,
        //   3. no MCP servers / hooks / prompts on the returned extras.
        // Operators who need extras must either run with permissive mode
        // or ship those declarations via a separately-trusted host
        // config (`mcp_servers` + `hooks` on `Config`/`ProfileConfig`).
        let mut extras = if require_signed {
            if manifest.has_extras() || manifest.tools.iter().any(|t| t.spawn_only) {
                warn!(
                    plugin = %manifest.name,
                    "dropping manifest extras + auto-SKILL.md under \
                     `plugins.require_signed`: the digest does not cover them"
                );
            }
            SkillExtras::default()
        } else {
            // Permissive mode: resolve extras the legacy way (MCP, hooks,
            // SKILL.md auto-inject for spawn-only, prompt globs).
            resolve_extras(&manifest, plugin_dir)
        };

        // If no tools declared, skip executable search entirely.
        if manifest.tools.is_empty() {
            if manifest.has_extras() {
                info!(
                    plugin = %manifest.name,
                    "loaded extras-only skill (no tools)"
                );
            }
            return Ok((vec![], extras, vec![]));
        }

        if manifest.canonical_id().trim().is_empty() {
            eyre::bail!("plugin manifest must define a non-empty id or name");
        }

        // Generates a launcher for a source skill that has no executable, and
        // replaces a generated one when the source runtime changed.
        let _ = ensure_plugin_executable_for_manifest(plugin_dir, &manifest)?;

        let executable = find_plugin_executable(plugin_dir, manifest.executable_name()).ok_or_else(|| {
            let dir_name = plugin_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("main");
            eyre::eyre!(
                "no executable found in plugin '{}' (tried '{}', '{}', 'main', and directory scan)",
                manifest.canonical_id(),
                manifest.executable_name(),
                dir_name
            )
        })?;

        // Reject oversized executables (100 MB limit) before reading into memory.
        let exe_meta = std::fs::metadata(&executable)
            .map_err(|e| eyre::eyre!("cannot stat plugin executable: {e}"))?;
        if exe_meta.len() > MAX_EXECUTABLE_SIZE {
            eyre::bail!(
                "plugin '{}' executable too large: {} bytes (max {})",
                manifest.name,
                exe_meta.len(),
                MAX_EXECUTABLE_SIZE
            );
        }

        // Read executable content once for hash verification. The
        // hash is recorded in the verified-hash ledger (cache) so a
        // restart can short-circuit re-hashing when the in-place
        // binary is unchanged. The pre-spawn re-hash gate in
        // `PluginTool::execute` re-reads this same on-disk path and
        // compares against `load_time_hash` to close the load->exec
        // TOCTOU window.
        let exe_bytes = std::fs::read(&executable)
            .map_err(|e| eyre::eyre!("cannot read plugin executable: {e}"))?;

        // Section C: capture the SHA-256 of the verified bytes so the
        // pre-spawn re-hash gate (in `tool.rs::execute`) can compare against
        // exactly what we approved at load time. The hash is computed once
        // here and never recomputed — re-hashing only happens at invocation
        // time, on the verified-exe path on disk.
        let load_time_hash = format!("{:x}", Sha256::digest(&exe_bytes));

        match &manifest.sha256 {
            Some(expected_hash) => {
                if load_time_hash != expected_hash.to_lowercase() {
                    eyre::bail!(
                        "plugin '{}' failed integrity check (hash mismatch)",
                        manifest.name,
                    );
                }
                info!(
                    plugin = %manifest.name,
                    "plugin hash verified"
                );
            }
            None => {
                // Section B: when `require_signed` is on, reject the plugin
                // immediately instead of the legacy "warn and proceed". The
                // operator opted into strict integrity and an undeclared
                // hash means we cannot prove the bytes on disk came from a
                // known good source.
                if require_signed {
                    eyre::bail!(
                        "plugin '{}' rejected: `plugins.require_signed` is enabled \
                         and manifest.json has no `sha256` field",
                        manifest.name,
                    );
                }
                warn!(
                    plugin = %manifest.name,
                    version = %manifest.version,
                    executable = %executable.display(),
                    "loaded unverified plugin (no sha256 in manifest)"
                );
            }
        }

        // Write a verified-hash ledger entry OUTSIDE the skill source dir
        // recording "we hashed the in-place binary at time T and got hash
        // H". The ledger lives at `<verified_cache_dir>/<plugin>/hash.txt`
        // (production: `~/.octos/cache/verified/<plugin>/hash.txt`). The
        // plugin executes from its skill source directory unchanged so
        // asset-resolution (sibling `<skill>/styles/`, `<skill>/templates/`,
        // etc. that plugins like `mofa-slides` walk via `exe_parent`) keeps
        // working.
        //
        // The original 2026-05 ownership taint bug came from writing INTO
        // the skill dir (`.<name>_verified` sibling); moving the metadata
        // out alone is enough to fix that. PR #1319 additionally copied
        // the binary bytes here and ran from the cache copy — that broke
        // asset-bearing plugins (#1325 — mofa-slides "no styles installed
        // on this deployment"). The binary-copy did not actually defend
        // against the threat model either: an attacker with write access
        // to the skill dir can swap the binary BEFORE the loader hashes,
        // in which case we cache the wrong hash anyway. The pre-spawn
        // re-hash gate in `PluginTool::execute` re-reads the in-place
        // binary and compares against `load_time_hash`, which is what
        // closes the load->exec TOCTOU window in practice.
        let verified_hash_path =
            resolve_verified_hash_path(verified_cache_dir.as_deref(), manifest.canonical_id())?;
        if let Some(parent) = verified_hash_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                eyre::eyre!("cannot create verified cache dir {}: {e}", parent.display())
            })?;
        }
        // Best-effort ledger refresh: write the current hash so a future
        // load (same process or a restart) can short-circuit re-hashing
        // when nothing has changed. Failure to write the ledger MUST NOT
        // block plugin load — the gate keys on the in-memory
        // `verified_exe_sha256` we've already computed.
        if let Err(err) = std::fs::write(&verified_hash_path, &load_time_hash) {
            warn!(
                plugin = %manifest.name,
                cache_path = %verified_hash_path.display(),
                error = %err,
                "failed to write verified-hash ledger (continuing — load not blocked)"
            );
        }

        // Collect env vars to filter out
        let blocked_env: Vec<String> = BLOCKED_ENV_VARS.iter().map(|s| s.to_string()).collect();

        // Cancellation-safety (codex review of 7c3e5eac): clamp a manifest
        // `timeout_secs` to the registry's per-tool backstop
        // (`MAX_TOOL_TIMEOUT_SECS` = 1800s). The registry wraps every tool in a
        // `tokio::time::timeout` at the dispatch boundary; if a plugin manifest
        // declared a timeout LARGER than that backstop, the registry guard
        // would preempt the plugin's own graceful kill branch — dropping the
        // future before the plugin could process-group-kill its child. Clamping
        // here guarantees the plugin's own `self.timeout` always fires first,
        // so the graceful kill path runs before the registry backstop ever
        // engages. (`kill_on_drop(true)` is the orphan backstop regardless.)
        let timeout = manifest
            .timeout_secs
            .map(|secs| Duration::from_secs(secs.min(MAX_TOOL_TIMEOUT_SECS)))
            .unwrap_or(PluginTool::DEFAULT_TIMEOUT);
        // A skill that runs from source compiles on its first call; a
        // timeout sized for the built binary would cut that call off.
        let timeout = if source_runtime::is_generated_relax_launcher(&executable) {
            timeout.max(Duration::from_secs(source_runtime::MIN_SOURCE_TIMEOUT_SECS))
        } else {
            timeout
        };

        // Collect spawn_only tool names and messages before consuming
        // manifest.tools. Schema validation has already rejected provider-
        // incompatible tool schemas; tools that fail registration hygiene
        // below are skipped, so drop them from spawn_only metadata too.
        let spawn_only_names: Vec<String> = manifest
            .tools
            .iter()
            .filter(|t| t.spawn_only && t.validate_for_registration().is_ok())
            .map(|t| t.name.clone())
            .collect();
        let spawn_only_msgs: std::collections::HashMap<String, String> = manifest
            .tools
            .iter()
            .filter(|t| {
                t.spawn_only
                    && t.spawn_only_message.is_some()
                    && t.validate_for_registration().is_ok()
            })
            .map(|t| {
                (
                    t.name.clone(),
                    t.spawn_only_message.clone().unwrap_or_default(),
                )
            })
            .collect();

        let plugin_name = manifest.canonical_id().to_string();
        // RFC-1: snapshot the dispatcher metadata BEFORE `manifest.tools`
        // is consumed by `into_iter()` below. Resolution of the target
        // tool name uses [`PluginManifest::make_target_tool_name`] which
        // walks `manifest.tools` — so we materialise the strings here
        // and consume them after `tools` is built (so we can verify the
        // target survived per-tool validation filtering).
        let manifest_make_type: Option<String> = manifest.make_type.clone();
        let manifest_content_desc: Option<String> = manifest.content_type_description.clone();
        let manifest_make_target: Option<String> =
            manifest.make_target_tool_name().map(|s| s.to_string());
        let mut action_ids = std::collections::HashSet::<String>::new();
        // Check every raw ID before validating labels, bindings, or the
        // declaring plugin name. Otherwise an invalid duplicate can be
        // skipped first and leave a second declaration ambiguously active.
        for definition in &manifest.actions {
            if !action_ids.insert(definition.id.clone()) {
                eyre::bail!(
                    "plugin '{}' rejected: duplicate action id '{}'",
                    manifest.name,
                    definition.id
                );
            }
        }
        let action_name_is_valid = if manifest.actions.is_empty() {
            true
        } else {
            match manifest.validate_for_action_registration() {
                Ok(()) => true,
                Err(error) => {
                    warn!(
                        plugin = %manifest.name,
                        %error,
                        "skipping actions with invalid plugin identity"
                    );
                    false
                }
            }
        };
        // Build one token cache for the whole loaded plugin. Every tool gets a
        // clone of this Arc, so classifier/enhancer/lesson calls launched as
        // separate processes do not each repeat Google's OAuth exchange.
        // Creating the source does not make a network request; the first tool
        // that explicitly allowlists VERTEX_ACCESS_TOKEN refreshes it lazily.
        let vertex_token_source: Option<(Arc<dyn TokenSource>, String)> = manifest
            .tools
            .iter()
            .any(|tool| tool.env.iter().any(|name| name == "VERTEX_ACCESS_TOKEN"))
            .then(|| {
                extra_env
                    .iter()
                    .find(|(name, _)| name == "VERTEX_SA_JSON")
                    .map(|(_, value)| value)
            })
            .flatten()
            .and_then(|raw| match ServiceAccount::from_json(raw) {
                Ok(account) => {
                    let project_id = account.project_id.clone();
                    Some((
                        Arc::new(VertexTokenProvider::from_service_account(account))
                            as Arc<dyn TokenSource>,
                        project_id,
                    ))
                }
                Err(error) => {
                    warn!(
                        plugin = %manifest.name,
                        error = %error,
                        "cannot prepare shared Vertex token cache; plugin fallback remains available"
                    );
                    None
                }
            });
        let manifest_actions = manifest.actions;

        let tools: Vec<LoadedPluginTool> = manifest
            .tools
            .into_iter()
            .filter_map(|def| {
                // M6 req 4: registration-time gate for env allowlist hygiene.
                // A malformed manifest entry (empty name, '=', whitespace,
                // process-hijack vars like LD_PRELOAD) is rejected here so
                // the runtime allowlist gate cannot be subverted by a
                // crafted entry that the runtime check would later
                // mis-handle.
                if let Err(err) = def.validate_for_registration() {
                    warn!(
                        plugin = %plugin_name,
                        tool = %def.name,
                        error = %err,
                        "skipping plugin tool with invalid manifest field"
                    );
                    return None;
                }
                // Codex review #1 + issue #718: warn (don't reject) on
                // unknown concurrency_class so authors notice typos like
                // `"exclusive "` (trailing space → silently Safe) or
                // `"exclusve"`. The runtime resolver in tool.rs now
                // fails-closed to Exclusive on Unknown — matches MCP's
                // `resolved_concurrency_class`. This warn keeps the
                // misconfiguration visible even though it is no longer
                // a silent downgrade.
                if let ConcurrencyClassClassification::Unknown(raw) =
                    def.classify_concurrency_class()
                {
                    warn!(
                        plugin = %plugin_name,
                        tool = %def.name,
                        concurrency_class = %raw,
                        "manifest declares unknown concurrency_class; falling back to Exclusive (fail-closed)"
                    );
                }
                let manifest_risk = def.risk.clone();
                let def = apply_builtin_env_allowlist(&plugin_name, def);
                // Run the plugin from its original skill source dir path
                // — NOT from the cache. The pre-spawn re-hash gate in
                // `PluginTool::execute` re-reads this same path and
                // compares against `load_time_hash` (closes the
                // load->exec TOCTOU window). Executing in place keeps
                // plugin asset-resolution intact: skills like mofa-slides
                // walk from `exe_parent` to find sibling `styles/` /
                // `templates/` directories that don't exist in the cache
                // dir. Copying the binary into the cache (the PR #1319
                // approach) silently broke those plugins on the fleet
                // (#1325).
                let mut tool = PluginTool::new(plugin_name.clone(), def, executable.clone())
                    .with_blocked_env(blocked_env.clone())
                    .with_extra_env(extra_env.to_vec())
                    .with_timeout(timeout);
                if let Some((source, project_id)) = vertex_token_source.clone() {
                    tool = tool.with_vertex_token_source(source, project_id);
                }
                // Section C (codex review P2): stash the load-time hash ONLY
                // when the operator opted into integrity for this plugin —
                // either the manifest declared `sha256` (the author signaled
                // care) OR `require_signed = true` (the host signaled care).
                // For legacy unsigned plugins under permissive mode we skip
                // the rehash gate entirely so we don't add a full executable
                // read to every invocation and so the verified-copy refresh
                // path stays cheap. Under strict mode the rehash gate fires
                // unconditionally (`require_signed` propagated to the tool).
                if manifest.sha256.is_some() || require_signed {
                    tool = tool.with_verified_sha256(load_time_hash.clone(), require_signed);
                }
                // Section C (codex review round-5 P1.1): also stash the
                // manifest digest under strict mode so a runtime manifest
                // swap (changing `risk`, `env`, schemas) is detected at
                // the pre-spawn gate.
                if require_signed {
                    tool = tool.with_manifest_sha256(
                        manifest_load_hash.clone(),
                        manifest_path.clone(),
                    );
                }
                if let Some(dir) = work_dir {
                    tool = tool.with_work_dir(dir.to_path_buf());
                }
                // S2 plumbing: attach synthesis_config when the tool's
                // manifest opts in. The runtime check inside
                // `prepare_effective_args` is what gates injection — wiring
                // it onto every tool is harmless because the gate keys off
                // `accepts_host_config_key`.
                if let Some(cfg) = synthesis_config.clone() {
                    tool = tool.with_synthesis_config(cfg);
                }
                Some(LoadedPluginTool {
                    tool,
                    risk: manifest_risk,
                })
            })
            .collect();

        let mut actions = Vec::new();
        if action_name_is_valid {
            let loaded_tool_names: std::collections::HashSet<&str> =
                tools.iter().map(|loaded| loaded.tool.name()).collect();
            for definition in manifest_actions {
                if let Err(error) = definition.validate_for_registration() {
                    warn!(
                        plugin = %plugin_name,
                        action = %definition.id,
                        %error,
                        "skipping plugin action with invalid manifest field"
                    );
                    continue;
                }
                let tool_name = definition
                    .binding
                    .tool_name()
                    .unwrap_or_default()
                    .to_string();
                if !loaded_tool_names.contains(tool_name.as_str()) {
                    warn!(
                        plugin = %plugin_name,
                        action = %definition.id,
                        tool = %tool_name,
                        "skipping plugin action whose bound tool was not loaded from the same plugin"
                    );
                    continue;
                }
                actions.push(LoadedSkillAction {
                    plugin_name: plugin_name.clone(),
                    plugin_dir: plugin_dir.to_path_buf(),
                    definition,
                    tool_name,
                });
            }
        }

        // Return extras with spawn_only info
        extras.spawn_only_tools = spawn_only_names;
        extras.spawn_only_messages = spawn_only_msgs;

        // RFC-1: if the manifest declares `make_type`, build a
        // dispatcher entry now using the snapshot captured above.
        // The aggregating loader (`load_into_with_options`) dedups
        // these by content_type for per-profile shadowing and uses
        // them to register the dispatcher + hide target tools.
        //
        // Tools that failed manifest validation above were filtered
        // out of `tools`. If the target tool is one of those skipped
        // tools the dispatcher entry would be a dead pointer at
        // dispatch time, so we verify the target tool survived first.
        if let Some(make_type) = manifest_make_type.as_deref() {
            if let Some(target_tool) = manifest_make_target.as_deref() {
                let target_survived = tools.iter().any(|loaded| loaded.tool.name() == target_tool);
                if !target_survived {
                    warn!(
                        skill = %plugin_name,
                        target_tool = %target_tool,
                        "make_type declared but resolved target tool was \
                         filtered out by manifest validation; dispatcher \
                         entry skipped"
                    );
                } else {
                    let description = manifest_content_desc.unwrap_or_else(|| {
                        // Fall back to the tool's description so the
                        // dispatcher spec still has something useful.
                        tools
                            .iter()
                            .find(|loaded| loaded.tool.name() == target_tool)
                            .map(|loaded| loaded.tool.description().to_string())
                            .unwrap_or_default()
                    });
                    extras.make_type_entries.push(MakeTypeEntry::new(
                        make_type.to_string(),
                        plugin_name.clone(),
                        target_tool.to_string(),
                        description,
                    ));
                }
            } else {
                warn!(
                    skill = %plugin_name,
                    make_type = %make_type,
                    "make_type declared but no resolvable target tool \
                     (no `mofa_<make_type>`, no spawn_only tool, no tools \
                     declared); dispatcher entry skipped"
                );
            }
        }

        Ok((tools, extras, actions))
    }
}

fn validate_manifest_tool_schemas(manifest: &PluginManifest) -> Result<()> {
    validate_manifest_tool_schemas_with(manifest, octos_plugin::ValidationProfile::from_env())
}

fn validate_manifest_tool_schemas_with(
    manifest: &PluginManifest,
    profile: octos_plugin::ValidationProfile,
) -> Result<()> {
    if matches!(profile, octos_plugin::ValidationProfile::Off) {
        return Ok(());
    }

    let mut errors = Vec::new();
    for tool in &manifest.tools {
        errors.extend(octos_plugin::validate_schema(
            &tool.name,
            octos_plugin::SchemaKind::Input,
            &tool.input_schema,
            profile,
        ));
    }

    if errors.is_empty() {
        return Ok(());
    }

    let details = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    eyre::bail!(
        "plugin '{}' has {} schema violation(s):\n{}\n\nSet OCTOS_MANIFEST_VALIDATION=lenient to relax the strict octos profile, or =off to disable validation entirely.",
        manifest.name,
        errors.len(),
        details
    );
}

fn apply_builtin_env_allowlist(plugin_name: &str, mut def: PluginToolDef) -> PluginToolDef {
    let envs = match (plugin_name, def.name.as_str()) {
        ("mofa-slides", "mofa_slides") | ("mofa-infographic", "mofa_infographic") => {
            GENERATIVE_SKILL_ENV_ALLOWLIST
        }
        _ => return def,
    };

    for env in envs {
        if !def.env.iter().any(|existing| existing == env) {
            def.env.push((*env).to_string());
        }
    }
    def
}

/// Ensure a plugin directory has a runnable executable for manifests that
/// declare tools. Returns `true` if a fallback executable was created.
pub(crate) fn ensure_plugin_executable(plugin_dir: &Path) -> Result<bool> {
    let manifest_path = plugin_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(false);
    }
    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| eyre::eyre!("no manifest.json: {e}"))?;
    let manifest: PluginManifest =
        serde_json::from_str(&content).map_err(|e| eyre::eyre!("invalid manifest.json: {e}"))?;
    ensure_plugin_executable_for_manifest(plugin_dir, &manifest)
}

fn ensure_plugin_executable_for_manifest(
    plugin_dir: &Path,
    manifest: &PluginManifest,
) -> Result<bool> {
    if manifest.tools.is_empty() {
        return Ok(false);
    }
    let main_path = plugin_dir.join("main");
    let existing = find_plugin_executable(plugin_dir, manifest.executable_name());
    if let Some(existing) = &existing {
        // A real executable stays. So does a dev skill's launcher: its own
        // sync wrote it. Only a launcher generated here is ever replaced.
        let generated_here = *existing == main_path
            && !source_runtime::is_dev_skill(plugin_dir)
            && (is_generated_lazy_cargo_launcher(existing)
                || source_runtime::is_generated_relax_launcher(existing));
        if !generated_here {
            return Ok(false);
        }
    }
    if manifest
        .sha256
        .as_ref()
        .is_some_and(|hash| !hash.trim().is_empty())
    {
        return Ok(false);
    }

    // mofa-publish / mofa-site are now Cargo-based Rust skills (they ship a
    // Cargo.toml + [[bin]]), so they fall through to the generic Cargo path
    // below alongside mofa-podcast — no bespoke bash+python wrapper.

    // Cargo-based skills: create a lazy launcher so runtime can self-heal if
    // install-time build/download was skipped or unavailable. The launcher
    // builds with cargo on its first call, or, when the operator chose the
    // Relax source runtime and it is set up, runs the source through the JIT.
    if !plugin_dir.join("Cargo.toml").exists() {
        return Ok(false);
    }
    let relax_launcher = source_runtime::installed_source_skills_use_relax()
        .then(|| source_runtime::installed_relax_launcher(plugin_dir))
        .flatten();
    let (script, runtime) = match relax_launcher {
        Some(script) => (script, "relax"),
        None => match detect_cargo_bin_name(plugin_dir) {
            Some(bin_name) => (lazy_cargo_wrapper_script(&bin_name), "cargo"),
            None => return Ok(false),
        },
    };
    if existing.is_some() && std::fs::read_to_string(&main_path).is_ok_and(|now| now == script) {
        return Ok(false);
    }
    write_executable_wrapper(&main_path, &script)?;
    info!(
        plugin = %manifest.name,
        executable = %main_path.display(),
        runtime,
        "generated source skill launcher"
    );
    Ok(true)
}

fn is_generated_lazy_cargo_launcher(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|contents| {
        contents.contains("Skill binary is missing and cargo is not installed")
            && contents.contains("cargo build --release")
            && contents.contains("target/release/")
    })
}

fn find_plugin_executable(plugin_dir: &Path, manifest_name: &str) -> Option<PathBuf> {
    let dir_name = plugin_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("main");

    [manifest_name, dir_name, "main"]
        .iter()
        .map(|name| plugin_dir.join(name))
        .find(|p| p.exists() && is_executable(p))
        .or_else(|| {
            std::fs::read_dir(plugin_dir).ok()?.flatten().find_map(|e| {
                let p = e.path();
                if p.is_file() && is_executable(&p) {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.')
                        && !name.ends_with(".json")
                        && !name.ends_with(".md")
                        && !name.ends_with(".toml")
                        && !name.ends_with(".tar.gz")
                    {
                        return Some(p);
                    }
                }
                None
            })
        })
}

fn detect_cargo_bin_name(plugin_dir: &Path) -> Option<String> {
    let cargo_toml = std::fs::read_to_string(plugin_dir.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = toml::from_str(&cargo_toml).ok()?;

    if let Some(bin_name) = parsed
        .get("bin")
        .and_then(|v| v.as_array())
        .and_then(|bins| {
            bins.iter()
                .find_map(|bin| bin.get("name").and_then(|name| name.as_str()))
        })
    {
        return Some(bin_name.to_string());
    }

    parsed
        .get("package")
        .and_then(|pkg| pkg.get("name"))
        .and_then(|name| name.as_str())
        .map(str::to_string)
}

fn write_executable_wrapper(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn lazy_cargo_wrapper_script(bin_name: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${{BASH_SOURCE[0]}}")" && pwd)"
BIN="$SCRIPT_DIR/target/release/{bin_name}"

if [[ ! -x "$BIN" ]]; then
  if ! command -v cargo >/dev/null 2>&1; then
    printf '{{"output":"Skill binary is missing and cargo is not installed. Run: cargo build --release in {bin_name}","success":false}}\n'
    exit 0
  fi
  if ! (cd "$SCRIPT_DIR" && cargo build --release >/dev/null 2>&1); then
    printf '{{"output":"Failed to build skill binary with cargo build --release.","success":false}}\n'
    exit 0
  fi
fi

exec "$BIN" "$@"
"#
    )
}

/// Resolve where to store the verified-hash ledger entry for a plugin.
///
/// The cache directory layout is `<base>/<plugin_name>/hash.txt`, where
/// `hash.txt` contains the SHA-256 (lowercase hex) of the plugin
/// executable as last hashed by the loader. The directory is a ledger
/// only — the binary itself stays in the skill source directory so
/// asset-bearing plugins (`<skill>/styles/`, `<skill>/templates/`, etc.)
/// keep working. See the comment block at the call site for the full
/// rationale.
///
/// Resolution order:
/// 1. Explicit `override_dir` from [`PluginLoadOptions::verified_cache_dir`]
///    (used by tests to isolate from the real cache).
/// 2. Under `cargo test`, a process-scoped tempdir auto-cleaned at exit —
///    keeps the test suite from polluting `~/.octos/cache/verified/` and
///    avoids cross-test races on shared plugin names.
/// 3. `~/.octos/cache/verified/` derived from `dirs::home_dir()`.
/// 4. `std::env::temp_dir().join("octos-verified")` as a last resort when
///    HOME is unavailable (e.g. sandbox).
///
/// Returns `<base>/<plugin_name>/hash.txt`. The caller is responsible
/// for `create_dir_all` on the parent before writing.
fn resolve_verified_hash_path(override_dir: Option<&Path>, plugin_name: &str) -> Result<PathBuf> {
    // Guard against a plugin name with path separators that would let a
    // malicious manifest escape the cache root. The manifest loader already
    // validates this for tool registration, but we belt-and-brace here so
    // future call sites that bypass tool validation still land in a safe
    // subdir.
    if plugin_name.contains('/')
        || plugin_name.contains('\\')
        || plugin_name == "."
        || plugin_name == ".."
        || plugin_name.is_empty()
    {
        eyre::bail!("plugin name {plugin_name:?} not safe for cache path");
    }
    let base = if let Some(dir) = override_dir {
        dir.to_path_buf()
    } else if cfg!(test) {
        // Tests that don't care about the verified-hash location still go
        // through this default path; keep them out of the user's real
        // cache (and let TempDir auto-cleanup at process exit).
        test_default_cache_dir()
    } else if let Some(home) = dirs::home_dir() {
        home.join(".octos").join("cache").join("verified")
    } else {
        std::env::temp_dir().join("octos-verified")
    };
    Ok(base.join(plugin_name).join("hash.txt"))
}

/// Process-scoped tempdir for tests that don't explicitly pass a
/// `verified_cache_dir`. Created once on first access; auto-cleaned when
/// the test process exits. Without this, every test would write into the
/// dev machine's real `~/.octos/cache/verified/` and tests reusing the
/// same plugin name in parallel would race.
#[cfg(test)]
fn test_default_cache_dir() -> PathBuf {
    use std::sync::OnceLock;
    static TEST_CACHE: OnceLock<tempfile::TempDir> = OnceLock::new();
    TEST_CACHE
        .get_or_init(|| {
            tempfile::Builder::new()
                .prefix("octos-verified-test-")
                .tempdir()
                .expect("create test verified cache tempdir")
        })
        .path()
        .to_path_buf()
}

/// Non-test stub so the default branch in `resolve_verified_hash_path`
/// compiles outside `cfg(test)` without a `cfg!(test)` flicker.
#[cfg(not(test))]
#[allow(dead_code)]
fn test_default_cache_dir() -> PathBuf {
    PathBuf::new()
}

/// Check if a path is a regular executable file (Unix).
/// Rejects symlinks as defense-in-depth against link-swap attacks.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Use symlink_metadata to detect symlinks (metadata() follows them).
    match path.symlink_metadata() {
        Ok(m) => m.file_type().is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// On non-Unix, just check existence (no symlink check).
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.exists()
}
