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
// Optional capabilities declared by UPCR-2026-020 §3.
//
// `image_view` and `dynamic_tool_search` are wired in by #972 / M14-B P1 and
// are advertised whenever the server's `with_builtins` registers
// `view_image`, `tool_search`, and `tool_suggest` (see
// `ui_protocol_transport::derived_capabilities`).
//
// `image_generation` has no native or skill backend bound to it yet, so the
// constant exists for vocabulary parity but the capability is not
// advertised. See UPCR §5: capability-gated fields must be omitted when the
// corresponding capability is not negotiated.
pub(crate) const CODING_IMAGE_VIEW_CAPABILITY_V1: &str = "coding.image_view.v1";
pub(crate) const CODING_DYNAMIC_TOOL_SEARCH_CAPABILITY_V1: &str = "coding.dynamic_tool_search.v1";
#[allow(dead_code)]
pub(crate) const CODING_IMAGE_GENERATION_CAPABILITY_V1: &str = "coding.image_generation.v1";
// #1172 — Codex naming-parity aliases. The underlying capability already
// rides on `shell` / `exec_command`, `spawn_agent` + `wait_agent`, and the
// (compile-time) browser tool, so these flags exist to advertise the
// Codex-compatible spellings to clients negotiating tool surface area.
pub(crate) const CODING_BASH_CAPABILITY_V1: &str = "coding.bash.v1";
pub(crate) const CODING_DELEGATE_CAPABILITY_V1: &str = "coding.delegate.v1";
pub(crate) const CODING_BROWSER_CAPABILITY_V1: &str = "coding.browser.v1";

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
/// #1149 — typed error kind returned by `image_generation` when the tool
/// is registered but no native or skill backend is bound for the active
/// profile. Distinguishes "the contract advertises this name, but no
/// implementation is available right now" from `coding_tool_missing`
/// (path/resource not found) and `coding_tool_denied` (policy refusal),
/// so AppUI clients can surface the right next step (install a skill,
/// switch provider) instead of generic "tool error".
#[allow(dead_code)]
pub(crate) const ERROR_KIND_CODING_TOOL_UNSUPPORTED: &str = "coding_tool_unsupported";

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
    // #972 / M14-B P1 — Codex-compatible image inspection and dynamic
    // tool discovery. These resolve through the same profile runtime as
    // the P0 set and respect the active filesystem scope.
    "view_image",
    "tool_search",
    "tool_suggest",
    // Compaction replaces evicted tool outputs with a placeholder that tells
    // the model to `recall` them by tool_call_id; the tool has to be visible
    // on the coding contract for that advice to be actionable, otherwise the
    // only way back is a fresh read or command.
    "recall",
    // #1172 — Codex naming-parity aliases. `bash` shares the runtime with
    // `shell` / `exec_command`; `delegate` chains `spawn_agent` +
    // `wait_agent` so the Codex-compatible one-call lifecycle is visible
    // alongside the canonical primitives.
    "bash",
    "delegate",
    // #1149 / M14-B P2 — Codex-compatible `image_generation` entry. The
    // tool is registered so the wire-level contract is complete and the
    // model gets a typed `coding_tool_unsupported` response instead of a
    // generic "tool not found", but no native or skill backend is bound
    // yet. See `crates/octos-agent/src/tools/coding_tools.rs`
    // (`ImageGenerationTool`) and #1149 for the follow-up to wire a
    // real backend (OpenAI image API or a bundled skill).
    "image_generation",
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
    /// Names of tools registered but not visible yet recoverable. RFC-0
    /// (#1289) removed LRU tool deferral, so this is now always empty in
    /// practice; the field is retained for wire-contract compatibility.
    pub deferred_model_tools: &'a [&'a str],
    pub include_coding_tool_contract: bool,
}

