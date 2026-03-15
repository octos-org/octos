//! Plugin loader: scans directories for plugins and registers their tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::Result;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::sandbox::BLOCKED_ENV_VARS;
use crate::tools::ToolRegistry;

use super::manifest::PluginManifest;
use super::tool::PluginTool;

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
    /// Returns the number of tools registered.
    pub fn load_into(
        registry: &mut ToolRegistry,
        dirs: &[PathBuf],
        extra_env: &[(String, String)],
    ) -> Result<usize> {
        Self::load_into_with_work_dir(registry, dirs, extra_env, None)
    }

    /// Like `load_into`, but sets a working directory for plugin processes.
    pub fn load_into_with_work_dir(
        registry: &mut ToolRegistry,
        dirs: &[PathBuf],
        extra_env: &[(String, String)],
        work_dir: Option<&Path>,
    ) -> Result<usize> {
        let mut count = 0;

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
                    Ok(tools) => {
                        let n = tools.len();
                        for tool in tools {
                            registry.register(tool);
                        }
                        count += n;
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

        if count > 0 {
            info!(tools = count, "loaded plugin tools");
        }

        Ok(count)
    }

    /// Load a single plugin directory and return its tools.
    pub fn load_plugin(
        plugin_dir: &Path,
        extra_env: &[(String, String)],
    ) -> Result<Vec<PluginTool>> {
        Self::load_plugin_with_work_dir(plugin_dir, extra_env, None)
    }

    /// Load a single plugin directory with an optional working directory.
    pub fn load_plugin_with_work_dir(
        plugin_dir: &Path,
        extra_env: &[(String, String)],
        work_dir: Option<&Path>,
    ) -> Result<Vec<PluginTool>> {
        let manifest_path = plugin_dir.join("manifest.json");
        let content = std::fs::read_to_string(&manifest_path)
            .map_err(|e| eyre::eyre!("no manifest.json: {e}"))?;
        let manifest: PluginManifest = serde_json::from_str(&content)
            .map_err(|e| eyre::eyre!("invalid manifest.json: {e}"))?;

        // Find executable: try plugin name, then "main"
        let dir_name = plugin_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("main");
        let executable = [dir_name, "main"]
            .iter()
            .map(|name| plugin_dir.join(name))
            .find(|p| p.exists() && is_executable(p))
            .ok_or_else(|| {
                eyre::eyre!(
                    "no executable found in plugin '{}' (tried '{}', 'main')",
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

        let tools = manifest
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

        Ok(tools)
    }
}

/// Compute SHA-256 hex digest of a file.
#[cfg(test)]
fn compute_sha256(path: &Path) -> Result<String> {
    let data = std::fs::read(path)?;
    let hash = Sha256::digest(&data);
    Ok(format!("{hash:x}"))
}

/// Check if a path is executable (Unix).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// On non-Unix, just check existence.
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
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_load_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        let count =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(count, 0);
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
        let count =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(count, 1);
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
        let count =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(count, 1);
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
        let count =
            PluginLoader::load_into(&mut registry, &[dir.path().to_path_buf()], &[]).unwrap();
        assert_eq!(count, 0);
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
}
