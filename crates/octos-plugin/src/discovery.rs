use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use eyre::Result;
use tracing::{debug, warn};

use crate::gating::{self, GatingResult};
use crate::manifest::PluginManifest;
use crate::types::{DiscoveredPlugin, PluginOrigin, PluginStatus};

/// A directory to scan for plugins, paired with its origin.
#[derive(Debug, Clone)]
pub struct PluginSource {
    /// Absolute path to the directory containing plugin subdirectories.
    pub path: PathBuf,
    /// Where this source came from (determines priority).
    pub origin: PluginOrigin,
}

/// Discover plugins from a list of sources.
///
/// Sources are listed in priority order (highest first). If the same plugin
/// `id` appears in multiple sources, the first occurrence wins.
///
/// `extra_env` contains additional environment variables (e.g. from profile
/// config) that should be considered when checking `requires.env`.
pub fn discover_plugins(
    sources: &[PluginSource],
    extra_env: &HashMap<String, String>,
) -> Vec<DiscoveredPlugin> {
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut plugins: Vec<DiscoveredPlugin> = Vec::new();

    // Collect real env + extra env for gating.
    let mut env_vars: HashMap<String, String> = std::env::vars().collect();
    env_vars.extend(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));

    for source in sources {
        let discovered = scan_directory(&source.path, &source.origin, &env_vars);
        for plugin in discovered {
            if seen_ids.contains(&plugin.manifest.id) {
                debug!(
                    id = %plugin.manifest.id,
                    path = %plugin.path.display(),
                    origin = ?plugin.origin,
                    "skipping duplicate plugin (higher-priority copy already loaded)"
                );
                continue;
            }
            seen_ids.insert(plugin.manifest.id.clone());
            plugins.push(plugin);
        }
    }

    plugins
}

/// Scan a single directory for plugin subdirectories.
///
/// Each immediate child directory that contains a `manifest.json` is treated
/// as a plugin.
fn scan_directory(
    dir: &Path,
    origin: &PluginOrigin,
    env_vars: &HashMap<String, String>,
) -> Vec<DiscoveredPlugin> {
    let mut results = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            debug!(
                path = %dir.display(),
                error = %err,
                "could not read plugin source directory"
            );
            return results;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let child_path = entry.path();
        if !child_path.is_dir() {
            continue;
        }

        let manifest_path = child_path.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }

        match load_plugin_entry(&child_path, &manifest_path, origin, env_vars) {
            Ok(plugin) => results.push(plugin),
            Err(err) => {
                warn!(
                    path = %manifest_path.display(),
                    error = %err,
                    "failed to load plugin manifest"
                );
            }
        }
    }

    results
}

