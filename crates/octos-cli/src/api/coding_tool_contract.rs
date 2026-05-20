//! AppUI coding tool contract payload helpers.
//!
//! This module is intentionally side-effect free. The WebSocket / stdio
//! dispatcher owns when these payloads are exposed; helpers here only build
//! backend-authored status JSON from the effective runtime inputs passed by
//! the caller.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

pub(crate) const CODING_TOOL_CONTRACT_FEATURE_V1: &str = "coding.tool_contract.v1";
pub(crate) const CODING_TOOL_CONTRACT_ID: &str = "codex-compatible-coding-v1";
pub(crate) const CODING_TOOL_CONTRACT_VERSION: &str = "1";
pub(crate) const CODING_MODEL_TOOLSET: &str = "coding";
pub(crate) const CODING_TOOL_POLICY_ID: &str = "coding-v1";
pub(crate) const CODING_DYNAMIC_TOOL_DISCOVERY: &str = "enabled";

pub(crate) const TOOL_STATUS_AVAILABLE: &str = "available";
pub(crate) const TOOL_STATUS_ALIASED: &str = "aliased";
pub(crate) const TOOL_STATUS_DISABLED_BY_POLICY: &str = "disabled_by_policy";
pub(crate) const TOOL_STATUS_MISSING: &str = "missing";
pub(crate) const TOOL_STATUS_UNIMPLEMENTED: &str = "unimplemented";
/// #970 — the tool is registered but currently in the deferred set (LRU
/// auto-eviction). The model can recover it via `activate_tools`, so for
/// contract purposes it counts as available; the explicit status lets
/// clients render "available, currently inactive" UX.
pub(crate) const TOOL_STATUS_DEFERRED: &str = "deferred";

pub(crate) const MCP_STATUS_CONNECTED: &str = "connected";
pub(crate) const MCP_STATUS_CONNECTING: &str = "connecting";
pub(crate) const MCP_STATUS_FAILED: &str = "failed";
pub(crate) const MCP_STATUS_DISABLED: &str = "disabled";

pub(crate) const CODING_PATCH_TOOL_CAPABILITY_V1: &str = "coding.patch_tool.v1";
pub(crate) const CODING_EXEC_SESSION_CAPABILITY_V1: &str = "coding.exec_session.v1";
pub(crate) const CODING_PLAN_TOOL_CAPABILITY_V1: &str = "coding.plan_tool.v1";
pub(crate) const CODING_USER_INPUT_TOOL_CAPABILITY_V1: &str = "coding.user_input_tool.v1";
pub(crate) const CODING_SUBAGENT_ALIASES_CAPABILITY_V1: &str = "coding.subagent_aliases.v1";
// Optional capabilities declared by UPCR-2026-020 §3 that have no canonical
// P0 tool yet. The constants exist so the protocol vocabulary is single-
// sourced; an actual server advertises them only when the underlying
// feature is wired (see UPCR §5 "Capability-gated fields must be omitted
// when the corresponding capability is not negotiated").
#[allow(dead_code)]
pub(crate) const CODING_IMAGE_VIEW_CAPABILITY_V1: &str = "coding.image_view.v1";
#[allow(dead_code)]
pub(crate) const CODING_DYNAMIC_TOOL_SEARCH_CAPABILITY_V1: &str = "coding.dynamic_tool_search.v1";
#[allow(dead_code)]
pub(crate) const CODING_IMAGE_GENERATION_CAPABILITY_V1: &str = "coding.image_generation.v1";

// UPCR-2026-020 §8 typed error kinds. Used in structured RpcError `data.kind`
// fields when the corresponding failure mode is hit. Declared centrally so
// callers and tests agree on the wire strings; not all are emitted yet, see
// #970 for the wiring work.
#[allow(dead_code)]
pub(crate) const ERROR_KIND_TOOL_CONTRACT_UNAVAILABLE: &str = "tool_contract_unavailable";
#[allow(dead_code)]
pub(crate) const ERROR_KIND_CODING_TOOL_DENIED: &str = "coding_tool_denied";
#[allow(dead_code)]
pub(crate) const ERROR_KIND_CODING_TOOL_MISSING: &str = "coding_tool_missing";
#[allow(dead_code)]
pub(crate) const ERROR_KIND_EXEC_SESSION_UNKNOWN: &str = "exec_session_unknown";

pub(crate) const CODING_P0_REQUIRED_TOOL_NAMES: &[&str] = &[
    "apply_patch",
    "exec_command",
    "write_stdin",
    "update_plan",
    "request_user_input",
    "spawn_agent",
    "send_input",
    "resume_agent",
    "wait_agent",
    "close_agent",
];

