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
    /// Required tags: only tools matching at least one tag are visible.
    /// Empty = no tag filtering. Composable with allow/deny (deny still wins).
    #[serde(default)]
    pub require_tags: Vec<String>,
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
    /// Tools with no tags always pass the tag check (they are universal).
    pub fn is_allowed_with_tags(&self, tool_name: &str, tool_tags: &[&str]) -> bool {
        if !self.is_allowed(tool_name) {
            return false;
        }

        // If no tag requirements, pass
        if self.require_tags.is_empty() {
            return true;
        }

        // Tools with no tags are universal (pass any filter)
        if tool_tags.is_empty() {
            return true;
        }

        // Tool must have at least one matching required tag
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
fn entry_matches(entry: &str, tool_name: &str) -> bool {
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

/// Metadata about a tool group, used by the `activate_tools` tool to present
/// available deferred groups to the LLM.
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
        tools: &["read_file", "write_file", "edit_file", "diff_edit"],
    },
    ToolGroupInfo {
        name: "group:runtime",
        description: "Shell command execution",
        tools: &["shell"],
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
        description: "Spawn background subagents for parallel tasks",
        tools: &["spawn"],
    },
    ToolGroupInfo {
        name: "group:memory",
        description: "Long-term memory: save and recall knowledge across sessions",
        tools: &["recall_memory", "save_memory"],
    },
    ToolGroupInfo {
        name: "group:research",
        description: "Deep multi-round web research and synthesis",
        tools: &["deep_search", "synthesize_research", "deep_crawl"],
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
            "mofa_comic",
            "mofa_slides",
            "mofa_infographic",
            "mofa_cards",
            "fm_tts",
            "fm_voice_list",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_policy_allows_all() {
        let policy = ToolPolicy::default();
        assert!(policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
        assert!(policy.is_allowed("anything"));
        assert!(policy.is_empty());
    }

    #[test]
    fn test_deny_wins_over_allow() {
        let policy = ToolPolicy {
            allow: vec!["shell".into(), "read_file".into()],
            deny: vec!["shell".into()],
            ..Default::default()
        };
        assert!(!policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
        assert!(!policy.is_allowed("write_file")); // not in allow list
    }

    #[test]
    fn test_group_expansion() {
        let policy = ToolPolicy {
            allow: vec!["group:fs".into()],
            ..Default::default()
        };
        assert!(policy.is_allowed("read_file"));
        assert!(policy.is_allowed("write_file"));
        assert!(policy.is_allowed("edit_file"));
        assert!(policy.is_allowed("diff_edit"));
        assert!(!policy.is_allowed("shell"));
        assert!(!policy.is_allowed("glob"));
    }

    #[test]
    fn test_wildcard_matching() {
        let policy = ToolPolicy {
            deny: vec!["web_*".into()],
            ..Default::default()
        };
        assert!(!policy.is_allowed("web_search"));
        assert!(!policy.is_allowed("web_fetch"));
        assert!(policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
    }

    #[test]
    fn test_allow_list_filters() {
        let policy = ToolPolicy {
            allow: vec!["group:fs".into(), "group:search".into()],
            ..Default::default()
        };
        assert!(policy.is_allowed("read_file"));
        assert!(policy.is_allowed("glob"));
        assert!(policy.is_allowed("grep"));
        assert!(!policy.is_allowed("shell"));
        assert!(!policy.is_allowed("spawn"));
        assert!(!policy.is_allowed("web_fetch"));
    }

    #[test]
    fn test_deny_group() {
        let policy = ToolPolicy {
            deny: vec!["group:runtime".into()],
            ..Default::default()
        };
        assert!(!policy.is_allowed("shell"));
        assert!(policy.is_allowed("read_file"));
    }

    #[test]
    fn test_serde_roundtrip() {
        let policy = ToolPolicy {
            allow: vec!["group:fs".into()],
            deny: vec!["shell".into()],
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: ToolPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.allow, policy.allow);
        assert_eq!(parsed.deny, policy.deny);
    }

    #[test]
    fn test_require_tags_filters_by_tag() {
        let policy = ToolPolicy {
            require_tags: vec!["code".into()],
            ..Default::default()
        };
        // Tool with matching tag passes
        assert!(policy.is_allowed_with_tags("shell", &["runtime", "code"]));
        // Tool without matching tag fails
        assert!(!policy.is_allowed_with_tags("web_search", &["web"]));
        // Tool with no tags passes (empty tags = universal)
        assert!(policy.is_allowed_with_tags("custom_tool", &[]));
    }

    #[test]
    fn test_require_tags_deny_still_wins() {
        let policy = ToolPolicy {
            deny: vec!["shell".into()],
            require_tags: vec!["code".into()],
            ..Default::default()
        };
        // Shell has matching tag but is denied
        assert!(!policy.is_allowed_with_tags("shell", &["runtime", "code"]));
        // read_file has matching tag and is not denied
        assert!(policy.is_allowed_with_tags("read_file", &["fs", "code"]));
    }

    #[test]
    fn test_empty_require_tags_allows_all() {
        let policy = ToolPolicy::default();
        assert!(policy.is_allowed_with_tags("anything", &["web"]));
        assert!(policy.is_allowed_with_tags("anything", &[]));
    }
}
