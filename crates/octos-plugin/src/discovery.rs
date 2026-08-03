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

/// Discovery output preserving both accepted plugins and rejected manifests.
#[derive(Debug, Default)]
pub struct PluginDiscoveryResult {
    /// Plugins that passed discovery and precedence checks.
    pub plugins: Vec<DiscoveredPlugin>,
    /// Per-plugin manifest or same-root duplicate rejections.
    pub errors: Vec<PluginDiscoveryError>,
}

/// A plugin directory rejected during discovery.
#[derive(Debug, Clone)]
pub struct PluginDiscoveryError {
    /// Directory containing the rejected manifest.
    pub plugin_dir: PathBuf,
    /// Canonical manifest identity when it can be recovered safely.
    pub plugin_id: Option<String>,
    /// Sanitized diagnostic suitable for aggregating callers.
    pub message: String,
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
    discover_plugins_with_errors(sources, extra_env).plugins
}

/// Discover plugins while retaining manifest rejections for strict callers.
///
/// The legacy [`discover_plugins`] API continues to expose only accepted
/// plugins. Hosts that rebuild an already-published runtime can use this
/// result to reject a partial replacement rather than silently publishing it.
pub fn discover_plugins_with_errors(
    sources: &[PluginSource],
    extra_env: &HashMap<String, String>,
) -> PluginDiscoveryResult {
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut plugins: Vec<DiscoveredPlugin> = Vec::new();
    let mut errors = Vec::new();

    // Collect real env + extra env for gating.
    let mut env_vars: HashMap<String, String> = std::env::vars().collect();
    env_vars.extend(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));

    for source in sources {
        let (discovered, mut rejected) = scan_directory(&source.path, &source.origin, &env_vars);
        let mut id_counts: HashMap<String, usize> = HashMap::new();
        for plugin in &discovered {
            *id_counts.entry(plugin.manifest.id.clone()).or_default() += 1;
        }
        for error in &rejected {
            if let Some(id) = &error.plugin_id {
                *id_counts.entry(id.clone()).or_default() += 1;
            }
        }
        let duplicate_ids: HashSet<String> = id_counts
            .into_iter()
            .filter_map(|(id, count)| (count > 1).then_some(id))
            .collect();

        for id in &duplicate_ids {
            warn!(
                id = %id,
                root = %source.path.display(),
                "rejecting same-root duplicate plugin id"
            );
            // This source has already claimed the precedence slot for the ID,
            // but ambiguously. Fail closed instead of falling through to a
            // lower-priority root whose copy was meant to be shadowed.
            seen_ids.insert(id.clone());
        }

        for plugin in discovered {
            if duplicate_ids.contains(plugin.manifest.id.as_str()) {
                rejected.push(PluginDiscoveryError {
                    plugin_dir: plugin.path,
                    plugin_id: Some(plugin.manifest.id.clone()),
                    message: format!(
                        "same-root duplicate plugin id '{}' is ambiguous",
                        plugin.manifest.id
                    ),
                });
                continue;
            }
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
        errors.extend(rejected);
    }

    PluginDiscoveryResult { plugins, errors }
}

/// Scan a single directory for plugin subdirectories.
///
/// Each immediate child directory that contains a `manifest.json` is treated
/// as a plugin.
fn scan_directory(
    dir: &Path,
    origin: &PluginOrigin,
    env_vars: &HashMap<String, String>,
) -> (Vec<DiscoveredPlugin>, Vec<PluginDiscoveryError>) {
    let mut results = Vec::new();
    let mut errors = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            debug!(
                path = %dir.display(),
                error = %err,
                "could not read plugin source directory"
            );
            return (results, errors);
        }
    };

    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
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
                let plugin_id = recover_manifest_id(&manifest_path);
                warn!(
                    path = %manifest_path.display(),
                    error = %err,
                    "failed to load plugin manifest"
                );
                errors.push(PluginDiscoveryError {
                    plugin_dir: child_path,
                    plugin_id,
                    message: err.to_string(),
                });
            }
        }
    }

    (results, errors)
}

