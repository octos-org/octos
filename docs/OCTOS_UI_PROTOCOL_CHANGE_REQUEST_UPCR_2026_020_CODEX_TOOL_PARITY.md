# UPCR-2026-020: AppUI Coding Tool Contract Inspection

Status: proposed
Date: 2026-05-15

## Summary

Expose the server-owned coding tool contract through AppUI so clients can see
whether an Octos coding session is Codex-compatible without letting the client
construct or invoke model tools directly.

The Codex comparison showed that Octos has a richer platform tool system
(profiles, memory, app bundle skills, customer tools, MCP, MoFA, workspace
contracts), but it is missing several coding-agent primitives that Codex models
can rely on:

- `apply_patch`
- `exec_command`
- `write_stdin`
- `update_plan`
- `request_user_input`
- `spawn_agent`
- `send_input`
- `resume_agent`
- `wait_agent`
- `close_agent`
- `view_image`
- `tool_search`
- `tool_suggest`
- optional generic `image_generation`

These are model-visible tools, not AppUI client commands. TUI and web clients
must observe tool availability, policy, aliases, and runtime evidence through
server truth.

This UPCR does not define the durable agent lifecycle, persisted goals, or
Claude Code-style `/loop` recurring prompt runtime. Those are defined by
UPCR-2026-021 and `workstreams/M15-agent-goal-loop-autonomy.md`.

## Decision

Do not add AppUI methods that let clients invoke arbitrary model tools.

Do extend `tool/status/list` and `session/status/read` so clients can inspect
the effective coding tool contract for the selected profile/session.

Do add backend model-visible tool implementations or aliases that are resolved
through the same profile runtime factory as every other Octos coding session.
The tool contract must respect profile, memory, MCP, skill, sandbox, approval,
QoE, and model-portfolio policy.

## Capabilities

Servers that support this contract advertise:

- `coding.tool_contract.v1`

Optional finer-grained capabilities:

- `coding.patch_tool.v1`
- `coding.exec_session.v1`
- `coding.plan_tool.v1`
- `coding.user_input_tool.v1`
- `coding.subagent_aliases.v1`
- `coding.image_view.v1`
- `coding.dynamic_tool_search.v1`
- `coding.image_generation.v1`

Capability-gated fields must be omitted when the corresponding capability is
not negotiated.

## AppUI Surface

No new command method is required. This UPCR extends existing methods from
UPCR-2026-017.

### `session/status/read`

When `coding.tool_contract.v1` is negotiated, `runtime_policy_stamp` must add:

```json
{
  "tool_contract_id": "codex-compatible-coding-v1",
  "tool_contract_version": "1",
  "model_toolset": "coding",
  "dynamic_tool_discovery": "enabled"
}
```

The stamp must describe the effective server state. It must not echo a
client-requested or locally inferred state.

### `tool/status/list`

When `coding.tool_contract.v1` is negotiated, the result includes
`coding_tool_contract`:

```json
{
  "profile_id": "coding",
  "session_id": "coding:local:tui#coding",
  "coding_tool_contract": {
    "id": "codex-compatible-coding-v1",
    "version": "1",
    "status": "ready",
    "required_tools": [
      {
        "name": "apply_patch",
        "category": "edit",
        "status": "available",
        "backend_tool": "apply_patch",
        "aliases": [],
        "capability": "coding.patch_tool.v1",
        "policy": "allowed"
      },
      {
        "name": "exec_command",
        "category": "runtime",
        "status": "available",
        "backend_tool": "exec_command",
        "aliases": ["shell"],
        "capability": "coding.exec_session.v1",
        "policy": "approval_gated"
      }
    ],
    "missing_required_tools": [],
    "policy": {
      "tool_policy_id": "coding-v1",
      "sandbox_mode": "workspace-write",
      "approval_policy": "on-request"
    }
  }
}
```

Tool status values:

- `available`
- `aliased`
- `disabled_by_policy`
- `missing`
- `unimplemented`

`missing_required_tools` must list any Codex-parity tool that the backend
cannot provide for the effective profile. Clients may render this as a warning,
but they must not fabricate local fallback tools.

## Backend Tool Contract

Required P0 tools for coding parity:

- `apply_patch` — structured/freeform patch edits with preview/evidence events.
- `exec_command` — command execution with session id, timeout, PTY option, and
  long-running process state.
- `write_stdin` — send input to an existing `exec_command` session.
- `update_plan` — model-visible structured task plan updates.
- `request_user_input` — model-visible structured user decision request.
- `spawn_agent`, `send_input`, `resume_agent`, `wait_agent`, `close_agent` —
  Codex-compatible aliases backed by Octos `TaskSupervisor`.

Required P1 tools:

- `view_image` — local image or screenshot inspection.
- `tool_search` and `tool_suggest` — dynamic discovery for MCP, app bundle,
  customer, platform, and skill tools.

Optional P2 tool:

- `image_generation` — generic alias to an installed Octos/MoFA media tool.

Existing Octos tools remain valid and should not be removed: `read_file`,
`write_file`, `edit_file`, `diff_edit`, `shell`, `glob`, `grep`, `list_dir`,
`web_search`, `web_fetch`, `browser`, `manage_skills`, memory tools, workspace
contract tools, research tools, MCP tools, app bundle tools, customer tools,
and MoFA tools.

## Security And Runtime Rules

- Tool contract resolution happens inside the server-owned session runtime
  factory.
- The backend must deny by default when profile, model, memory, MCP, skill,
  sandbox, approval, or tool policy cannot be resolved.
- Tool aliases must be policy-equivalent to their backend tools.
- Dangerous command/session tools must honor the effective permission profile.
- AppUI clients must not infer tool availability from local config files.
- Tool contract state must be identical over WebSocket and stdio.

## Error Model

Add these structured error kinds where the existing AppUI taxonomy needs more
specific data:

- `tool_contract_unavailable`
- `coding_tool_denied`
- `coding_tool_missing`
- `exec_session_unknown`

Structured error `data` should include `session_id`, `profile_id`,
`tool_name`, `tool_contract_id`, `policy`, and `recoverable` when applicable.
Secret values must be omitted.

## Compatibility

Older servers omit `coding.tool_contract.v1` and do not include
`coding_tool_contract`. Clients must keep the existing tool/MCP UI usable and
show only a capability-gated warning for missing Codex-compatible coding
contract data.

## Tests

- AppUI `tool/status/list` exposes the effective coding tool contract only when
  `coding.tool_contract.v1` is negotiated.
- `session/status/read` includes the coding tool contract stamp.
- Missing P0 tools appear in `missing_required_tools`.
- Disabled tools include policy reasons and are not advertised to the model.
- WebSocket and stdio return the same contract payloads.
- Live tmux soak proves `apply_patch`, `exec_command`/`write_stdin`,
  `update_plan`, `request_user_input`, and subagent aliases work through the
  backend runtime without TUI orchestration.
