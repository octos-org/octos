spec: task
name: "Native ARC Agent Task v1 over MCP"
tags: [arc, mcp, agent-task, interoperability, schema]
estimate: 1d
---

## Intent

Octos 的 `run_octos_session` 当前只把 `input.prompt` 当作普通用户文本。ARC
虽然可以把编译任务序列化进 prompt，但 Octos 无法识别任务版本、无法把
`system_prompt` 映射为真实 system message，也无法在返回前验证阶段 JSON
交付物。

本任务让 Octos 原生接受 `input.arc_task` 中的 `arc.agent-task.v1`，把它编译为
Octos 的 system prompt、结构化 custom task 和 workspace artifact contract。
旧的 `input.prompt` 调用保持兼容。

## Decisions

- MCP 工具保持只有 `run_octos_session` 一个，不新增 ARC 专用工具，避免破坏
  现有 outer orchestrator 的工具发现假设。
- `contract` 继续表示 Octos workspace contract（ARC 默认使用 `coding`）；
  ARC 执行包放在 `input.arc_task`，其 `schema` 必须精确等于
  `arc.agent-task.v1`。
- `arc_task.system_prompt` 必须通过 Octos Agent 的 system-prompt API 注入，
  不得复制进普通 user prompt。
- `arc_task.message`、`inputs`、`acceptance`、`response_schema` 和任务标识编译为
  `TaskKind::Custom` 的结构化参数；`expected_artifact` 作为明确的 delivery
  参数传入。
- 原生 ARC 模式要求 `expected_artifact` 是 workspace 内的相对路径，拒绝绝对
  路径、`..` 穿越以及与 `mcp-serve --cwd` 不一致的 `workspace_root`。
- 当 `response_schema` 存在时，Octos 在返回 Ready 前读取 JSON artifact 并验证
  ARC 当前使用的 JSON Schema 子集：`$ref/$defs`、`type`、`required`、
  `properties`、`items`、`anyOf`、`oneOf`、`enum`、`const` 和
  `additionalProperties: false`。
- 原生 ARC task 解析或 artifact schema 验证失败时 fail closed；不得回退到
  `input.prompt` 或把失败结果标记为 Ready。
- 没有 `input.arc_task` 的旧调用继续使用原有 prompt 和 artifact 语义。

## Boundaries

### Allowed Changes

- specs/arc-agent-task-v1-mcp.spec.md
- crates/octos-agent/src/arc_task.rs
- crates/octos-agent/src/lib.rs
- crates/octos-agent/src/mcp_server.rs
- crates/octos-agent/tests/mcp_server.rs
- crates/octos-cli/src/commands/mcp_serve.rs
- crates/octos-cli/tests/mcp_serve_integration.rs
- docs/ARC_AGENT_TASK_MCP.md

### Forbidden

- 不修改 `octos-tui` 或 UI protocol。
- 不新增第二个 MCP tool。
- 不移除或重命名 `input.prompt`、`expected_artifact`、`artifact_name`。
- 不允许 ARC task 中的 workspace 路径扩大 `mcp-serve --cwd` 的权限边界。
- 不在校验失败后调用 LLM。
- 不增加新的 crate 依赖。
- 不修改本任务开始前已存在的未跟踪文档。

## Out of Scope

- MCP 内部 tool-call/progress 流式事件。
- ARC task 的远程队列、resume 或多 workspace 进程池。
- 把 ARC `skills` 路径自动安装为 Octos skills。
- 更改 Octos workspace-policy 格式。
- 使用真实付费模型的效果评估。

## Completion Criteria

### Rule: arc-contract — MCP exposes and validates the versioned ARC task

Scenario: MCP tool advertises the native ARC task input
  Test:
    Package: octos-agent
    Filter: run_octos_session_schema_advertises_arc_agent_task_v1
  Level: integration
  Targets: crates/octos-agent/src/mcp_server.rs, crates/octos-agent/tests/mcp_server.rs
  Given an MCP client calls `tools/list`
  When Octos describes `run_octos_session`
  Then the `input` schema documents optional `arc_task`
  And the nested task schema requires `schema`, task identity, workspace, prompts, inputs and acceptance
  And legacy `contract` plus `input` remain the only top-level required arguments

