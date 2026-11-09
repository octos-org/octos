//! Tool policy system with allow/deny lists, groups, and wildcards.

use metrics::counter;
use serde::{Deserialize, Serialize};

use super::robot_groups;

/// Outcome of `ToolPolicy::evaluate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Tool is permitted.
    Allow,
    /// Tool is denied. `reason` is the metric label emitted for observability.
    Deny { reason: &'static str },
}

/// Metric counter name for policy denials.
pub const POLICY_DENIAL_COUNTER: &str = "octos_tool_policy_denial_total";

/// Deny reason label used when a robot-tier group gates a tool.
pub const ROBOT_TIER_GATE_REASON: &str = "robot_tier_gate";

/// Deny reason label used for a non-robot policy deny.
pub const GENERIC_DENY_REASON: &str = "policy_deny";

/// Tool policy with allow/deny lists and tag-based filtering. Deny always wins over allow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Tools, groups, or wildcards to allow. Empty = allow all.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools, groups, or wildcards to deny. Always wins over allow.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Required tags: only tools declaring at least one matching tag are
    /// visible. Empty = no tag filtering. Composable with allow/deny (deny
    /// still wins). Untagged tools FAIL a non-empty filter (fail closed) —
    /// see [`ToolPolicy::is_allowed_with_tags`].
    #[serde(default)]
    pub require_tags: Vec<String>,
    /// #28b — policy for FILE-WRITING via the bash/shell tool. `allow`
    /// (default) = zero behavior change; `warn` = the change receipt's tail
    /// appends a nudge to prefer `edit_file`/`diff_edit` when the command
    /// actually changed files; `deny` = a heuristic pre-screen refuses
    /// write-shaped commands and points at `edit_file` (escape hatch: a
    /// trailing `# octos:allow-write` comment on the command line).
    /// Loaded ONCE with the policy (never re-read per call).
    #[serde(default)]
    pub bash_file_writes: BashFileWrites,
}

/// #28b — the three-position knob for bash-driven file writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BashFileWrites {
    /// Default: the shell tool behaves exactly as before (28a receipt only).
    #[default]
    Allow,
    /// Receipt tail appends a nudge when files actually changed.
    Warn,
    /// Write-shaped commands are refused before execution; heuristic-based,
    /// false-negatives tolerated (the 28a receipt still backs it up),
    /// false-positives escape via the `# octos:allow-write` comment.
    Deny,
}

impl ToolPolicy {
    /// Check if a tool name is permitted by this policy (name-based only).
    pub fn is_allowed(&self, tool_name: &str) -> bool {
        matches!(self.evaluate(tool_name), PolicyDecision::Allow)
    }

    /// Full evaluation that returns an allow / deny decision plus a metric
    /// label on deny. Emits `octos_tool_policy_denial_total` with
    /// `reason="robot_tier_gate"` when the deny was driven by a
    /// `group:robot:*` entry, otherwise `reason="policy_deny"`.
    pub fn evaluate(&self, tool_name: &str) -> PolicyDecision {
        // Deny-wins: explicit deny entries take precedence.
        for entry in &self.deny {
            if entry_matches(entry, tool_name) {
                let reason = if entry_is_robot_group(entry) {
                    ROBOT_TIER_GATE_REASON
                } else {
                    GENERIC_DENY_REASON
                };
                counter!(POLICY_DENIAL_COUNTER, "reason" => reason).increment(1);
                return PolicyDecision::Deny { reason };
            }
        }

        // Empty allow list = allow everything not denied.
        if self.allow.is_empty() {
            return PolicyDecision::Allow;
        }

        for entry in &self.allow {
            if entry_matches(entry, tool_name) {
                return PolicyDecision::Allow;
            }
        }

        // Tool wasn't matched by any allow entry. If the allow list contains
        // robot-tier groups AND the tool is registered in a robot tier, the
        // gate is a robot-tier gate — that's the case robotic integrators
        // care about observing.
        let reason = if self.allow.iter().any(|entry| entry_is_robot_group(entry))
            && robot_groups::tool_has_tier(tool_name)
        {
            ROBOT_TIER_GATE_REASON
        } else {
            GENERIC_DENY_REASON
        };
        counter!(POLICY_DENIAL_COUNTER, "reason" => reason).increment(1);
        PolicyDecision::Deny { reason }
    }

