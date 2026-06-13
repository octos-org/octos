//! Reply-voice selection: where the registry lives + the sticky-with-live
//! override mechanism that lets a user switch voices mid-conversation.
//!
//! The persisted source of truth is `profile.config.voice.default_voice`
//! (survives restarts). On top of it we keep a process-wide live override
//! keyed by profile id, set by `PUT /api/my/voice`, so a switch takes effect
//! on the *next* turn without rebuilding the cached profile/session runtimes.
//! The turn handler resolves the voice via [`resolve_reply_voice`].
//!
//! A process-wide `LazyLock` (rather than an `AppState` field) keeps the blast
//! radius tiny — `AppState` has ~50 construction sites — while matching the
//! actual scope: one override map per `octos serve` process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

/// Live per-profile reply-voice override. Empty until a user picks a voice.
static VOICE_OVERRIDES: LazyLock<RwLock<HashMap<String, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Record `voice` as `profile_id`'s live reply-voice override.
pub fn set_override(profile_id: &str, voice: &str) {
    VOICE_OVERRIDES
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(profile_id.to_string(), voice.to_string());
}

/// The live override for `profile_id`, if one has been set this process.
pub fn get_override(profile_id: &str) -> Option<String> {
    VOICE_OVERRIDES
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(profile_id)
        .cloned()
}

/// Resolve the reply voice for a turn: the live override wins, otherwise the
/// profile's persisted default.
pub fn resolve_reply_voice(profile_id: &str, profile_default: &str) -> String {
    get_override(profile_id).unwrap_or_else(|| profile_default.to_string())
}

/// Path to the platform voice registry: `$OMINIX_HOME/models/voices.json`
/// (default `~/.OminiX/models/voices.json`).
pub fn registry_path() -> PathBuf {
    let home = std::env::var_os("OMINIX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(default_ominix_home);
    registry_path_under(&home)
}

fn default_ominix_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".OminiX")
}

fn registry_path_under(home: &Path) -> PathBuf {
    home.join("models").join("voices.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_takes_precedence_over_default() {
        // Unique id so the shared map doesn't collide with other tests.
        let pid = "test-profile-precedence";
        assert_eq!(resolve_reply_voice(pid, "doubao"), "doubao"); // no override yet
        set_override(pid, "yangmi");
        assert_eq!(get_override(pid).as_deref(), Some("yangmi"));
        assert_eq!(resolve_reply_voice(pid, "doubao"), "yangmi");
    }

    #[test]
    fn resolve_falls_back_to_default_without_override() {
        let pid = "test-profile-fallback";
        assert_eq!(resolve_reply_voice(pid, "doubao"), "doubao");
    }

    #[test]
    fn registry_path_is_models_voices_json_under_home() {
        assert_eq!(
            registry_path_under(Path::new("/x/.OminiX")),
            Path::new("/x/.OminiX/models/voices.json")
        );
    }
}
