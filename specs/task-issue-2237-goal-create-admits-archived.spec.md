spec: task
name: "goal_create 准入把 archived 视为终态(issue #2237)"
tags: [goals, autonomy, agent-orchestrator, octos-cli]
estimate: 0.5d
---

## Intent

`model_create_goal` 的准入判定写的是"现有 goal 状态是否为 `complete`",语义应是"是否
未完成"。会话上一个 goal 为 `archived`(操作者归档,终态)时,新 goal 被拒绝
`cannot create a new goal … (status \`archived\`)`,`goal_update` 也拒绝,master 只能裸驱
peer、收工无法 `/goal stop`(2026-09-04 OLP #45 实证,issue #2237)。本任务把准入谓词
改为"任一终态均可替换",并保持"未完成拒绝、替换终态必铸新 id"两条既有承诺不变。

## Decisions

- 终态集合定义为一个私有常量函数 `goal_status_is_terminal(status: &str) -> bool`,对
  `"complete"` 与 `"archived"` 返回 true,其余(含 `active`、`blocked`、`paused`、
  `budget_limited`)返回 false;放在 `agent_orchestrator.rs` 中 `model_create_goal` 附近,
  并被 `model_create_goal` 的准入判定(现 L8992 `existing.status != "complete"`)调用。
- 拒绝文案不变(`cannot create a new goal because this session has an unfinished goal …`),
  只是不再对终态触发。
- 替换终态 goal 走既有 Fix B 路径:铸新 goal id,旧 ledger 不触碰。
- `goal_update` 对 `archived` goal 的拒绝文案改为
  `goal is archived; create a new goal instead`(替换现有 stale-verdict 文案在该分支的用词),
  其它分支不动。
- 不改 TUI 头部投影(另立;本任务只修准入与 update 文案)。
- 测试与既有 `model_create_goal_gates_on_unfinished_goal`(L23229 附近)同模块、同夹具风格。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/autonomy/agent_orchestrator.rs

### Forbidden
- 不改 goal 状态机的其它转移(archive/reopen/complete/blocked)。
- 不改 goal ledger 的持久化格式与 wire 形状。
- 不改 `goal_create` 的并发准入(#1935 round 7 的 set_goal 原子检查)。
- 不引入新依赖。

## Out of Scope

- TUI 头部把 archived goal 当当前 goal 显示的投影修复。
- 离线 `goal archive` 被 live cache 反盖(#43 已在线化)。

## Acceptance Criteria

Scenario: archived 之后可以创建新 goal(critical)
  Tags: critical
  Test: model_create_goal_admits_after_archived
  Given 会话现有 goal 状态为 archived
  When 以 actor model 调用 model_create_goal 创建新目标
  Then 返回 Ok 且新 goal 的 id 不等于旧 goal 的 id
  And 旧 goal 的 ledger 记录保持不变

Scenario: complete 之后仍可创建新 goal
  Test: model_create_goal_admits_after_complete_unchanged
  Given 会话现有 goal 状态为 complete
  When 调用 model_create_goal
  Then 返回 Ok 且铸出新 id

Scenario: active 仍被拒绝
  Test: model_create_goal_rejects_active_unchanged
  Given 会话现有 goal 状态为 active
  When 调用 model_create_goal
  Then 返回 Err 且错误文案含 "unfinished goal (status `active`)"

Scenario: blocked 与 paused 仍被拒绝
  Test: model_create_goal_rejects_blocked_and_paused
  Given 会话现有 goal 状态分别为 blocked 与 paused
  When 分别调用 model_create_goal
  Then 两次均返回 Err 且文案各含对应状态

Scenario: budget_limited 仍被拒绝
  Test: model_create_goal_rejects_budget_limited
  Given 会话现有 goal 状态为 budget_limited
  When 调用 model_create_goal
  Then 返回 Err 且文案含 "budget_limited"

Scenario: goal_update 对 archived 给出明确指引
  Test: goal_update_on_archived_says_create_new
  Given 会话现有 goal 状态为 archived
  When 以 actor model 调用 goal_update
  Then 返回 Err 且文案含 "goal is archived; create a new goal instead"
