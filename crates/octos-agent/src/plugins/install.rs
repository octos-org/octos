//! Install-time and runtime-startup async wrappers around the
//! (otherwise-sync) [`PluginLoader`]. PR #1260 split this off from
//! `loader.rs` so the loader can remain sync (matching upstream's
//! hash-ledger cache) while the SPEC-VENDOR-NODE-V1 HTTP discovery
//! path stays async (catalog fetch).
//!
//! Three entry points:
//!
//! - [`activate_skill`] — used by `octos skills install` to run a
//!   skill's hardware lifecycle (preflight → init → ready_check) and
//!   then register its tools (Static binary-protocol OR HTTP-discovered).
//! - [`run_shutdown_phase`] — used by `octos skills remove` to fire
//!   the optional `shutdown` lifecycle steps before deletion.
//! - [`register_http_skills_on_startup`] — used by every agent boot
//!   path (chat / gateway / serve) AFTER the sync `PluginLoader::load_into`
//!   has registered the static skills; walks each plugin dir for
//!   `tool_discovery: { type: "http", base_url: "..." }` manifests
//!   and registers their HTTP-backed tools.
//!
//! Finding 2 (PR #1260 review): each HTTP-discovery call site
//! `?`-propagates the catalog error so a missing or broken bridge
//! turns into a load failure rather than a silent zero-tools install.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr, eyre};
use octos_plugin::{LifecycleExecutor, LifecyclePhase, NoSandbox, ToolDiscovery};
use tracing::{info, warn};

use crate::tools::{Tool, ToolRegistry};

use super::http_discovery::install_http_tools_from_catalog;
use super::loader::PluginLoader;
use super::manifest::PluginManifest;

/// Result of activating a single skill from a directory.
#[derive(Debug, Default)]
pub struct SkillActivateResult {
    /// Names of all tools registered (static or HTTP-discovered).
    pub tool_names: Vec<String>,
}

/// Run hardware lifecycle phases (preflight → init → ready_check) and then
/// register the skill's tools (static binary-protocol or HTTP-discovered)
/// into `registry`.
///
/// This is an **async** entry point designed for use by the CLI install
/// path and by tests. It does NOT copy files — the skill directory must
/// already exist on disk.
///
/// Returns an error (and registers **no** tools) if any critical lifecycle
/// phase fails or if HTTP tool discovery returns a non-2xx response.
pub async fn activate_skill(
    registry: &mut ToolRegistry,
    skill_dir: &Path,
    extra_env: &[(String, String)],
) -> Result<SkillActivateResult> {
    let manifest_path = skill_dir.join("manifest.json");
    if !manifest_path.exists() {
        // No manifest → nothing to activate. Skills without manifest.json
        // are app-level docs only and have no tools or lifecycle.
        return Ok(SkillActivateResult { tool_names: vec![] });
    }
    let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
        eyre!(
            "could not read manifest.json in {}: {e}",
            skill_dir.display()
        )
    })?;
    let manifest: PluginManifest = serde_json::from_str(&content)
        .map_err(|e| eyre!("invalid manifest.json in {}: {e}", skill_dir.display()))?;

    let skill_name = manifest.name.clone();

    // 1) Run hardware_lifecycle phases if present.
    if let Some(lifecycle) = manifest.hardware_lifecycle.as_ref() {
        let executor = LifecycleExecutor::new(Box::new(NoSandbox), skill_dir.to_path_buf());
        for (phase, steps) in [
            (LifecyclePhase::Preflight, &lifecycle.preflight),
            (LifecyclePhase::Init, &lifecycle.init),
            (LifecyclePhase::ReadyCheck, &lifecycle.ready_check),
        ] {
            if steps.is_empty() {
                continue;
            }
            let result = executor.run_phase(phase, steps).await;
            if !result.success {
                return Err(eyre!(
                    "skill {skill_name} {} failed: {}",
                    phase.as_str(),
                    result.error.unwrap_or_default()
                ));
            }
            info!(
                skill = %skill_name,
                phase = phase.as_str(),
                "lifecycle phase completed"
            );
        }
    }

    // 2) Tool discovery (static binary-protocol is the existing path;
    //    Http triggers GET <base_url>/tools registration).
    let mut tool_names = Vec::new();
    match &manifest.tool_discovery {
        ToolDiscovery::Static => {
            // Important #2 fix from PR #1260 review: load ONLY this skill,
            // not its parent directory. The previous `load_into(&[parent])`
            // scanned every sibling skill folder as a side-effect, leaking
            // their tools into the registry on install.
            //
            // `extras` (MCP servers, hooks, prompts) are intentionally
            // NOT resolved here — `activate_skill`'s contract is tool
            // registration only; extras flow through
            // `PluginLoader::load_into_with_options` at runtime startup.
            //
            // Error handling: hard-fail on every `load_plugin` error EXCEPT
            // "no executable found", which is a legitimate "tools declared
            // in the manifest but binary deferred" case (matches upstream's
            // RFC-2 valid-manifest install flow: copy files now, register
            // tools later at agent boot). Integrity errors (sha256 mismatch,
            // oversized binary, `require_signed` rejection) MUST propagate
            // so install rolls back instead of silently parking a broken
            // skill on disk. Reviewer post-merge feedback (Critical A).
            match PluginLoader::load_plugin(skill_dir, extra_env) {
                Ok((tools, _extras)) => {
                    for tool in tools {
                        let name = tool.name().to_string();
                        registry.mark_as_plugin(&name);
                        tool_names.push(name);
                        registry.register(tool);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("no executable found") {
                        // Manifest declares `tools: [...]` but ships no
                        // binary — typical for RFC-2 docs-first skills.
                        // Tool registration will happen at agent boot if
                        // a binary appears later; otherwise the skill
                        // legitimately has no static tools.
                        info!(
                            skill = %skill_name,
                            "skill has no executable; static tool registration deferred to agent boot"
                        );
                    } else {
                        return Err(e).wrap_err_with(|| {
                            format!(
                                "skill {skill_name} static load_plugin failed; install must roll back"
                            )
                        });
                    }
                }
            }
        }
        ToolDiscovery::Http { base_url } => {
            // Finding 2 (PR #1260 review): hard-fail on HTTP discovery
            // error rather than silently registering zero tools.
            let names = install_http_tools_from_catalog(registry, base_url, Some(&manifest))
                .await
                .wrap_err_with(|| {
                    format!("skill {skill_name} HTTP tool discovery from {base_url} failed")
                })?;
            info!(
                skill = %skill_name,
                base_url = %base_url,
                count = names.len(),
                "HTTP tool discovery completed"
            );
            tool_names.extend(names);
        }
    }

    Ok(SkillActivateResult { tool_names })
}

