spec: task
name: "原生 peer 报告绑定 runtime turn_id"
tags: [peer, review, provenance, octos-cli]
---

## Intent

让原生 peer 报告与模型事件使用同一个运行时轮次身份。报告文件写入失败后，
后续 result-N 编号可能与 ledger 轮次不同，审查器不能依据文件编号认领模型证据。

## Decisions

- 原生 result.md 与 result-N.md 的 frontmatter 增加当前运行时的 `turn_id`。
- completed 与 error 写入路径传入已有 TurnId，不生成另一个 ID；文件编号和 turns.txt 格式保留。
- 既有文件写入失败仍按原逻辑报告；后续成功报告写入后续真实 ID，不按文件计数回填身份。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/api/ui_protocol_transport.rs
- crates/octos-cli/src/api/ui_protocol_tests.rs
- specs/task-peer-result-turn-identity.spec.md

### Forbidden
- 不改变模型路由、goal 状态机、报告单写者语义或原有文件安全写入方式。
- 不从文件编号推导 TurnId，不伪造旧报告的身份。

## Acceptance Criteria

Scenario: 正常与失败终态报告携带各自真实 ID(critical)
  Test:
    Package: octos-cli
    Filter: peer_fleet_result_writer_and_gather_roundtrip
  Given 一个真实暂存的 peer 与两个不同 TurnId
  When writer 先写 completed 报告再写 errored 报告
  Then result-1.md 保留第一个 ID，result-2.md 与 result.md 携带第二个 ID
  And 既有轮次、历史、gather、非 peer 与未暂存 peer 行为不变

Scenario: 原生文件写入失败后恢复仍携带后续真实 ID(critical)
  Test:
    Package: octos-cli
    Filter: peer_fleet_result_writer_and_gather_roundtrip
  Given peer 的 result-1.md 和 turns.txt 位置被测试自建目录占据
  When writer 遇到实际文件写入错误，移除仅由该测试创建的阻挡目录后写入下一轮
  Then result 文件计数仍从 1 开始，但 turn_id 为后一次真实 TurnId
  And 报告不含前一次丢失写入轮次的 ID

## Out of Scope

- octoscode 的模型证据消费逻辑在关联 PR637 修复。
- 既有报告迁移、goal 自动唤醒与完成验证器故障。
