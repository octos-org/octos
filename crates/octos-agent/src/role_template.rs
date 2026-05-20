//! Role templates for subagent / spawn-only child tasks.
//!
//! Issue #971 (M14-C). The wire `TaskListEntry.role` field (PR #1103) and
//! `BackgroundTask.role` projection (PR #1109/#1113) already carry a role
//! string forward from spawn-time to UX hydration, but the set of legal
//! values has lived as a free-form `Option<String>` agreed in comments
//! across `octos-core::ui_protocol`, `octos-agent::task_supervisor`, and
//! the orchestrator paths in `octos-cli::api`. Without a typed registry
//! every new caller risks coining a slightly different spelling
//! (`"review"` vs `"reviewer"`, `"test"` vs `"test_worker"`) and the UX
//! has no place to look up role metadata (allowed tool budget, default
//! sandbox + approval policy, model preference, prompt prefix).
//!
//! This module is the single source of truth for the four canonical
//! roles M14-C targets:
//!
//! `reviewer` — repository/code reviewer that walks a diff and emits
//! findings. `implementer` — implementation worker that edits files to
//! satisfy a task. `test_worker` — verification worker that runs the
//! test/lint/build suite and reports failures. `explorer` — read-only
//! codebase analyst that gathers context for an upstream planner.
//!
//! Scope cap (per #971 partial PR plan): this PR ships the type shape,
//! the lookup, and guard tests pinning the canonical names + tool-group
//! membership. Wiring callers (spawn paths in `task_supervisor`,
//! `agent_orchestrator`, `specialist_runner`, ...) to actually CONSULT
//! the registry instead of hard-coding role strings is follow-on work.

use std::fmt;
use std::str::FromStr;

/// Canonical name for the repository / code reviewer role.
pub const ROLE_REVIEWER: &str = "reviewer";
/// Canonical name for the implementation worker role.
pub const ROLE_IMPLEMENTER: &str = "implementer";
/// Canonical name for the test / verification worker role.
pub const ROLE_TEST_WORKER: &str = "test_worker";
/// Canonical name for the read-only codebase analyst role.
pub const ROLE_EXPLORER: &str = "explorer";

/// Sentinel for `RoleTemplate::default_sandbox_mode` meaning "use the
/// session's auto-detected sandbox" (matches `SandboxMode::Auto`).
pub const SANDBOX_AUTO: &str = "auto";
/// Sentinel for `RoleTemplate::default_sandbox_mode` meaning "the role
/// is read-only and does not need an exec sandbox" (matches
/// `SandboxMode::None`).
pub const SANDBOX_NONE: &str = "none";

/// Sentinel for `RoleTemplate::default_approval_policy` meaning "ask
/// the upstream client before exec" (matches `ApprovalPolicy::Ask`).
pub const APPROVAL_ASK: &str = "ask";
/// Sentinel for `RoleTemplate::default_approval_policy` meaning "never
/// prompt; reject ask-required commands at the tool boundary" (matches
/// `ApprovalPolicy::Never`).
pub const APPROVAL_NEVER: &str = "never";

/// Soft model preference hint. Templates set this so the orchestrator
/// can route review / implementation children to a coding-grade lane
/// while letting explorers fall onto the cheap analyst lane. Treated as
/// advisory — concrete model resolution still flows through
/// `ModelStylesheet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelPreference {
    /// Coding / reasoning grade model (e.g. `claude-opus-4-7`).
    Coding,
    /// Lighter analyst-grade model that prioritises throughput / cost.
    Analyst,
    /// Cheap / fast model suitable for read-only fanout.
    Cheap,
}

impl ModelPreference {
    /// Stable string representation used in metadata payloads and
    /// `runtime_policy_stamp.model_preference`. Round-trips via
    /// `ModelPreference::from_str`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ModelPreference::Coding => "coding",
            ModelPreference::Analyst => "analyst",
            ModelPreference::Cheap => "cheap",
        }
    }

    /// Parse the stable string representation. Unknown values map to
    /// `None` — callers should treat that as "no preference". Mirrors
    /// the `FromStr` impl but returns `Option` so callers can keep a
    /// soft-fallback "no preference" path without converting an error.
    pub fn parse(value: &str) -> Option<Self> {
        Self::from_str(value).ok()
    }
}