Scenario: Valid ARC task v1 parses as a typed contract
  Test:
    Package: octos-agent
    Filter: arc_agent_task_v1_parses_and_preserves_structured_fields
  Level: unit
  Targets: crates/octos-agent/src/arc_task.rs parser
  Given `input.arc_task` contains every required `arc.agent-task.v1` field
  When Octos parses it for a matching workspace
  Then Octos returns a typed ARC task
  And inputs, acceptance, response schema and requested skills are preserved

Scenario: Invalid ARC task fails closed before execution
  Test:
    Package: octos-agent
    Filter: arc_agent_task_v1_rejects_invalid_schema_and_workspace_escape
  Level: unit
  Test Double: temporary workspace
  Targets: ARC task parser and workspace boundary validation
  Given an ARC task has an unsupported schema, mismatched workspace, escaping artifact path,
  or `acceptance.response_schema_required=true` with a null response schema
  When Octos validates the MCP input
  Then validation returns a typed `arc_task_invalid` error
  And no legacy prompt fallback is selected

### Rule: native-mapping — ARC fields map to Octos execution semantics

Scenario: ARC role is mapped to the real Octos system prompt
  Test:
    Package: octos-cli
    Filter: arc_agent_task_uses_native_system_prompt_and_structured_task
  Level: integration
  Test Double: recording LLM provider
  Given a valid ARC task and a conflicting legacy prompt sentinel
  When `RealSessionDispatch` runs the task
  Then the first LLM request contains the ARC role in a system message
  And no user message contains the ARC role or legacy prompt sentinel
  And a user message contains the task id, instruction, inputs, acceptance and delivery path

### Rule: artifact-verification — ARC response schemas gate Ready outcomes

Scenario: Valid ARC JSON artifact reaches Ready
  Test:
    Package: octos-cli
    Filter: arc_agent_task_accepts_artifact_matching_response_schema
  Given a valid ARC task declares an object response schema
  And the expected artifact contains matching JSON
  When the Octos session completes
  Then the outcome is Ready
  And `artifact_content` contains the validated JSON

Scenario: Invalid ARC JSON artifact fails schema verification
  Test:
    Package: octos-cli
    Filter: arc_agent_task_rejects_artifact_violating_response_schema
  Given a valid ARC task declares required nested response fields
  And the expected artifact contains invalid JSON or a schema mismatch
  When Octos verifies the artifact
  Then the outcome is Failed
  And the error starts with `artifact_schema_invalid:`

Scenario: An unsuccessful agent attempt cannot reuse a stale artifact
  Test:
    Package: octos-cli
    Filter: arc_agent_task_rejects_stale_artifact_when_agent_reports_failure
  Level: integration
  Test Double: scripted LLM provider with a one-iteration budget
  Targets: crates/octos-cli/src/commands/mcp_serve.rs terminal outcome handling
  Given an artifact from an older attempt already exists
  And the current Agent run returns `TaskResult { success: false }`
  When Octos handles the ARC session outcome
  Then the outcome is Failed without entering Verifying
  And no stale artifact path or content is returned

### Rule: backwards-compatibility — Native ARC support does not break prompt callers

Scenario: Legacy prompt sessions remain compatible
  Test:
    Package: octos-cli
    Filter: legacy_prompt_session_remains_compatible_without_arc_task
  Level: integration
  Test Double: recording LLM provider
  Targets: crates/octos-cli/src/commands/mcp_serve.rs legacy dispatch path
  Given an MCP input has `prompt` and no `arc_task`
  When `RealSessionDispatch` executes it
  Then the legacy prompt reaches the LLM
  And the existing artifact path and Ready semantics are unchanged