pub(crate) const OCTOS_KNOWN_MODEL_VISIBLE_TOOLS: &[&str] = &[
    "apply_patch",
    "exec_command",
    "write_stdin",
    "update_plan",
    "request_user_input",
    "spawn_agent",
    "send_input",
    "resume_agent",
    "wait_agent",
    "close_agent",
    "read_file",
    "write_file",
    "edit_file",
    "diff_edit",
    "shell",
    "glob",
    "grep",
    "list_dir",
    "web_search",
    "web_fetch",
    "browser",
    "spawn",
    "read_task_output",
    "activate_tools",
    "configure_tool",
    "manage_skills",
    "check_workspace_contract",
    "workspace_log",
    "workspace_show",
    "workspace_diff",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolPolicyView<'a> {
    pub tool_policy_id: &'a str,
    pub sandbox_mode: &'a str,
    pub approval_policy: &'a str,
}

impl Default for ToolPolicyView<'static> {
    fn default() -> Self {
        Self {
            tool_policy_id: CODING_TOOL_POLICY_ID,
            sandbox_mode: "workspace-write",
            approval_policy: "on-request",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolStatusListContext<'a> {
    pub profile_id: Option<&'a str>,
    pub session_id: &'a str,
    pub policy: ToolPolicyView<'a>,
    pub available_model_tools: &'a [&'a str],
    pub disabled_model_tools: &'a [&'a str],
    /// #970 — names of tools currently in the LRU deferred set. Empty by
    /// default; populated from `ToolRegistry::deferred_tool_names()` so
    /// the contract resolver can distinguish "registered but inactive"
    /// from "not registered at all".
    pub deferred_model_tools: &'a [&'a str],
    pub include_coding_tool_contract: bool,
}

impl<'a> ToolStatusListContext<'a> {
    #[cfg(test)]
    pub(crate) fn default_for_session(session_id: &'a str) -> Self {
        Self {
            profile_id: None,
            session_id,
            policy: ToolPolicyView::default(),
            available_model_tools: OCTOS_KNOWN_MODEL_VISIBLE_TOOLS,
            disabled_model_tools: &[],
            deferred_model_tools: &[],
            include_coding_tool_contract: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct McpStatusListContext<'a> {
    pub profile_id: Option<&'a str>,
    pub session_id: &'a str,
    pub servers: &'a [McpServerStatusView<'a>],
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RuntimePolicyStampContext<'a> {
    pub policy: ToolPolicyView<'a>,
    pub mcp_servers: &'a [McpServerStatusView<'a>],
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct McpServerStatusView<'a> {
    pub id: &'a str,
    pub display_name: Option<&'a str>,
    pub transport: Option<&'a str>,
    pub status: &'a str,
    pub tool_count: usize,
    pub tools: &'a [&'a str],
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
struct RequiredToolSpec {
    name: &'static str,
    category: &'static str,
    capability: &'static str,
    aliases: &'static [&'static str],
    policy: &'static str,
    partial_alias_detail: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct OctosToolSpec {
    name: &'static str,
    category: &'static str,
    aliases: &'static [&'static str],
    policy: &'static str,
    detail: Option<&'static str>,
}

const REQUIRED_CODING_TOOLS: &[RequiredToolSpec] = &[
    RequiredToolSpec {
        name: "apply_patch",
        category: "edit",
        capability: CODING_PATCH_TOOL_CAPABILITY_V1,
        aliases: &["diff_edit", "edit_file"],
        policy: "allowed",
        partial_alias_detail: Some(
            "Octos exposes diff_edit/edit_file today, but not Codex-compatible apply_patch semantics.",
        ),
    },
    RequiredToolSpec {
        name: "exec_command",
        category: "runtime",
        capability: CODING_EXEC_SESSION_CAPABILITY_V1,
        aliases: &["shell"],
        policy: "approval_gated",
        partial_alias_detail: Some(
            "Octos shell is one-shot and lacks Codex exec sessions, PTY state, and write_stdin.",
        ),
    },
    RequiredToolSpec {
        name: "write_stdin",
        category: "runtime",
        capability: CODING_EXEC_SESSION_CAPABILITY_V1,
        aliases: &[],
        policy: "approval_gated",
        partial_alias_detail: None,
    },
    RequiredToolSpec {
        name: "update_plan",
        category: "planning",
        capability: CODING_PLAN_TOOL_CAPABILITY_V1,
        aliases: &[],
        policy: "allowed",
        partial_alias_detail: None,
    },
    RequiredToolSpec {
        name: "request_user_input",
        category: "interaction",
        capability: CODING_USER_INPUT_TOOL_CAPABILITY_V1,
        aliases: &[],
        policy: "allowed",
        partial_alias_detail: None,
    },
    RequiredToolSpec {
        name: "spawn_agent",
        category: "agent",
        capability: CODING_SUBAGENT_ALIASES_CAPABILITY_V1,
        aliases: &["spawn"],
        policy: "allowed",
        partial_alias_detail: Some(
            "Octos spawn exists, but Codex-compatible agent alias lifecycle is not model-visible yet.",
        ),
    },
    RequiredToolSpec {
        name: "send_input",
        category: "agent",
        capability: CODING_SUBAGENT_ALIASES_CAPABILITY_V1,
        aliases: &[],
        policy: "allowed",
        partial_alias_detail: None,
    },
    RequiredToolSpec {
        name: "resume_agent",
        category: "agent",
        capability: CODING_SUBAGENT_ALIASES_CAPABILITY_V1,
        aliases: &[],
        policy: "allowed",
        partial_alias_detail: None,
    },
    RequiredToolSpec {
        name: "wait_agent",
        category: "agent",
        capability: CODING_SUBAGENT_ALIASES_CAPABILITY_V1,
        aliases: &["read_task_output"],
        policy: "allowed",
        partial_alias_detail: Some(
            "Octos can inspect background task output, but does not expose Codex wait_agent.",
        ),
    },
    RequiredToolSpec {
        name: "close_agent",
        category: "agent",
        capability: CODING_SUBAGENT_ALIASES_CAPABILITY_V1,
        aliases: &[],
        policy: "allowed",
        partial_alias_detail: None,
    },
];

