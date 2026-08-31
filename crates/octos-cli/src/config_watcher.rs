//! Config file watcher with hot-reload support.
//!
//! Polls config files every 5 seconds using SHA-256 hash comparison.
//! Classifies changes as hot-reloadable or restart-required.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::Config;
use crate::profiles::{ProfileConfig, UserProfile, merge_profile_defaults};

/// What changed in the config.
#[derive(Debug, Clone)]
pub enum ConfigChange {
    /// Fields that can be applied without restart.
    HotReload {
        system_prompt: Option<String>,
        max_history: Option<usize>,
    },
    /// Fields changed that require a restart. Log warning only.
    RestartRequired(Vec<String>),
}

/// Watches config file(s) and emits changes via a watch channel.
pub struct ConfigWatcher {
    paths: Vec<PathBuf>,
    last_hash: Option<[u8; 32]>,
    last_config: Config,
    tx: watch::Sender<Option<ConfigChange>>,
    /// Path of the global `profile-defaults.json` base layer, when the gateway
    /// runs in profile-mode. Watched alongside the main config so an edit to
    /// the shared defaults triggers the same reload path (FIX 3).
    defaults_path: Option<PathBuf>,
    /// Last successfully-parsed defaults. Retained as last-known-good so a
    /// malformed edit to a previously-valid defaults file does NOT silently
    /// drop the whole base layer (a fresh never-valid file stays `None`).
    last_good_defaults: Option<ProfileConfig>,
}

impl ConfigWatcher {
    pub fn new(
        paths: Vec<PathBuf>,
        initial_config: Config,
        tx: watch::Sender<Option<ConfigChange>>,
    ) -> Self {
        let buffers = Self::read_files(&paths);
        let hash = Self::hash_buffers(&buffers);
        Self {
            paths,
            last_hash: hash,
            last_config: initial_config,
            tx,
            defaults_path: None,
            last_good_defaults: None,
        }
    }

    /// Also watch the store's global `profile-defaults.json` at `path` so a
    /// change to the shared base layer triggers a reload (FIX 3). The current
    /// contents are parsed now to seed the last-known-good base; the path is
    /// added to the watched set (a missing file is fine — it is picked up on
    /// create). Only meaningful in profile-mode, where the effective config is
    /// `profile.config` layered over these defaults.
    pub fn with_profile_defaults(mut self, path: PathBuf) -> Self {
        self.last_good_defaults = Self::parse_defaults(&path);
        // Watch the defaults file AFTER the main config so `parse_first` still
        // reads the main config from `buffers.first()`.
        self.paths.push(path.clone());
        self.defaults_path = Some(path);
        // Re-seed the hash so the newly-watched defaults file contributes to
        // change detection from the first poll onward.
        self.last_hash = Self::hash_buffers(&Self::read_files(&self.paths));
        self
    }