impl<'a> ToolStatusListContext<'a> {}

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
    pub enabled: bool,
    pub status: &'a str,
    pub tool_count: usize,
    pub tools: &'a [&'a str],
    pub error: Option<&'a str>,
    /// T-0538: whether this connector uses OAuth. Always present in the wire
    /// (false included) so the web can offer re-authorization on failure.
    pub oauth: bool,
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
            "Octos spawn exists; spawn_agent is the Codex-compatible model-visible alias when the session runtime binds a supervisor-backed delegate.",
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
        detail: Some(
            "Octos subagent launcher; spawn_agent is the Codex-compatible supervisor alias.",
        ),
    },
    OctosToolSpec {
        name: "read_task_output",
        category: "agent",
        aliases: &["wait_agent"],
        policy: "allowed",
        detail: Some(
            "Background task output reader; wait_agent inspects the same TaskSupervisor state.",
        ),
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
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Skill management. Codex dynamic tool discovery aliases live on dedicated tool_search / tool_suggest entries.",
        ),
    },
    // #972 / M14-B P1: canonical Codex dynamic tool discovery surface.
    OctosToolSpec {
        name: "tool_search",
        category: "discovery",
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Canonical Codex tool_search entry. Returns ranked matches from the active coding tool contract.",
        ),
    },
    OctosToolSpec {
        name: "tool_suggest",
        category: "discovery",
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Canonical Codex tool_suggest entry. Recommends tools for a free-form task description.",
        ),
    },
    // Restore an evicted tool output by the tool_call_id carried on its
    // compaction placeholder (exact recorded output, no re-execution).
    OctosToolSpec {
        name: "recall",
        category: "read",
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Restores a tool output that compaction replaced with a placeholder, by the placeholder's tool_call_id.",
        ),
    },
    // #972 / M14-B P1: canonical Codex image-view surface.
    OctosToolSpec {
        name: "view_image",
        category: "read",
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Canonical Codex view_image entry. Returns format / MIME / byte length for a workspace image.",
        ),
    },
    // #1149 / M14-B P2: canonical Codex image-generation surface.
    //
    // Registered as a model-visible stub so the wire-level contract is
    // complete; the tool currently returns `coding_tool_unsupported` for
    // every call because no native or skill backend is bound yet. The
    // `coding.image_generation.v1` capability is intentionally NOT
    // advertised in `ui_protocol_transport::advertised_capabilities` so clients
    // don't render it as usable (UPCR-2026-020 §5: capability-gated
    // fields are omitted when the implementation isn't bound). Follow-up
    // to wire a real backend tracked in #1149.
    OctosToolSpec {
        name: "image_generation",
        category: "media",
        aliases: &[],
        policy: "allowed",
        detail: Some(
            "Canonical Codex image_generation entry. Stub: no native or skill backend bound yet; calls return a typed coding_tool_unsupported response (#1149 follow-up).",
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
    // #1172 — Codex naming-parity aliases. Surface them in the tool
    // contract so `tool/status/list` advertises the new spellings.
    OctosToolSpec {
        name: "bash",
        category: "runtime",
        aliases: &["shell", "exec_command"],
        policy: "approval_gated",
        detail: Some(
            "Codex-compatible one-shot shell alias. Shares the command policy / approval policy / sandbox with shell and exec_command.",
        ),
    },
    OctosToolSpec {
        name: "delegate",
        category: "agent",
        aliases: &["spawn_agent", "wait_agent"],
        policy: "allowed",
        detail: Some(
            "Codex-compatible one-call wrapper: spawn_agent + wait_agent + result extraction. Role resolves through the M14-C role template registry.",
        ),
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

    // Issue #971 (M14-C): advertise the four backend-owned role
    // templates so the AppUI/TUI can render a spawn-role picker
    // without hard-coding the names client-side. Each entry is the
    // bounded `RoleTemplateSummary` projection — the server-owned
    // `prompt_prefix` stays off the wire. The acceptance gate
    // (`Test: role/tool/sandbox/model policy is resolved by the
    // server runtime`) checks for this field's presence + shape.
    let role_templates: Vec<Value> = octos_agent::RoleTemplate::all()
        .iter()
        .map(|tpl| {
            serde_json::to_value(tpl.summary()).unwrap_or_else(|_| {
                // Should not happen — RoleTemplateSummary is plain
                // &str / &[&str] and infallibly serializable. Fall
                // back to the role name so the client still gets
                // SOMETHING bounded to render.
                json!({ "name": tpl.name })
            })
        })
        .collect();

    json!({
        "id": CODING_TOOL_CONTRACT_ID,
        "version": CODING_TOOL_CONTRACT_VERSION,
        "feature": CODING_TOOL_CONTRACT_FEATURE_V1,
        "status": status,
        "required_tool_names": CODING_P0_REQUIRED_TOOL_NAMES,
        "required_tools": required_tools,
        "missing_required_tools": missing_required_tools,
        "deferred_model_tools": deferred_names,
        "role_templates": role_templates,
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

    // #970: a tool registered but currently in the deferred set is
    // recoverable through `activate_tools`. Surface it as `deferred` so
    // clients can render "available, currently inactive" UX; it counts
    // as available for the contract's `missing_required_tools` filter.
    // The live registry can include deferred tools in the disabled set
    // because `specs()` hides them while `tool_names()` still reports
    // them as registered, so deferred must win over disabled-by-policy.
    if deferred_model_tools.contains(spec.name) {
        entry.insert("status".into(), json!(TOOL_STATUS_DEFERRED));
        entry.insert("backend_tool".into(), json!(spec.name));
        entry.insert(
            "detail".into(),
            json!("registered but currently auto-deferred; recoverable via activate_tools"),
        );
        return Value::Object(entry);
    }

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

    if let Some(alias) = first_present(spec.aliases, available_model_tools) {
        let status = match spec.name {
            "spawn_agent" | "wait_agent" => TOOL_STATUS_ALIASED,
            _ => TOOL_STATUS_UNIMPLEMENTED,
        };
        entry.insert("status".into(), json!(status));
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
        "enabled": server.enabled,
        "status": server.status,
        "tool_count": server.tool_count,
        "tools": server.tools,
        // UPCR-2026-R3B3ADF57 (T-0538): always-present additive oauth flag.
        "oauth": server.oauth,
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
    let mut closed = 0;
    for server in servers {
        match server.status {
            MCP_STATUS_CONNECTED => connected += 1,
            MCP_STATUS_CONNECTING => connecting += 1,
            MCP_STATUS_FAILED => failed += 1,
            MCP_STATUS_DISABLED => disabled += 1,
            "closed" => closed += 1,
            _ => {}
        }
    }
    json!({
        "connected": connected,
        "connecting": connecting,
        "failed": failed,
        "disabled": disabled,
        "closed": closed,
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
