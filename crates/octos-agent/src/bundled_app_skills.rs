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
    // voice-skill removed — voice TTS/ASR is handled by platform-skill "voice".
    // Voice cloning is handled by mofa-fm.
    (
        "pipeline-guard",
        "pipeline-guard",
        include_str!("../../app-skills/pipeline-guard/SKILL.md"),
        include_str!("../../app-skills/pipeline-guard/manifest.json"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_app_skills_is_non_empty() {
        assert!(!BUNDLED_APP_SKILLS.is_empty());
    }

    #[test]
    fn bundled_app_skills_entries_have_non_empty_fields() {
        for &(dir_name, binary_name, skill_md, manifest_json) in BUNDLED_APP_SKILLS {
            assert!(!dir_name.is_empty(), "dir_name must not be empty");
            assert!(!binary_name.is_empty(), "binary_name must not be empty");
            assert!(!skill_md.is_empty(), "skill_md must not be empty");
            assert!(!manifest_json.is_empty(), "manifest_json must not be empty");
        }
    }

    #[test]
    fn platform_skills_is_non_empty() {
        assert!(!PLATFORM_SKILLS.is_empty());
    }

    #[test]
    fn platform_skills_entries_have_non_empty_fields() {
        for &(dir_name, binary_name, skill_md, manifest_json) in PLATFORM_SKILLS {
            assert!(!dir_name.is_empty(), "dir_name must not be empty");
            assert!(!binary_name.is_empty(), "binary_name must not be empty");
            assert!(!skill_md.is_empty(), "skill_md must not be empty");
            assert!(!manifest_json.is_empty(), "manifest_json must not be empty");
        }
    }

    #[test]
    fn manifest_json_entries_are_valid_json() {
        for &(dir_name, _, _, manifest_json) in
            BUNDLED_APP_SKILLS.iter().chain(PLATFORM_SKILLS.iter())
        {
            let result: Result<serde_json::Value, _> = serde_json::from_str(manifest_json);
            assert!(
                result.is_ok(),
                "manifest.json for '{dir_name}' is not valid JSON: {}",
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn skill_md_entries_contain_frontmatter_or_heading() {
        // Some SKILL.md files use YAML frontmatter (---), others use plain markdown.
        // All must contain at least a markdown heading (#).
        for &(dir_name, _, skill_md, _) in BUNDLED_APP_SKILLS.iter().chain(PLATFORM_SKILLS.iter()) {
            let has_frontmatter = skill_md.contains("---");
            let has_heading = skill_md.contains('#');
            assert!(
                has_frontmatter || has_heading,
                "SKILL.md for '{dir_name}' should contain frontmatter '---' or a markdown heading '#'"
            );
        }
    }
}
