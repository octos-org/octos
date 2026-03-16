//! Built-in system skills that are always available.
//!
//! These are infrastructure skills that cannot be removed — they teach the agent
//! how to manage the platform itself (scheduling, skill installation, skill creation).

/// (name, content) pairs for built-in system skills.
pub const BUILTIN_SKILLS: &[(&str, &str)] = &[
    ("cron", include_str!("../skills/cron/SKILL.md")),
    (
        "skill-store",
        include_str!("../skills/skill-store/SKILL.md"),
    ),
    (
        "skill-creator",
        include_str!("../skills/skill-creator/SKILL.md"),
    ),
];
