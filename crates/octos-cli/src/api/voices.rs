//! Reply-voice selection: where the registry lives + the sticky-with-live
//! override mechanism that lets a user switch voices mid-conversation.
//!
//! The persisted source of truth is `profile.config.voice_default`
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

/// The profile that owns the voice whose reference audio is `ref_audio`, if it
/// is a per-profile clone. The fleet registration writes each tenant's clone
/// into the global `voices.json` with the absolute clone path as `ref_audio`
/// (`.../profiles/<id>/data/voice_profiles/<name>.wav`), so the owning profile
/// is the path segment after `profiles/` — but only when a later
/// `voice_profiles` segment confirms it's actually a voice-clone path. A shared
/// preset (ref audio not under any profile's clone dir) returns `None`.
///
/// Splits on both `/` and `\` so a Windows-style clone path can't be
/// mis-classified as a shared preset (which would re-open the cross-tenant leak).
fn owning_profile_of(ref_audio: &str) -> Option<&str> {
    let comps: Vec<&str> = ref_audio
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    let profiles_idx = comps.iter().position(|&c| c == "profiles")?;
    let owner = *comps.get(profiles_idx + 1)?;
    comps
        .get(profiles_idx + 2..)?
        .contains(&"voice_profiles")
        .then_some(owner)
}

/// Whether the voice whose reference audio is `ref_audio` may be listed/selected
/// by `profile_id`: a shared preset is visible to everyone; a per-profile clone
/// is visible only to the profile that owns it. This is the cross-tenant
/// boundary for `GET /api/voices` and `PUT /api/my/voice`.
pub fn voice_visible_to(profile_id: &str, ref_audio: &str) -> bool {
    match owning_profile_of(ref_audio) {
        Some(owner) => owner == profile_id,
        None => true,
    }
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

    #[test]
    fn owning_profile_parsed_only_from_voice_clone_paths() {
        assert_eq!(
            owning_profile_of("/Users/cloud/.octos/profiles/alice/data/voice_profiles/clone.wav"),
            Some("alice")
        );
        // Windows-style clone path (backslash separators) must resolve too,
        // otherwise it would be treated as a shared preset (cross-tenant leak).
        assert_eq!(
            owning_profile_of(
                "C:\\Users\\cloud\\.octos\\profiles\\alice\\data\\voice_profiles\\clone.wav"
            ),
            Some("alice")
        );
        // Shared preset: relative path, no profile segment.
        assert_eq!(owning_profile_of("ref_audios/doubao_ref.wav"), None);
        // A `profiles/` segment without a later `voice_profiles` is not a clone.
        assert_eq!(owning_profile_of("/srv/profiles/alice/models/x.wav"), None);
    }

    #[test]
    fn voice_visible_only_to_owner_but_presets_visible_to_all() {
        let clone = "/Users/cloud/.octos/profiles/alice/data/voice_profiles/clone.wav";
        assert!(voice_visible_to("alice", clone), "owner sees own clone");
        assert!(
            !voice_visible_to("bob", clone),
            "another tenant must not see alice's clone"
        );
        let preset = "ref_audios/doubao_ref.wav";
        assert!(
            voice_visible_to("bob", preset),
            "shared preset visible to all"
        );
    }
}