/// Load a single plugin from its directory.
fn load_plugin_entry(
    plugin_dir: &Path,
    manifest_path: &Path,
    origin: &PluginOrigin,
    env_vars: &HashMap<String, String>,
) -> Result<DiscoveredPlugin> {
    let manifest = PluginManifest::from_file(manifest_path)?;

    // Run gating checks.
    let gating_result = match &manifest.requires {
        Some(reqs) => gating::check_requirements(reqs, env_vars),
        None => GatingResult::all_passed(),
    };

    let status = if gating_result.passed {
        PluginStatus::Available
    } else {
        PluginStatus::Unavailable {
            reason: gating_result.summary,
        }
    };

    Ok(DiscoveredPlugin {
        manifest,
        path: plugin_dir.to_path_buf(),
        origin: origin.clone(),
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, name: &str, json: &str) {
        let plugin_dir = dir.join(name);
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("manifest.json"), json).unwrap();
    }

    #[test]
    fn discover_single_plugin() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "weather",
            r#"{ "id": "weather", "version": "1.0.0", "type": "tool",
                 "tools": [{"name": "get_weather", "description": "weather"}] }"#,
        );

        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::User,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.id, "weather");
        assert_eq!(plugins[0].origin, PluginOrigin::User);
        assert!(plugins[0].status.is_available());
    }

    #[test]
    fn higher_priority_wins_dedup() {
        let profile_dir = TempDir::new().unwrap();
        let user_dir = TempDir::new().unwrap();

        write_manifest(
            profile_dir.path(),
            "weather",
            r#"{ "id": "weather", "version": "2.0.0", "type": "tool",
                 "tools": [{"name": "get_weather", "description": "v2"}] }"#,
        );
        write_manifest(
            user_dir.path(),
            "weather",
            r#"{ "id": "weather", "version": "1.0.0", "type": "tool",
                 "tools": [{"name": "get_weather", "description": "v1"}] }"#,
        );

        let sources = vec![
            PluginSource {
                path: profile_dir.path().to_path_buf(),
                origin: PluginOrigin::Profile,
            },
            PluginSource {
                path: user_dir.path().to_path_buf(),
                origin: PluginOrigin::User,
            },
        ];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.version, "2.0.0");
        assert_eq!(plugins[0].origin, PluginOrigin::Profile);
    }

    #[test]
    fn multiple_plugins_from_one_dir() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "alpha",
            r#"{ "id": "alpha", "version": "1.0.0" }"#,
        );
        write_manifest(
            tmp.path(),
            "beta",
            r#"{ "id": "beta", "version": "1.0.0" }"#,
        );

        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::Bundled,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert_eq!(plugins.len(), 2);
        let ids: HashSet<_> = plugins.iter().map(|p| p.manifest.id.as_str()).collect();
        assert!(ids.contains("alpha"));
        assert!(ids.contains("beta"));
    }

    #[test]
    fn gating_marks_unavailable() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "gated",
            r#"{ "id": "gated", "version": "1.0.0",
                 "requires": { "env": ["NONEXISTENT_SECRET_XYZ_99"] } }"#,
        );

        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::User,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert_eq!(plugins.len(), 1);
        assert!(!plugins[0].status.is_available());
        match &plugins[0].status {
            PluginStatus::Unavailable { reason } => {
                assert!(reason.contains("NONEXISTENT_SECRET_XYZ_99"));
            }
            _ => panic!("expected Unavailable status"),
        }
    }

    #[test]
    fn extra_env_satisfies_gating() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "needs-key",
            r#"{ "id": "needs-key", "version": "1.0.0",
                 "requires": { "env": ["MY_SPECIAL_KEY"] } }"#,
        );

        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::User,
        }];

        // Without extra env → unavailable.
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert!(!plugins[0].status.is_available());

        // With extra env → available.
        let mut extra = HashMap::new();
        extra.insert("MY_SPECIAL_KEY".to_string(), "secret".to_string());
        let plugins = discover_plugins(&sources, &extra);
        assert!(plugins[0].status.is_available());
    }

    #[test]
    fn skips_dirs_without_manifest() {
        let tmp = TempDir::new().unwrap();
        // Directory without manifest.json
        fs::create_dir_all(tmp.path().join("no-manifest")).unwrap();
        // File (not a directory)
        fs::write(tmp.path().join("a-file.txt"), "not a plugin").unwrap();

        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::User,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert!(plugins.is_empty());
    }

    #[test]
    fn nonexistent_source_dir_is_harmless() {
        let sources = vec![PluginSource {
            path: PathBuf::from("/tmp/nonexistent_octos_plugin_dir_xyz"),
            origin: PluginOrigin::User,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert!(plugins.is_empty());
    }

    #[test]
    fn legacy_manifest_with_name_field() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "news",
            r#"{
                "name": "news",
                "version": "1.0.0",
                "description": "Fetches news",
                "tools": [
                    {
                        "name": "news_fetch",
                        "description": "Fetch news",
                        "entrypoint": "target/release/news_fetch",
                        "input_schema": {}
                    }
                ]
            }"#,
        );

        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::Legacy,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.id, "news");
        assert_eq!(plugins[0].origin, PluginOrigin::Legacy);
    }
}