impl FromStr for ModelPreference {
    type Err = UnknownModelPreference;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "coding" => Ok(ModelPreference::Coding),
            "analyst" => Ok(ModelPreference::Analyst),
            "cheap" => Ok(ModelPreference::Cheap),
            other => Err(UnknownModelPreference(other.to_owned())),
        }
    }
}

/// Error returned by `<ModelPreference as FromStr>::from_str` when the
/// input is not one of the registered preference names. Carries the
/// offending input so callers can surface diagnostic context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownModelPreference(pub String);

impl fmt::Display for UnknownModelPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown model preference: {:?}", self.0)
    }
}

impl std::error::Error for UnknownModelPreference {}

impl fmt::Display for ModelPreference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registered subagent role: typed metadata clients consult to render
/// UX and the orchestrator consults to gate tool budget + sandbox.
///
/// All fields are `'static` so the registry is a `const` table. To
/// extend the registry, add a `RoleTemplate` to `ROLE_TEMPLATES` below
/// and update the guard tests so the canonical name + tool group
/// membership are pinned.
#[derive(Debug, Clone, Copy)]
pub struct RoleTemplate {
    /// Canonical, machine-readable role identifier. Must match one of
    /// the `ROLE_*` constants in this module. Pinned by guard tests so
    /// downstream code can rely on the spelling.
    pub name: &'static str,
    /// Human-readable role label suitable for inline UX badges
    /// (e.g. "Reviewer", "Test Worker").
    pub display_name: &'static str,
    /// One-line description of what the role does. Bounded so it can
    /// be surfaced in tooltips without truncation.
    pub description: &'static str,
    /// The tool groups (matching `tools::policy::TOOL_GROUPS` names)
    /// the role advertises as in-budget. The runtime is free to
    /// further restrict via `ToolPolicy`; this is the role's declared
    /// surface, not the only enforcement.
    pub allowed_tool_groups: &'static [&'static str],
    /// Default sandbox mode the role suggests. One of `SANDBOX_AUTO`
    /// or `SANDBOX_NONE`. Templates intentionally do not advertise
    /// "bwrap" / "docker" — backend selection is environment-driven.
    pub default_sandbox_mode: &'static str,
    /// Default approval policy the role suggests. One of
    /// `APPROVAL_ASK` or `APPROVAL_NEVER`.
    pub default_approval_policy: &'static str,
    /// Soft model preference. Advisory only — the orchestrator still
    /// resolves the concrete provider via the stylesheet.
    pub model_preference: ModelPreference,
    /// Bounded prompt prefix the orchestrator prepends to the system
    /// prompt for this role. Kept short (< ~600 chars) so it does not
    /// crowd the user-supplied system prompt or the per-task brief.
    pub prompt_prefix: &'static str,
}

impl RoleTemplate {
    /// Look up a role template by its canonical name. Returns `None`
    /// for unknown values so callers can defensively reject drift
    /// instead of silently defaulting.
    pub fn for_name(name: &str) -> Option<&'static RoleTemplate> {
        ROLE_TEMPLATES.iter().find(|tpl| tpl.name == name)
    }

    /// Slice of every registered role template, in stable declaration
    /// order. UX surfaces (e.g. the spawn-role dropdown in the admin
    /// dashboard) can iterate this for free.
    pub fn all() -> &'static [RoleTemplate] {
        ROLE_TEMPLATES
    }

    /// True if `tool_group` is advertised as in-budget for this role.
    /// Pure-string equality on the group name — the registry stores
    /// strings that match `tools::policy::TOOL_GROUPS` names verbatim.
    pub fn permits_group(&self, tool_group: &str) -> bool {
        self.allowed_tool_groups.contains(&tool_group)
    }
}

