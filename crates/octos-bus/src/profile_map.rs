//! #1506 — explicit chat/channel/guild → `target_profile_id` routing maps.
//!
//! Channel entries declare these in their `settings`:
//!
//! ```json
//! { "chat_profile_map":  { "<chat_id>":   "<profile_id>" },
//!   "guild_profile_map": { "<guild_id>":  "<profile_id>" } }
//! ```
//!
//! `guild_profile_map` is Discord-only (a fallback when the channel itself
//! has no explicit entry). A chat hit always wins over a guild hit; no hit
//! means no `target_profile_id` is stamped and the legacy single-profile
//! routing applies unchanged. Empty or malformed ids are a CONFIG error at
//! parse time; a well-formed but unknown profile id is refused per message
//! at dispatch time (`profile_route_explicit`), so a mapped message can
//! never silently land on the wrong (default) profile.
//!
//! The maps live in each channel entry's own settings, so the same chat id
//! on two bot instances can route to two different profiles.

use std::collections::HashMap;

/// Parsed, validated routing maps for one channel entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatProfileMap {
    chat: HashMap<String, String>,
    guild: HashMap<String, String>,
}

impl ChatProfileMap {
    /// Parse `chat_profile_map` / `guild_profile_map` out of a channel
    /// entry's settings. Absent keys yield an empty map (legacy behavior).
    /// Any malformed shape — non-object, non-string values, empty or
    /// whitespace-only ids — is an explicit config error naming the key.
    pub fn from_settings(settings: &serde_json::Value) -> Result<Self, String> {
        Ok(Self {
            chat: parse_map(settings, "chat_profile_map")?,
            guild: parse_map(settings, "guild_profile_map")?,
        })
    }

    /// Resolve the explicit profile for an inbound message: an exact
    /// chat/channel id match wins; otherwise (Discord) the guild map is
    /// consulted. `None` means "no explicit route" — never a default.
    pub fn route(&self, chat_id: &str, guild_id: Option<&str>) -> Option<&str> {
        if let Some(profile) = self.chat.get(chat_id) {
            return Some(profile.as_str());
        }
        guild_id
            .and_then(|id| self.guild.get(id))
            .map(String::as_str)
    }

    /// Discord thread-aware resolution: a message inside a thread carries
    /// the THREAD's channel id, not its parent channel's. Try the thread id
    /// first, then the parent channel id, then the guild fallback — a thread
    /// under a mapped channel must not silently fall through to the guild or
    /// default profile (forum channels are ALL threads, so without the parent
    /// hop a mapped forum channel never matched).
    pub fn route_with_parent(
        &self,
        chat_id: &str,
        parent_id: Option<&str>,
        guild_id: Option<&str>,
    ) -> Option<&str> {
        if let Some(profile) = self.chat.get(chat_id) {
            return Some(profile.as_str());
        }
        if let Some(profile) = parent_id.and_then(|id| self.chat.get(id)) {
            return Some(profile.as_str());
        }
        guild_id
            .and_then(|id| self.guild.get(id))
            .map(String::as_str)
    }
}

fn parse_map(settings: &serde_json::Value, key: &str) -> Result<HashMap<String, String>, String> {
    let Some(value) = settings.get(key) else {
        return Ok(HashMap::new());
    };
    if value.is_null() {
        return Ok(HashMap::new());
    }
    let object = value
        .as_object()
        .ok_or_else(|| format!("{key} must be an object of <id>: <profile_id>"))?;
    let mut map = HashMap::new();
    for (id, profile) in object {
        let profile = profile
            .as_str()
            .ok_or_else(|| format!("{key}[{id:?}] must be a profile id string, got {profile}"))?;
        if id.trim().is_empty() {
            return Err(format!("{key} contains an empty chat/guild id"));
        }
        if profile.trim().is_empty() {
            return Err(format!("{key}[{id:?}] must not be an empty profile id"));
        }
        // #1506 review — surface likely typos: Telegram/Discord ids are
        // numeric (Telegram adds a leading `-`/`-100`). A non-numeric key
        // can never match a real chat and would silently use the default
        // profile; warn at startup rather than refusing (a future id
        // format must not hard-fail the channel).
        if !id.chars().all(|c| c.is_ascii_digit() || c == '-') {
            tracing::warn!(
                key,
                id = %id,
                "chat/guild id in a profile map is not numeric and can never match;                  this entry is inert (the chat will use the default route)"
            );
        }
        map.insert(id.clone(), profile.to_owned());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1506 — the routing table itself: chat beats guild, no hit is no
    /// route (never a default profile), and malformed config is refused
    /// loudly instead of being dropped into the default profile.
    #[test]
    fn should_route_explicitly_and_refuse_malformed_maps() {
        let map = ChatProfileMap::from_settings(&serde_json::json!({
            "chat_profile_map": {"-100200": "work", "333": "play"},
            "guild_profile_map": {"999": "guild-profile"}
        }))
        .unwrap();
        assert_eq!(map.route("-100200", None), Some("work"));
        assert_eq!(
            map.route("333", Some("999")),
            Some("play"),
            "chat beats guild"
        );
        assert_eq!(
            map.route("444", Some("999")),
            Some("guild-profile"),
            "guild fallback"
        );
        assert_eq!(map.route("444", None), None, "no hit means no route");
        assert_eq!(map.route("-100200", Some("999")), Some("work"));

        // Thread-aware resolution: thread id first, then the parent
        // channel, then the guild — a thread under a mapped channel never
        // falls through to the guild/default.
        assert_eq!(map.route_with_parent("333", None, None), Some("play"));
        assert_eq!(
            map.route_with_parent("thread-1", Some("333"), Some("999")),
            Some("play"),
            "thread under a mapped parent channel"
        );
        assert_eq!(
            map.route_with_parent("thread-2", Some("-100200"), Some("999")),
            Some("work")
        );
        assert_eq!(
            map.route_with_parent("thread-3", Some("unmapped"), Some("999")),
            Some("guild-profile"),
            "unmapped parent falls back to the guild map"
        );
        assert_eq!(map.route_with_parent("thread-4", None, None), None);

        // Absent or null maps parse to empty (legacy entries unchanged).
        let empty = ChatProfileMap::from_settings(&serde_json::json!({})).unwrap();
        assert_eq!(empty.route("1", None), None);
        let null = ChatProfileMap::from_settings(&serde_json::json!({
            "chat_profile_map": null
        }))
        .unwrap();
        assert_eq!(null.route("1", None), None);

        // Same chat id on a different instance is an independent map.
        let other = ChatProfileMap::from_settings(&serde_json::json!({
            "chat_profile_map": {"-100200": "other-profile"}
        }))
        .unwrap();
        assert_eq!(other.route("-100200", None), Some("other-profile"));
        assert_eq!(map.route("-100200", None), Some("work"));

        for bad in [
            serde_json::json!({"chat_profile_map": ["work"]}),
            serde_json::json!({"chat_profile_map": {"1": 2}}),
            serde_json::json!({"chat_profile_map": {"1": " "}}),
            serde_json::json!({"chat_profile_map": {"  ": "work"}}),
            serde_json::json!({"guild_profile_map": "work"}),
        ] {
            assert!(
                ChatProfileMap::from_settings(&bad).is_err(),
                "malformed map must be a config error, not a silent default: {bad}"
            );
        }
    }
}
