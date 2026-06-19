spec: task
name: "Approval Post-Tool Continuation"
tags: [approval, matrix, gateway, agent-loop]
estimate: 1d
---

## Intent

OctOS 的 Matrix/Robrix human-approval 流程在用户批准后只执行被批准的 tool 并发出 tool output，随后立即结束该 turn。这个行为让 agent 无法基于真实 tool result 继续完成原任务，也容易诱导在审批恢复路径里硬编码后续动作。本任务把 approved tool 的结果重新接回正常 agent loop，让 agent 依据会话历史、tool result、工具策略和审批策略决定下一步。

## Decisions

- 审批恢复方式: `handle_approval_response_message()` 仍负责校验审批、执行 digest-bound approved tool、记录 audit 和持久化 approval outcome，但不得直接执行 `send_file` 或其他任务特定 follow-up tool。
- continuation 入口: approved tool 执行完成后，SessionActor 生成一条 runtime/internal synthetic inbound，并重新走现有 `process_inbound()` agent loop。
- continuation 上下文: synthetic inbound 必须包含 approved tool 的 tool name、approval title、success/failure 状态、plain output、`file_modified` 和 `files_to_send` 摘要；真实 tool result 仍必须持久化到 session history。
- continuation prompt 边界: runtime synthetic inbound 的 prompt text 使用英文；内容只能陈述 approved tool result 和可供 agent 决策的 metadata，不得注入固定后续指令或预选 follow-up action。
- continuation 策略: continuation turn 使用正常 tool registry、normal LLM decision loop、human approval rules 和 send_file tool policy；如果下一步 tool 再次命中 approval rule，必须再次挂起审批。
- 用户意图边界: approval handler 不把 approved `write_file` 产物自动发送给用户；是否发送文件由 continuation 中的 agent 询问用户或按正常任务语境决定。
- 内部消息标记: synthetic inbound 必须标记为 runtime/internal continuation，不能被当作普通用户新请求，也不能重置自动 recovery/continuation 计数。

## Boundaries

### Allowed Changes
- .gitignore
- **/.gitignore
- crates/octos-cli/src/session_actor.rs
- **/crates/octos-cli/src/session_actor.rs
- crates/octos-agent/src/agent/execution.rs
- crates/octos-agent/src/agent/loop_runner.rs
- crates/octos-agent/src/agent/mod.rs
- crates/octos-cli/tests/**
- crates/octos-agent/tests/**
- specs/task-approval-post-tool-continuation.spec.md
- **/specs/task-approval-post-tool-continuation.spec.md

### Forbidden
- 不要在 approval handler 中直接调用 `send_file`、`message` 或任何任务特定 follow-up tool
- 不要在 English synthetic continuation prompt 中写固定后续指令，例如要求发送文件、询问下载、发送消息或执行某个 follow-up tool
- 不要为 `write_file`、`.html`、`.pptx` 或特定文件名写硬编码分支
- 不要绕过 `human_approval_rules` 或 `ToolRegistry` 的正常执行路径
- 不要修改 Matrix channel media upload/download 协议
- 不要修改 Robrix 客户端代码
- 不要添加新的 crate 依赖

## Out of Scope

- Robrix 下载 spinner 或 Cancel UI 修复
- Matrix 文件上传/下载协议能力扩展
- 新增可视化审批组件
- 改写整个 SessionActor queue mode 架构
- 多 agent/background task completion review 策略重构

## Completion Criteria

Scenario: Approved tool resumes the normal agent loop
  Test:
    Package: octos-cli
    Filter: approved_tool_success_enqueues_internal_continuation_turn
  Given an agent turn is suspended by `human_approval_rules` before executing `list_dir`
  And an authorized approver approves the request with the matching digest
  When `handle_approval_response_message()` executes the approved tool successfully
  Then the tool output is persisted as an approval outcome in session history
  And a runtime/internal synthetic inbound is enqueued or processed
  And the continuation invokes the normal agent loop with the approved tool result visible in context

