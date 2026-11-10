use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::manifest::PluginManifest;

/// Where a plugin was discovered from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginOrigin {
    /// Per-profile plugin directory (`<data_dir>/plugins/`).
    Profile,
    /// User-installed (`~/.octos/plugins/`).
    User,
    /// Bundled into the binary.
    Bundled,
    /// Legacy app-skills directory (`~/.octos/skills/`).
    Legacy,
}

/// Whether a plugin is available to be loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PluginStatus {
    /// All requirements met; ready to use.
    Available,
    /// One or more requirements not met.
    Unavailable { reason: String },
    /// Explicitly disabled by profile config.
    Disabled,
}

impl PluginStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, PluginStatus::Available)
    }
}

/// A fully-resolved plugin discovered during scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredPlugin {
    /// Parsed manifest.
    pub manifest: PluginManifest,
    /// Absolute path to the plugin directory.
    pub path: PathBuf,
    /// Where this plugin was found.
    pub origin: PluginOrigin,
    /// Whether the plugin passed gating checks.
    pub status: PluginStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_origin_rejects_unknown_wire_name() {
        // No catch-all variant exists: an unrecognized origin must fail
        // decoding, not silently default. If a fallback is ever added this
        // pin turns red and the change becomes a deliberate decision.
        assert!(serde_json::from_str::<PluginOrigin>("\"system\"").is_err());
    }

    #[test]
    fn plugin_status_rejects_unknown_status_tag() {
        // Unknown tags must error rather than decode into a default state —
        // an empty PluginStatus variant does not exist to fall back to.
        assert!(serde_json::from_str::<PluginStatus>(r#"{"status":"pending"}"#).is_err());
        // Untagged JSON (a bare string) is not a valid PluginStatus either.
        assert!(serde_json::from_str::<PluginStatus>(r#""available""#).is_err());
    }
}