const OCTOS_TOOL_SPECS: &[OctosToolSpec] = &[
    OctosToolSpec {
        name: "apply_patch",
        category: "edit",
        aliases: &["diff_edit", "edit_file"],
        policy: "allowed",
        detail: Some("Codex-compatible patch entrypoint backed by Octos file mutation policy."),
    },
    OctosToolSpec {
        name: "exec_command",
        category: "runtime",
        aliases: &["shell"],
        policy: "approval_gated",
        detail: Some("Codex-compatible command entrypoint with session output polling."),
    },
    OctosToolSpec {
        name: "write_stdin",
        category: "runtime",
        aliases: &[],
        policy: "approval_gated",
        detail: Some("Writes to exec_command sessions."),
    },
    OctosToolSpec {
        name: "update_plan",
        category: "planning",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "request_user_input",
        category: "interaction",
        aliases: &[],
        policy: "allowed",
        detail: Some("Visible host-interaction shim; synchronous UI blocking is host-dependent."),
    },
    OctosToolSpec {
        name: "spawn_agent",
        category: "agent",
        aliases: &["spawn"],
        policy: "allowed",
        detail: Some(
            "Canonical Codex subagent entrypoint; forwards to Octos spawn when the session runtime registers it.",
        ),
    },
    OctosToolSpec {
        name: "send_input",
        category: "agent",
        aliases: &[],
        policy: "allowed",
        detail: Some("Visible for Codex agent-control parity; conversational backends may no-op."),
    },
    OctosToolSpec {
        name: "resume_agent",
        category: "agent",
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Relaunches a supervised Octos agent task when the session runtime has a relaunch callback.",
        ),
    },
    OctosToolSpec {
        name: "wait_agent",
        category: "agent",
        aliases: &["read_task_output"],
        policy: "allowed",
        detail: Some("Inspects Octos task-supervisor state for Codex-compatible agent handles."),
    },
    OctosToolSpec {
        name: "close_agent",
        category: "agent",
        aliases: &[],
        policy: "allowed",
        detail: Some("Cancels active Octos supervised tasks when a task supervisor is bound."),
    },
    OctosToolSpec {
        name: "read_file",
        category: "read",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "write_file",
        category: "edit",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "edit_file",
        category: "edit",
        aliases: &["apply_patch"],
        policy: "allowed",
        detail: Some("Partial apply_patch-adjacent editor; not Codex apply_patch parity."),
    },
    OctosToolSpec {
        name: "diff_edit",
        category: "edit",
        aliases: &["apply_patch"],
        policy: "allowed",
        detail: Some("Unified-diff editor; not Codex apply_patch parity."),
    },
    OctosToolSpec {
        name: "shell",
        category: "runtime",
        aliases: &["exec_command"],
        policy: "approval_gated",
        detail: Some("One-shot command runner; not Codex exec session parity."),
    },
    OctosToolSpec {
        name: "glob",
        category: "search",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "grep",
        category: "search",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "list_dir",
        category: "search",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "web_search",
        category: "web",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "web_fetch",
        category: "web",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "browser",
        category: "web",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "spawn",
        category: "agent",
        aliases: &["spawn_agent"],
        policy: "allowed",
        detail: Some("Octos subagent launcher; Codex agent lifecycle aliases are not parity yet."),
    },
    OctosToolSpec {
        name: "read_task_output",
        category: "agent",
        aliases: &["wait_agent"],
        policy: "allowed",
        detail: Some("Background task output reader; not Codex wait_agent parity."),
    },
    OctosToolSpec {
        name: "activate_tools",
        category: "discovery",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "configure_tool",
        category: "configuration",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "manage_skills",
        category: "discovery",
        aliases: &["tool_search", "tool_suggest"],
        policy: "allowed",
        detail: Some(
            "Skill management exists, but Codex dynamic tool discovery aliases are not parity yet.",
        ),
    },
    OctosToolSpec {
        name: "check_workspace_contract",
        category: "workspace",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "workspace_log",
        category: "workspace",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "workspace_show",
        category: "workspace",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
    OctosToolSpec {
        name: "workspace_diff",
        category: "workspace",
        aliases: &[],
        policy: "allowed",
        detail: None,
    },
];

