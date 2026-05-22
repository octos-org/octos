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
//! M14-C runtime wiring: `spawn` / `spawn_agent` consult this registry
//! when a child declares `role`, native review specialists stamp the
//! resolved runtime template onto their `TaskSupervisor` task records,
//! and AppUI receives the resulting role/source/artifact/runtime-policy
//! projection through `task/list` and `task/updated`.

use std::fmt;
use std::str::FromStr;

use serde_json::{Value, json};

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
    /// The tools the role advertises as in-budget. Each entry is
    /// either a group identifier (e.g. `"group:search"`, matching
    /// `tools::policy::TOOL_GROUPS`) OR a single exact tool name
    /// (e.g. `"read_file"`). This matches the shape `ToolPolicy.allow`
    /// already accepts, so a downstream caller can feed this slice
    /// straight into the policy. Mixing the two is intentional:
    /// read-only roles need to grant `read_file` WITHOUT pulling in
    /// the mutating tools that `group:fs` expands to (`write_file`,
    /// `apply_patch`, `edit_file`, `diff_edit`).
    pub allowed_tools: &'static [&'static str],
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

    /// True if `entry` is advertised as in-budget for this role.
    /// Pure-string equality — the registry stores either group
    /// identifiers (`"group:search"`) or exact tool names
    /// (`"read_file"`). Callers asking "is `group:fs` in the budget?"
    /// should pass `"group:fs"` literally; callers asking "is
    /// `read_file` in the budget?" should pass `"read_file"`. To
    /// expand groups to their tool members use
    /// `tools::policy::tool_group_info`.
    pub fn permits(&self, entry: &str) -> bool {
        self.allowed_tools.contains(&entry)
    }

    /// Return the template tool budget in the owned shape expected by
    /// `SpawnTool::Input.allowed_tools`.
    pub fn allowed_tools_vec(&self) -> Vec<String> {
        self.allowed_tools
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect()
    }

    /// Infer a canonical role from Codex-style `agent_type` values.
    ///
    /// Codex prompts commonly ask for generic `worker`, `reviewer`,
    /// `tester`, or `explorer` agents. This helper keeps that vocabulary
    /// model-visible while resolving the actual role through the backend
    /// registry.
    pub fn for_codex_agent_type(agent_type: &str) -> Option<&'static RoleTemplate> {
        let normalized = agent_type.trim().to_ascii_lowercase();
        let role = match normalized.as_str() {
            "review"
            | "reviewer"
            | "code_reviewer"
            | "code-reviewer"
            | "repo_reviewer"
            | "repository_reviewer" => ROLE_REVIEWER,
            "worker"
            | "implementer"
            | "implementation_worker"
            | "implementation-worker"
            | "coder"
            | "developer" => ROLE_IMPLEMENTER,
            "test"
            | "tester"
            | "test_worker"
            | "test-worker"
            | "verification_worker"
            | "verifier" => ROLE_TEST_WORKER,
            "explore" | "explorer" | "analyst" | "codebase_analyst" | "read_only" | "read-only" => {
                ROLE_EXPLORER
            }
            other => other,
        };
        Self::for_name(role)
    }

    /// Backend-owned runtime stamp for task/list and task/updated
    /// projection. The stamp is intentionally descriptive rather than a
    /// command surface: it records which server template resolved the
    /// child role, tool budget, sandbox, approval, and model preference.
    pub fn runtime_policy_stamp(
        &self,
        source: &str,
        backend: &str,
        requested_model: Option<&str>,
    ) -> Value {
        json!({
            "template_id": "m14-c.subagent_runtime.v1",
            "role": self.name,
            "role_name": self.display_name,
            "source": source,
            "backend": backend,
            "sandbox": self.default_sandbox_mode,
            "approval_policy": self.default_approval_policy,
            "tool_policy_id": format!("role:{}", self.name),
            "allowed_tools": self.allowed_tools,
            "model_preference": self.model_preference.as_str(),
            "requested_model": requested_model,
        })
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
        // Every entry below is either an exact read-only tool name or
        // a group that contains ONLY read-only tools — because if a
        // future caller pipes `allowed_tools` straight into
        // `ToolPolicy.allow`, group expansion would silently grant
        // every member tool. The set below deliberately excludes:
        // `group:fs` (`write_file`/`edit_file`/...), `group:memory`
        // (`save_memory`), `group:web` (`browser` persists
        // screenshots), and `group:research` (`deep_crawl` /
        // `search` persist crawled pages and research dirs to disk).
        // Reviewers stay strictly stateless.
        allowed_tools: &[
            "read_file",
            "group:search",
            "web_search",
            "web_fetch",
            "recall_memory",
            "synthesize_research",
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
        // test_worker for verification. `group:fs` is appropriate
        // here because the role IS supposed to mutate files.
        allowed_tools: &[
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
        // spawn further children. Same as reviewer: explicit
        // `read_file` + `recall_memory` so `group:fs` / `group:memory`
        // mutating tools do NOT leak in.
        allowed_tools: &[
            "read_file",
            "group:search",
            "group:runtime",
            "recall_memory",
        ],
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
        // Explorers are STRICTLY READ-ONLY. Same caveats as reviewer:
        // every entry is either an exact read-only tool name or a
        // group containing only read-only tools. `group:web` is NOT
        // used (browser persists screenshots); `group:research` is
        // NOT used (deep_crawl / search persist crawled pages). The
        // explorer can still fetch a page via `web_fetch` and
        // summarise via `synthesize_research` without writing files.
        allowed_tools: &[
            "read_file",
            "group:search",
            "web_search",
            "web_fetch",
            "recall_memory",
            "synthesize_research",
        ],
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
    use serde_json::json;

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

    /// Guard: reviewer is read-only. The budget MUST NOT include
    /// `group:fs` (which expands to write_file / apply_patch /
    /// edit_file / diff_edit), `group:memory` (which expands to
    /// save_memory), or `group:runtime` / `group:sessions`. Catches
    /// the codex P1 from review: granting `group:fs` to a role
    /// documented as non-mutating silently smuggles in write tools
    /// once the registry is wired into `ToolPolicy.allow`.
    #[test]
    fn reviewer_is_read_only() {
        let tpl = RoleTemplate::for_name(ROLE_REVIEWER).expect("reviewer must be registered");
        // Permitted: explicit read tools + non-mutating groups only.
        assert!(tpl.permits("read_file"));
        assert!(tpl.permits("group:search"));
        assert!(tpl.permits("web_search"));
        assert!(tpl.permits("web_fetch"));
        assert!(tpl.permits("recall_memory"));
        assert!(tpl.permits("synthesize_research"));
        // Denied: every mutating group AND every group that contains
        // at least one disk-writing tool. The reviewer prompt promises
        // "do not edit files, do not run the test suite, do not spawn
        // further agents" — group expansion would silently smuggle in
        // write tools that violate the promise.
        assert!(!tpl.permits("group:fs"));
        assert!(!tpl.permits("group:memory"));
        assert!(!tpl.permits("group:runtime"));
        assert!(!tpl.permits("group:sessions"));
        assert!(!tpl.permits("group:web"));
        assert!(!tpl.permits("group:research"));
        assert!(!tpl.permits("save_memory"));
        assert!(!tpl.permits("write_file"));
        assert!(!tpl.permits("edit_file"));
        assert!(!tpl.permits("browser"));
        assert!(!tpl.permits("deep_crawl"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_NONE);
        assert_eq!(tpl.default_approval_policy, APPROVAL_NEVER);
        assert_eq!(tpl.model_preference, ModelPreference::Coding);
    }

    /// Guard: implementer is the only role with both runtime AND
    /// sessions. If a future template adds runtime to test_worker
    /// without dropping it from implementer this still passes — what
    /// it actually pins is that implementer cannot regress out of the
    /// runtime+sessions budget. Implementer DOES advertise `group:fs`
    /// because the role's documented purpose is to mutate files.
    #[test]
    fn implementer_has_runtime_and_sessions() {
        let tpl = RoleTemplate::for_name(ROLE_IMPLEMENTER).expect("implementer must be registered");
        assert!(tpl.permits("group:fs"));
        assert!(tpl.permits("group:runtime"));
        assert!(tpl.permits("group:sessions"));
        assert!(tpl.permits("group:memory"));
        assert!(!tpl.permits("group:research"));
        assert!(!tpl.permits("group:web"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_AUTO);
        assert_eq!(tpl.default_approval_policy, APPROVAL_ASK);
        assert_eq!(tpl.model_preference, ModelPreference::Coding);
    }

    /// Guard: test_worker can run commands but cannot edit files or
    /// spawn further children. Same safety property as reviewer:
    /// `group:fs` and `group:memory` MUST NOT appear in the budget
    /// because they expand to mutating tools the role's prompt
    /// explicitly forbids.
    #[test]
    fn test_worker_runs_commands_but_does_not_spawn() {
        let tpl = RoleTemplate::for_name(ROLE_TEST_WORKER).expect("test_worker must be registered");
        assert!(tpl.permits("group:runtime"));
        assert!(tpl.permits("read_file"));
        assert!(tpl.permits("recall_memory"));
        assert!(!tpl.permits("group:fs"));
        assert!(!tpl.permits("group:memory"));
        assert!(!tpl.permits("group:sessions"));
        assert!(!tpl.permits("group:web"));
        assert!(!tpl.permits("save_memory"));
        assert!(!tpl.permits("write_file"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_AUTO);
        assert_eq!(tpl.default_approval_policy, APPROVAL_ASK);
        assert_eq!(tpl.model_preference, ModelPreference::Analyst);
    }

    /// Guard: explorer is strictly read-only AND cheap-lane. Pins
    /// both the no-runtime / no-sessions / no-mutating-groups budget
    /// AND the model preference, because the UX uses the cheap-lane
    /// hint to route fanout.
    #[test]
    fn explorer_is_strictly_read_only_and_cheap() {
        let tpl = RoleTemplate::for_name(ROLE_EXPLORER).expect("explorer must be registered");
        assert!(tpl.permits("read_file"));
        assert!(tpl.permits("group:search"));
        assert!(tpl.permits("web_search"));
        assert!(tpl.permits("web_fetch"));
        assert!(tpl.permits("recall_memory"));
        assert!(tpl.permits("synthesize_research"));
        assert!(!tpl.permits("group:fs"));
        assert!(!tpl.permits("group:memory"));
        assert!(!tpl.permits("group:runtime"));
        assert!(!tpl.permits("group:sessions"));
        assert!(!tpl.permits("group:web"));
        assert!(!tpl.permits("group:research"));
        assert!(!tpl.permits("save_memory"));
        assert!(!tpl.permits("write_file"));
        assert!(!tpl.permits("browser"));
        assert!(!tpl.permits("deep_crawl"));
        assert_eq!(tpl.default_sandbox_mode, SANDBOX_NONE);
        assert_eq!(tpl.default_approval_policy, APPROVAL_NEVER);
        assert_eq!(tpl.model_preference, ModelPreference::Cheap);
    }

    /// Safety guard (post codex review): roles whose `prompt_prefix`
    /// promises "do not edit" / "never mutates state" must not
    /// transitively grant a mutating tool through a coarse group.
    /// The set below covers every `tools::policy::TOOL_GROUPS` entry
    /// whose expansion contains at least one tool that writes to
    /// disk, mutates session/profile state, or spawns further work:
    ///
    /// - `group:fs` -> write_file / apply_patch / edit_file / diff_edit
    /// - `group:memory` -> save_memory
    /// - `group:runtime` -> shell / exec_command / write_stdin
    /// - `group:sessions` -> spawn / spawn_agent / ...
    /// - `group:admin` -> manage_skills / configure_tool / model_check
    /// - `group:media` -> mofa_* / fm_tts (write generated media)
    /// - `group:web` -> browser (persists screenshots to disk)
    /// - `group:research` -> deep_crawl / search (persist crawled pages)
    /// - `group:delegated` -> delegate_task / spawn / send_message /
    ///   save_memory / execute_code (every kind of side effect at once)
    ///
    /// Read-only roles (`reviewer`, `explorer`) MUST advertise none
    /// of these; `test_worker` is allowed `group:runtime` because
    /// running commands IS the role's job. This catches the codex P1
    /// (smuggling `group:fs` into a non-mutating role) and the
    /// follow-up codex P2 (`group:research` / `group:web` smuggling
    /// in `deep_crawl` / `browser` disk writes).
    #[test]
    fn read_only_roles_do_not_advertise_mutating_groups() {
        const ALL_MUTATING_GROUPS: &[&str] = &[
            "group:fs",
            "group:memory",
            "group:runtime",
            "group:sessions",
            "group:admin",
            "group:media",
            "group:web",
            "group:research",
            "group:delegated",
        ];
        // Reviewer + explorer are documented as fully read-only.
        for name in [ROLE_REVIEWER, ROLE_EXPLORER] {
            let tpl = RoleTemplate::for_name(name).unwrap();
            for group in ALL_MUTATING_GROUPS {
                assert!(
                    !tpl.permits(group),
                    "{name} advertises mutating group {group:?} but its prompt_prefix \
                     promises not to mutate state",
                );
            }
        }
        // test_worker is allowed `group:runtime` (running commands
        // IS the role's job) but MUST NOT advertise any other
        // mutating group.
        let test_worker = RoleTemplate::for_name(ROLE_TEST_WORKER).unwrap();
        for group in ALL_MUTATING_GROUPS {
            if *group == "group:runtime" {
                continue;
            }
            assert!(
                !test_worker.permits(group),
                "test_worker advertises mutating group {group:?} but the role docs forbid \
                 anything beyond running test/lint/build commands",
            );
        }
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
                !tpl.allowed_tools.is_empty(),
                "{} allowed_tools must be non-empty",
                tpl.name
            );
        }
    }

    /// Guard: every advertised tool entry is either a `group:`
    /// identifier OR a non-empty bare tool name. Catches typos like
    /// `"Group:fs"`, `" group:fs"`, or an empty `""` slot. The
    /// `tools::policy::entry_matches` function treats unknown strings
    /// as exact-name matches, so silently mis-typed entries would
    /// never match and the role would lose tools without a build
    /// error.
    #[test]
    fn every_advertised_tool_entry_is_well_formed() {
        for tpl in RoleTemplate::all() {
            for entry in tpl.allowed_tools {
                assert!(
                    !entry.is_empty(),
                    "{} advertises an empty tool entry",
                    tpl.name
                );
                assert!(
                    !entry.contains(' '),
                    "{} advertises {:?} which contains whitespace",
                    tpl.name,
                    entry
                );
                if let Some(rest) = entry.strip_prefix("group:") {
                    assert!(
                        !rest.is_empty(),
                        "{} advertises bare `group:` with no body",
                        tpl.name
                    );
                }
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

    #[test]
    fn codex_agent_type_aliases_resolve_to_backend_roles() {
        assert_eq!(
            RoleTemplate::for_codex_agent_type("review").map(|template| template.name),
            Some(ROLE_REVIEWER)
        );
        assert_eq!(
            RoleTemplate::for_codex_agent_type("worker").map(|template| template.name),
            Some(ROLE_IMPLEMENTER)
        );
        assert_eq!(
            RoleTemplate::for_codex_agent_type("tester").map(|template| template.name),
            Some(ROLE_TEST_WORKER)
        );
        assert_eq!(
            RoleTemplate::for_codex_agent_type("read-only").map(|template| template.name),
            Some(ROLE_EXPLORER)
        );
        assert!(RoleTemplate::for_codex_agent_type("planner").is_none());
    }

    #[test]
    fn runtime_policy_stamp_is_template_shaped() {
        let tpl = RoleTemplate::for_name(ROLE_REVIEWER).expect("reviewer");
        let stamp = tpl.runtime_policy_stamp("model", "builtin", Some("fast-coding"));

        assert_eq!(stamp["template_id"], "m14-c.subagent_runtime.v1");
        assert_eq!(stamp["role"], ROLE_REVIEWER);
        assert_eq!(stamp["role_name"], "Reviewer");
        assert_eq!(stamp["source"], "model");
        assert_eq!(stamp["backend"], "builtin");
        assert_eq!(stamp["sandbox"], SANDBOX_NONE);
        assert_eq!(stamp["approval_policy"], APPROVAL_NEVER);
        assert_eq!(stamp["tool_policy_id"], "role:reviewer");
        assert_eq!(stamp["model_preference"], "coding");
        assert_eq!(stamp["requested_model"], "fast-coding");
        assert_eq!(
            stamp["allowed_tools"],
            json!([
                "read_file",
                "group:search",
                "web_search",
                "web_fetch",
                "recall_memory",
                "synthesize_research"
            ])
        );
    }
}
