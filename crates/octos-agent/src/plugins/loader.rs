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
use super::manifest::PluginManifest;
use super::tool::PluginTool;

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
    /// `extra_env` is injected into every plugin process (e.g. provider base URLs, API keys).
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
        let mut result = PluginLoadResult::default();

        for dir in dirs {
            if !dir.exists() {
                continue;
            }

            let entries = std::fs::read_dir(dir)?;
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }

                // Skip DOT-only pipeline skills (no manifest.json, only .dot files)
                if !path.join("manifest.json").exists() {
                    continue;
                }

                match Self::load_plugin_with_work_dir(&path, extra_env, work_dir) {
                    Ok((tools, extras)) => {
                        let n = tools.len();
                        let spawn_only = extras.spawn_only_tools.clone();
                        for tool in tools {
                            let name = tool.name().to_string();
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
        let manifest_path = plugin_dir.join("manifest.json");
        let content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| eyre::eyre!("no manifest.json: {e}"))?;
        let manifest: PluginManifest = serde_json::from_str(&content)
            .map_err(|e| eyre::eyre!("invalid manifest.json: {e}"))?;

        // Resolve extras (MCP servers, hooks, prompt fragments) regardless of tools.
        let extras = resolve_extras(&manifest, plugin_dir);

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

        // Find executable: try manifest name, dir name, "main", then any executable in dir
        let dir_name = plugin_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("main");
        let executable = [&manifest.name as &str, dir_name, "main"]
            .iter()
            .map(|name| plugin_dir.join(name))
            .find(|p| p.exists() && is_executable(p))
            .or_else(|| {
                // Fallback: find any executable file in the plugin dir
                std::fs::read_dir(plugin_dir).ok()?.flatten().find_map(|e| {
                    let p = e.path();
                    if p.is_file() && is_executable(&p) {
                        let name = e.file_name().to_string_lossy().to_string();
                        // Skip hidden files and known non-executables
                        if !name.starts_with('.') && !name.ends_with(".json") && !name.ends_with(".md") && !name.ends_with(".toml") && !name.ends_with(".tar.gz") {
                            return Some(p);
                        }
                    }
                    None
                })
            })
            .ok_or_else(|| {
                eyre::eyre!(
                    "no executable found in plugin '{}' (tried '{}', '{}', 'main', and directory scan)",
                    manifest.name,
                    manifest.name,
                    dir_name
                )
            })?;

        // Reject oversized executables (100 MB limit) before reading into memory.
        const MAX_EXECUTABLE_SIZE: u64 = 100_000_000;
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

        match &manifest.sha256 {
            Some(expected_hash) => {
                let actual_hash = format!("{:x}", Sha256::digest(&exe_bytes));
                if actual_hash != expected_hash.to_lowercase() {
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
        // Remove existing verified file first (it has 0o500 perms and can't be overwritten)
        let _ = std::fs::remove_file(&verified_exe);
        std::fs::write(&verified_exe, &exe_bytes)
            .map_err(|e| eyre::eyre!("cannot write verified executable: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&verified_exe, std::fs::Permissions::from_mode(0o500))?;
        }

        // Collect env vars to filter out
        let blocked_env: Vec<String> = BLOCKED_ENV_VARS.iter().map(|s| s.to_string()).collect();

        let timeout = manifest
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(PluginTool::DEFAULT_TIMEOUT);

        // Collect spawn_only tool names and messages before consuming manifest.tools
        let spawn_only_names: Vec<String> = manifest
            .tools
            .iter()
            .filter(|t| t.spawn_only)
            .map(|t| t.name.clone())
            .collect();
        let spawn_only_msgs: std::collections::HashMap<String, String> = manifest
            .tools
            .iter()
            .filter(|t| t.spawn_only && t.spawn_only_message.is_some())
            .map(|t| {
                (
                    t.name.clone(),
                    t.spawn_only_message.clone().unwrap_or_default(),
                )
            })
            .collect();

        let tools: Vec<PluginTool> = manifest
            .tools
            .into_iter()
            .map(|def| {
                let mut tool = PluginTool::new(manifest.name.clone(), def, verified_exe.clone())
                    .with_blocked_env(blocked_env.clone())
                    .with_extra_env(extra_env.to_vec())
                    .with_timeout(timeout);
                if let Some(dir) = work_dir {
                    tool = tool.with_work_dir(dir.to_path_buf());
                }
                tool
            })
            .collect();

        // Return extras with spawn_only info
        let mut extras = extras;
        extras.spawn_only_tools = spawn_only_names;
        extras.spawn_only_messages = spawn_only_msgs;

        Ok((tools, extras))
    }
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
}
