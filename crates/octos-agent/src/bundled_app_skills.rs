//! Embedded metadata for app-skill binaries that ship alongside the `octos` binary.
//!
//! Each entry contains: (dir_name, binary_name, SKILL.md content, manifest.json content).
//! The actual binaries are sibling executables in the same directory as the `octos` binary;
//! [`super::bootstrap`] copies them into `.octos/skills/` at gateway startup.

/// (dir_name, binary_name, skill_md, manifest_json)
pub const BUNDLED_APP_SKILLS: &[(&str, &str, &str, &str)] = &[
    (
        "news",
        "news_fetch",
        include_str!("../../app-skills/news/SKILL.md"),
        include_str!("../../app-skills/news/manifest.json"),
    ),
    (
        "deep-search",
        "deep-search",
        include_str!("../../app-skills/deep-search/SKILL.md"),
        include_str!("../../app-skills/deep-search/manifest.json"),
    ),
    (
        "deep-crawl",
        "deep_crawl",
        include_str!("../../app-skills/deep-crawl/SKILL.md"),
        include_str!("../../app-skills/deep-crawl/manifest.json"),
    ),
    (
        "send-email",
        "send_email",
        include_str!("../../app-skills/send-email/SKILL.md"),
        include_str!("../../app-skills/send-email/manifest.json"),
    ),
    (
        "account-manager",
        "account_manager",
        include_str!("../../app-skills/account-manager/SKILL.md"),
        include_str!("../../app-skills/account-manager/manifest.json"),
    ),
    (
        "clock",
        "clock",
        include_str!("../../app-skills/time/SKILL.md"),
        include_str!("../../app-skills/time/manifest.json"),
    ),
    (
        "weather",
        "weather",
        include_str!("../../app-skills/weather/SKILL.md"),
        include_str!("../../app-skills/weather/manifest.json"),
    ),
    (
        "office",
        "office",
        include_str!("../../app-skills/office/SKILL.md"),
        include_str!("../../app-skills/office/manifest.json"),
    ),
    (
        "smart-home",
        "smart_home",
        include_str!("../../app-skills/smart-home/SKILL.md"),
        include_str!("../../app-skills/smart-home/manifest.json"),
    ),
    // voice-skill removed — voice TTS/ASR is handled by platform-skill "voice".
    // Voice cloning is handled by mofa-fm.
    // pipeline-guard removed — its before_tool_call hook was a category
    // mismatch for correctness-critical logic. Its inline replacement was
    // later removed with the unused pipeline crate; see git history and PR
    // #962 for the design rationale.
    (
        "skill-evolve",
        "skill-evolve",
        include_str!("../../app-skills/skill-evolve/SKILL.md"),
        include_str!("../../app-skills/skill-evolve/manifest.json"),
    ),
];

/// Platform skills: bootstrapped once by `octos serve` (admin bot) at startup,
/// shared across all gateway profiles. Only installed when their backend is reachable.
/// Same tuple format as BUNDLED_APP_SKILLS: (dir_name, binary_name, skill_md, manifest_json).
pub const PLATFORM_SKILLS: &[(&str, &str, &str, &str)] = &[(
    "voice",
    "voice",
    include_str!("../../platform-skills/voice/SKILL.md"),
    include_str!("../../platform-skills/voice/manifest.json"),
)];