Scenario: Approved write_file does not auto-send files from the approval handler
  Test:
    Package: octos-cli
    Filter: approved_write_file_continuation_does_not_directly_send_media
  Level: integration
  Test Double: mock LLM provider plus outbound channel receiver
  Targets: SessionActor approval response path and OutboundMessage media emission
  Given an approved `write_file` call writes `rust_slides.html` successfully
  When the approval response is handled
  Then `handle_approval_response_message()` does not call `send_file`
  And no outbound media message is emitted directly by the approval handler
  And the continuation context contains the exact generated path string `rust_slides.html`

Scenario: Synthetic continuation carries facts, not directives
  Test:
    Package: octos-cli
    Filter: approval_continuation_prompt_contains_facts_not_directives
  Level: unit
  Test Double: direct synthetic inbound builder invocation
  Targets: approval continuation prompt and metadata construction
  Given an approved tool succeeds with `file_modified` set to `rust_slides.html`
  When the runtime/internal synthetic inbound is built
  Then its prompt text is English
  And it contains the approved tool name, success status, plain output and generated path
  And it does not contain a fixed instruction to call `send_file`, send media, request download confirmation or execute another follow-up tool
  And the normal agent loop receives it as result context rather than a forced action plan

Scenario: Agent can ask the user for next action after approved write_file
  Test:
    Package: octos-cli
    Filter: approved_write_file_continuation_can_ask_user_to_send_artifact
  Given an approved `write_file` call creates a user-facing file
  And the mock LLM continuation chooses to ask whether the user wants the file sent
  When the continuation turn completes
  Then the outbound assistant message contains the generated filename
  And the outbound assistant message asks for the user's desired next action
  And the outbound assistant message has empty media

Scenario: Continuation may call ordinary tools through the normal policy path
  Test:
    Package: octos-cli
    Filter: approved_tool_continuation_can_use_allowed_tool_normally
  Level: integration
  Test Double: mock LLM provider plus test tool registered in `ToolRegistry`
  Targets: SessionActor continuation path and ToolRegistry execution
  Given an approved tool succeeds and the continuation mock LLM chooses an allowed non-gated tool
  When the continuation turn runs
  Then that tool is executed through the normal `ToolRegistry`
  And the resulting assistant response is persisted like an ordinary agent response

Scenario: Continuation that needs another gated tool suspends for approval again
  Test:
    Package: octos-cli
    Filter: approved_tool_continuation_reenters_approval_for_gated_tool
  Given an approved tool succeeds
  And the continuation mock LLM chooses another tool covered by `human_approval_rules`
  When the continuation turn reaches that tool call
  Then a new approval request is emitted
  And the second gated tool is not executed before approval
  And the original approval decision does not auto-approve the second tool call

Scenario: Approved tool failure is surfaced through continuation without follow-up tool execution
  Test:
    Package: octos-cli
    Filter: approved_tool_failure_continuation_reports_failure_without_followup_tool
  Given an approved tool returns `success: false`
  When the approval response is handled
  Then the failure output is persisted as an approval outcome
  And the continuation context includes the failure status and output
  And no task-specific follow-up tool is executed by the approval handler

Scenario: Invalid or unauthorized approval responses do not trigger continuation
  Test:
    Package: octos-cli
    Filter: rejected_approval_response_does_not_enqueue_continuation
  Given a pending approval exists
  When an unauthorized sender or mismatched digest submits an approval response
  Then the approval response is rejected
  And the approved tool is not executed
  And no synthetic continuation inbound is enqueued or processed

Scenario: Internal continuation is not persisted as a user-authored request
  Test:
    Package: octos-cli
    Filter: approval_continuation_inbound_is_internal_not_user_message
  Given a successful approved tool triggers a synthetic continuation inbound
  When the continuation turn is processed
  Then the synthetic prompt is marked runtime/internal in metadata
  And it does not reset recovery or continuation counters as a normal user message
  And durable history does not present it as a user-authored chat message