pub(crate) fn tool_status_list_payload(context: ToolStatusListContext<'_>) -> Value {
    let available = names_set(context.available_model_tools);
    let disabled = names_set(context.disabled_model_tools);
    let deferred = names_set(context.deferred_model_tools);
    let mut payload = json!({
        "profile_id": context.profile_id,
        "session_id": context.session_id,
        "policy_id": context.policy.tool_policy_id,
        "tools": octos_tool_status_entries(&available, &disabled),
    });

    if context.include_coding_tool_contract {
        payload["coding_tool_contract"] =
            coding_tool_contract_payload(context.policy, &available, &disabled, &deferred);
    }

    payload
}

pub(crate) fn mcp_status_list_payload(context: McpStatusListContext<'_>) -> Value {
    json!({
        "profile_id": context.profile_id,
        "session_id": context.session_id,
        "servers": mcp_server_status_values(context.servers),
        "summary": mcp_status_summary(context.servers),
    })
}

pub(crate) fn coding_runtime_policy_stamp_extensions(
    context: RuntimePolicyStampContext<'_>,
) -> Value {
    json!({
        "tool_policy_id": context.policy.tool_policy_id,
        "tool_contract_id": CODING_TOOL_CONTRACT_ID,
        "tool_contract_version": CODING_TOOL_CONTRACT_VERSION,
        "model_toolset": CODING_MODEL_TOOLSET,
        "dynamic_tool_discovery": CODING_DYNAMIC_TOOL_DISCOVERY,
        "mcp_servers": mcp_server_stamp_values(context.mcp_servers),
    })
}

pub(crate) fn apply_coding_runtime_policy_stamp_extensions(
    stamp: &mut Value,
    context: RuntimePolicyStampContext<'_>,
) {
    let Some(stamp) = stamp.as_object_mut() else {
        return;
    };
    let Value::Object(extensions) = coding_runtime_policy_stamp_extensions(context) else {
        return;
    };
    stamp.extend(extensions);
}

pub(crate) fn coding_tool_contract_payload(
    policy: ToolPolicyView<'_>,
    available_model_tools: &HashSet<&str>,
    disabled_model_tools: &HashSet<&str>,
    deferred_model_tools: &HashSet<&str>,
) -> Value {
    let required_tools = required_tool_status_entries(
        available_model_tools,
        disabled_model_tools,
        deferred_model_tools,
    );
    let missing_required_tools: Vec<String> = required_tools
        .iter()
        .filter_map(|entry| {
            let status = entry.get("status").and_then(Value::as_str);
            let name = entry.get("name").and_then(Value::as_str);
            match (status, name) {
                (
                    Some(TOOL_STATUS_AVAILABLE | TOOL_STATUS_ALIASED | TOOL_STATUS_DEFERRED),
                    Some(_),
                ) => None,
                (_, Some(name)) => Some(name.to_owned()),
                _ => None,
            }
        })
        .collect();
    let status = if missing_required_tools.is_empty() {
        "ready"
    } else {
        "incomplete"
    };

    // #970 — emit the deferred set for evidence/observability. Clients
    // that render the coding tool contract can show "registered but
    // currently auto-deferred" alongside the per-tool status.
    let mut deferred_names: Vec<&str> = deferred_model_tools.iter().copied().collect();
    deferred_names.sort();

    json!({
        "id": CODING_TOOL_CONTRACT_ID,
        "version": CODING_TOOL_CONTRACT_VERSION,
        "feature": CODING_TOOL_CONTRACT_FEATURE_V1,
        "status": status,
        "required_tool_names": CODING_P0_REQUIRED_TOOL_NAMES,
        "required_tools": required_tools,
        "missing_required_tools": missing_required_tools,
        "deferred_model_tools": deferred_names,
        "policy": {
            "tool_policy_id": policy.tool_policy_id,
            "sandbox_mode": policy.sandbox_mode,
            "approval_policy": policy.approval_policy,
        },
    })
}

