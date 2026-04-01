//! Config file watcher with hot-reload support.
//!
//! Polls config files every 5 seconds using SHA-256 hash comparison.
//! Classifies changes as hot-reloadable or restart-required.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::Config;
use crate::profiles::UserProfile;

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
        }
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

        let new_config = match Self::parse_first(&buffers) {
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
    /// Tries `Config` format first, then `UserProfile` format (for --profile mode).
    fn parse_first(buffers: &[(PathBuf, Vec<u8>)]) -> Option<Config> {
        let (path, bytes) = buffers.first()?;
        // Try Config format first
        if let Ok(c) = serde_json::from_slice::<Config>(bytes) {
            return Some(c);
        }
        // Try UserProfile format (for --profile mode)
        match serde_json::from_slice::<UserProfile>(bytes) {
            Ok(profile) => Some(crate::profiles::config_from_profile(&profile, None, None)),
            Err(e) => {
                warn!("config reload failed for {}: {e}", path.display());
                None
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
        let bufs1 = ConfigWatcher::read_files(&[path.clone()]);
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
        let bufs1 = ConfigWatcher::read_files(&[path.clone()]);
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
        match change {
            Some(ConfigChange::RestartRequired(fields)) => {
                panic!(
                    "provider change should not require restart, got fields: {:?}",
                    fields
                );
            }
            _ => {} // None or HotReload is fine
        }
    }
}
