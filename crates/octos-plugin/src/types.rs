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

    /// The wire names below are the types' public serialization contract:
    /// these are re-exported types whose serde derives define their JSON
    /// shape, so a renamed variant or serde attribute would silently change
    /// emitted JSON. Pin the exact bytes.
    #[test]
    fn plugin_origin_serializes_to_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&PluginOrigin::Profile).unwrap(),
            "\"profile\""
        );
        assert_eq!(
            serde_json::to_string(&PluginOrigin::User).unwrap(),
            "\"user\""
        );
        assert_eq!(
            serde_json::to_string(&PluginOrigin::Bundled).unwrap(),
            "\"bundled\""
        );
        assert_eq!(
            serde_json::to_string(&PluginOrigin::Legacy).unwrap(),
            "\"legacy\""
        );
    }

    #[test]
    fn plugin_origin_deserializes_each_wire_name_back() {
        for (wire, expected) in [
            ("\"profile\"", PluginOrigin::Profile),
            ("\"user\"", PluginOrigin::User),
            ("\"bundled\"", PluginOrigin::Bundled),
            ("\"legacy\"", PluginOrigin::Legacy),
        ] {
            let back: PluginOrigin = serde_json::from_str(wire)
                .unwrap_or_else(|e| panic!("{wire} must deserialize: {e}"));
            assert_eq!(back, expected);
        }
    }

    #[test]
    fn plugin_origin_rejects_unknown_wire_name() {
        // No catch-all variant exists: an unrecognized origin must fail
        // decoding, not silently default. If a fallback is ever added this
        // pin turns red and the change becomes a deliberate decision.
        assert!(serde_json::from_str::<PluginOrigin>("\"system\"").is_err());
    }

    #[test]
    fn plugin_status_serializes_with_internal_status_tag() {
        assert_eq!(
            serde_json::to_string(&PluginStatus::Available).unwrap(),
            r#"{"status":"available"}"#
        );
        assert_eq!(
            serde_json::to_string(&PluginStatus::Unavailable {
                reason: "missing binary".to_string()
            })
            .unwrap(),
            r#"{"status":"unavailable","reason":"missing binary"}"#
        );
        assert_eq!(
            serde_json::to_string(&PluginStatus::Disabled).unwrap(),
            r#"{"status":"disabled"}"#
        );
    }

    #[test]
    fn plugin_status_deserializes_each_wire_form() {
        let available: PluginStatus = serde_json::from_str(r#"{"status":"available"}"#).unwrap();
        assert_eq!(available, PluginStatus::Available);

        let unavailable: PluginStatus =
            serde_json::from_str(r#"{"status":"unavailable","reason":"ffmpeg not on PATH"}"#)
                .unwrap();
        assert_eq!(
            unavailable,
            PluginStatus::Unavailable {
                reason: "ffmpeg not on PATH".to_string()
            }
        );

        let disabled: PluginStatus = serde_json::from_str(r#"{"status":"disabled"}"#).unwrap();
        assert_eq!(disabled, PluginStatus::Disabled);
    }

    #[test]
    fn plugin_status_rejects_unknown_status_tag() {
        // Unknown tags must error rather than decode into a default state —
        // an empty PluginStatus variant does not exist to fall back to.
        assert!(serde_json::from_str::<PluginStatus>(r#"{"status":"pending"}"#).is_err());
        // Untagged JSON (a bare string) is not a valid PluginStatus either.
        assert!(serde_json::from_str::<PluginStatus>(r#""available""#).is_err());
    }

    #[test]
    fn is_available_is_true_only_for_available() {
        assert!(PluginStatus::Available.is_available());
        assert!(
            !PluginStatus::Unavailable {
                reason: "missing binary".to_string()
            }
            .is_available()
        );
        assert!(!PluginStatus::Disabled.is_available());
    }

    /// Round-trip the aggregate: every field of DiscoveredPlugin must
    /// survive serde (path as a JSON string, nested manifest included).
    #[test]
    fn discovered_plugin_round_trips_through_serde_json() {
        let manifest = PluginManifest::from_json(
            r#"{ "id": "weather", "version": "1.0.0", "type": "tool",
                 "tools": [{"name": "get_weather", "description": "weather",
                            "input_schema": {"type": "object", "properties": {}}}] }"#,
        )
        .unwrap();
        let plugin = DiscoveredPlugin {
            manifest,
            path: PathBuf::from("/plugins/weather"),
            origin: PluginOrigin::User,
            status: PluginStatus::Unavailable {
                reason: "ffmpeg not on PATH".to_string(),
            },
        };

        let json = serde_json::to_string(&plugin).unwrap();
        let back: DiscoveredPlugin = serde_json::from_str(&json).unwrap();

        assert_eq!(back.path, PathBuf::from("/plugins/weather"));
        assert_eq!(back.origin, PluginOrigin::User);
        assert_eq!(
            back.status,
            PluginStatus::Unavailable {
                reason: "ffmpeg not on PATH".to_string()
            }
        );
        assert_eq!(back.manifest.id, "weather");
        assert_eq!(back.manifest.version, "1.0.0");
        assert_eq!(back.manifest.tools.len(), 1);
        assert_eq!(back.manifest.tools[0].name, "get_weather");
    }
}