fn required_tool_status_entries(
    available_model_tools: &HashSet<&str>,
    disabled_model_tools: &HashSet<&str>,
    deferred_model_tools: &HashSet<&str>,
) -> Vec<Value> {
    REQUIRED_CODING_TOOLS
        .iter()
        .map(|spec| {
            required_tool_status_entry(
                spec,
                available_model_tools,
                disabled_model_tools,
                deferred_model_tools,
            )
        })
        .collect()
}

fn required_tool_status_entry(
    spec: &RequiredToolSpec,
    available_model_tools: &HashSet<&str>,
    disabled_model_tools: &HashSet<&str>,
    deferred_model_tools: &HashSet<&str>,
) -> Value {
    let mut entry = Map::new();
    entry.insert("name".into(), json!(spec.name));
    entry.insert("category".into(), json!(spec.category));
    entry.insert("aliases".into(), json!(spec.aliases));
    entry.insert("capability".into(), json!(spec.capability));
    entry.insert("policy".into(), json!(spec.policy));

    if disabled_model_tools.contains(spec.name) {
        entry.insert("status".into(), json!(TOOL_STATUS_DISABLED_BY_POLICY));
        entry.insert("backend_tool".into(), json!(spec.name));
        entry.insert("detail".into(), json!("disabled by effective tool policy"));
        return Value::Object(entry);
    }

    if available_model_tools.contains(spec.name) {
        entry.insert("status".into(), json!(TOOL_STATUS_AVAILABLE));
        entry.insert("backend_tool".into(), json!(spec.name));
        return Value::Object(entry);
    }

    // #970: a tool registered but currently in the deferred set is
    // recoverable through `activate_tools`. Surface it as `deferred` so
    // clients can render "available, currently inactive" UX; it counts
    // as available for the contract's `missing_required_tools` filter.
    if deferred_model_tools.contains(spec.name) {
        entry.insert("status".into(), json!(TOOL_STATUS_DEFERRED));
        entry.insert("backend_tool".into(), json!(spec.name));
        entry.insert(
            "detail".into(),
            json!("registered but currently auto-deferred; recoverable via activate_tools"),
        );
        return Value::Object(entry);
    }

    if let Some(alias) = first_present(spec.aliases, available_model_tools) {
        entry.insert("status".into(), json!(TOOL_STATUS_UNIMPLEMENTED));
        entry.insert("backend_tool".into(), json!(alias));
        if let Some(detail) = spec.partial_alias_detail {
            entry.insert("detail".into(), json!(detail));
        }
        return Value::Object(entry);
    }

    // Same alias check, but against the deferred set: an alias that's
    // registered yet deferred still beats reporting the tool as missing.
    if let Some(alias) = first_present(spec.aliases, deferred_model_tools) {
        entry.insert("status".into(), json!(TOOL_STATUS_DEFERRED));
        entry.insert("backend_tool".into(), json!(alias));
        let detail = spec.partial_alias_detail.unwrap_or(
            "alias registered but currently auto-deferred; recoverable via activate_tools",
        );
        entry.insert("detail".into(), json!(detail));
        return Value::Object(entry);
    }

    entry.insert("status".into(), json!(TOOL_STATUS_MISSING));
    entry.insert("backend_tool".into(), Value::Null);
    Value::Object(entry)
}

fn octos_tool_status_entries(
    available_model_tools: &HashSet<&str>,
    disabled_model_tools: &HashSet<&str>,
) -> Vec<Value> {
    let mut entries: Vec<Value> = OCTOS_TOOL_SPECS
        .iter()
        .filter(|spec| {
            available_model_tools.contains(spec.name) || disabled_model_tools.contains(spec.name)
        })
        .map(|spec| octos_tool_status_entry(spec, disabled_model_tools))
        .collect();

    for name in available_model_tools {
        if OCTOS_TOOL_SPECS.iter().any(|spec| spec.name == *name) {
            continue;
        }
        entries.push(json!({
            "name": name,
            "category": "custom",
            "status": TOOL_STATUS_AVAILABLE,
            "backend_tool": name,
            "aliases": [],
            "policy": "allowed",
        }));
    }

    entries.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    entries
}

fn octos_tool_status_entry(spec: &OctosToolSpec, disabled_model_tools: &HashSet<&str>) -> Value {
    let status = if disabled_model_tools.contains(spec.name) {
        TOOL_STATUS_DISABLED_BY_POLICY
    } else {
        TOOL_STATUS_AVAILABLE
    };
    let mut entry = json!({
        "name": spec.name,
        "category": spec.category,
        "status": status,
        "backend_tool": spec.name,
        "aliases": spec.aliases,
        "policy": spec.policy,
    });
    if let Some(detail) = spec.detail {
        entry["detail"] = json!(detail);
    }
    entry
}

