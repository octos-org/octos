spec: task
name: "turn/interrupt 立即跳出 progress 等待，不依赖下一条 progress 事件"
tags: [ui-protocol, interrupt, latency, octos-cli]
estimate: 0.5d
---

## Intent

真机验证（2026-08-18）发现：`run_standalone_turn` 的事件循环在
`tokio::select!` 的 interrupt 分支里 `interrupt_observed = true; continue;`，随后
再次进入 select 时该分支已被 `if !interrupt_observed` 禁用，只能等
`progress_rx.recv()` 返回下一条 progress 事件才走到 `if interrupt_observed { break }`。
当 agent 正在执行没有进度输出的工具（如 `bash sleep`）时，终态要等到 ~8 秒一次的
`status_word` 心跳才发出：客户端 `turn/interrupt` 的 ack 在 5 秒超时
（`ack=ack_timed_out`），用户以为 Esc 无效而连按，多余的 Esc 又打断了随后自动
提交的下一回合。本任务让中断在被观察到的那一刻就跳出循环，与 progress 节奏无关。

## Decisions

<!-- lint-ack: verification-metadata-suggestion — 场景为进程内 tokio channel 单元测试，无外部 I/O -->
<!-- lint-ack: decision-coverage — "不改变中断之后的处理顺序"是不变项声明，由结构检查场景（循环体只替换取事件方式）间接约束 -->
<!-- lint-ack: observable-decision-coverage — 同上，处理顺序不变由既有 interrupt 测试与结构检查共同覆盖 -->

- 把循环里的 select 抽成可测的 `next_turn_loop_step(interrupt_rx, progress_rx,
  interrupt_observed) -> TurnLoopStep`（`Interrupted | Progress(String) |
  Closed`），保持 `biased` 且 interrupt 优先；循环里 `Interrupted => { interrupt_observed
  = true; break; }`——不再 `continue`。
- 行为不变项：interrupt 未到时 progress 事件照常返回；progress 通道关闭返回
  `Closed`；interrupt 已观察过后不再重复报告（与原 `if !interrupt_observed` 门等价）。
- 不改变中断之后的处理顺序（abort agent → 取消后台任务 → 终态闸门），只改"何时
  开始处理"。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/api/ui_protocol_transport.rs
- crates/octos-cli/src/api/ui_protocol_tests.rs
- crates/octos-cli/src/turn_loop.rs
- crates/octos-cli/src/lib.rs
- specs/task-interrupt-breaks-progress-wait.spec.md

### Forbidden
- 不改变 `turn/interrupt` 的 RPC 结果形状或 5 秒 ack 超时。
- 不改变 progress 事件的解析与转发逻辑。
- 不新增 crate 依赖。

## Completion Criteria

### Rule: interrupt-wakes-immediately — 中断不等 progress
Scenario: 没有任何 progress 事件时，interrupt 立即返回 Interrupted（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: interrupt_returns_immediately_without_progress_events
  Given progress 通道空闲（模拟无输出的长工具）
  When 发送 interrupt 信号
  Then `next_turn_loop_step(interrupt_rx, progress_rx, interrupt_observed)` 在 100ms 内返回 `TurnLoopStep::Interrupted`
  And 不需要任何 progress 事件

Scenario: 无 interrupt 时 progress 事件正常返回
  Test:
    Package: octos-cli
    Filter: progress_events_flow_when_no_interrupt
  Given 一条 progress 事件已入队
  When 调用 `next_turn_loop_step`
  Then 返回 `Progress` 且载荷原样

Scenario: interrupt 与 progress 同时就绪时 interrupt 优先（biased）
  Test:
    Package: octos-cli
    Filter: interrupt_wins_over_ready_progress
  Given interrupt 与一条 progress 都已就绪
  When 调用 `next_turn_loop_step`
  Then 返回 `Interrupted`

Scenario: interrupt 已观察后不再重复报告，progress 关闭返回 Closed（错误路径）
  Test:
    Package: octos-cli
    Filter: observed_interrupt_is_not_reported_twice_and_closed_channel_ends_loop
  Given `interrupt_observed = true` 且 progress 通道已关闭
  When 调用 `next_turn_loop_step`
  Then 返回 `Closed` 而不是 `Interrupted`

Scenario: 生产循环使用 helper 且 interrupt 分支直接 break（结构检查）
  Test:
    Package: octos-cli
    Filter: standalone_turn_loop_breaks_on_interrupt_step
  When 扫描 `crates/octos-cli/src/api/ui_protocol_transport.rs`
  Then `run_standalone_turn` 的循环通过 `next_turn_loop_step` 取事件
  And 不再存在 `interrupt_observed = true;` 后接 `continue;` 的写法

## Out of Scope

- stdio 请求分发在等待 interrupt ack 期间的队头阻塞（另立任务）。
- 客户端对 `ack_timed_out` 的展示。