/// All registered role templates. Keep this list aligned with the
/// guard tests at the bottom of this module — any drift in the name
/// set or tool-group budget is a load-bearing change that downstream
/// `TaskListEntry.role` consumers care about.
const ROLE_TEMPLATES: &[RoleTemplate] = &[
    RoleTemplate {
        name: ROLE_REVIEWER,
        display_name: "Reviewer",
        description: "Repository / code reviewer. Walks the diff and emits structured findings; \
                      does not mutate workspace files.",
        // Reviewers READ files, search, and may fetch reference docs.
        // No write/edit, no shell, no spawning further children.
        allowed_tool_groups: &[
            "group:fs",
            "group:search",
            "group:web",
            "group:memory",
            "group:research",
        ],
        default_sandbox_mode: SANDBOX_NONE,
        default_approval_policy: APPROVAL_NEVER,
        model_preference: ModelPreference::Coding,
        prompt_prefix: "You are a code reviewer. Read the diff and the surrounding context, \
                        then emit findings as a bounded list. Do not edit files, do not run \
                        the test suite, do not spawn further agents. Prefer concrete file:line \
                        references and explain the WHY of each finding.",
    },
    RoleTemplate {
        name: ROLE_IMPLEMENTER,
        display_name: "Implementer",
        description: "Implementation worker. Edits workspace files to satisfy a bounded task; \
                      may run shell commands inside the session sandbox.",
        // Implementers need fs read/write, search, shell, and the
        // delegated-child sessions group so they can fan out to
        // test_worker for verification.
        allowed_tool_groups: &[
            "group:fs",
            "group:search",
            "group:runtime",
            "group:sessions",
            "group:memory",
        ],
        default_sandbox_mode: SANDBOX_AUTO,
        default_approval_policy: APPROVAL_ASK,
        model_preference: ModelPreference::Coding,
        prompt_prefix: "You are an implementation worker. Make the smallest change that \
                        satisfies the brief. Read before writing, prefer Edit over Write, \
                        and stop once the change compiles and the relevant tests pass. \
                        Surface any out-of-scope drift in the final summary instead of \
                        silently expanding the patch.",
    },
    RoleTemplate {
        name: ROLE_TEST_WORKER,
        display_name: "Test Worker",
        description: "Verification worker. Runs the test / lint / build suite the upstream \
                      task implies and reports concrete failures.",
        // Test workers run commands and read files. They should not
        // edit files (a fix is the implementer's job) and should not
        // spawn further children.
        allowed_tool_groups: &["group:fs", "group:search", "group:runtime", "group:memory"],
        default_sandbox_mode: SANDBOX_AUTO,
        default_approval_policy: APPROVAL_ASK,
        model_preference: ModelPreference::Analyst,
        prompt_prefix: "You are a verification worker. Run the test, lint, and build commands \
                        implied by the brief. Do not edit source files. Report concrete \
                        failures with the offending command, exit code, and the most \
                        diagnostic 20-40 lines of output.",
    },
    RoleTemplate {
        name: ROLE_EXPLORER,
        display_name: "Explorer",
        description: "Read-only codebase analyst. Gathers context (files, call sites, prior \
                      art) for an upstream planner; never mutates state.",
        // Explorers are READ-ONLY: fs (read), search, optional web.
        // No runtime, no sessions, no memory writes.
        allowed_tool_groups: &["group:fs", "group:search", "group:web", "group:research"],
        default_sandbox_mode: SANDBOX_NONE,
        default_approval_policy: APPROVAL_NEVER,
        model_preference: ModelPreference::Cheap,
        prompt_prefix: "You are a codebase explorer. Read files, search, and summarise. Do \
                        not edit, do not run commands, do not spawn further agents. Return \
                        a bounded summary plus absolute file paths the upstream planner \
                        should consult next.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard: the four canonical role names M14-C promises must remain
    /// the EXACT spelling the wire schema, `TaskListEntry.role` doc
    /// comment, and `BackgroundTask.role` projection comment agreed
    /// on. Drift here breaks every downstream consumer.
    #[test]
    fn registry_exposes_the_four_canonical_role_names() {
        let names: Vec<&'static str> = RoleTemplate::all().iter().map(|tpl| tpl.name).collect();
        assert_eq!(
            names,
            vec!["reviewer", "implementer", "test_worker", "explorer"],
            "M14-C canonical role names drifted; update guard + wire docs together"
        );
    }

    /// Guard: `for_name` returns the same struct as iterating `all()`.
    /// Catches a future refactor that adds e.g. a HashMap index out of
    /// sync with the const slice.
    #[test]
    fn for_name_looks_up_each_registered_role() {
        for tpl in RoleTemplate::all() {
            let fetched = RoleTemplate::for_name(tpl.name)
                .unwrap_or_else(|| panic!("for_name failed to find {}", tpl.name));
            assert_eq!(fetched.name, tpl.name);
            assert_eq!(fetched.display_name, tpl.display_name);
            assert_eq!(fetched.default_sandbox_mode, tpl.default_sandbox_mode);
            assert_eq!(fetched.default_approval_policy, tpl.default_approval_policy);
        }
    }

    /// Guard: unknown role names return `None` instead of falling back
    /// to a default template. The TaskListEntry.role field is
    /// `Option<String>`; the caller is expected to handle the unknown
    /// case explicitly rather than receive a spoofed reviewer.
    #[test]
    fn for_name_returns_none_for_unknown_role() {
        assert!(RoleTemplate::for_name("review").is_none());
        assert!(RoleTemplate::for_name("Reviewer").is_none());
        assert!(RoleTemplate::for_name("").is_none());
        assert!(RoleTemplate::for_name("planner").is_none());
    }

    /// Guard: reviewer is read-only. If anyone adds `group:runtime` or `group:fs` write-implying groups to reviewer's budget, this breaks — and the matching `default_sandbox_mode = SANDBOX_NONE` + `default_approval_policy = APPROVAL_NEVER` assertion catches the policy half of the drift.
    #[test]
    fn reviewer_is_read_only() {
        let tpl = RoleTemplate::for_name(ROLE_REVIEWER).expect("reviewer must be registered");
        assert!(tpl.permits_group("group:fs"));
        assert!(tpl.permits_group("group:search"));
        assert!(!tpl.permits_group("group:runtime"));
        assert!(!tpl.permits_group("group:sessions"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_NONE);
        assert_eq!(tpl.default_approval_policy, APPROVAL_NEVER);
        assert_eq!(tpl.model_preference, ModelPreference::Coding);
    }

    /// Guard: implementer is the only role with both runtime AND
    /// sessions. If a future template adds runtime to test_worker
    /// without dropping it from implementer this still passes — what
    /// it actually pins is that implementer cannot regress out of the
    /// runtime+sessions budget.
    #[test]
    fn implementer_has_runtime_and_sessions() {
        let tpl = RoleTemplate::for_name(ROLE_IMPLEMENTER).expect("implementer must be registered");
        assert!(tpl.permits_group("group:fs"));
        assert!(tpl.permits_group("group:runtime"));
        assert!(tpl.permits_group("group:sessions"));
        assert!(!tpl.permits_group("group:research"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_AUTO);
        assert_eq!(tpl.default_approval_policy, APPROVAL_ASK);
        assert_eq!(tpl.model_preference, ModelPreference::Coding);
    }

    /// Guard: test_worker can run commands but cannot edit files or
    /// spawn further children. Catches a refactor that conflates
    /// implementer + test_worker into a single fix-and-verify role.
    #[test]
    fn test_worker_runs_commands_but_does_not_spawn() {
        let tpl = RoleTemplate::for_name(ROLE_TEST_WORKER).expect("test_worker must be registered");
        assert!(tpl.permits_group("group:runtime"));
        assert!(tpl.permits_group("group:fs"));
        assert!(!tpl.permits_group("group:sessions"));
        assert!(!tpl.permits_group("group:web"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_AUTO);
        assert_eq!(tpl.default_approval_policy, APPROVAL_ASK);
        assert_eq!(tpl.model_preference, ModelPreference::Analyst);
    }

    /// Guard: explorer is strictly read-only AND cheap-lane. Pins both
    /// the no-runtime / no-sessions budget AND the model preference,
    /// because the UX uses the cheap-lane hint to route fanout.
    #[test]
    fn explorer_is_strictly_read_only_and_cheap() {
        let tpl = RoleTemplate::for_name(ROLE_EXPLORER).expect("explorer must be registered");
        assert!(tpl.permits_group("group:fs"));
        assert!(tpl.permits_group("group:search"));
        assert!(!tpl.permits_group("group:runtime"));
        assert!(!tpl.permits_group("group:sessions"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_NONE);
        assert_eq!(tpl.default_approval_policy, APPROVAL_NEVER);
        assert_eq!(tpl.model_preference, ModelPreference::Cheap);
    }

    /// Guard: every template advertises a non-empty prompt prefix and
    /// a non-empty tool budget. A template with an empty budget is a
    /// misconfiguration — the role would be unable to do anything.
    #[test]
    fn every_template_has_prefix_and_budget() {
        for tpl in RoleTemplate::all() {
            assert!(
                !tpl.prompt_prefix.is_empty(),
                "{} prompt_prefix must be non-empty",
                tpl.name
            );
            assert!(
                !tpl.display_name.is_empty(),
                "{} display_name must be non-empty",
                tpl.name
            );
            assert!(
                !tpl.description.is_empty(),
                "{} description must be non-empty",
                tpl.name
            );
            assert!(
                !tpl.allowed_tool_groups.is_empty(),
                "{} allowed_tool_groups must be non-empty",
                tpl.name
            );
        }
    }

    /// Guard: every advertised tool group string starts with
    /// `group:`. The matching `tools::policy::TOOL_GROUPS` table
    /// already uses that prefix; if a template ever advertises a bare
    /// tool name it would silently never match.
    #[test]
    fn every_advertised_group_uses_group_prefix() {
        for tpl in RoleTemplate::all() {
            for group in tpl.allowed_tool_groups {
                assert!(
                    group.starts_with("group:"),
                    "{} advertises {:?} which is not a group: identifier",
                    tpl.name,
                    group
                );
            }
        }
    }

    /// Guard: sandbox + approval sentinels stay in the known set.
    /// Anything outside `SANDBOX_AUTO|SANDBOX_NONE` /
    /// `APPROVAL_ASK|APPROVAL_NEVER` would force callers to grow
    /// extra branches and is not what M14-C agreed to ship.
    #[test]
    fn every_template_uses_known_sandbox_and_approval_sentinels() {
        for tpl in RoleTemplate::all() {
            assert!(
                matches!(tpl.default_sandbox_mode, SANDBOX_AUTO | SANDBOX_NONE),
                "{} uses unknown sandbox mode {:?}",
                tpl.name,
                tpl.default_sandbox_mode
            );
            assert!(
                matches!(tpl.default_approval_policy, APPROVAL_ASK | APPROVAL_NEVER),
                "{} uses unknown approval policy {:?}",
                tpl.name,
                tpl.default_approval_policy
            );
        }
    }

    /// Guard: `ModelPreference::as_str` round-trips through both
    /// `parse` and the `FromStr` impl for every registered variant.
    #[test]
    fn model_preference_round_trips() {
        for pref in [
            ModelPreference::Coding,
            ModelPreference::Analyst,
            ModelPreference::Cheap,
        ] {
            let s = pref.as_str();
            assert_eq!(ModelPreference::parse(s), Some(pref));
            assert_eq!(s.parse::<ModelPreference>().ok(), Some(pref));
            assert_eq!(format!("{pref}"), s);
        }
        assert_eq!(ModelPreference::parse("nope"), None);
        assert_eq!(ModelPreference::parse(""), None);
        assert!("nope".parse::<ModelPreference>().is_err());
    }
}
