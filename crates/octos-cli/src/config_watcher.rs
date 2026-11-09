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
        // #2217: tool_policy is applied to the tool registry at bootstrap
        // (`apply_policy`); a live edit never reaches the running registry,
        // so a policy-only change must surface as restart-required.
        if old.tool_policy != new.tool_policy {
            restart_fields.push("tool_policy".into());
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