    /// Check if a tool is permitted by both name policy and tag requirements.
    /// When `require_tags` is non-empty, the tool must have at least one matching tag.
    ///
    /// SECURITY / BEHAVIOR CHANGE (peer-review fix): untagged tools FAIL a
    /// non-empty `require_tags` gate. The previous rule ("tools with no tags
    /// are universal") made the gate fail open on exactly the inputs least
    /// likely to be audited — plugin/skill binaries, MCP server tools, and
    /// newly added builtins all default to `tags() == &[]`, so a profile
    /// confined to `require_tags: [..]` silently exposed all of them. Any
    /// tool that should pass a tag filter must now declare a matching tag.
    /// No compat shim: this deployment does not label tools at all (every
    /// agent is a universal replica of the master, so `require_tags` goes
    /// unused and the blast radius is zero here), and fail-closed is the
    /// defensible default for anyone who does opt in.
    pub fn is_allowed_with_tags(&self, tool_name: &str, tool_tags: &[&str]) -> bool {
        if !self.is_allowed(tool_name) {
            return false;
        }

        // If no tag requirements, pass
        if self.require_tags.is_empty() {
            return true;
        }

        // Tool must have at least one matching required tag. An empty
        // `tool_tags` therefore never matches — fail closed, per above.
        tool_tags
            .iter()
            .any(|tag| self.require_tags.iter().any(|req| req == tag))
    }

    /// True if the policy has no restrictions.
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.require_tags.is_empty()
    }
}

/// Check if a policy entry (group, wildcard, or exact name) matches a tool name.
pub(crate) fn entry_matches(entry: &str, tool_name: &str) -> bool {
    // Robot-tier groups resolve through the dynamic registry so integrators
    // register tool-to-tier mappings at runtime.
    if entry_is_robot_group(entry) {
        return robot_groups::group_covers_tool(entry, tool_name);
    }
    // Static named groups (group:fs, group:runtime, ...)
    if let Some(tools) = expand_group(entry) {
        return tools.contains(&tool_name);
    }
    // Wildcard: suffix `*` means prefix match
    if let Some(prefix) = entry.strip_suffix('*') {
        return tool_name.starts_with(prefix);
    }
    // Exact match
    entry == tool_name
}

fn entry_is_robot_group(entry: &str) -> bool {
    robot_groups::parse_group_name(entry).is_some()
}

/// Metadata about a named tool group (`group:web`, `group:runtime`, …) used
/// by [`ToolPolicy`] matching and profile/role tool declarations.
#[derive(Debug, Clone)]
pub struct ToolGroupInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub tools: &'static [&'static str],
}