fn mcp_server_status_values(servers: &[McpServerStatusView<'_>]) -> Vec<Value> {
    servers.iter().map(mcp_server_status_value).collect()
}

fn mcp_server_status_value(server: &McpServerStatusView<'_>) -> Value {
    let mut value = json!({
        "id": server.id,
        "display_name": server.display_name,
        "transport": server.transport,
        "status": server.status,
        "tool_count": server.tool_count,
        "tools": server.tools,
    });
    if let Some(error) = server.error {
        value["error"] = json!(error);
    }
    value
}

fn mcp_server_stamp_values(servers: &[McpServerStatusView<'_>]) -> Vec<Value> {
    servers
        .iter()
        .map(|server| {
            json!({
                "id": server.id,
                "display_name": server.display_name,
                "status": server.status,
                "tool_count": server.tool_count,
            })
        })
        .collect()
}

fn mcp_status_summary(servers: &[McpServerStatusView<'_>]) -> Value {
    let mut connected = 0;
    let mut connecting = 0;
    let mut failed = 0;
    let mut disabled = 0;
    for server in servers {
        match server.status {
            MCP_STATUS_CONNECTED => connected += 1,
            MCP_STATUS_CONNECTING => connecting += 1,
            MCP_STATUS_FAILED => failed += 1,
            MCP_STATUS_DISABLED => disabled += 1,
            _ => {}
        }
    }
    json!({
        "connected": connected,
        "connecting": connecting,
        "failed": failed,
        "disabled": disabled,
    })
}

fn names_set<'a>(names: &'a [&'a str]) -> HashSet<&'a str> {
    names.iter().copied().collect()
}

fn first_present<'a>(
    names: &'a [&'a str],
    available_model_tools: &HashSet<&str>,
) -> Option<&'a str> {
    names
        .iter()
        .copied()
        .find(|name| available_model_tools.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #970 — protocol vocabulary smoke test. Pins the wire strings of the
    /// UPCR-2026-020 optional capability flags and typed error kinds so a
    /// future rename (`coding.image_generation.v1` -> `…image_gen.v1`,
    /// `coding_tool_denied` -> `tool_denied`, etc.) becomes a compile-time
    /// diff against the spec instead of a silent break.
    #[test]
    fn upcr_2026_020_vocabulary_matches_spec_strings() {
        // Optional capability flags (UPCR §3).
        assert_eq!(CODING_PATCH_TOOL_CAPABILITY_V1, "coding.patch_tool.v1");
        assert_eq!(CODING_EXEC_SESSION_CAPABILITY_V1, "coding.exec_session.v1");
        assert_eq!(CODING_PLAN_TOOL_CAPABILITY_V1, "coding.plan_tool.v1");
        assert_eq!(
            CODING_USER_INPUT_TOOL_CAPABILITY_V1,
            "coding.user_input_tool.v1"
        );
        assert_eq!(
            CODING_SUBAGENT_ALIASES_CAPABILITY_V1,
            "coding.subagent_aliases.v1"
        );
        assert_eq!(CODING_IMAGE_VIEW_CAPABILITY_V1, "coding.image_view.v1");
        assert_eq!(
            CODING_DYNAMIC_TOOL_SEARCH_CAPABILITY_V1,
            "coding.dynamic_tool_search.v1"
        );
        assert_eq!(
            CODING_IMAGE_GENERATION_CAPABILITY_V1,
            "coding.image_generation.v1"
        );

        // Typed error kinds (UPCR §8).
        assert_eq!(
            ERROR_KIND_TOOL_CONTRACT_UNAVAILABLE,
            "tool_contract_unavailable"
        );
        assert_eq!(ERROR_KIND_CODING_TOOL_DENIED, "coding_tool_denied");
        assert_eq!(ERROR_KIND_CODING_TOOL_MISSING, "coding_tool_missing");
        assert_eq!(ERROR_KIND_EXEC_SESSION_UNKNOWN, "exec_session_unknown");
    }

    fn required_tool<'a>(contract: &'a Value, name: &str) -> &'a Value {
        contract["required_tools"]
            .as_array()
            .expect("required_tools array")
            .iter()
            .find(|tool| tool["name"] == json!(name))
            .expect("required tool")
    }

    #[test]
    fn default_tool_contract_reports_canonical_codex_p0_ready() {
        let payload =
            tool_status_list_payload(ToolStatusListContext::default_for_session("coding:test"));
        let contract = &payload["coding_tool_contract"];
        assert_eq!(contract["id"], json!(CODING_TOOL_CONTRACT_ID));
        assert_eq!(contract["version"], json!(CODING_TOOL_CONTRACT_VERSION));
        assert_eq!(contract["feature"], json!(CODING_TOOL_CONTRACT_FEATURE_V1));
        assert_eq!(contract["status"], json!("ready"));
        assert_eq!(contract["missing_required_tools"], json!([]));

        let apply_patch = required_tool(contract, "apply_patch");
        assert_eq!(apply_patch["status"], json!(TOOL_STATUS_AVAILABLE));
        assert_eq!(apply_patch["backend_tool"], json!("apply_patch"));
        assert_eq!(apply_patch["aliases"], json!(["diff_edit", "edit_file"]));

        let exec_command = required_tool(contract, "exec_command");
        assert_eq!(exec_command["status"], json!(TOOL_STATUS_AVAILABLE));
        assert_eq!(exec_command["backend_tool"], json!("exec_command"));
        assert_eq!(exec_command["policy"], json!("approval_gated"));
    }

    #[test]
    fn contract_becomes_ready_when_canonical_required_tools_are_available() {
        let context = ToolStatusListContext {
            available_model_tools: CODING_P0_REQUIRED_TOOL_NAMES,
            ..ToolStatusListContext::default_for_session("coding:test")
        };
        let payload = tool_status_list_payload(context);
        let contract = &payload["coding_tool_contract"];

        assert_eq!(contract["status"], json!("ready"));
        assert_eq!(contract["missing_required_tools"], json!([]));
        for name in CODING_P0_REQUIRED_TOOL_NAMES {
            assert_eq!(
                required_tool(contract, name)["status"],
                json!(TOOL_STATUS_AVAILABLE)
            );
        }
    }

    #[test]
    fn deferred_canonical_tool_is_reported_as_available_via_deferred_status() {
        // #970: when ProfileRuntime auto-defers `group:runtime` /
        // `group:sessions`, the P0 tools `shell`, `exec_command`,
        // `spawn_agent`, ... move out of `specs()` but stay registered.
        // The contract used to report them as `missing` and the soak's
        // tool-registry snapshot showed 7 of 10 P0 tools missing. With
        // the deferred-aware resolver, they surface as `deferred` and
        // drop out of `missing_required_tools`.
        let deferred = &[
            "exec_command",
            "write_stdin",
            "spawn_agent",
            "send_input",
            "resume_agent",
            "wait_agent",
            "close_agent",
        ];
        let available = &["apply_patch", "update_plan", "request_user_input"];
        let context = ToolStatusListContext {
            available_model_tools: available,
            deferred_model_tools: deferred,
            ..ToolStatusListContext::default_for_session("coding:test")
        };
        let payload = tool_status_list_payload(context);
        let contract = &payload["coding_tool_contract"];

        assert_eq!(contract["status"], json!("ready"));
        assert_eq!(contract["missing_required_tools"], json!([]));

        let exec_command = required_tool(contract, "exec_command");
        assert_eq!(exec_command["status"], json!(TOOL_STATUS_DEFERRED));
        assert_eq!(exec_command["backend_tool"], json!("exec_command"));
        assert!(
            exec_command["detail"]
                .as_str()
                .is_some_and(|d| d.contains("activate_tools"))
        );

        let apply_patch = required_tool(contract, "apply_patch");
        assert_eq!(apply_patch["status"], json!(TOOL_STATUS_AVAILABLE));
    }

    #[test]
    fn deferred_alias_resolves_required_tool_as_deferred_not_missing() {
        // #970: also exercise the alias-deferred path — when the
        // canonical name is absent but a deferred alias is registered
        // (e.g. `shell` aliasing `exec_command`), the contract should
        // surface `deferred` against the alias rather than `missing`.
        let context = ToolStatusListContext {
            available_model_tools: &["apply_patch", "update_plan", "request_user_input"],
            deferred_model_tools: &["shell", "spawn"],
            ..ToolStatusListContext::default_for_session("coding:test")
        };
        let payload = tool_status_list_payload(context);
        let contract = &payload["coding_tool_contract"];

        let exec_command = required_tool(contract, "exec_command");
        assert_eq!(exec_command["status"], json!(TOOL_STATUS_DEFERRED));
        assert_eq!(exec_command["backend_tool"], json!("shell"));

        let spawn_agent = required_tool(contract, "spawn_agent");
        assert_eq!(spawn_agent["status"], json!(TOOL_STATUS_DEFERRED));
        assert_eq!(spawn_agent["backend_tool"], json!("spawn"));

        // Tools with no canonical or deferred coverage still report
        // missing — verify by leaving write_stdin out of every set.
        let write_stdin = required_tool(contract, "write_stdin");
        assert_eq!(write_stdin["status"], json!(TOOL_STATUS_MISSING));
    }

    /// #972 — guard against accidental removal of any P0 canonical tool
    /// from the per-session tool registry. The M14 contract relies on
    /// every name in `CODING_P0_REQUIRED_TOOL_NAMES` being registered
    /// (whether active or deferred). If a future change drops one of
    /// the registrations in `with_builtins_and_permissions`, this test
    /// fails loudly instead of letting the live contract silently
    /// regress to `status: incomplete` only when the strict M12 soak
    /// runs in CI.
    #[test]
    fn p0_canonical_tools_are_registered_by_session_builtins() {
        use octos_agent::ToolRegistry;
        use octos_agent::sandbox::NoSandbox;

        let cwd = std::path::Path::new("/tmp");
        let registry = ToolRegistry::with_builtins_and_sandbox(cwd, Box::new(NoSandbox));
        let names: std::collections::HashSet<String> = registry.tool_names().into_iter().collect();

        let mut missing: Vec<&str> = Vec::new();
        for required in CODING_P0_REQUIRED_TOOL_NAMES {
            if !names.contains(*required) {
                missing.push(*required);
            }
        }
        assert!(
            missing.is_empty(),
            "P0 canonical tools must be registered by ToolRegistry::with_builtins_and_sandbox \
             so the M14 coding tool contract can resolve them as active or deferred. \
             Missing: {missing:?}. Registered tool names: {names:?}"
        );
    }

    #[test]
    fn disabled_canonical_tool_is_reported_as_policy_gap() {
        let context = ToolStatusListContext {
            available_model_tools: &["apply_patch"],
            disabled_model_tools: &["apply_patch"],
            ..ToolStatusListContext::default_for_session("coding:test")
        };
        let payload = tool_status_list_payload(context);
        let contract = &payload["coding_tool_contract"];

        assert_eq!(
            required_tool(contract, "apply_patch")["status"],
            json!(TOOL_STATUS_DISABLED_BY_POLICY)
        );
        assert!(
            contract["missing_required_tools"]
                .as_array()
                .expect("missing array")
                .iter()
                .any(|candidate| candidate == "apply_patch")
        );
    }

    #[test]
    fn mcp_status_payload_summarizes_server_states() {
        let servers = [
            McpServerStatusView {
                id: "fs",
                display_name: Some("Filesystem"),
                transport: Some("stdio"),
                status: MCP_STATUS_CONNECTED,
                tool_count: 2,
                tools: &["stat", "read"],
                error: None,
            },
            McpServerStatusView {
                id: "remote",
                display_name: None,
                transport: Some("http"),
                status: MCP_STATUS_FAILED,
                tool_count: 0,
                tools: &[],
                error: Some("connection refused"),
            },
        ];

        let payload = mcp_status_list_payload(McpStatusListContext {
            profile_id: Some("coding"),
            session_id: "coding:test",
            servers: &servers,
        });

        assert_eq!(payload["profile_id"], json!("coding"));
        assert_eq!(payload["summary"]["connected"], json!(1));
        assert_eq!(payload["summary"]["failed"], json!(1));
        assert_eq!(payload["servers"][0]["tools"], json!(["stat", "read"]));
        assert_eq!(payload["servers"][1]["error"], json!("connection refused"));
    }

    #[test]
    fn runtime_policy_stamp_extensions_are_contract_shaped() {
        let servers = [McpServerStatusView {
            id: "github",
            display_name: Some("GitHub"),
            transport: Some("stdio"),
            status: MCP_STATUS_CONNECTED,
            tool_count: 4,
            tools: &["issue_read"],
            error: None,
        }];
        let mut stamp = json!({
            "tool_policy_id": "profile",
            "sandbox_mode": "workspace-write",
        });

        apply_coding_runtime_policy_stamp_extensions(
            &mut stamp,
            RuntimePolicyStampContext {
                policy: ToolPolicyView {
                    tool_policy_id: CODING_TOOL_POLICY_ID,
                    sandbox_mode: "danger-full-access",
                    approval_policy: "never",
                },
                mcp_servers: &servers,
            },
        );

        assert_eq!(stamp["tool_policy_id"], json!(CODING_TOOL_POLICY_ID));
        assert_eq!(stamp["tool_contract_id"], json!(CODING_TOOL_CONTRACT_ID));
        assert_eq!(
            stamp["tool_contract_version"],
            json!(CODING_TOOL_CONTRACT_VERSION)
        );
        assert_eq!(stamp["model_toolset"], json!(CODING_MODEL_TOOLSET));
        assert_eq!(
            stamp["dynamic_tool_discovery"],
            json!(CODING_DYNAMIC_TOOL_DISCOVERY)
        );
        assert_eq!(stamp["mcp_servers"][0]["id"], json!("github"));
        assert_eq!(stamp["mcp_servers"][0]["tool_count"], json!(4));
    }
}