/// Recover only the manifest ID from a rejected manifest so precedence
/// accounting can fail closed on same-root duplicates. This intentionally
/// does not make an invalid manifest discoverable; it only prevents a valid
/// sibling or lower-priority root from being selected ambiguously.
fn recover_manifest_id(manifest_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    ["id", "name"].into_iter().find_map(|field| {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
    })
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
        // RFC-2: tool manifests must carry an `input_schema` rooted at
        // `type: "object"`. Omitting it (as the original test did)
        // surfaces as a discovery rejection in the strict profile.
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "weather",
            r#"{ "id": "weather", "version": "1.0.0", "type": "tool",
                 "tools": [{"name": "get_weather", "description": "weather",
                            "input_schema": {"type": "object", "properties": {}}}] }"#,
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
                 "tools": [{"name": "get_weather", "description": "v2",
                            "input_schema": {"type": "object", "properties": {}}}] }"#,
        );
        write_manifest(
            user_dir.path(),
            "weather",
            r#"{ "id": "weather", "version": "1.0.0", "type": "tool",
                 "tools": [{"name": "get_weather", "description": "v1",
                            "input_schema": {"type": "object", "properties": {}}}] }"#,
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
    fn distinct_ids_with_the_same_display_name_are_discovered_independently() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "first",
            r#"{ "id": "first-plugin", "name": "shared-display", "version": "1.0.0",
                 "tools": [{"name": "shared_tool", "description": "first",
                            "input_schema": {"type": "object", "properties": {}}}] }"#,
        );
        write_manifest(
            tmp.path(),
            "second",
            r#"{ "id": "second-plugin", "name": "shared-display", "version": "1.0.0",
                 "tools": [{"name": "shared_tool", "description": "second",
                            "input_schema": {"type": "object", "properties": {}}}] }"#,
        );

        let plugins = discover_plugins(
            &[PluginSource {
                path: tmp.path().to_path_buf(),
                origin: PluginOrigin::User,
            }],
            &HashMap::new(),
        );

        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].manifest.id, "first-plugin");
        assert_eq!(plugins[1].manifest.id, "second-plugin");
    }

    #[test]
    fn should_reject_all_same_root_plugins_when_manifest_ids_duplicate() {
        let root = TempDir::new().unwrap();
        write_manifest(
            root.path(),
            "first-weather-copy",
            r#"{ "id": "weather", "version": "1.0.0" }"#,
        );
        write_manifest(
            root.path(),
            "second-weather-copy",
            r#"{ "id": "weather", "version": "2.0.0" }"#,
        );

        let plugins = discover_plugins(
            &[PluginSource {
                path: root.path().to_path_buf(),
                origin: PluginOrigin::User,
            }],
            &HashMap::new(),
        );

        assert!(
            plugins.is_empty(),
            "same-root duplicate IDs must fail closed instead of selecting a sibling: {plugins:?}"
        );
    }

    #[test]
    fn should_reject_valid_sibling_when_same_root_duplicate_manifest_is_invalid() {
        let high_priority = TempDir::new().unwrap();
        let lower_priority = TempDir::new().unwrap();
        write_manifest(
            high_priority.path(),
            "a-valid-weather",
            r#"{
                "id": "weather",
                "version": "1.0.0",
                "tools": [{
                    "name": "get_weather",
                    "description": "valid sibling",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        );
        write_manifest(
            high_priority.path(),
            "z-invalid-weather",
            r#"{
                "id": "weather",
                "version": "2.0.0",
                "tools": [{
                    "name": "get_weather_invalid",
                    "description": "invalid sibling",
                    "input_schema": {"type": "string"}
                }]
            }"#,
        );
        write_manifest(
            lower_priority.path(),
            "weather",
            r#"{
                "id": "weather",
                "version": "0.1.0",
                "tools": [{
                    "name": "get_weather_lower",
                    "description": "lower priority copy",
                    "input_schema": {"type": "object", "properties": {}}
                }]
            }"#,
        );

        let plugins = discover_plugins(
            &[
                PluginSource {
                    path: high_priority.path().to_path_buf(),
                    origin: PluginOrigin::Profile,
                },
                PluginSource {
                    path: lower_priority.path().to_path_buf(),
                    origin: PluginOrigin::User,
                },
            ],
            &HashMap::new(),
        );

        assert!(
            plugins.is_empty(),
            "an invalid same-root duplicate must poison the ID and block lower roots: {plugins:?}"
        );
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
        // RFC-2: legacy `name`-using manifests still parse, but the
        // tool's `input_schema` must now declare `type: "object"`.
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
                        "input_schema": {
                            "type": "object",
                            "properties": {}
                        }
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

    /// RFC-2: discovery must not load a manifest that violates the
    /// strict profile — it should `warn!` and continue. This is the
    /// daemon-startup hook from RFC-2 (item 2: "Plugin discovery on
    /// daemon startup").
    #[test]
    fn rfc2_discovery_skips_manifest_with_invalid_schema() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "bad-skill",
            // Reproduces the mofa-slides v0.5.0 anyOf branch missing type.
            r#"{
                "id": "bad-skill",
                "version": "0.1.0",
                "tools": [{
                    "name": "do_thing",
                    "description": "...",
                    "input_schema": {
                        "type": "object",
                        "anyOf": [
                            { "required": ["a"] },
                            { "required": ["b"] }
                        ]
                    }
                }]
            }"#,
        );
        let sources = vec![PluginSource {
            path: tmp.path().to_path_buf(),
            origin: PluginOrigin::User,
        }];
        let plugins = discover_plugins(&sources, &HashMap::new());
        assert!(
            plugins.is_empty(),
            "expected discovery to skip manifest with invalid schema, got {plugins:?}"
        );
    }
}
