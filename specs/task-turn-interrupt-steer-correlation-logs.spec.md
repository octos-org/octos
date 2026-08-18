spec: task
name: "interrupt/steer 生命周期的 session/turn 关联日志"
tags: [ui-protocol, observability, interrupt, steer, octos-cli]
estimate: 0.5d
---

## Intent

事故（2026-08-17）排查时，服务端日志里 `turn/interrupt` 的收到/裁决/ack 没有任何
INFO 记录，agent 侧的 `calling LLM`、`executing tool batch`、`draining mid-turn
steer input` 等行也不带 session/turn，只能靠 ledger 反推中断时刻与 steer 命运。
本任务补齐关联：把 agent 任务包进带 `session`/`turn` 字段的 tracing span，让
agent 侧所有日志自动携带二者；并在 interrupt 收到/裁决/ack 与 steer 受理（含
"turn 正在 Interrupting 时仍受理"）处输出 INFO 级结构化日志。日志只记 id、计数与
状态，绝不记录用户文本。

## Decisions

<!-- lint-ack: verification-metadata-suggestion — 日志场景以进程内 tracing 订阅器捕获文本断言，无外部 I/O -->

- 新增不依赖 `api` feature 的模块 `octos_cli::turn_trace`：
  `turn_span(session_id, turn_id) -> tracing::Span`（`info_span!("turn", session, turn)`），
  以及 `log_interrupt_received`、`log_interrupt_outcome(outcome)`、
  `log_interrupt_ack(ack)`、`log_steer_accepted(interrupting)`；全部 INFO 级、字段
  名固定为 `session`/`turn`/`outcome`/`ack`/`interrupting`。
- `run_standalone_turn` 中 `tokio::spawn` 的 agent future 用 `turn_span` 包裹
  （`tracing::Instrument`），使 agent 侧既有日志继承 `session`/`turn`。
- `handle_turn_interrupt`：进入时 `log_interrupt_received`；按 `InterruptOutcome`
  分支记录 `outcome` ∈ {`captured`,`already_interrupting`,`already_terminal:<reason>`,
  `mismatch`,`unknown`}；`Captured` 分支等待 ack 后记录 `ack` ∈
  {`interrupted`,`ack_timed_out`}。
- `turn/steer` 受理日志改为 `log_steer_accepted(interrupting)`，`interrupting` 表示
  受理时 turn 已处于 `Interrupting`（这类输入大概率会在 turn 退出时被返还）。
- 日志内容边界：不记录 steer 文本、prompt 或工具参数。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/lib.rs
- crates/octos-cli/src/turn_trace.rs
- crates/octos-cli/src/api/ui_protocol_transport.rs
- specs/task-turn-interrupt-steer-correlation-logs.spec.md

### Forbidden
- 不改变 interrupt/steer 的任何行为、返回值或时序（只加日志与 span）。
- 不在日志中输出用户文本、prompt、工具参数或 token 内容。
- 不把日志级别提高到 WARN/ERROR（正常生命周期为 INFO）。
- 不新增 crate 依赖。

## Completion Criteria

### Rule: correlated-lifecycle-logs — 生命周期日志携带 session/turn
Scenario: interrupt 收到/裁决/ack 三条日志都带 session 与 turn（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: interrupt_lifecycle_logs_carry_session_and_turn
  Given 一个捕获输出的 tracing 订阅器与 `octos_cli::turn_trace` 模块
  When 依次调用 `log_interrupt_received`、`log_interrupt_outcome("captured")`、`log_interrupt_ack("interrupted")`
  Then 三行日志都包含 `session=<id>` 与 `turn=<id>`
  And 分别包含 `outcome=captured` 与 `ack=interrupted`
  And 级别均为 INFO

Scenario: agent 侧日志在 turn span 内自动携带 session/turn
  Test:
    Package: octos-cli
    Filter: agent_logs_inside_turn_span_carry_session_and_turn
  Given `turn_span(session, turn)` 已进入作用域
  When 记录一条不带任何字段的 `tracing::info!`
  Then 捕获输出的该行包含 span 名 `turn` 及 `session=<id>`、`turn=<id>`

Scenario: steer 受理日志标明 turn 是否正在 Interrupting
  Test:
    Package: octos-cli
    Filter: steer_accepted_log_marks_interrupting_turns
  When 分别以 `interrupting = false` 与 `true` 调用 `log_steer_accepted`
  Then 两行都包含 `session=`、`turn=` 且分别包含 `interrupting=false`、`interrupting=true`

### Rule: no-user-text — 日志不泄露用户内容
Scenario: 日志函数不接收也不输出用户文本（错误路径）
  Test:
    Package: octos-cli
    Filter: correlation_logs_never_contain_user_text
  Given 一个含唯一标记的 steer 文本存在于调用方
  When 调用全部 `turn_trace` 日志函数
  Then 捕获输出不包含该标记

Scenario: 未知/不匹配裁决同样记录且不升级级别（错误路径）
  Test:
    Package: octos-cli
    Filter: interrupt_outcome_logs_cover_rejections_at_info
  When 以 `unknown`、`mismatch`、`already_terminal:interrupted` 调用 `log_interrupt_outcome`
  Then 三行都以 INFO 记录并包含各自的 `outcome=` 值

## Out of Scope

- 把 span 传播到 octos-agent crate 内部的更细粒度（LLM provider、tool 执行）日志。
- 日志格式/JSON 输出配置。
- F8：`octos serve` 的 fd 累积。