    /// Parse a `profile-defaults.json` into a partial [`ProfileConfig`].
    /// A missing or malformed file yields `None`.
    fn parse_defaults(path: &std::path::Path) -> Option<ProfileConfig> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice::<ProfileConfig>(&bytes).ok()
    }

    /// Spawn the polling loop. Returns a JoinHandle.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut watcher = self;
            // NOTE(#149): The 5-second poll interval is hardcoded. This could be made
            // configurable for deployments that need faster or slower change detection.
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                watcher.check();
            }
        })
    }

    fn check(&mut self) {
        // Read all files once to avoid TOCTOU between hash and parse.
        let buffers = Self::read_files(&self.paths);
        let new_hash = Self::hash_buffers(&buffers);
        if new_hash == self.last_hash {
            return;
        }
        self.last_hash = new_hash;

        // Refresh the defaults base layer (FIX 3). A malformed edit to a
        // previously-valid file retains the last-known-good base rather than
        // dropping it; a deleted file clears it.
        self.refresh_defaults();

        let new_config = match Self::parse_first(&buffers, self.last_good_defaults.as_ref()) {
            Some(c) => c,
            None => return,
        };

        // Validate before applying
        let warnings = new_config.validate();
        for w in &warnings {
            warn!("config reload validation: {w}");
        }

        self.diff_and_emit(&new_config);
        self.last_config = new_config;
    }

    /// Parse config from the first non-empty buffer.
    ///
    /// Sniffs the JSON shape first so a `UserProfile` is parsed as such
    /// instead of silently coercing to a default-everything `Config`. Without
    /// this discrimination, `Config` (which has `#[serde(default)]` on every
    /// field) succeeds first and returns an all-defaults blob — masking
    /// every non-Config field including the new `config.plugins` block.
    /// That regression would skip the policy-change restart for profile
    /// files (codex review round-8 P2).
    ///
    /// Section B (codex review round-7 P2): apply the same
    /// `OCTOS_PLUGINS_REQUIRE_SIGNED` env-merge that `Config::from_file`
    /// does so the diff doesn't see spurious "plugins changed from true
    /// to false" transitions on a hot edit. Without this, a gateway
    /// spawned with the env-forced policy would emit a bogus restart on
    /// every unrelated edit.
    ///
    /// FIX 3: `defaults` is the store's global `profile-defaults.json` base
    /// (when watching in profile-mode). It is layered UNDER the parsed
    /// profile's own config so the diff sees the same effective config the
    /// gateway runs with — an edit to the shared defaults therefore emits the
    /// correct hot-reload / restart, and an unrelated edit does not spuriously
    /// diff the inherited hooks / sandbox / plugins / memory.
    fn parse_first(
        buffers: &[(PathBuf, Vec<u8>)],
        defaults: Option<&ProfileConfig>,
    ) -> Option<Config> {
        let (path, bytes) = buffers.first()?;
        // Build a flattened `Config` from a profile, layering the global
        // defaults UNDER the profile's own config first (FIX 3).
        let profile_to_config = |mut profile: UserProfile| -> Config {
            if let Some(defaults) = defaults {
                profile.config = merge_profile_defaults(&profile.config, defaults);
            }
            let mut c = crate::profiles::config_from_profile(&profile, None, None);
            crate::config::merge_env_plugin_policy_pub(&mut c);
            c
        };
        // Discrimination: a UserProfile JSON has top-level "id" + "config"
        // keys; a top-level Config does not. Try UserProfile first when the
        // shape matches so the watcher actually sees the profile's nested
        // `config.plugins` block rather than silently falling back to a
        // default-everything Config from the all-`serde(default)` shape.
        let looks_like_profile = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .is_some_and(|map| map.contains_key("id") && map.contains_key("config"));
        if looks_like_profile {
            match serde_json::from_slice::<UserProfile>(bytes) {
                Ok(profile) => {
                    return Some(profile_to_config(profile));
                }
                Err(e) => {
                    warn!(
                        "config reload: profile-shaped JSON failed to parse for {}: {e}",
                        path.display()
                    );
                    // fall through to Config attempt
                }
            }
        }
        // Try Config format
        if let Ok(mut c) = serde_json::from_slice::<Config>(bytes) {
            crate::config::merge_env_plugin_policy_pub(&mut c);
            return Some(c);
        }
        // Last-chance: try UserProfile even on non-profile-shaped JSON to
        // preserve legacy behavior if a profile lacks the discriminator
        // keys for some reason.
        match serde_json::from_slice::<UserProfile>(bytes) {
            Ok(profile) => Some(profile_to_config(profile)),
            Err(e) => {
                warn!("config reload failed for {}: {e}", path.display());
                None
            }
        }
    }

    /// Re-read the watched `profile-defaults.json` (FIX 3). A valid file
    /// updates the last-known-good base; a malformed edit to a previously-valid
    /// file keeps the last-known-good base (fail-safe); a deleted file clears
    /// it.
    fn refresh_defaults(&mut self) {
        let Some(path) = self.defaults_path.clone() else {
            return;
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ProfileConfig>(&bytes) {
                Ok(defaults) => self.last_good_defaults = Some(defaults),
                Err(e) => {
                    warn!(
                        "config reload: malformed profile-defaults.json at {} — retaining \
                         last-known-good base: {e}",
                        path.display()
                    );
                    // Keep `last_good_defaults` unchanged (fail-safe).
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File deleted → the base layer is removed.
                self.last_good_defaults = None;
            }
            Err(e) => {
                warn!(
                    "config reload: failed to read profile-defaults.json at {} — retaining \
                     last-known-good base: {e}",
                    path.display()
                );
            }
        }
    }

    fn diff_and_emit(&self, new: &Config) {
        let old = &self.last_config;
        let mut restart_fields = Vec::new();
        let mut hot_prompt = None;
        let mut hot_history = None;
        let mut has_hot = false;

        // Provider/model changes are hot-reloadable (switch_model tool does
        // live swap via SwappableProvider; restarting would kill in-flight
        // responses).
        if old.base_url != new.base_url {
            restart_fields.push("base_url".into());
        }
        if old.api_key_env != new.api_key_env {
            restart_fields.push("api_key_env".into());
        }
        if old.sandbox != new.sandbox {
            restart_fields.push("sandbox".into());
        }
        if old.mcp_servers != new.mcp_servers {
            restart_fields.push("mcp_servers".into());
        }
        if old.hooks != new.hooks {
            restart_fields.push("hooks".into());
        }
        // #1774: post-edit formatting is baked into AgentConfig at startup
        // (chat / gateway / serve all copy it into their agent configs), so
        // a live toggle needs a restart to take effect.
        if old.format_after_edit != new.format_after_edit {
            restart_fields.push("format_after_edit".into());
        }
        // Section B (codex review round-6 P2): plugin loader policy
        // (`plugins.require_signed`) is consumed only during plugin
        // load. A toggle in a running gateway must trigger a restart
        // so the stale registry is flushed and the new gate applies.
        if old.plugins != new.plugins {
            restart_fields.push("plugins".into());
        }

        // Queue mode change requires restart (affects message processing loop)
        let old_queue_mode = old.gateway.as_ref().map(|g| &g.queue_mode);
        let new_queue_mode = new.gateway.as_ref().map(|g| &g.queue_mode);
        if old_queue_mode != new_queue_mode {
            restart_fields.push("gateway.queue_mode".into());
        }

        // Hot-reloadable fields (gateway sub-fields)
        let old_gw = old.gateway.as_ref();
        let new_gw = new.gateway.as_ref();

        let old_prompt = old_gw.and_then(|g| g.system_prompt.as_deref());
        let new_prompt = new_gw.and_then(|g| g.system_prompt.as_deref());
        if old_prompt != new_prompt {
            hot_prompt = new_prompt.map(String::from);
            has_hot = true;
        }

        let old_hist = old_gw.map(|g| g.max_history);
        let new_hist = new_gw.map(|g| g.max_history);
        if old_hist != new_hist {
            hot_history = new_hist;
            has_hot = true;
        }

        // Channels are restart-required for now
        let old_channels = old_gw.map(|g| &g.channels);
        let new_channels = new_gw.map(|g| &g.channels);
        if old_channels != new_channels {
            restart_fields.push("gateway.channels".into());
        }

        if !restart_fields.is_empty() {
            warn!(
                "Config fields changed that require restart: {}. Restart gateway to apply.",
                restart_fields.join(", ")
            );
            let _ = self
                .tx
                .send(Some(ConfigChange::RestartRequired(restart_fields)));
        }

        if has_hot {
            info!("Hot-reloading config changes");
            let _ = self.tx.send(Some(ConfigChange::HotReload {
                system_prompt: hot_prompt,
                max_history: hot_history,
            }));
        }
    }

    /// Read all existing config files into memory.
    fn read_files(paths: &[PathBuf]) -> Vec<(PathBuf, Vec<u8>)> {
        paths
            .iter()
            .filter_map(|p| std::fs::read(p).ok().map(|b| (p.clone(), b)))
            .collect()
    }

    /// Hash all file buffers combined. Returns None if no files were read.
    fn hash_buffers(buffers: &[(PathBuf, Vec<u8>)]) -> Option<[u8; 32]> {
        if buffers.is_empty() {
            return None;
        }
        let mut hasher = Sha256::new();
        for (_, bytes) in buffers {
            hasher.update(bytes);
        }
        Some(hasher.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_config(dir: &TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("config.json");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_hash_detects_change() {
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, r#"{"provider": "anthropic"}"#);
        let bufs1 = ConfigWatcher::read_files(std::slice::from_ref(&path));
        let hash1 = ConfigWatcher::hash_buffers(&bufs1);

        std::fs::write(&path, r#"{"provider": "openai"}"#).unwrap();
        let bufs2 = ConfigWatcher::read_files(&[path]);
        let hash2 = ConfigWatcher::hash_buffers(&bufs2);

        assert!(hash1.is_some());
        assert!(hash2.is_some());
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_no_change_same_hash() {
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, r#"{"provider": "anthropic"}"#);
        let bufs1 = ConfigWatcher::read_files(std::slice::from_ref(&path));
        let hash1 = ConfigWatcher::hash_buffers(&bufs1);
        let bufs2 = ConfigWatcher::read_files(&[path]);
        let hash2 = ConfigWatcher::hash_buffers(&bufs2);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_includes_all_files() {
        let dir = TempDir::new().unwrap();
        let path1 = dir.path().join("a.json");
        let path2 = dir.path().join("b.json");
        std::fs::write(&path1, r#"{"provider": "anthropic"}"#).unwrap();
        std::fs::write(&path2, r#"{"model": "gpt-4o"}"#).unwrap();

        let bufs = ConfigWatcher::read_files(&[path1.clone(), path2.clone()]);
        let hash1 = ConfigWatcher::hash_buffers(&bufs);

        // Change second file only
        std::fs::write(&path2, r#"{"model": "claude"}"#).unwrap();
        let bufs = ConfigWatcher::read_files(&[path1, path2]);
        let hash2 = ConfigWatcher::hash_buffers(&bufs);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hot_reload_system_prompt() {
        let dir = TempDir::new().unwrap();
        let path = write_config(
            &dir,
            r#"{"gateway": {"system_prompt": "old prompt", "channels": []}}"#,
        );
        let old_config = Config::from_file(&path).unwrap();

        std::fs::write(
            &path,
            r#"{"gateway": {"system_prompt": "new prompt", "channels": []}}"#,
        )
        .unwrap();
        let new_config = Config::from_file(&path).unwrap();

        let (tx, rx) = watch::channel(None);
        let watcher = ConfigWatcher::new(vec![path], old_config, tx);
        watcher.diff_and_emit(&new_config);

        let change = rx.borrow().clone();
        assert!(change.is_some());
        if let Some(ConfigChange::HotReload {
            system_prompt,
            max_history,
        }) = change
        {
            assert_eq!(system_prompt.as_deref(), Some("new prompt"));
            assert!(max_history.is_none());
        } else {
            panic!("expected HotReload");
        }
    }

    #[test]
    fn test_provider_change_no_restart() {
        // Provider/model changes are hot-reloadable (switch_model does live swap)
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, r#"{"provider": "anthropic"}"#);
        let old_config = Config::from_file(&path).unwrap();

        std::fs::write(&path, r#"{"provider": "openai"}"#).unwrap();
        let new_config = Config::from_file(&path).unwrap();

        let (tx, rx) = watch::channel(None);
        let watcher = ConfigWatcher::new(vec![path], old_config, tx);
        watcher.diff_and_emit(&new_config);

        let change = rx.borrow().clone();
        // Should NOT trigger RestartRequired for provider-only change
        // None or HotReload is fine; provider-only changes must not restart.
        if let Some(ConfigChange::RestartRequired(fields)) = change {
            panic!("provider change should not require restart, got fields: {fields:?}");
        }
    }

    #[test]
    fn should_require_restart_when_format_after_edit_toggled() {
        // #1774: `format_after_edit` is baked into AgentConfig at startup, so
        // a live toggle must surface as restart-required (like `hooks`).
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, r#"{"provider": "anthropic"}"#);
        let old_config = Config::from_file(&path).unwrap();

        std::fs::write(
            &path,
            r#"{"provider": "anthropic", "format_after_edit": true}"#,
        )
        .unwrap();
        let new_config = Config::from_file(&path).unwrap();

        let (tx, rx) = watch::channel(None);
        let watcher = ConfigWatcher::new(vec![path], old_config, tx);
        watcher.diff_and_emit(&new_config);

        let change = rx.borrow().clone();
        match change {
            Some(ConfigChange::RestartRequired(fields)) => {
                assert!(
                    fields.iter().any(|f| f == "format_after_edit"),
                    "expected format_after_edit in restart fields, got: {fields:?}"
                );
            }
            other => panic!("expected RestartRequired, got: {other:?}"),
        }
    }

    // ---- FIX 3: profile-defaults.json watching + fail-safe ----

    fn default_hook(cmd: &str) -> octos_agent::HookConfig {
        octos_agent::HookConfig {
            event: octos_agent::HookEvent::BeforeToolCall,
            command: vec![cmd.to_string()],
            timeout_ms: 5000,
            tool_filter: Vec::new(),
            path_filter: Vec::new(),
            requires_bin: None,
        }
    }

    const PROFILE_JSON: &str = r#"{"id":"p","name":"p","enabled":true,"config":{},"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z"}"#;

    #[test]
    fn parse_first_layers_profile_defaults_under_profile() {
        let buffers = vec![(PathBuf::from("p.json"), PROFILE_JSON.as_bytes().to_vec())];

        // Without a defaults base, the empty profile has no hooks.
        let bare = ConfigWatcher::parse_first(&buffers, None).unwrap();
        assert!(bare.hooks.is_empty());

        // With a defaults base, the profile inherits the default hook.
        let defaults = ProfileConfig {
            hooks: vec![default_hook("dh")],
            ..Default::default()
        };
        let merged = ConfigWatcher::parse_first(&buffers, Some(&defaults)).unwrap();
        assert_eq!(merged.hooks.len(), 1);
        assert_eq!(merged.hooks[0].command, vec!["dh".to_string()]);
    }

    #[test]
    fn malformed_defaults_edit_retains_last_known_good() {
        let dir = TempDir::new().unwrap();
        let profile_path = dir.path().join("p.json");
        std::fs::write(&profile_path, PROFILE_JSON).unwrap();
        let defaults_path = dir.path().join("profile-defaults.json");
        let good = ProfileConfig {
            hooks: vec![default_hook("dh")],
            ..Default::default()
        };
        std::fs::write(&defaults_path, serde_json::to_string(&good).unwrap()).unwrap();

        let (tx, _rx) = watch::channel(None);
        let mut watcher = ConfigWatcher::new(vec![profile_path], Config::default(), tx)
            .with_profile_defaults(defaults_path.clone());
        assert!(
            watcher.last_good_defaults.is_some(),
            "valid defaults seeded"
        );

        // A malformed edit must NOT drop the base layer.
        std::fs::write(&defaults_path, "{ not valid json").unwrap();
        watcher.refresh_defaults();
        assert_eq!(
            watcher.last_good_defaults.as_ref().map(|d| d.hooks.len()),
            Some(1),
            "malformed edit retains last-known-good base"
        );

        // Deleting the file removes the base layer.
        std::fs::remove_file(&defaults_path).unwrap();
        watcher.refresh_defaults();
        assert!(
            watcher.last_good_defaults.is_none(),
            "deleted file clears base"
        );
    }

    #[test]
    fn editing_profile_defaults_emits_restart_required() {
        let dir = TempDir::new().unwrap();
        let profile_path = dir.path().join("p.json");
        std::fs::write(&profile_path, PROFILE_JSON).unwrap();
        let defaults_path = dir.path().join("profile-defaults.json");
        let v1 = ProfileConfig {
            hooks: vec![default_hook("dh")],
            ..Default::default()
        };
        std::fs::write(&defaults_path, serde_json::to_string(&v1).unwrap()).unwrap();

        // Seed the watcher with the effective config it currently runs with.
        let buffers = ConfigWatcher::read_files(&[profile_path.clone(), defaults_path.clone()]);
        let initial = ConfigWatcher::parse_first(
            &buffers,
            ConfigWatcher::parse_defaults(&defaults_path).as_ref(),
        )
        .unwrap();
        let (tx, mut rx) = watch::channel(None);
        let mut watcher = ConfigWatcher::new(vec![profile_path], initial, tx)
            .with_profile_defaults(defaults_path.clone());

        // Change ONLY the shared defaults — the profile file is untouched.
        let v2 = ProfileConfig {
            hooks: vec![default_hook("dh2")],
            ..Default::default()
        };
        std::fs::write(&defaults_path, serde_json::to_string(&v2).unwrap()).unwrap();
        watcher.check();

        match rx.borrow_and_update().clone() {
            Some(ConfigChange::RestartRequired(fields)) => {
                assert!(
                    fields.iter().any(|f| f == "hooks"),
                    "a defaults hooks change must emit a hooks restart, got {fields:?}"
                );
            }
            other => panic!("expected RestartRequired(hooks) from a defaults edit, got {other:?}"),
        }
    }
}
