//! Plugin loader: scans directories for plugins and registers their tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::Result;
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
            let path = plugin.path;
            // Re-parse via the agent-side manifest type below: octos_plugin's
            // PluginManifest is a structural subset and doesn't model
            // mcp_servers / hooks / prompts / spawn_only. Discovery has
            // already filtered for `manifest.json` presence, so we skip
            // re-checking and head straight into the rich load path.
            match Self::load_plugin_with_options_and_risks(&path, extra_env, options.clone()) {
                Ok((tools, extras, actions)) => {
                    let n = tools.len();
                    let spawn_only = extras.spawn_only_tools.clone();
                    for loaded in tools {
                        let tool = loaded.tool;
                        let name = tool.name().to_string();
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

        if find_plugin_executable(plugin_dir, manifest.executable_name()).is_none() {
            let _ = ensure_plugin_executable_for_manifest(plugin_dir, &manifest)?;
        }

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
    if find_plugin_executable(plugin_dir, manifest.executable_name()).is_some() {
        return Ok(false);
    }
    if manifest
        .sha256
        .as_ref()
        .is_some_and(|hash| !hash.trim().is_empty())
    {
        return Ok(false);
    }

    let main_path = plugin_dir.join("main");

    // mofa-publish / mofa-site are now Cargo-based Rust skills (they ship a
    // Cargo.toml + [[bin]]), so they fall through to the generic Cargo path
    // below alongside mofa-podcast — no bespoke bash+python wrapper.

    // Cargo-based skills: create a lazy launcher so runtime can self-heal if
    // install-time build/download was skipped or unavailable.
    if plugin_dir.join("Cargo.toml").exists()
        && let Some(bin_name) = detect_cargo_bin_name(plugin_dir)
    {
        write_executable_wrapper(&main_path, &lazy_cargo_wrapper_script(&bin_name))?;
        info!(
            plugin = %manifest.name,
            executable = %main_path.display(),
            bin = %bin_name,
            "generated lazy cargo fallback executable"
        );
        return Ok(true);
    }

    Ok(false)
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

/// Compute SHA-256 hex digest of a file.
#[cfg(test)]
fn compute_sha256(path: &Path) -> Result<String> {
    let data = std::fs::read(path)?;
    let hash = Sha256::digest(&data);
    Ok(format!("{hash:x}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_test_plugin_executable(plugin_dir: &Path, plugin_name: &str) {
        use std::os::unix::fs::PermissionsExt;

        let executable = plugin_dir.join(plugin_name);
        std::fs::write(
            &executable,
            "#!/bin/sh\necho '{\"output\":\"ok\",\"success\":true}'",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn test_load_nonexistent_dir() {
        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[PathBuf::from("/nonexistent/path")], &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().tool_count, 0);
    }

    #[test]
    fn test_load_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn should_register_action_for_successfully_loaded_owned_tool() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("action-owner");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "action-owner",
                "version": "1.0",
                "tools": [{
                    "name": "owned_tool",
                    "description": "Owned tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "document.open",
                    "label": "Open document",
                    "binding": {"type": "tool", "tool": "owned_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&plugin_dir, "action-owner");

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        assert_eq!(result.loaded_actions.len(), 1);
        assert_eq!(result.loaded_actions[0].plugin_name, "action-owner");
        assert_eq!(result.loaded_actions[0].definition.id, "document.open");
        assert_eq!(result.loaded_actions[0].tool_name, "owned_tool");
    }

    #[cfg(unix)]
    #[test]
    fn should_register_nothing_when_same_root_duplicate_has_richer_validation_failure() {
        let root = tempfile::tempdir().unwrap();
        let valid_dir = root.path().join("a-valid-copy");
        let invalid_dir = root.path().join("z-invalid-copy");
        std::fs::create_dir(&valid_dir).unwrap();
        std::fs::create_dir(&invalid_dir).unwrap();

        std::fs::write(
            valid_dir.join("manifest.json"),
            r#"{
                "name": "duplicate-owner",
                "version": "1.0.0",
                "tools": [{
                    "name": "duplicate_owned_tool",
                    "description": "valid sibling tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "document.open",
                    "label": "Open",
                    "binding": {"type": "tool", "tool": "duplicate_owned_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&valid_dir, "a-valid-copy");

        // Discovery accepts this structural subset, while the richer agent
        // manifest rejects the unsupported action execution value.
        std::fs::write(
            invalid_dir.join("manifest.json"),
            r#"{
                "name": "duplicate-owner",
                "version": "2.0.0",
                "tools": [{
                    "name": "invalid_duplicate_tool",
                    "description": "invalid sibling tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "document.open",
                    "label": "Open",
                    "execution": "unsupported",
                    "binding": {"type": "tool", "tool": "invalid_duplicate_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&invalid_dir, "z-invalid-copy");

        let mut registry = ToolRegistry::new();
        let loaded =
            PluginLoader::load_into(&mut registry, &[root.path().to_path_buf()], &[]).unwrap();

        assert_eq!(loaded.tool_count, 0);
        assert!(loaded.tool_names.is_empty());
        assert!(loaded.loaded_actions.is_empty());
        assert!(registry.get("duplicate_owned_tool").is_none());
        assert!(registry.get("invalid_duplicate_tool").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn should_block_lower_priority_copy_when_same_root_duplicate_includes_invalid_manifest() {
        let high_priority = tempfile::tempdir().unwrap();
        let lower_priority = tempfile::tempdir().unwrap();
        let valid_dir = high_priority.path().join("a-valid-copy");
        let invalid_dir = high_priority.path().join("z-invalid-copy");
        let lower_dir = lower_priority.path().join("lower-copy");
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        std::fs::create_dir_all(&lower_dir).unwrap();

        for (dir, tool, schema) in [
            (
                &valid_dir,
                "high_priority_tool",
                r#"{"type":"object","properties":{}}"#,
            ),
            (&invalid_dir, "invalid_tool", r#"{"type":"string"}"#),
            (
                &lower_dir,
                "lower_priority_tool",
                r#"{"type":"object","properties":{}}"#,
            ),
        ] {
            std::fs::write(
                dir.join("manifest.json"),
                format!(
                    r#"{{"name":"duplicate-owner","version":"1.0.0","tools":[{{"name":"{tool}","description":"tool","input_schema":{schema}}}]}}"#
                ),
            )
            .unwrap();
        }
        write_test_plugin_executable(&valid_dir, "a-valid-copy");
        write_test_plugin_executable(&invalid_dir, "z-invalid-copy");
        write_test_plugin_executable(&lower_dir, "lower-copy");

        let mut registry = ToolRegistry::new();
        let loaded = PluginLoader::load_into(
            &mut registry,
            &[
                high_priority.path().to_path_buf(),
                lower_priority.path().to_path_buf(),
            ],
            &[],
        )
        .unwrap();

        assert_eq!(loaded.tool_count, 0);
        assert!(loaded.tool_names.is_empty());
        assert!(registry.get("high_priority_tool").is_none());
        assert!(registry.get("lower_priority_tool").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn should_reject_action_when_bound_tool_is_not_owned_by_declaring_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let owner_dir = dir.path().join("a-tool-owner");
        let borrower_dir = dir.path().join("b-action-borrower");
        std::fs::create_dir(&owner_dir).unwrap();
        std::fs::create_dir(&borrower_dir).unwrap();
        std::fs::write(
            owner_dir.join("manifest.json"),
            r#"{
                "name": "a-tool-owner",
                "version": "1.0",
                "tools": [{
                    "name": "shared_tool",
                    "description": "Shared tool",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&owner_dir, "a-tool-owner");
        std::fs::write(
            borrower_dir.join("manifest.json"),
            r#"{
                "name": "b-action-borrower",
                "version": "1.0",
                "tools": [{
                    "name": "borrower_tool",
                    "description": "Borrower's own tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "shared.run",
                    "label": "Run shared tool",
                    "binding": {"type": "tool", "tool": "shared_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&borrower_dir, "b-action-borrower");

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        assert_eq!(result.tool_count, 2);
        assert!(registry.get("shared_tool").is_some());
        assert!(registry.get("borrower_tool").is_some());
        assert!(result.loaded_actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn should_reject_mixed_validity_duplicate_action_ids_before_field_validation() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mixed-duplicate-actions");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "mixed-duplicate-actions",
                "version": "1.0",
                "tools": [{
                    "name": "mixed_tool",
                    "description": "Mixed validity tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [
                    {
                        "id": "mixed.run",
                        "label": "",
                        "binding": {"type": "tool", "tool": "mixed_tool"}
                    },
                    {
                        "id": "mixed.run",
                        "label": "Run mixed tool",
                        "binding": {"type": "tool", "tool": "mixed_tool"}
                    }
                ]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&plugin_dir, "mixed-duplicate-actions");

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        assert_eq!(result.tool_count, 0);
        assert!(registry.get("mixed_tool").is_none());
        assert!(result.loaded_actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn should_reject_actions_for_plugins_with_invalid_qualified_identity_names() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("whitespace-name", " bad-name ", "whitespace_action_tool"),
            ("control-name", "bad\nname", "control_action_tool"),
            ("slash-name", "bad/name", "slash_action_tool"),
        ];

        for (directory, plugin_name, tool_name) in cases {
            let plugin_dir = dir.path().join(directory);
            std::fs::create_dir(&plugin_dir).unwrap();
            let manifest = serde_json::json!({
                "name": plugin_name,
                "version": "1.0",
                "tools": [{
                    "name": tool_name,
                    "description": "Action identity test tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "identity.check",
                    "label": "Check identity",
                    "binding": {"type": "tool", "tool": tool_name}
                }]
            });
            std::fs::write(
                plugin_dir.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            write_test_plugin_executable(&plugin_dir, "main");
        }

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        assert!(result.loaded_actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn should_reject_action_when_a_later_plugin_replaces_the_bound_tool_name() {
        let dir = tempfile::tempdir().unwrap();
        let action_root = dir.path().join("action-root");
        let replacement_root = dir.path().join("replacement-root");
        let action_plugin_dir = action_root.join("a-action-owner");
        let replacement_plugin_dir = replacement_root.join("b-tool-replacement");
        std::fs::create_dir(&action_root).unwrap();
        std::fs::create_dir(&replacement_root).unwrap();
        std::fs::create_dir(&action_plugin_dir).unwrap();
        std::fs::create_dir(&replacement_plugin_dir).unwrap();
        std::fs::write(
            action_plugin_dir.join("manifest.json"),
            r#"{
                "name": "a-action-owner",
                "version": "1.0",
                "tools": [{
                    "name": "shared_tool",
                    "description": "Action owner's tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "owned.run",
                    "label": "Run owned tool",
                    "binding": {"type": "tool", "tool": "shared_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&action_plugin_dir, "a-action-owner");
        std::fs::write(
            replacement_plugin_dir.join("manifest.json"),
            r#"{
                "name": "b-tool-replacement",
                "version": "1.0",
                "tools": [{
                    "name": "shared_tool",
                    "description": "Replacement tool",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&replacement_plugin_dir, "b-tool-replacement");

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[action_root, replacement_root], &[]).unwrap();

        assert_eq!(result.tool_count, 2);
        assert!(result.loaded_actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn should_not_bind_action_to_replacement_when_distinct_ids_share_display_name_and_tool() {
        let dir = tempfile::tempdir().unwrap();
        let action_root = dir.path().join("action-root");
        let replacement_root = dir.path().join("replacement-root");
        let action_plugin_dir = action_root.join("a-action-owner");
        let replacement_plugin_dir = replacement_root.join("b-tool-replacement");
        std::fs::create_dir(&action_root).unwrap();
        std::fs::create_dir(&replacement_root).unwrap();
        std::fs::create_dir(&action_plugin_dir).unwrap();
        std::fs::create_dir(&replacement_plugin_dir).unwrap();

        std::fs::write(
            action_plugin_dir.join("manifest.json"),
            r#"{
                "id": "owner-alpha",
                "name": "shared-display-name",
                "version": "1.0",
                "tools": [{
                    "name": "shared_tool",
                    "description": "Action owner's tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "owned.run",
                    "label": "Run owned tool",
                    "binding": {"type": "tool", "tool": "shared_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&action_plugin_dir, "shared-display-name");
        std::fs::write(
            replacement_plugin_dir.join("manifest.json"),
            r#"{
                "id": "replacement-beta",
                "name": "shared-display-name",
                "version": "1.0",
                "tools": [{
                    "name": "shared_tool",
                    "description": "Replacement tool",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&replacement_plugin_dir, "shared-display-name");

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[action_root, replacement_root], &[]).unwrap();

        assert_eq!(
            result.tool_count, 2,
            "plugin errors: {:?}",
            result.plugin_errors
        );
        assert!(
            result.loaded_actions.is_empty(),
            "an action must not remain trusted after another plugin replaces its tool"
        );
        let replacement = registry
            .get("shared_tool")
            .and_then(|tool| tool.as_any().downcast_ref::<PluginTool>())
            .expect("replacement plugin tool should be registered");
        assert_eq!(replacement.plugin_name(), "replacement-beta");
    }

    #[test]
    fn should_report_manifest_rejections_discovered_before_plugin_loading() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("invalid-discovery-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "id": "invalid-discovery-plugin",
                "version": "1.0.0",
                "tools": [{
                    "name": "invalid_schema_tool",
                    "description": "Invalid schema",
                    "input_schema": {"type": "array"}
                }]
            }"#,
        )
        .unwrap();

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[])
            .expect("legacy loading remains best-effort");

        assert_eq!(result.plugin_errors.len(), 1);
        assert_eq!(result.plugin_errors[0].plugin_dir, plugin_dir);
        assert!(
            result.plugin_errors[0]
                .message
                .contains("manifest validation failed")
        );
        assert!(registry.get("invalid_schema_tool").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn should_not_register_actions_when_strict_signing_rejects_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("unsigned-actions");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "unsigned-actions",
                "version": "1.0",
                "tools": [{
                    "name": "unsigned_tool",
                    "description": "Unsigned tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [{
                    "id": "unsigned.run",
                    "label": "Run unsigned tool",
                    "binding": {"type": "tool", "tool": "unsigned_tool"}
                }]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&plugin_dir, "unsigned-actions");

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                require_signed: true,
                verified_cache_dir: Some(dir.path().join("verified")),
                ..PluginLoadOptions::default()
            },
        )
        .unwrap();

        assert_eq!(result.tool_count, 0);
        assert!(result.loaded_actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn should_reject_plugin_with_duplicate_qualified_action_ids() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("duplicate-actions");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "duplicate-actions",
                "version": "1.0",
                "tools": [{
                    "name": "duplicate_tool",
                    "description": "Duplicate tool",
                    "input_schema": {"type": "object", "properties": {}}
                }],
                "actions": [
                    {
                        "id": "document.open",
                        "label": "Open document",
                        "binding": {"type": "tool", "tool": "duplicate_tool"}
                    },
                    {
                        "id": "document.open",
                        "label": "Open document again",
                        "binding": {"type": "tool", "tool": "duplicate_tool"}
                    }
                ]
            }"#,
        )
        .unwrap();
        write_test_plugin_executable(&plugin_dir, "duplicate-actions");

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        assert_eq!(result.tool_count, 0);
        assert!(registry.get("duplicate_tool").is_none());
        assert!(result.loaded_actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_load_plugin_with_manifest() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("my-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Write manifest
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{"name": "my-plugin", "version": "1.0", "tools": [{"name": "greet", "description": "Greet someone", "input_schema": {"type": "object", "properties": {}}}]}"#,
        ).unwrap();

        // Write executable
        let exec_path = plugin_dir.join("my-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"hi\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 1);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn manifest_tool_schema_validation_rejects_untyped_anyof_branch() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{
              "name": "bad-schema-plugin",
              "version": "1.0",
              "tools": [{
                "name": "mofa_slides",
                "description": "Generate slides",
                "input_schema": {
                  "type": "object",
                  "anyOf": [
                    { "required": ["slides"] },
                    { "required": ["input"] }
                  ]
                }
              }]
            }"#,
        )
        .unwrap();

        let err =
            validate_manifest_tool_schemas_with(&manifest, octos_plugin::ValidationProfile::Strict)
                .expect_err("strict schema validation must reject provider-hostile schemas");
        let msg = err.to_string();
        assert!(msg.contains("plugin 'bad-schema-plugin'"));
        assert!(msg.contains("/anyOf/0"));
        assert!(msg.contains("must declare a `type`"));
    }

    #[test]
    fn manifest_tool_schema_validation_honors_lenient_profile() {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{
              "name": "legacy-schema-plugin",
              "version": "1.0",
              "tools": [{
                "name": "legacy_tool",
                "description": "Legacy",
                "input_schema": {
                  "type": "object",
                  "anyOf": [
                    { "required": ["legacy"] }
                  ]
                }
              }]
            }"#,
        )
        .unwrap();

        validate_manifest_tool_schemas_with(&manifest, octos_plugin::ValidationProfile::Lenient)
            .expect("lenient profile should preserve the documented escape hatch");
    }

    #[cfg(unix)]
    #[test]
    fn test_hash_verification_pass() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("hash-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho ok";
        let hash = format!("{:x}", Sha256::digest(exec_content));

        let manifest = format!(
            r#"{{"name": "hash-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "t", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]}}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

        let exec_path = plugin_dir.join("hash-plugin");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_hash_verification_fail() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("bad-hash");
        std::fs::create_dir(&plugin_dir).unwrap();

        let manifest = r#"{"name": "bad-hash", "version": "1.0", "sha256": "0000000000000000000000000000000000000000000000000000000000000000", "tools": [{"name": "t", "description": "d", "input_schema": {"type": "object", "properties": {}}}]}"#;
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

        let exec_path = plugin_dir.join("bad-hash");
        std::fs::write(&exec_path, b"#!/bin/sh\necho tampered").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        // Should succeed overall (skips failed plugin) but register 0 tools
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 0);
    }

    #[test]
    fn test_compute_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_file");
        std::fs::write(&path, b"hello world").unwrap();
        let hash = compute_sha256(&path).unwrap();
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    /// Section B: a plugin without `manifest.sha256` is REJECTED at load
    /// time when `require_signed = true` — instead of the legacy "warn and
    /// proceed" path.
    #[cfg(unix)]
    #[test]
    fn require_signed_rejects_unsigned_plugin() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("unsigned-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // No `sha256` declared → unsigned.
        let manifest = r#"{
            "name": "unsigned-plugin",
            "version": "1.0",
            "tools": [{"name": "t", "description": "d", "input_schema": {"type": "object", "properties": {}}}]
        }"#;
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

        let exec_path = plugin_dir.join("unsigned-plugin");
        std::fs::write(&exec_path, b"#!/bin/sh\necho unsigned").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: true,
                verified_cache_dir: None,
            },
        )
        .unwrap();
        assert_eq!(
            result.tool_count, 0,
            "unsigned plugin must be rejected under require_signed"
        );
    }

    /// Section B: with `require_signed = true`, signed plugins (those that
    /// declare a matching `manifest.sha256`) still load normally.
    #[cfg(unix)]
    #[test]
    fn require_signed_accepts_signed_plugin() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("signed-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho ok";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{"name": "signed-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "t", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]}}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("signed-plugin");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: true,
                verified_cache_dir: None,
            },
        )
        .unwrap();
        assert_eq!(
            result.tool_count, 1,
            "signed plugin must still load under require_signed"
        );
    }

    /// Section B (codex review follow-up): under strict signing, an
    /// extras-only skill (no tools, but with MCP servers / hooks / prompts)
    /// is rejected because the `manifest.sha256` field can never anchor a
    /// hash check for its executable extras — the load path otherwise
    /// returns extras unconditionally on `tools.is_empty()`.
    #[test]
    fn require_signed_rejects_extras_only_skill() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("extras-only");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Extras-only manifest: declares an MCP server but NO tools, and
        // even claims a fake `sha256`. Under strict mode we must reject
        // because hashing the executable bytes never happens for skills
        // with no tools.
        let manifest = r#"{
            "name": "extras-only",
            "version": "1.0",
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "mcp_servers": [{
                "command": "/bin/echo",
                "args": ["mcp"]
            }],
            "tools": []
        }"#;
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: true,
                verified_cache_dir: None,
            },
        )
        .unwrap();
        assert_eq!(
            result.tool_count, 0,
            "extras-only skill must be rejected under require_signed"
        );
        assert!(
            result.mcp_servers.is_empty(),
            "rejected skill's MCP servers must not be installed; got: {:?}",
            result.mcp_servers
        );
    }

    /// Section B (codex review round-3): under strict signing, a
    /// tools-bearing skill that ALSO declares MCP servers / hooks /
    /// prompts in its manifest loads ONLY its tools — the unsigned
    /// extras are dropped because `manifest.sha256` does not cover the
    /// manifest itself. This prevents a manifest-only patch from
    /// installing executable extras while keeping the executable hash
    /// matching.
    #[cfg(unix)]
    #[test]
    fn require_signed_drops_extras_on_mixed_signed_manifest() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mixed-signed");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho ok";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{
                "name": "mixed-signed",
                "version": "1.0",
                "sha256": "{hash}",
                "mcp_servers": [{{
                    "command": "/bin/echo",
                    "args": ["unauthorized"]
                }}],
                "tools": [{{"name": "t", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]
            }}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("mixed-signed");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: true,
                verified_cache_dir: None,
            },
        )
        .unwrap();
        assert_eq!(result.tool_count, 1, "signed tool still registers");
        assert!(
            result.mcp_servers.is_empty(),
            "unsigned MCP extras must be dropped under strict signing; got: {:?}",
            result.mcp_servers
        );
        assert!(
            result.hooks.is_empty(),
            "unsigned hook extras must be dropped under strict signing; got: {:?}",
            result.hooks
        );
    }

    /// Section B (codex review round-4 P2): under strict signing, a
    /// signed spawn-only plugin's SKILL.md auto-injected prompt fragment
    /// is ALSO dropped. The fragment lives outside the executable digest,
    /// so it's not covered by `manifest.sha256` — and an unsigned edit to
    /// SKILL.md would otherwise still slip into the agent system prompt.
    #[cfg(unix)]
    #[test]
    fn require_signed_drops_auto_skill_md_for_spawn_only() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("spawn-only-signed");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho ok";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{
                "name": "spawn-only-signed",
                "version": "1.0",
                "sha256": "{hash}",
                "tools": [{{
                    "name": "do_thing",
                    "description": "d",
                    "spawn_only": true,
                    "input_schema": {{"type": "object", "properties": {{}}}}
                }}]
            }}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        std::fs::write(plugin_dir.join("SKILL.md"), b"# UNSIGNED PROMPT").unwrap();
        let exec_path = plugin_dir.join("spawn-only-signed");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: true,
                verified_cache_dir: None,
            },
        )
        .unwrap();
        assert_eq!(result.tool_count, 1);
        assert!(
            result.prompt_fragments.is_empty(),
            "auto-SKILL.md must be dropped under strict signing; got: {:?}",
            result.prompt_fragments
        );
    }

    /// Section B (codex review round-3): under permissive mode, the same
    /// mixed manifest installs both the tool AND the extras (legacy
    /// behaviour — no regression).
    #[cfg(unix)]
    #[test]
    fn require_signed_off_keeps_mixed_extras_on_signed_manifest() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mixed-permissive");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho ok";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{
                "name": "mixed-permissive",
                "version": "1.0",
                "sha256": "{hash}",
                "mcp_servers": [{{
                    "command": "/bin/echo",
                    "args": ["legacy-mcp"]
                }}],
                "tools": [{{"name": "t2", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]
            }}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("mixed-permissive");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 1);
        assert_eq!(
            result.mcp_servers.len(),
            1,
            "permissive mode preserves extras for backward compat"
        );
    }

    /// Section B (codex review follow-up): under permissive mode, an
    /// extras-only skill still loads its extras as it always did — this
    /// is a backward-compatibility check.
    #[test]
    fn require_signed_off_keeps_extras_only_skill_loading() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("extras-only-legacy");
        std::fs::create_dir(&plugin_dir).unwrap();

        let manifest = r#"{
            "name": "extras-only-legacy",
            "version": "1.0",
            "mcp_servers": [{
                "command": "/bin/echo",
                "args": ["mcp"]
            }],
            "tools": []
        }"#;
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 0, "extras-only skill registers no tools");
        assert_eq!(
            result.mcp_servers.len(),
            1,
            "extras-only skill must surface its MCP server under permissive mode"
        );
    }

    /// Section B: with `require_signed = false` (the legacy default),
    /// unsigned plugins still load with a warning — backward compatibility
    /// is preserved.
    #[cfg(unix)]
    #[test]
    fn require_signed_off_keeps_legacy_unsigned_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("unsigned-legacy");
        std::fs::create_dir(&plugin_dir).unwrap();

        let manifest = r#"{
            "name": "unsigned-legacy",
            "version": "1.0",
            "tools": [{"name": "t", "description": "d", "input_schema": {"type": "object", "properties": {}}}]
        }"#;
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("unsigned-legacy");
        std::fs::write(&exec_path, b"#!/bin/sh\necho legacy").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(
            result.tool_count, 1,
            "unsigned plugin must still load under the legacy default"
        );
    }

    /// Section C: when the in-place plugin executable on disk is swapped
    /// between load and invocation, the pre-spawn re-hash gate refuses
    /// to spawn the process. After the 2026-05 fleet fix the plugin
    /// executes from its skill source dir directly (the cache only
    /// records a hash ledger, not the binary itself) — so the swap is
    /// simulated by overwriting the in-place binary after
    /// `load_into_with_options` returns.
    #[cfg(unix)]
    #[tokio::test]
    async fn pre_spawn_rehash_detects_swap() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("swap-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho original";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{"name": "swap-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "swap_tool", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]}}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("swap-plugin");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cache_dir = dir.path().join("cache");
        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: false,
                verified_cache_dir: Some(cache_dir.clone()),
            },
        )
        .unwrap();
        assert_eq!(result.tool_count, 1);

        // Hash ledger lives in the cache dir; the binary itself is NOT
        // copied there (the in-place skill binary stays canonical for
        // asset-resolution). Sanity-check both invariants before we
        // simulate the tampering.
        let hash_ledger = cache_dir.join("swap-plugin").join("hash.txt");
        assert!(
            hash_ledger.exists(),
            "loader must write a verified-hash ledger at {}",
            hash_ledger.display()
        );
        let cache_binary = cache_dir.join("swap-plugin").join("main");
        assert!(
            !cache_binary.exists(),
            "binary must NOT be copied into the cache (regression: #1325 \
             mofa-slides asset-resolution broke when it was); cache contents: {:?}",
            std::fs::read_dir(cache_dir.join("swap-plugin"))
                .ok()
                .map(|rd| rd
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .collect::<Vec<_>>())
        );

        // Swap the in-place binary so the re-hash gate fires.
        std::fs::write(&exec_path, b"#!/bin/sh\necho TAMPERED").unwrap();

        // Execute the registered tool and assert the gate refused to spawn.
        let tool = registry.get("swap_tool").expect("tool registered");
        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(!result.success, "tampered plugin must not succeed");
        assert!(
            result.output.contains("hash mismatch"),
            "refusal message must explain the cause; got: {}",
            result.output
        );
    }

    /// Section C (codex review round-5 P1.1): under strict signing, a
    /// manifest tampered with between load and invocation is detected by
    /// the pre-spawn gate. We swap the manifest.json bytes on disk
    /// AFTER `load_into` returns and assert the next `execute()` refuses.
    #[cfg(unix)]
    #[tokio::test]
    async fn pre_spawn_rehash_detects_manifest_swap_under_strict() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("manifest-swap");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho '{\"output\":\"ok\",\"success\":true}'";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{"name": "manifest-swap", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "ms_tool", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]}}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), &manifest).unwrap();
        let exec_path = plugin_dir.join("manifest-swap");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: true,
                verified_cache_dir: None,
            },
        )
        .unwrap();

        // Swap manifest.json on disk to a different value. Note: we keep
        // the same `name` so registry lookup still works, but altered
        // `version` ensures the bytes differ.
        let tampered = manifest.replace("\"version\": \"1.0\"", "\"version\": \"99.9\"");
        std::fs::write(plugin_dir.join("manifest.json"), tampered).unwrap();

        let tool = registry.get("ms_tool").expect("tool registered");
        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(!result.success, "tampered manifest must not succeed");
        assert!(
            result.output.contains("manifest.json hash mismatch"),
            "refusal message must call out the manifest mismatch; got: {}",
            result.output
        );
    }

    /// Section C: when the verified-exe bytes are intact, the re-hash gate
    /// passes silently and the plugin spawns. We assert by invoking a
    /// trivial plugin that writes a known JSON to stdout.
    #[cfg(unix)]
    #[tokio::test]
    async fn pre_spawn_rehash_allows_intact_executable() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("intact-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        let exec_content = b"#!/bin/sh\necho '{\"output\":\"ok\",\"success\":true}'";
        let hash = format!("{:x}", Sha256::digest(exec_content));
        let manifest = format!(
            r#"{{"name": "intact-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "intact_tool", "description": "d", "input_schema": {{"type": "object", "properties": {{}}}}}}]}}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("intact-plugin");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        let tool = registry.get("intact_tool").expect("tool registered");
        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(
            result.success,
            "intact plugin must succeed; output: {}",
            result.output
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_is_executable_rejects_symlink() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();

        // Create a real executable
        let real_exec = dir.path().join("real-binary");
        std::fs::write(&real_exec, b"#!/bin/sh\necho hi").unwrap();
        std::fs::set_permissions(&real_exec, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&real_exec), "real file should be executable");

        // Create a symlink to the executable
        let link = dir.path().join("link-to-binary");
        std::os::unix::fs::symlink(&real_exec, &link).unwrap();
        assert!(
            !is_executable(&link),
            "symlink should be rejected by is_executable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_plugin_loader_rejects_symlink_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();

        // Create a real executable somewhere else
        let real_exec = dir.path().join("real-binary");
        std::fs::write(&real_exec, b"#!/bin/sh\necho ok").unwrap();
        std::fs::set_permissions(&real_exec, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Create plugin dir with manifest and symlink as executable
        let plugin_dir = dir.path().join("evil-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{"name": "evil-plugin", "version": "1.0", "tools": [{"name": "evil", "description": "d", "input_schema": {"type": "object", "properties": {}}}]}"#,
        )
        .unwrap();

        // Symlink as the plugin executable
        std::os::unix::fs::symlink(&real_exec, plugin_dir.join("evil-plugin")).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        // Should not load any tools because the executable is a symlink
        assert_eq!(
            result.tool_count, 0,
            "symlink executable should be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_loader_registers_manifest_approval_risk_and_overwrites_unspecified() {
        use std::os::unix::fs::PermissionsExt;

        fn write_plugin(root: &Path, plugin_name: &str, manifest: String) {
            let plugin_dir = root.join(plugin_name);
            std::fs::create_dir(&plugin_dir).unwrap();
            std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

            let exec_path = plugin_dir.join(plugin_name);
            std::fs::write(
                &exec_path,
                "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
            )
            .unwrap();
            std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let declared_tool = "risk_declared_tool";
        let missing_tool = "risk_overwrite_missing_tool";
        let blank_tool = "risk_overwrite_blank_tool";

        let first_root = tempfile::tempdir().unwrap();
        write_plugin(
            first_root.path(),
            "risk-plugin-first",
            format!(
                r#"{{
                    "name": "risk-plugin-first",
                    "version": "1.0",
                    "tools": [
                        {{"name": "{declared_tool}", "description": "declared", "risk": "medium", "input_schema": {{"type": "object", "properties": {{}}}}}},
                        {{"name": "{missing_tool}", "description": "missing first", "risk": "high", "input_schema": {{"type": "object", "properties": {{}}}}}},
                        {{"name": "{blank_tool}", "description": "blank first", "risk": "high", "input_schema": {{"type": "object", "properties": {{}}}}}}
                    ]
                }}"#
            ),
        );

        let mut registry = ToolRegistry::new();
        let first = PluginLoader::load_into(&mut registry, &[first_root.path().to_path_buf()], &[])
            .unwrap();
        assert_eq!(first.tool_count, 3);
        assert_eq!(
            octos_core::ui_protocol::tool_approval_risk(declared_tool),
            "medium"
        );
        assert_eq!(
            octos_core::ui_protocol::tool_approval_risk(missing_tool),
            "high"
        );
        assert_eq!(
            octos_core::ui_protocol::tool_approval_risk(blank_tool),
            "high"
        );

        let second_root = tempfile::tempdir().unwrap();
        write_plugin(
            second_root.path(),
            "risk-plugin-second",
            format!(
                r#"{{
                    "name": "risk-plugin-second",
                    "version": "1.0",
                    "tools": [
                        {{"name": "{missing_tool}", "description": "missing second", "input_schema": {{"type": "object", "properties": {{}}}}}},
                        {{"name": "{blank_tool}", "description": "blank second", "risk": "   ", "input_schema": {{"type": "object", "properties": {{}}}}}}
                    ]
                }}"#
            ),
        );

        let second =
            PluginLoader::load_into(&mut registry, &[second_root.path().to_path_buf()], &[])
                .unwrap();
        assert_eq!(second.tool_count, 2);
        assert_eq!(
            octos_core::ui_protocol::tool_approval_risk(missing_tool),
            "unspecified"
        );
        assert_eq!(
            octos_core::ui_protocol::tool_approval_risk(blank_tool),
            "unspecified"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_loader_bootstraps_cargo_skill_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-publish");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-publish",
  "version": "0.1.0",
  "tools": [{"name": "mofa_publish", "description": "deploy", "input_schema": {"type": "object", "properties": {}}}]
}"#,
        )
        .unwrap();
        // mofa-publish now ships as a Cargo-based Rust skill.
        std::fs::write(
            plugin_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-publish"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "mofa-publish"
path = "src/main.rs"
"#,
        )
        .unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 1);
        assert!(plugin_dir.join("main").exists());
    }

    #[test]
    fn test_builtin_env_allowlist_augments_first_party_mofa_tools_only() {
        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "slides".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            contexts: vec![],
            spawn_only: false,
            env: vec!["EXISTING_ENV".to_string(), "GEMINI_API_KEY".to_string()],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };

        let augmented = apply_builtin_env_allowlist("mofa-slides", def);
        assert!(augmented.env.iter().any(|env| env == "GEMINI_API_KEY"));
        assert!(augmented.env.iter().any(|env| env == "DASHSCOPE_API_KEY"));
        assert!(augmented.env.iter().any(|env| env == "OPENAI_BASE_URL"));
        assert_eq!(
            augmented
                .env
                .iter()
                .filter(|env| env.as_str() == "GEMINI_API_KEY")
                .count(),
            1
        );

        let untrusted = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "slides".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            contexts: vec![],
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let untrusted = apply_builtin_env_allowlist("custom-plugin", untrusted);
        assert!(untrusted.env.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_plugin_executable_creates_lazy_cargo_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-podcast");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-podcast",
  "version": "0.4.5",
  "tools": [{"name": "podcast_generate", "description": "podcast", "input_schema": {"type": "object", "properties": {}}}]
}"#,
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-podcast"
version = "0.4.5"
edition = "2021"
"#,
        )
        .unwrap();

        let changed = ensure_plugin_executable(&plugin_dir).unwrap();
        assert!(changed);
        let wrapper = std::fs::read_to_string(plugin_dir.join("main")).unwrap();
        assert!(wrapper.contains("cargo build --release"));
        assert!(wrapper.contains("target/release/mofa-podcast"));
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_plugin_executable_creates_lazy_cargo_wrapper_for_mofa_publish() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-publish");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-publish",
  "version": "0.1.0",
  "tools": [{"name": "mofa_publish", "description": "deploy", "input_schema": {"type": "object", "properties": {}}}]
}"#,
        )
        .unwrap();
        // mofa-publish now ships as a Cargo-based Rust skill with an explicit
        // [[bin]] name, so it gets the generic lazy-cargo wrapper.
        std::fs::write(
            plugin_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-publish-crate"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "mofa-publish"
path = "src/main.rs"
"#,
        )
        .unwrap();

        let changed = ensure_plugin_executable(&plugin_dir).unwrap();
        assert!(changed);
        let wrapper = std::fs::read_to_string(plugin_dir.join("main")).unwrap();
        assert!(wrapper.contains("cargo build --release"));
        assert!(wrapper.contains("target/release/mofa-publish"));
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_plugin_executable_creates_lazy_cargo_wrapper_for_mofa_site() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-site");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-site",
  "version": "0.1.0",
  "tools": [{"name": "mofa_site", "description": "site", "input_schema": {"type": "object", "properties": {}}}]
}"#,
        )
        .unwrap();
        // mofa-site now ships as a Cargo-based Rust skill with an explicit
        // [[bin]] name, so it gets the generic lazy-cargo wrapper.
        std::fs::write(
            plugin_dir.join("Cargo.toml"),
            r#"[package]
name = "mofa-site-crate"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "mofa-site"
path = "src/main.rs"
"#,
        )
        .unwrap();

        let changed = ensure_plugin_executable(&plugin_dir).unwrap();
        assert!(changed);
        let wrapper = std::fs::read_to_string(plugin_dir.join("main")).unwrap();
        assert!(wrapper.contains("cargo build --release"));
        assert!(wrapper.contains("target/release/mofa-site"));
    }

    #[cfg(unix)]
    #[test]
    fn test_in_place_executable_runs_with_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("perm-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "perm-plugin",
  "version": "0.1.0",
  "tools": [{"name": "perm_tool", "description": "perm", "input_schema": {"type": "object", "properties": {}}}]
}"#,
        )
        .unwrap();
        let in_place_exec = plugin_dir.join("perm-plugin");
        std::fs::write(
            &in_place_exec,
            "#!/usr/bin/env bash\nset -euo pipefail\necho '{\"output\":\"ok\",\"success\":true}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&in_place_exec, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cache_dir = dir.path().join("cache");
        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: false,
                verified_cache_dir: Some(cache_dir.clone()),
            },
        )
        .unwrap();
        assert_eq!(result.tool_count, 1);

        // The plugin executes from the in-place skill source binary —
        // not from the cache. The cache only holds a hash ledger; no
        // binary copy is written there (regression guard for #1325).
        let hash_ledger = cache_dir.join("perm-plugin").join("hash.txt");
        assert!(
            hash_ledger.is_file(),
            "hash ledger must be written at {}",
            hash_ledger.display()
        );
        let cache_binary = cache_dir.join("perm-plugin").join("main");
        assert!(
            !cache_binary.exists(),
            "binary must NOT be copied into the cache (regression: #1325)"
        );

        // The in-place binary keeps its 0o755 permissions — the loader
        // does not chmod the skill source tree under the
        // skill-dir-ownership cleanup.
        let mode = std::fs::metadata(&in_place_exec)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o755,
            "in-place binary must keep its original 0o755 mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_into_with_options_attaches_synthesis_config_to_opted_in_plugins() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("research-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Manifest opts in via x-octos-host-config-keys.
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
              "name": "research-plugin",
              "version": "1.0",
              "tools": [{
                "name": "search",
                "description": "Research",
                "input_schema": {
                  "type": "object",
                  "properties": {"query": {"type": "string"}},
                  "x-octos-host-config-keys": ["synthesis_config"]
                }
              }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("research-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cfg = SynthesisConfig {
            endpoint: "https://api.example.com/v1".to_string(),
            api_key: "sk-loader-test".to_string(),
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
        };

        let (tools, _extras) = PluginLoader::load_plugin_with_options(
            &plugin_dir,
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: Some(cfg),
                require_signed: false,
                verified_cache_dir: None,
            },
        )
        .unwrap();

        assert_eq!(tools.len(), 1);
        // Inject through prepare_effective_args to verify the loader propagated
        // the config into the constructed PluginTool.
        let prepared = tools[0]
            .prepare_effective_args(&serde_json::json!({"query": "x"}), None)
            .unwrap();
        assert_eq!(prepared["synthesis_config"]["api_key"], "sk-loader-test");
    }

    /// Cancellation-safety (codex review of 7c3e5eac): a plugin manifest that
    /// declares `timeout_secs` LARGER than the registry's per-tool backstop
    /// (`MAX_TOOL_TIMEOUT_SECS` = 1800s) must be clamped at load. Otherwise the
    /// registry's dispatch-boundary `tokio::time::timeout` would preempt the
    /// plugin's own graceful kill branch — dropping the future before the
    /// plugin could process-group-kill its child.
    #[cfg(unix)]
    #[test]
    fn manifest_timeout_secs_is_clamped_to_registry_backstop() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("slow-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // 7200s (2h) — far above the 1800s registry backstop.
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
              "name": "slow-plugin",
              "version": "1.0",
              "timeout_secs": 7200,
              "tools": [{
                "name": "slow",
                "description": "Long runner",
                "input_schema": {"type": "object", "properties": {}}
              }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("slow-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tools, _extras) =
            PluginLoader::load_plugin_with_options(&plugin_dir, &[], PluginLoadOptions::default())
                .unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].timeout(),
            Duration::from_secs(MAX_TOOL_TIMEOUT_SECS),
            "manifest timeout_secs > MAX_TOOL_TIMEOUT_SECS must be clamped to the backstop \
             so the plugin's own kill path fires before the registry guard preempts it"
        );
    }

    /// A manifest timeout at or below the backstop is preserved verbatim.
    #[cfg(unix)]
    #[test]
    fn manifest_timeout_secs_below_backstop_is_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("brisk-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
              "name": "brisk-plugin",
              "version": "1.0",
              "timeout_secs": 120,
              "tools": [{
                "name": "brisk",
                "description": "Quick runner",
                "input_schema": {"type": "object", "properties": {}}
              }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("brisk-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (tools, _extras) =
            PluginLoader::load_plugin_with_options(&plugin_dir, &[], PluginLoadOptions::default())
                .unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].timeout(), Duration::from_secs(120));
    }

    #[cfg(unix)]
    #[test]
    fn load_into_with_options_skips_synthesis_config_for_non_opted_in_plugins() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("other-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // No x-octos-host-config-keys → should not receive synthesis_config.
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
              "name": "other-plugin",
              "version": "1.0",
              "tools": [{
                "name": "innocuous",
                "description": "Does not need credentials",
                "input_schema": {"type": "object"}
              }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("other-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cfg = SynthesisConfig {
            endpoint: "https://api.example.com/v1".to_string(),
            api_key: "sk-must-not-leak".to_string(),
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
        };

        let (tools, _extras) = PluginLoader::load_plugin_with_options(
            &plugin_dir,
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: Some(cfg),
                require_signed: false,
                verified_cache_dir: None,
            },
        )
        .unwrap();
        assert_eq!(tools.len(), 1);
        let prepared = tools[0]
            .prepare_effective_args(&serde_json::json!({}), None)
            .unwrap();
        assert!(
            prepared.get("synthesis_config").is_none(),
            "non-opted-in plugin must not see synthesis_config: {prepared}"
        );
    }

    /// M6 req 4: a manifest that declares an env allowlist entry whose
    /// name is a known process-hijack var (`LD_PRELOAD`) must be rejected
    /// at registration time so the malicious entry never reaches the
    /// runtime gate.
    #[cfg(unix)]
    #[test]
    fn loader_skips_tool_with_invalid_env_allowlist_entry() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("evil-env-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "evil-env-plugin",
                "version": "1.0",
                "tools": [
                    {"name": "good_tool", "description": "ok", "env": ["MY_VAR"], "input_schema": {"type": "object", "properties": {}}},
                    {"name": "bad_tool", "description": "bad", "env": ["LD_PRELOAD"], "input_schema": {"type": "object", "properties": {}}}
                ]
            }"#,
        )
        .unwrap();

        let exec_path = plugin_dir.join("evil-env-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        // good_tool registered, bad_tool skipped.
        assert_eq!(result.tool_count, 1);
        assert!(result.tool_names.contains(&"good_tool".to_string()));
        assert!(!result.tool_names.contains(&"bad_tool".to_string()));
    }

    /// Pin that registration-time validation rejects manifests with
    /// `env` entries containing `=` (a shell-injection vector).
    #[cfg(unix)]
    #[test]
    fn loader_skips_tool_with_equals_in_env_name() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("eq-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "eq-plugin",
                "version": "1.0",
                "tools": [{"name": "bad", "description": "d", "env": ["FOO=bar"], "input_schema": {"type": "object", "properties": {}}}]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("eq-plugin");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_plugin_id_first_dir_wins() {
        use std::os::unix::fs::PermissionsExt;

        let write_skill = |dir: &std::path::Path, marker: &str| {
            let plugin_dir = dir.join("shared-skill");
            std::fs::create_dir(&plugin_dir).unwrap();
            std::fs::write(
                plugin_dir.join("manifest.json"),
                format!(
                    r#"{{"name": "shared-skill", "version": "1.0",
                          "tools": [{{"name": "shared_tool",
                                     "description": "from-{marker}",
                                     "input_schema": {{"type": "object", "properties": {{}}}}}}]}}"#
                ),
            )
            .unwrap();
            let exec_path = plugin_dir.join("shared-skill");
            std::fs::write(
                &exec_path,
                format!("#!/bin/sh\necho '{{\"output\": \"{marker}\", \"success\": true}}'"),
            )
            .unwrap();
            std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };

        // dir_a = global-skills equivalent (corrected build).
        // dir_b = profile-scoped equivalent (stale shadow).
        // Loader receives [dir_a, dir_b] — first occurrence must win.
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        write_skill(dir_a.path(), "corrected");
        write_skill(dir_b.path(), "stale");

        let mut registry = ToolRegistry::new();
        let result = PluginLoader::load_into(
            &mut registry,
            &[dir_a.path().to_path_buf(), dir_b.path().to_path_buf()],
            &[],
        )
        .unwrap();

        assert_eq!(
            result.tool_count, 1,
            "duplicate plugin id should register only once"
        );
        assert_eq!(registry.len(), 1);
        let tool = registry.get_tool("shared_tool").expect("tool registered");
        assert_eq!(
            tool.description(),
            "from-corrected",
            "first dir (dir_a / corrected) must win — got the shadow copy"
        );
    }

    /// Regression: the verified-hash ledger must live OUTSIDE the skill
    /// source directory so that running `octos serve` as root (or any
    /// non-operator uid) does not taint the skill tree with foreign
    /// ownership and lock out the operator's interactive CLI. The cache
    /// is keyed by plugin name and lives under `verified_cache_dir`.
    ///
    /// Additionally pins the post-#1325 invariant: the cache holds only
    /// a `hash.txt` ledger entry, NOT a copy of the binary. PR #1319's
    /// original revision copied the bytes to `<cache>/<plugin>/main` and
    /// ran from there, which broke asset-bearing plugins like
    /// `mofa-slides` (their `<skill>/styles/` sibling directories did
    /// not exist under the cache parent).
    #[cfg(unix)]
    #[test]
    fn verified_hash_lives_under_cache_dir_not_skill_dir() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("isolated-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{"name": "isolated-plugin", "version": "1.0", "tools": [{"name": "iso_tool", "description": "d", "input_schema": {"type": "object", "properties": {}}}]}"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("isolated-plugin");
        std::fs::write(&exec_path, b"#!/bin/sh\necho hi").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cache_dir = dir.path().join("cache-root");
        let mut registry = ToolRegistry::new();
        PluginLoader::load_into_with_options(
            &mut registry,
            &[dir.path().to_path_buf()],
            &[],
            PluginLoadOptions {
                work_dir: None,
                synthesis_config: None,
                require_signed: false,
                verified_cache_dir: Some(cache_dir.clone()),
            },
        )
        .unwrap();

        // The verified-hash ledger must live under
        // cache_dir/<plugin>/hash.txt.
        let expected_hash = cache_dir.join("isolated-plugin").join("hash.txt");
        assert!(
            expected_hash.is_file(),
            "verified-hash ledger must live at {} (got nothing); cache_dir contents: {:?}",
            expected_hash.display(),
            std::fs::read_dir(&cache_dir).ok().map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .collect::<Vec<_>>()
            }),
        );

        // Post-#1325: the cache must NOT contain a `main` binary copy.
        let unwanted_binary = cache_dir.join("isolated-plugin").join("main");
        assert!(
            !unwanted_binary.exists(),
            "cache must not host a copy of the plugin binary (asset-resolution \
             regression #1325); found {}",
            unwanted_binary.display(),
        );

        // The skill source dir must NOT contain a `.isolated-plugin_verified`
        // or `.main_verified` sibling — that was the old taint vector.
        let skill_entries: Vec<_> = std::fs::read_dir(&plugin_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for name in &skill_entries {
            assert!(
                !name.ends_with("_verified") && name != ".main_verified",
                "skill source dir should not contain verified-copy file '{}' (full list: {:?})",
                name,
                skill_entries,
            );
        }
    }

    /// PR-F: the loader's `merge_extras` step folds duplicate
    /// `SKILL_EXPLORATION_PREAMBLE` strings across plugins so the
    /// system prompt only contains the preamble once even when N
    /// plugins each declare a `discovery` block. Distinct skill cards
    /// must survive — the dedup only collapses the constant string.
    #[test]
    fn merge_extras_dedupes_preamble_across_plugins() {
        let mut result = PluginLoadResult::default();

        // First plugin: preamble + card-one.
        let e1 = SkillExtras {
            mcp_servers: vec![],
            hooks: vec![],
            prompt_fragments: vec![
                SKILL_EXPLORATION_PREAMBLE.to_string(),
                "- name: card-one\n  purpose: a\n  tools: t1\n  skill_dir: /tmp/a".to_string(),
            ],
            spawn_only_tools: vec![],
            spawn_only_messages: Default::default(),
            make_type_entries: vec![],
        };
        result.merge_extras(e1);

        // Second plugin: preamble (duplicate) + card-two.
        let e2 = SkillExtras {
            mcp_servers: vec![],
            hooks: vec![],
            prompt_fragments: vec![
                SKILL_EXPLORATION_PREAMBLE.to_string(),
                "- name: card-two\n  purpose: b\n  tools: t2\n  skill_dir: /tmp/b".to_string(),
            ],
            spawn_only_tools: vec![],
            spawn_only_messages: Default::default(),
            make_type_entries: vec![],
        };
        result.merge_extras(e2);

        // Third plugin: preamble (duplicate) + card-three.
        let e3 = SkillExtras {
            mcp_servers: vec![],
            hooks: vec![],
            prompt_fragments: vec![
                SKILL_EXPLORATION_PREAMBLE.to_string(),
                "- name: card-three\n  purpose: c\n  tools: t3\n  skill_dir: /tmp/c".to_string(),
            ],
            spawn_only_tools: vec![],
            spawn_only_messages: Default::default(),
            make_type_entries: vec![],
        };
        result.merge_extras(e3);

        // Preamble appears exactly once across the merged result.
        let preamble_count = result
            .prompt_fragments
            .iter()
            .filter(|f| f.as_str() == SKILL_EXPLORATION_PREAMBLE)
            .count();
        assert_eq!(
            preamble_count, 1,
            "preamble must be deduped to a single occurrence; got {preamble_count} \
             in {:?}",
            result.prompt_fragments
        );

        // All three cards survive.
        assert!(
            result
                .prompt_fragments
                .iter()
                .any(|f| f.contains("name: card-one"))
        );
        assert!(
            result
                .prompt_fragments
                .iter()
                .any(|f| f.contains("name: card-two"))
        );
        assert!(
            result
                .prompt_fragments
                .iter()
                .any(|f| f.contains("name: card-three"))
        );

        // Length: 1 preamble + 3 cards = 4 fragments total.
        assert_eq!(
            result.prompt_fragments.len(),
            4,
            "expected 4 fragments (1 preamble + 3 cards); got {:?}",
            result.prompt_fragments
        );
    }

    /// RFC-1 fixup (codex P1): the `mofa_make` dispatcher's
    /// `Weak<ToolRegistry>` back-reference must be wired centrally so
    /// every plugin-loading path — chat, gateway, serve, spawn child
    /// registries, pipeline node agents — produces a dispatcher that
    /// can actually resolve its forwarding target. Pre-fixup, only
    /// callers that explicitly called `agent.wire_mofa_make_dispatcher()`
    /// got a working dispatcher; chat (`SessionActor::process_chat`),
    /// `SpawnTool` child registries, and pipeline node agents skipped
    /// that wiring and every `mofa_make` call returned
    /// `[DISPATCHER_ERROR]`.
    ///
    /// This test mints a real plugin with a `make_type` declaration,
    /// runs it through `PluginLoader::load_into` (the same entry-point
    /// every host uses), then constructs an `Agent` via `Agent::new`
    /// (the typical chat-session entry-point). Without the central
    /// wire in `Agent::new`, the dispatcher's `Weak` would be empty
    /// and dispatch would surface `[DISPATCHER_ERROR]`. With the fix,
    /// the wire happens automatically and the dispatcher forwards to
    /// the registered target.
    #[cfg(unix)]
    #[tokio::test]
    async fn mofa_make_dispatcher_wired_in_chat_session_path() {
        use std::os::unix::fs::PermissionsExt;

        use crate::agent::Agent;
        use crate::tools::MofaMakeTool;
        use async_trait::async_trait;
        use octos_core::AgentId;
        use octos_llm::{ChatResponse, LlmProvider, ToolSpec};
        use octos_memory::EpisodeStore;

        // Minimal LlmProvider stub so we can construct an Agent without
        // pulling in a real backend. `chat` is never called by this test.
        struct NoopLlm;
        #[async_trait]
        impl LlmProvider for NoopLlm {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                eyre::bail!("not exercised in this test")
            }
            fn model_id(&self) -> &str {
                "mock"
            }
            fn provider_name(&self) -> &str {
                "noop"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-slides");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Manifest declares `make_type: "slides"` so the loader's RFC-1
        // path mints a dispatcher + describe pair and routes the
        // `mofa_slides` target through it.
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "mofa-slides",
                "version": "1.0",
                "make_type": "slides",
                "content_type_description": "PPTX decks",
                "tools": [{
                    "name": "mofa_slides",
                    "description": "Render slides",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("mofa-slides");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"slides ready\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Load via the standard `load_into` entry-point. The loader
        // registers `mofa_slides`, mints `mofa_make` + `mofa_describe_content_type`,
        // and hides the target. The Weak<ToolRegistry> is NOT yet set —
        // that's `Agent::new`'s job in this fix.
        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert!(
            result.tool_names.iter().any(|n| n == "mofa_make"),
            "loader must register mofa_make dispatcher for make_type plugins; got {:?}",
            result.tool_names
        );

        // Confirm the dispatcher's Weak is empty BEFORE Agent::new
        // (precondition; locks in the failing path).
        let pre_arc = std::sync::Arc::new(registry);
        let pre_dispatcher = pre_arc
            .get("mofa_make")
            .and_then(|t| t.as_any().downcast_ref::<MofaMakeTool>())
            .expect("mofa_make registered");
        let exec_result = pre_dispatcher
            .execute(&serde_json::json!({
                "content_type": "slides",
                "args": {}
            }))
            .await
            .unwrap();
        assert!(
            exec_result.output.contains("[DISPATCHER_ERROR]"),
            "pre-Agent::new dispatcher must NOT be wired; got {:?}",
            exec_result.output
        );

        // Now exercise the central wire via `Agent::new`. Use a fresh
        // load + a fresh `Agent::new` so we measure THIS fix in
        // isolation (the previous `pre_arc` was a sanity probe).
        let mut registry = ToolRegistry::new();
        let _ = PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        let memory = std::sync::Arc::new(
            EpisodeStore::open(dir.path().join("episodes"))
                .await
                .expect("episode store"),
        );
        let llm: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(NoopLlm);
        let agent = Agent::new(AgentId::new("test-chat-session"), llm, registry, memory);

        // After `Agent::new`, the dispatcher must be able to upgrade
        // its Weak<ToolRegistry> and reach the target tool. With the
        // central wire in `Agent::new`, this dispatch resolves.
        let dispatcher = agent
            .tool_registry()
            .get("mofa_make")
            .and_then(|t| t.as_any().downcast_ref::<MofaMakeTool>())
            .expect("mofa_make registered after Agent::new");
        let result = dispatcher
            .execute(&serde_json::json!({
                "content_type": "slides",
                "args": {}
            }))
            .await
            .unwrap();
        assert!(
            !result.output.contains("[DISPATCHER_ERROR]"),
            "post-Agent::new dispatcher must be wired (no DISPATCHER_ERROR); got {:?}",
            result.output
        );
    }

    /// RFC-1 (issue #1290) + RFC-0 (#1289): when a `make_type` skill is
    /// loaded, the resolved target tool (e.g. `mofa_slides`) is registered
    /// (callable via `get()` so the dispatcher can forward to it) but
    /// hidden from the LLM-visible `specs()` set. The LLM only ever sees
    /// the `mofa_make` dispatcher.
    #[cfg(unix)]
    #[test]
    fn mofa_make_targets_hidden_from_specs_but_callable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-slides");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "mofa-slides",
                "version": "1.0",
                "make_type": "slides",
                "content_type_description": "PPTX decks",
                "tools": [{
                    "name": "mofa_slides",
                    "description": "Render slides",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("mofa-slides");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let _ = PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();

        // Sanity: the target tool IS registered (callable via get()),
        // just hidden from the LLM-facing spec set.
        assert!(
            registry.get("mofa_slides").is_some(),
            "mofa_slides target tool must remain registered for dispatcher forwarding"
        );

        // The LLM-facing spec set MUST NOT include the target tool.
        let visible: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert!(
            !visible.contains(&"mofa_slides".to_string()),
            "mofa_slides must NOT appear in LLM-visible specs after RFC-1 \
             internal-hidden registration; got {:?}",
            visible
        );

        // The dispatcher itself IS visible.
        assert!(
            visible.contains(&"mofa_make".to_string()),
            "mofa_make dispatcher must be visible; got {:?}",
            visible
        );
    }

    /// RFC-1 fixup (codex round 3 P2): when an owned `ToolRegistry`
    /// arrives at `Agent::new` carrying a SHARED dispatcher Arc (e.g.
    /// from `octos-pipeline`'s cached plugin registration that
    /// `register_arc`s the same `Arc<MofaMakeTool>` into every node
    /// registry), the central wire MUST mint a fresh dispatcher
    /// before wiring its `Weak<ToolRegistry>` — otherwise that wire
    /// would mutate the shared dispatcher and overlapping pipeline
    /// nodes would race on its `Mutex<Weak>`.
    ///
    /// This test simulates the pipeline pattern: build a shared
    /// `Arc<MofaMakeTool>`, `register_arc` it into TWO independent
    /// `ToolRegistry` instances, construct two `Agent`s, and assert
    /// the two agents' dispatchers are SEPARATE Arc objects (so
    /// wiring one's Weak doesn't touch the other's).
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_new_mints_fresh_dispatcher_when_input_is_shared() {
        use std::os::unix::fs::PermissionsExt;

        use crate::agent::Agent;
        use crate::tools::MofaMakeTool;
        use async_trait::async_trait;
        use octos_core::AgentId;
        use octos_llm::{ChatResponse, LlmProvider, ToolSpec};
        use octos_memory::EpisodeStore;

        struct NoopLlm;
        #[async_trait]
        impl LlmProvider for NoopLlm {
            async fn chat(
                &self,
                _messages: &[octos_core::Message],
                _tools: &[ToolSpec],
                _config: &octos_llm::ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                eyre::bail!("unused")
            }
            fn model_id(&self) -> &str {
                "mock"
            }
            fn provider_name(&self) -> &str {
                "noop"
            }
        }

        // Build a make_type plugin once.
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-slides");
        std::fs::create_dir(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
                "name": "mofa-slides",
                "version": "1.0",
                "make_type": "slides",
                "content_type_description": "PPTX decks",
                "tools": [{
                    "name": "mofa_slides",
                    "description": "Render slides",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        )
        .unwrap();
        let exec_path = plugin_dir.join("mofa-slides");
        std::fs::write(
            &exec_path,
            "#!/bin/sh\necho '{\"output\": \"ok\", \"success\": true}'",
        )
        .unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Stage a registry once and pull out the dispatcher `Arc` so
        // we can `register_arc` it into multiple per-node registries
        // (mirrors `octos-pipeline::CachedPluginRegistration::apply_to`).
        let mut staging = ToolRegistry::new();
        let _ = PluginLoader::load_into(&mut staging, &[dir.path().to_path_buf()], &[]).unwrap();
        let shared_dispatcher_arc = staging
            .get_tool("mofa_make")
            .expect("staging mofa_make registered");

        // Build TWO independent registries that share the same
        // dispatcher Arc — the pre-fix hazard.
        let make_registry = || -> ToolRegistry {
            let mut reg = ToolRegistry::new();
            // Register the target tool so the dispatcher's catalog
            // entry has somewhere to forward to.
            let target = staging
                .get_tool("mofa_slides")
                .expect("staging mofa_slides registered");
            reg.register_arc(target);
            reg.register_arc(shared_dispatcher_arc.clone());
            reg
        };
        let registry_a = make_registry();
        let registry_b = make_registry();

        // Construct two agents through `Agent::new`. With the round-3
        // fixup each call mints a FRESH dispatcher for that agent.
        let memory_a = std::sync::Arc::new(
            EpisodeStore::open(dir.path().join("episodes-a"))
                .await
                .expect("episode store"),
        );
        let memory_b = std::sync::Arc::new(
            EpisodeStore::open(dir.path().join("episodes-b"))
                .await
                .expect("episode store"),
        );
        let llm: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::new(NoopLlm);
        let agent_a = Agent::new(AgentId::new("a"), llm.clone(), registry_a, memory_a);
        let agent_b = Agent::new(AgentId::new("b"), llm, registry_b, memory_b);

        // Identity assertion: the two agents' dispatchers are separate
        // Arc objects. If the freshen step were skipped, both would
        // point at `shared_dispatcher_arc` and wiring one's Weak
        // would mutate the other's.
        let disp_a = agent_a.tool_registry().get("mofa_make").cloned().unwrap();
        let disp_b = agent_b.tool_registry().get("mofa_make").cloned().unwrap();
        let shared_ptr = std::sync::Arc::as_ptr(&shared_dispatcher_arc) as *const ();
        let a_ptr = std::sync::Arc::as_ptr(&disp_a) as *const ();
        let b_ptr = std::sync::Arc::as_ptr(&disp_b) as *const ();
        assert_ne!(
            a_ptr, shared_ptr,
            "Agent::new must mint a fresh dispatcher for agent A"
        );
        assert_ne!(
            b_ptr, shared_ptr,
            "Agent::new must mint a fresh dispatcher for agent B"
        );
        assert_ne!(
            a_ptr, b_ptr,
            "agent A and agent B must have SEPARATE dispatcher Arcs \
             (share-mutate hazard would have produced the same Arc)"
        );

        // Each agent's dispatcher must resolve through its OWN
        // registry (proving its Weak points at the right place).
        let disp_a_typed = disp_a
            .as_any()
            .downcast_ref::<MofaMakeTool>()
            .expect("downcast");
        let result_a = disp_a_typed
            .execute(&serde_json::json!({
                "content_type": "slides",
                "args": {}
            }))
            .await
            .unwrap();
        // The target IS in agent A's registry (registered above), so
        // dispatch should succeed (no DISPATCHER_ERROR for missing
        // registry or missing target).
        assert!(
            !result_a.output.contains("[DISPATCHER_ERROR]"),
            "agent A's dispatcher must resolve through agent A's \
             registry; got {:?}",
            result_a.output
        );
    }
}
