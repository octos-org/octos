//! OLP-CTRL slice 4 — operator notification for goal-scoped escalations.
//!
//! Contract: task-req-olp-ctrl-steer.spec.md — "goal-scoped escalation
//! 记录时,若 profile 配置了通知通道(复用 cron notify mode 的发送器),
//! 向 operator 发送含 slug 与 goal_id 的通知;未配置时静默跳过(不失败、
//! 不告警刷屏)".
//!
//! Delivery: the orchestrator (serve-side, no transport handle) emits an
//! `escalation_notify` row into events.jsonl carrying the target channel
//! type and the notice text (slug + goal_id + question). The gateway's
//! outbound consumer — the same verbatim-message path cron `Notify` mode
//! uses — delivers it to the operator's channel. When the profile has NO
//! channel configured, this module is a silent no-op: no event row, no
//! failure, no log spam.

use std::path::Path;

/// True when the profile has at least one notification channel configured.
/// Reads the profile registry from the SHARED state home (the config-like
/// registry, same root serve opens); a missing registry / profile / empty
/// channel list all read as UNCONFIGURED (silent skip).
pub(crate) fn profile_has_notification_channel(state_home: &Path, profile_id: &str) -> bool {
    let runtime_root = state_home; // registry+data unified at this root here
    let Ok(store) = crate::profiles::ProfileStore::open(state_home, runtime_root) else {
        return false;
    };
    match store.get(profile_id) {
        Ok(Some(profile)) => !profile.config.channels.is_empty(),
        _ => false,
    }
}

/// The first configured channel's type tag (for the notify row's routing
/// hint), e.g. "telegram" / "feishu". `None` when unconfigured.
pub(crate) fn profile_first_channel_type(state_home: &Path, profile_id: &str) -> Option<String> {
    let runtime_root = state_home;
    let store = crate::profiles::ProfileStore::open(state_home, runtime_root).ok()?;
    let profile = store.get(profile_id).ok().flatten()?;
    let first = profile.config.channels.first()?;
    Some(
        match first {
            crate::profiles::ChannelCredentials::Telegram { .. } => "telegram",
            crate::profiles::ChannelCredentials::Discord { .. } => "discord",
            crate::profiles::ChannelCredentials::DingTalk { .. } => "dingtalk",
            crate::profiles::ChannelCredentials::Slack { .. } => "slack",
            crate::profiles::ChannelCredentials::WhatsApp { .. } => "whatsapp",
            crate::profiles::ChannelCredentials::Feishu { .. } => "feishu",
            other => {
                // Any other configured channel still counts as configured;
                // route by its serde tag generically.
                return serde_json::to_value(other)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned));
            }
        }
        .to_owned(),
    )
}

/// Emit the escalation operator notice (or skip silently when the profile
/// has no notification channel). `profile_data_dir` is the per-instance
/// profile data root (events.jsonl lives at its parent chain's data root
/// — we reuse the same dir the escalation event already writes to).
pub(crate) fn maybe_notify_escalation(
    profile_data_dir: &Path,
    profile_id: &str,
    goal_id: &str,
    peer_slug: &str,
    question: &str,
) {
    // The registry root: profile_data_dir is <runtime>/profiles/<id>/data,
    // so the shared state home is two levels up
    // (<runtime> = <state_home>/instances/<hash> or <state_home> itself).
    // ProfileStore::open tolerates either as long as the registry lives at
    // <root>/profiles — walking up to the dir that CONTAINS `profiles`
    // gives us exactly the runtime root whose registry serve shares.
    let Some(state_home) = registry_root_for(profile_data_dir) else {
        return; // cannot locate a registry → unconfigured → silent skip
    };
    if !profile_has_notification_channel(&state_home, profile_id) {
        return; // contract: 未配置时静默跳过
    }
    let channel = profile_first_channel_type(&state_home, profile_id);
    let notice = format!(
        "escalation from peer `{peer_slug}` on goal `{goal_id}`: {}",
        question.chars().take(200).collect::<String>()
    );
    let mut event = crate::obs_events::ObsEvent::new("escalation_notify", &notice)
        .goal_id(Some(goal_id))
        .slug(Some(peer_slug));
    if let Some(channel) = channel.as_deref() {
        event = event.model_lane(Some(channel)); // routing hint in the lane slot
    }
    crate::obs_events::append_obs_event(profile_data_dir, &event);
}

/// Walk up from `<runtime>/profiles/<id>/data` to the runtime root that
/// CONTAINS the `profiles` dir (the registry root serve uses).
fn registry_root_for(profile_data_dir: &Path) -> Option<std::path::PathBuf> {
    let mut dir = profile_data_dir;
    loop {
        if dir.join("profiles").is_dir() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_profile_with_channels(root: &Path, id: &str, channels: serde_json::Value) {
        let profiles = root.join("profiles");
        std::fs::create_dir_all(&profiles).expect("profiles dir");
        let profile = serde_json::json!({
            "id": id,
            "name": id,
            "enabled": true,
            "config": { "channels": channels },
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        std::fs::write(
            profiles.join(format!("{id}.json")),
            serde_json::to_string_pretty(&profile).expect("json"),
        )
        .expect("write profile");
    }

    /// Contract: 未配置时静默跳过 — no channel → no event row, no error.
    #[test]
    fn olp_ctrl_escalation_notify_skips_when_unconfigured() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = temp.path().join("runtime");
        let data = runtime.join("profiles").join("octos").join("data");
        std::fs::create_dir_all(&data).expect("data dir");
        seed_profile_with_channels(&runtime, "octos", serde_json::json!([]));
        maybe_notify_escalation(&data, "octos", "goal_05", "edison", "question?");
        assert!(
            !data.join("events.jsonl").exists(),
            "unconfigured profile must produce NO notify row"
        );
    }

    /// Contract: 配置了通知通道才发 — a channelled profile gets an
    /// escalation_notify row carrying slug + goal_id.
    #[test]
    fn olp_ctrl_escalation_notify_emits_with_channel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime = temp.path().join("runtime");
        let data = runtime.join("profiles").join("octos").join("data");
        std::fs::create_dir_all(&data).expect("data dir");
        seed_profile_with_channels(
            &runtime,
            "octos",
            serde_json::json!([{ "type": "telegram", "token_env": "TG_TOKEN" }]),
        );
        maybe_notify_escalation(&data, "octos", "goal_05", "edison", "approve deploy?");
        let content = std::fs::read_to_string(data.join("events.jsonl")).expect("events");
        let line: serde_json::Value =
            serde_json::from_str(content.lines().next().expect("one line")).expect("json");
        assert_eq!(line["kind"], "escalation_notify");
        assert_eq!(line["goal_id"], "goal_05");
        assert_eq!(line["slug"], "edison");
        assert!(line["detail"].as_str().expect("detail").contains("edison"));
        assert!(line["detail"].as_str().expect("detail").contains("goal_05"));
    }
}
