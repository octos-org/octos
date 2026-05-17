//! Plugin loader: scans directories for plugins and registers their tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::Result;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::hooks::HookConfig;
use crate::mcp::McpServerConfig;
use crate::sandbox::BLOCKED_ENV_VARS;
use crate::tools::{Tool, ToolRegistry};

use super::extras::{SkillExtras, resolve_extras};
use super::manifest::{ConcurrencyClassClassification, PluginManifest, PluginToolDef};
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
    /// MCP server configs resolved from skill manifests.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Hook configs resolved from skill manifests.
    pub hooks: Vec<HookConfig>,
    /// Prompt fragments read from skill directories.
    pub prompt_fragments: Vec<String>,
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
}

impl PluginLoadResult {
    fn merge_extras(&mut self, extras: SkillExtras) {
        self.mcp_servers.extend(extras.mcp_servers);
        self.hooks.extend(extras.hooks);
        self.prompt_fragments.extend(extras.prompt_fragments);
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
        let discovered = octos_plugin::discover_plugins(&sources, &extra_env_map);

        for plugin in discovered {
            let path = plugin.path;
            // Re-parse via the agent-side manifest type below: octos_plugin's
            // PluginManifest is a structural subset and doesn't model
            // mcp_servers / hooks / prompts / spawn_only. Discovery has
            // already filtered for `manifest.json` presence, so we skip
            // re-checking and head straight into the rich load path.
            match Self::load_plugin_with_options_and_risks(&path, extra_env, options.clone()) {
                Ok((tools, extras)) => {
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
                    result.merge_extras(extras);
                }
                Err(e) => {
                    warn!(
                        plugin_dir = %path.display(),
                        error = %e,
                        "failed to load plugin, skipping"
                    );
                }
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
        let (tools, extras) =
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
    ) -> Result<(Vec<LoadedPluginTool>, SkillExtras)> {
        let work_dir = options.work_dir;
        let synthesis_config = options.synthesis_config;
        let require_signed = options.require_signed;
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
            return Ok((vec![], extras));
        }

        if find_plugin_executable(plugin_dir, &manifest.name).is_none() {
            let _ = ensure_plugin_executable_for_manifest(plugin_dir, &manifest)?;
        }

        let executable = find_plugin_executable(plugin_dir, &manifest.name).ok_or_else(|| {
            let dir_name = plugin_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("main");
            eyre::eyre!(
                "no executable found in plugin '{}' (tried '{}', '{}', 'main', and directory scan)",
                manifest.name,
                manifest.name,
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

        // Read executable content once for hash verification AND to write a
        // verified copy. This closes the TOCTOU gap: we hash the bytes we
        // read, then write those same bytes to a verified path that PluginTool
        // will execute. The original file can't be swapped after verification.
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

        // Write verified bytes to a sibling file so PluginTool executes
        // exactly what we hashed (prevents TOCTOU file swap attacks).
        let verified_exe = plugin_dir.join(format!(
            ".{}_verified",
            executable.file_name().unwrap_or_default().to_string_lossy()
        ));
        // Remove existing verified file first so we can refresh the copy on restart.
        let _ = std::fs::remove_file(&verified_exe);
        std::fs::write(&verified_exe, &exe_bytes)
            .map_err(|e| eyre::eyre!("cannot write verified executable: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Keep the verified copy executable by the runtime user even when
            // the skill directory itself is root-owned.
            std::fs::set_permissions(&verified_exe, std::fs::Permissions::from_mode(0o755))?;
        }

        // Collect env vars to filter out
        let blocked_env: Vec<String> = BLOCKED_ENV_VARS.iter().map(|s| s.to_string()).collect();

        let timeout = manifest
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(PluginTool::DEFAULT_TIMEOUT);

        // Collect spawn_only tool names and messages before consuming
        // manifest.tools. Tools that fail manifest validation below are
        // skipped — drop them from the spawn_only metadata too so the
        // execution loop doesn't try to auto-background a tool that was
        // never registered.
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

        let plugin_name = manifest.name.clone();
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
                let mut tool = PluginTool::new(plugin_name.clone(), def, verified_exe.clone())
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

        // Return extras with spawn_only info
        extras.spawn_only_tools = spawn_only_names;
        extras.spawn_only_messages = spawn_only_msgs;

        Ok((tools, extras))
    }
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
    if find_plugin_executable(plugin_dir, &manifest.name).is_some() {
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

    // mofa-publish: shell-script skill with JSON-over-stdin plugin protocol.
    if manifest.name == "mofa-publish"
        && manifest
            .tools
            .iter()
            .any(|tool| tool.name == "mofa_publish")
        && plugin_dir.join("scripts/publish_site.sh").exists()
    {
        write_executable_wrapper(&main_path, mofa_publish_wrapper_script())?;
        info!(
            plugin = %manifest.name,
            executable = %main_path.display(),
            "generated fallback executable wrapper"
        );
        return Ok(true);
    }

    // mofa-site: scaffold helper scripts routed through a thin wrapper.
    if manifest.name == "mofa-site"
        && manifest.tools.iter().any(|tool| tool.name == "mofa_site")
        && plugin_dir
            .join("scripts/bootstrap_quarto_lesson.sh")
            .exists()
        && plugin_dir.join("scripts/bootstrap_template.sh").exists()
    {
        write_executable_wrapper(&main_path, mofa_site_wrapper_script())?;
        info!(
            plugin = %manifest.name,
            executable = %main_path.display(),
            "generated fallback executable wrapper"
        );
        return Ok(true);
    }

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

fn mofa_publish_wrapper_script() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOOL="${1:-}"

if [[ "$TOOL" != "mofa_publish" ]]; then
  printf '{"output":"Unknown tool: %s","success":false}\n' "$TOOL"
  exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
  printf '{"output":"python3 is required to run mofa-publish.","success":false}\n'
  exit 0
fi

INPUT="$(cat)"
OCTOS_PLUGIN_INPUT="$INPUT" python3 - "$SCRIPT_DIR/scripts/publish_site.sh" <<'PY'
import json
import os
import subprocess
import sys

script_path = sys.argv[1]
raw = (os.environ.get("OCTOS_PLUGIN_INPUT") or "").strip() or "{}"
try:
    payload = json.loads(raw)
except Exception as exc:
    print(f'{{"output":"invalid JSON input: {exc}","success":false}}')
    sys.exit(0)

cmd = ["bash", script_path]

def add_value(key: str, flag: str) -> None:
    value = payload.get(key)
    if value is None:
        return
    if isinstance(value, bool):
        if value:
            cmd.append(flag)
        return
    text = str(value).strip()
    if text:
        cmd.extend([flag, text])

add_value("site_dir", "--site-dir")
add_value("target", "--target")
add_value("slug", "--slug")
add_value("repo", "--repo")
add_value("repo_root", "--repo-root")
add_value("mini_host", "--mini-host")
add_value("mini_user", "--mini-user")
add_value("ssh_key", "--ssh-key")
add_value("ssh_password_env", "--ssh-password-env")
add_value("ssh_port", "--ssh-port")
add_value("remote_root", "--remote-root")
add_value("cname", "--cname")
add_value("setup_ci", "--setup-ci")

proc = subprocess.run(cmd)
sys.exit(proc.returncode)
PY
"#
}

fn mofa_site_wrapper_script() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOOL="${1:-}"

if [[ "$TOOL" != "mofa_site" ]]; then
  printf '{"output":"Unknown tool: %s","success":false}\n' "$TOOL"
  exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
  printf '{"output":"python3 is required to run mofa-site.","success":false}\n'
  exit 0
fi

INPUT="$(cat)"
OCTOS_PLUGIN_INPUT="$INPUT" python3 - \
  "$SCRIPT_DIR/scripts/bootstrap_quarto_lesson.sh" \
  "$SCRIPT_DIR/scripts/bootstrap_template.sh" <<'PY'
import json
import os
import subprocess
import sys

quarto_script = sys.argv[1]
template_script = sys.argv[2]
raw = (os.environ.get("OCTOS_PLUGIN_INPUT") or "").strip() or "{}"
try:
    payload = json.loads(raw)
except Exception as exc:
    print(f'{{"output":"invalid JSON input: {exc}","success":false}}')
    sys.exit(0)

template = str(payload.get("template") or "quarto-lesson").strip() or "quarto-lesson"
title = str(payload.get("title") or "Generated Site").strip() or "Generated Site"
content_dir = payload.get("content_dir")
out_dir = payload.get("out_dir")
if not out_dir:
    if isinstance(content_dir, str) and content_dir.strip():
        out_dir = os.path.join(content_dir, "site")
    else:
        out_dir = "skill-output/mofa-site"

language = payload.get("language")
theme = payload.get("theme")
description = payload.get("description")

if template == "quarto-lesson":
    cmd = ["bash", quarto_script, "--out-dir", str(out_dir), "--title", title]
    if isinstance(description, str) and description.strip():
        cmd.extend(["--description", description.strip()])
    if isinstance(theme, str) and theme.strip():
        cmd.extend(["--theme", theme.strip()])
    if isinstance(language, str) and language.strip():
        cmd.extend(["--language", language.strip()])
else:
    cmd = [
        "bash",
        template_script,
        "--template",
        template,
        "--out-dir",
        str(out_dir),
        "--site-name",
        title,
    ]
    if isinstance(description, str) and description.strip():
        cmd.extend(["--description", description.strip()])
    if isinstance(language, str) and language.strip():
        cmd.extend(["--locale", language.strip()])

proc = subprocess.run(cmd)
sys.exit(proc.returncode)
PY
"#
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
    use std::io::Write;

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
    fn test_load_plugin_with_manifest() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("my-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        // Write manifest
        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{"name": "my-plugin", "version": "1.0", "tools": [{"name": "greet", "description": "Greet someone"}]}"#,
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
            r#"{{"name": "hash-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "t", "description": "d"}}]}}"#
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

        let manifest = r#"{"name": "bad-hash", "version": "1.0", "sha256": "0000000000000000000000000000000000000000000000000000000000000000", "tools": [{"name": "t", "description": "d"}]}"#;
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
            "tools": [{"name": "t", "description": "d"}]
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
            r#"{{"name": "signed-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "t", "description": "d"}}]}}"#
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
                "tools": [{{"name": "t", "description": "d"}}]
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
                    "spawn_only": true
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
                "tools": [{{"name": "t2", "description": "d"}}]
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
            "tools": [{"name": "t", "description": "d"}]
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

    /// Section C: when the verified-exe bytes on disk are swapped between
    /// load and invocation, the pre-spawn re-hash gate refuses to spawn the
    /// process. We simulate the swap by overwriting `.<name>_verified`
    /// after `load_into` returns.
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
            r#"{{"name": "swap-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "swap_tool", "description": "d"}}]}}"#
        );
        std::fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        let exec_path = plugin_dir.join("swap-plugin");
        std::fs::write(&exec_path, exec_content).unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 1);

        // Swap the verified-exe bytes on disk so the re-hash gate fires.
        let verified_exe = plugin_dir.join(".swap-plugin_verified");
        assert!(
            verified_exe.exists(),
            "loader must write a verified-exe sibling"
        );
        std::fs::write(&verified_exe, b"#!/bin/sh\necho TAMPERED").unwrap();

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
            r#"{{"name": "manifest-swap", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "ms_tool", "description": "d"}}]}}"#
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
            r#"{{"name": "intact-plugin", "version": "1.0", "sha256": "{hash}", "tools": [{{"name": "intact_tool", "description": "d"}}]}}"#
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
            r#"{"name": "evil-plugin", "version": "1.0", "tools": [{"name": "evil", "description": "d"}]}"#,
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
                        {{"name": "{declared_tool}", "description": "declared", "risk": "medium"}},
                        {{"name": "{missing_tool}", "description": "missing first", "risk": "high"}},
                        {{"name": "{blank_tool}", "description": "blank first", "risk": "high"}}
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
                        {{"name": "{missing_tool}", "description": "missing second"}},
                        {{"name": "{blank_tool}", "description": "blank second", "risk": "   "}}
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
    fn test_loader_bootstraps_script_skill_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-publish");
        std::fs::create_dir_all(plugin_dir.join("scripts")).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-publish",
  "version": "0.1.0",
  "tools": [{"name": "mofa_publish", "description": "deploy"}]
}"#,
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("scripts/publish_site.sh"),
            "#!/usr/bin/env bash\nset -euo pipefail\necho \"publish:$*\"\n",
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
  "tools": [{"name": "podcast_generate", "description": "podcast"}]
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
    fn test_mofa_publish_wrapper_executes_script() {
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("mofa-publish");
        std::fs::create_dir_all(plugin_dir.join("scripts")).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "mofa-publish",
  "version": "0.1.0",
  "tools": [{"name": "mofa_publish", "description": "deploy"}]
}"#,
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("scripts/publish_site.sh"),
            "#!/usr/bin/env bash\nset -euo pipefail\necho \"publish:$*\"\n",
        )
        .unwrap();

        ensure_plugin_executable(&plugin_dir).unwrap();
        let mut child = Command::new(plugin_dir.join("main"))
            .arg("mofa_publish")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(br#"{"site_dir":"./docs","slug":"demo","setup_ci":true}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(stdout.contains("--site-dir ./docs"));
        assert!(stdout.contains("--slug demo"));
        assert!(stdout.contains("--setup-ci"));
    }

    #[cfg(unix)]
    #[test]
    fn test_verified_executable_is_world_executable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("perm-plugin");
        std::fs::create_dir(&plugin_dir).unwrap();

        std::fs::write(
            plugin_dir.join("manifest.json"),
            r#"{
  "name": "perm-plugin",
  "version": "0.1.0",
  "tools": [{"name": "perm_tool", "description": "perm"}]
}"#,
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("perm-plugin"),
            "#!/usr/bin/env bash\nset -euo pipefail\necho '{\"output\":\"ok\",\"success\":true}'\n",
        )
        .unwrap();
        std::fs::set_permissions(
            plugin_dir.join("perm-plugin"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let mut registry = ToolRegistry::new();
        let result =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(result.tool_count, 1);

        let verified = plugin_dir.join(".perm-plugin_verified");
        let mode = std::fs::metadata(&verified).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
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
            },
        )
        .unwrap();

        assert_eq!(tools.len(), 1);
        // Inject through prepare_effective_args to verify the loader propagated
        // the config into the constructed PluginTool.
        let prepared = tools[0].prepare_effective_args(&serde_json::json!({"query": "x"}), None);
        assert_eq!(prepared["synthesis_config"]["api_key"], "sk-loader-test");
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
            },
        )
        .unwrap();
        assert_eq!(tools.len(), 1);
        let prepared = tools[0].prepare_effective_args(&serde_json::json!({}), None);
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
                    {"name": "good_tool", "description": "ok", "env": ["MY_VAR"]},
                    {"name": "bad_tool", "description": "bad", "env": ["LD_PRELOAD"]}
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
                "tools": [{"name": "bad", "description": "d", "env": ["FOO=bar"]}]
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
                                     "description": "from-{marker}"}}]}}"#
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
}