/// Run the `shutdown` lifecycle phase for a skill before uninstall.
///
/// Best-effort: failures are logged but do not abort the uninstall (since
/// the alternative is leaving the skill directory on disk, which is worse).
/// No-op when the manifest has no `hardware_lifecycle` block.
pub async fn run_shutdown_phase(skill_dir: &Path) {
    let manifest_path = skill_dir.join("manifest.json");
    if !manifest_path.exists() {
        return;
    }
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(skill_dir = %skill_dir.display(), error = %e, "shutdown: could not read manifest");
            return;
        }
    };
    let manifest: PluginManifest = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            warn!(skill_dir = %skill_dir.display(), error = %e, "shutdown: invalid manifest");
            return;
        }
    };

    if let Some(lifecycle) = manifest.hardware_lifecycle.as_ref() {
        if lifecycle.shutdown.is_empty() {
            return;
        }
        let executor = LifecycleExecutor::new(Box::new(NoSandbox), skill_dir.to_path_buf());
        let result = executor
            .run_phase(LifecyclePhase::Shutdown, &lifecycle.shutdown)
            .await;
        if !result.success {
            warn!(
                skill = %manifest.name,
                error = ?result.error,
                "shutdown phase failed (best-effort, continuing removal)"
            );
        } else {
            info!(skill = %manifest.name, "shutdown phase completed");
        }
    }
}

/// Walk `plugin_dirs` for skills declaring `tool_discovery: { type: "http", ... }`
/// and register their HTTP-backed tools into `registry`.
///
/// Called from the agent boot path (chat / gateway / serve) AFTER the
/// sync [`PluginLoader::load_into`] has registered the static skills.
/// Finding 2 (PR #1260 review): the first HTTP discovery failure hard-
/// fails the whole boot — an installed HTTP-backed skill with a missing
/// bridge must not register zero tools and pretend to succeed.
pub async fn register_http_skills_on_startup(
    registry: &mut ToolRegistry,
    plugin_dirs: &[PathBuf],
) -> Result<()> {
    for dir in plugin_dirs {
        if !dir.exists() {
            continue;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(plugin_dir = %dir.display(), error = %e, "could not read plugin dir for HTTP discovery");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&manifest_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let manifest: PluginManifest = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let ToolDiscovery::Http { base_url } = &manifest.tool_discovery {
                let names = install_http_tools_from_catalog(registry, base_url, Some(&manifest))
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "HTTP tool discovery failed for skill at {} (bridge {})",
                            path.display(),
                            base_url
                        )
                    })?;
                info!(
                    plugin_dir = %path.display(),
                    base_url = %base_url,
                    count = names.len(),
                    "registered HTTP-discovered tools at startup",
                );
            }
        }
    }
    Ok(())
}