/// All known tool groups with metadata.
pub const TOOL_GROUPS: &[ToolGroupInfo] = &[
    ToolGroupInfo {
        name: "group:fs",
        description: "File operations: read, write, edit, and diff-edit files",
        tools: &[
            "read_file",
            "watch_external_job",
            "write_file",
            "apply_patch",
            "edit_file",
            "diff_edit",
        ],
    },
    ToolGroupInfo {
        name: "group:runtime",
        // #1172: include the Codex-compatible `bash` alias so a policy
        // denying / deferring `group:runtime` covers every shell entry
        // point. Otherwise a profile that disabled runtime execution
        // would still be reachable via `bash(cmd=…)`.
        description: "Shell command execution",
        tools: &["shell", "exec_command", "write_stdin", "bash"],
    },
    ToolGroupInfo {
        name: "group:web",
        description: "Web search, page fetching, and headless browser",
        tools: &["web_search", "web_fetch", "browser"],
    },
    ToolGroupInfo {
        name: "group:search",
        description: "File and content search: glob patterns, grep, directory listing",
        tools: &["glob", "grep", "list_dir"],
    },
    ToolGroupInfo {
        name: "group:sessions",
        // #1172: include the Codex-compatible `delegate` one-call
        // wrapper so a policy denying / deferring `group:sessions`
        // covers every subagent entry point. The wrapper keeps an
        // Arc<dyn Tool> for the bound `spawn_agent`, so a policy that
        // removes `spawn` / `spawn_agent` from the visible registry
        // would still leave `delegate` capable of spawning a child
        // without this entry.
        description: "Spawn background subagents for parallel tasks",
        tools: &[
            "spawn",
            "spawn_agent",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
            "delegate",
        ],
    },
    ToolGroupInfo {
        name: "group:memory",
        description: "Long-term memory: save and recall knowledge across sessions",
        tools: &[
            "recall_memory",
            "save_memory",
            "memory_note",
            "record_memory_use",
            "memory_search",
            "memory_load",
        ],
    },
    ToolGroupInfo {
        name: "group:research",
        description: "Deep multi-round web research and synthesis",
        tools: &["search", "synthesize_research", "deep_crawl"],
    },
    ToolGroupInfo {
        name: "group:admin",
        description: "Skill management, tool configuration, and model switching",
        tools: &["manage_skills", "configure_tool", "model_check"],
    },
    ToolGroupInfo {
        name: "group:media",
        description: "Media generation: comics, slides, infographics, cards, and text-to-speech",
        tools: &[
            // RFC-1 fixup (codex round 3 P2): include the dispatcher
            // pair so profile/policy allow-lists that grant only
            // `group:media` still have a LLM-visible entry-point. Pre-
            // fixup, an `allow: [group:media]` policy would retain
            // only the (now internal-hidden) concrete targets and
            // drop `mofa_make` / `mofa_describe_content_type`,
            // leaving the LLM with no callable generation tool.
            "mofa_make",
            "mofa_describe_content_type",
            "mofa_comic",
            "mofa_slides",
            "mofa_infographic",
            "mofa_cards",
            "fm_tts",
            "fm_voice_list",
        ],
    },
    // M6.7 — the canonical deny list applied to every DelegateTool child.
    // It bounds re-delegation, spawning, user messaging, and memory writes.
    // Policy evaluation is deny-wins, so adding `group:delegated` to a
    // child's deny list gates those surfaces regardless of allow list.
    //
    // THIS IS NOT A CONFINEMENT BOUNDARY. Command execution is
    // INTENTIONALLY not restricted here: a delegated child keeps `shell` /
    // `exec_command` / `write_stdin` / `bash`. Delegated and peer agents are
    // deliberately universal replicas of the master, as capable as it is —
    // a child told "fix this test" must be able to run the build and the
    // test. Security cannot be guaranteed at the tool-policy layer, so it is
    // not attempted here; the real boundary is the OUTER SANDBOX (isolated
    // machine, SELinux-confined account, or container — see
    // [`crate::sandbox`]). A deny list that blocked shell would buy illusory
    // security at the cost of real capability.
    //
    // What this list DOES enforce is RECURSION AND RESOURCE control — a job
    // a tool policy can actually do: it stops unbounded
    // delegate -> delegate -> delegate chains and spawn storms. The
    // spawn/delegation family must therefore stay a SUPERSET of
    // `group:sessions`. The group machinery has no group-in-group
    // expansion, so that superset is maintained by hand and locked by
    // `group_delegated_supersets_session_spawn_family`. It had already
    // drifted: `spawn_agent`, `send_input`, `resume_agent`, `wait_agent`,
    // `close_agent` and the codex-compat `delegate` wrapper were all
    // reachable, so a child could spawn and drive its own sub-agents right
    // past the recursion guard.
    ToolGroupInfo {
        name: "group:delegated",
        description: "Delegated child deny list: re-delegation, spawning, user messaging, and memory writes. Command execution is intentionally NOT restricted — confinement is the sandbox's job.",
        tools: &[
            // Recursion + resource control: every delegation and spawn
            // entry point, including the codex-compat one-call `delegate`
            // wrapper — per the group:sessions #1172 note it keeps its own
            // Arc handle to the bound `spawn_agent` and can spawn a child
            // even when `spawn` / `spawn_agent` are gone from the registry.
            // The `*_agent` lifecycle tools ride along: they only operate on
            // children this child may not create.
            "delegate_task",
            "delegate",
            "spawn",
            "spawn_agent",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
            // User messaging — children report through the parent.
            "message",
            // Memory writes.
            "save_memory",
            "memory_note",
        ],
    },
];

/// Look up group info by name.
pub fn tool_group_info(name: &str) -> Option<&'static ToolGroupInfo> {
    TOOL_GROUPS.iter().find(|g| g.name == name)
}

/// Expand a group name to its tool names. Returns None if not a group.
fn expand_group(name: &str) -> Option<&'static [&'static str]> {
    tool_group_info(name).map(|g| g.tools)
}

/// Predicate that hides every `mofa_*` skill except `mofa_slides` for a
/// slides session. Designed to be passed to [`crate::ToolRegistry::retain`]
/// in `session_actor.rs` after `tools.activate("group:media")` so that:
///
/// 1. The slides system prompt's "ALWAYS use mofa_slides" rule is enforced
///    structurally — weaker LLMs (kimi-k2.6 fallback on mini1's dspfac
///    profile, 2026-05-24) cannot misroute the slides workflow to
///    `mofa_site` / `mofa_youtube` even when the prompt rule is buried
///    mid-text.
/// 2. Non-mofa skills (fm_tts, plugin tools that don't share the prefix)
///    pass through untouched.
/// 3. `mofa_slides` itself is preserved.
///
/// Returns `true` if the tool should be kept, `false` if it should be
/// evicted from the registry.
pub fn keep_tool_in_slides_session(tool_name: &str) -> bool {
    // RFC-1 fixup (codex P1 round 2): retain the dispatcher pair too.
    // After the RFC-1 switch from `defer` → `mark_internal_hidden`,
    // `mofa_slides` is registered but invisible to `specs()` — the
    // LLM only sees `mofa_make` + `mofa_describe_content_type`. If
    // `retain` evicts those (they share the `mofa_` prefix), slides
    // sessions end up with NO visible slides-generation tool.
    matches!(
        tool_name,
        "mofa_slides" | "mofa_make" | "mofa_describe_content_type"
    ) || !tool_name.starts_with("mofa_")
}
