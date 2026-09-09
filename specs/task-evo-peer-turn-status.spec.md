spec: task
name: "peer list 完成状态绑定生命周期、当前轮次与执行结果"
tags: [ui-protocol, observability, peers, octos-cli]
---

## Intent

`octos peer list`（CLI 表格与 `--json`）和 serve 内 `peer_list` 工具索引把
"目录里有 result 文件" 直接当成 done，两类真实事故无法从输出解释：
(1) peer 的 turn 以 errored 终止后 result.md 存在，status=done，消费端以为
成功；(2) round1 completed 之后 round2 已 queued/running，索引仍显示 round1
的 done 旧结果。mutable 的 `result.md` 任意正文不是成功证据；最高编号的
`result-<n>.md` / `turns.txt` 才是终止证据。本任务保留现有
running/done/closed 目录交付语义（status 字段不变），新增可序列化的当前
执行状态（execution）、最近终止 outcome（last_outcome）、当前/已交付轮次
（round / rounds_delivered）与执行身份字段（task/generation/turn_id），未知
处明确 `unknown`，绝不用 mtime 猜成功。读取保持 no-serve 直读、JSON 与人类
表共用同一组装层、symlink-safe。**不新增并行状态机**：current 执行与身份
来自现有 `lifetime.json` 的只读安全投影，outcome 来自 `turns.txt` /
numbered results 终止证据。

## Decisions

- 无新 authority：不新增 `turn-state.json` 或任何并行状态文件。当前执行
  状态与执行身份唯一来源是 `peers/<slug>/lifetime.json`（现有
  Pending/Running/Idle/Failed + generation fence + turn_id/task_id），只读
  投影、不写不改其语义；最近终止 outcome 唯一来源是 `turns.txt` 最后一条
  `(round, outcome, ts)`（与最高编号 `result-<n>.md` 互为印证）。
- lifetime 安全投影校验（fail-closed，验证通过才可采信，异常/不可信一律
  降级 unknown）：version==1、task_id 非空、registry_key 与
  `<profile>:peer:<slug>` 精确匹配、originator leaf 存在且等于
  lifetime.master。registry_key 的 `<profile>` 由调用方提供：CLI 新增
  `--profile <id>` 参数（默认 DEFAULT_PROFILE_ID="octos"），显式传入
  list_peers 与投影校验；serve 回调用其已知 profile_id。`--data-dir` 是
  任意合法路径，**不得**从目录名推导 profile（/tmp 自定义根同样有效）；
  必须有 `--data-dir 自定义根 + --profile octosfix` 组合测试。不从
  lifetime.json 文件内容反推 profile。任何校验失败 → 该 peer 的
  execution/身份字段全部 unknown/null，不用旧 terminal guessed-idle。
  legacy peer 无 lifetime.json：execution=unknown、last_outcome 按
  turns.txt 证据（有则用，无则 null）。
- 执行身份（identity fields，供外层任务监控消费）：投影暴露
  `master_session_id`（取 lifetime.master，即 staging originator session
  id——明确命名，不冒充 peer thread session）、`task_id`、`generation`
  （u64）、`turn_id`（nullable string）。消费端以
  runtime/master_session_id/slug/task_id/generation/turn_id 关联。
- 承载结构：新字段扩展在 `PeerBlackboardRow`（read_peer_blackboard 读取，
  raw_peer_gather / compose_peer_list_text / CLI 三端共用）；CLI
  `PeerListRow` 从 blackboard 行映射，不独立读盘，保证三端不漂移。
- 已知观测限制（gateway inbox 路径，session_actor.rs 无
  begin_peer_lifetime_turn 调用，代码事实 2026-09-09 核实）：Path 1
  in-process inbox 直投的 round2 从 invalidate(Pending) 到 terminal 之间
  无 running 中间态（queued→terminal）。呈现按投影如实输出 queued，为
  已知限制记录于 interface 契约，不在本任务修 gateway 写侧。
- crash 窗口语义（写序以 ui_protocol_transport.rs 为准）：result.md →
  lifetime(finish) → result-<n>.md → turns.txt。finish 前崩溃 →
  投影仍 Running：跨重启恢复扫描不处理 Running（仅 closed 退休与
  Idle 摘要重绑），孤儿 Running 保持 running 直到真实生命周期更新，
  无自动降级、可长期呈 running。finish 后、
  turns.txt append 前崩溃 → phase 已定（Idle/Pending/Failed），呈现
  idle/queued/failed；last_outcome 依 turns 尾行 × 最高 result-<n>.md
  互证——尾行未更新时为旧 outcome 或（双侧不一致时）null，不绝对
  保留旧值。不引入时间戳裁决。
- execution 派生（优先级；无可信 CURRENT authority 时 execution 恒
  unknown，turns.txt 只产出 last_outcome，不推导 execution）：
  1. `closed` 标记存在 → status=closed（不变），execution=`closed`（不是
     idle——closed 是生命周期终态，不代表上一执行成功），last_outcome 保留
     turns.txt 证据值；closed 分支身份四元组为 null（closed 标记优先于
     投影，保守契约）。
  2. lifetime 投影可信 → Pending→`queued`、Running→`running`、
     Failed→`failed`、Idle→`idle`。
  3. lifetime 缺失或不可信（含 legacy）→ execution=`unknown`（turns.txt
     的旧 terminal 不推导 idle/failed——那正是"凭旧 terminal 猜当前"）；
     last_outcome 仍可按 turns.txt 证据输出 completed/errored/interrupted/
     rate_limited（它描述最近已结束轮次，不宣称当前状态）。
  4. 无任何证据 → `unknown`。
- Idle 摘要绑定验证：投影为 Idle 时必须 fd-anchored 读取 result.md、
  计算 SHA256 与 lifetime.result_digest 实际比较，相等才采信 idle；
  空白摘要、错误摘要、result.md 缺失或被篡改 → 该投影不采信
  （execution=unknown）。此验证只比对已有摘要，不解析 result.md 正文
  猜 outcome。反例测试必须含"nonempty 但不匹配的 digest"。
- last_outcome 始终来自 turns.txt 最后一条（terminal 证据），lifetime 不
  产出 outcome；done+failed、done+queued/running 组合必须原样呈现，不得
  误作当前轮成功。
- round 语义：rounds_delivered = count(result-<n>.md)（bare result.md 无
  版本文件时下限 1，沿 #2024 floor）；round = queued/running 时
  delivered+1，idle/failed/unknown 时 delivered。
- 展示规则：`status` 保持 running|done|closed；`execution` ∈
  queued|running|idle|failed|closed|unknown；`last_outcome` ∈
  completed|errored|interrupted|rate_limited|null；人类表新增 CURRENT
  （execution）与 OUTCOME（last_outcome）两列——新 running+旧 completed
  的人类看板不再误导；JSON 全字段；两者共用 list_peers 组装。公开字段
  契约落在 tracked 文件 docs/peer-status-interface.json（CI 可依赖），
  .octos/ 下仅为交接草稿。
- serve `peer_list` 索引文本（compose_peer_list_text）同步：done 行追加
  `outcome=<last_outcome>` 与 `exec=<execution>`；awaiting_input 优先级
  不变；进程内 wire 活性只在 serve 回调叠加、只收紧不放宽。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/commands/peer.rs
- crates/octos-cli/src/commands/mod.rs（仅测试可见性 re-export）
- crates/octos-cli/src/peers/mod.rs
- crates/octos-cli/src/peers/recovery.rs（只读投影 + 真实 writer 回归测试；不改写侧语义）
- crates/octos-cli/src/api/ui_protocol_transport.rs（仅限 raw_peer_gather/compose_peer_list_text 的只读投影扩展；不改 interrupt/steer/审批/terminal 写入行为）
- crates/octos-cli/src/api/ui_protocol_tests.rs（既有 fixture 补字段）
- specs/task-evo-peer-turn-status.spec.md
- docs/superpowers/plans/2026-09-09-peer-turn-status.md
- docs/peer-status-interface.json
- .octos（progress/result/design/independent/cross 交接文档，gitignored）

### Forbidden
- 不新增任何并行状态文件/状态机（turn-state.json 方案已否决）；不写、不改 lifetime.json 的产生语义。
- 不改 verifier / 冻结旧 spec / 主树历史任务。
- 不改 status 字段语义（running|done|closed 保持目录交付语义）。
- 不用 mtime、result.md 正文内容推断成功。
- 未知场景必须 unknown，不给 legacy 凭旧 terminal guessed-idle。
- 不绕过 peer_io 的 fd-anchored 读写。
- 不改权限/模型/凭据配置，不 push/PR/合并主分支。
- 不得删除断言或削弱既有测试来满足门禁。

## Completion Criteria

### Rule: terminal-evidence — 终止证据与 outcome 呈现
Scenario: done+errored 可区分（critical）
  Test:
    Package: octos-cli
    Filter: peer_list_done_peer_with_errored_outcome_shows_failed_execution
  Given 一个 staged peer：result.md、result-1.md 存在且 turns.txt 记录 `1 errored <ts>`，lifetime.json 为 phase=failed（可信投影）
  When 调用 `list_peers`
  Then status=="done" 且 execution=="failed" 且 last_outcome=="errored" 且 rounds_delivered==1

Scenario: round1 completed 后 round2 queued 不显示旧完成
  Test:
    Package: octos-cli
    Filter: peer_list_round2_queued_shows_queued_not_stale_done
  Given turns.txt 记录 `1 completed <ts1>` 且 lifetime.json 为 phase=pending generation=2（可信投影）
  When 调用 `list_peers`
  Then status=="done" 且 execution=="queued" 且 round==2 且 last_outcome=="completed"

Scenario: round2 running 覆盖 round1 完成
  Test:
    Package: octos-cli
    Filter: peer_list_round2_running_overrides_round1_completion
  Given turns.txt 记录 `1 completed <ts1>` 且 lifetime.json 为 phase=running turn_id=t2（可信投影）
  When 调用 `list_peers`
  Then execution=="running" 且 round==2 且 turn_id=="t2"

Scenario: interrupted 轮呈现 failed
  Test:
    Package: octos-cli
    Filter: peer_list_interrupted_turn_reports_failed
  Given turns.txt 记录 `2 interrupted <ts>` 且 lifetime.json 为 phase=failed（可信投影）
  When 调用 `list_peers`
  Then execution=="failed" 且 last_outcome=="interrupted" 且 rounds_delivered==2

Scenario: 无 lifetime 时 execution 恒 unknown（last_outcome 保留证据）
  Test:
    Package: octos-cli
    Filter: peer_list_no_lifetime_execution_unknown_outcome_kept
  Given turns.txt 记录 `1 errored <ts>`，无 lifetime.json
  When 调用 `list_peers`
  Then execution=="unknown" 且 last_outcome=="errored"（不凭旧 terminal 猜 failed/idle）

### Rule: lifetime-projection — lifetime 安全投影
Scenario: 可信投影按 phase 呈现并暴露身份（critical）
  Test:
    Package: octos-cli
    Filter: peer_list_lifetime_projection_phases_and_identity
  Given 同一 fixture 工厂分别构造 phase=running/failed/idle 的可信 lifetime.json（含 task_id/generation/turn_id/result_digest）
  When 调用 `list_peers`
  Then execution 分别为 running/failed/idle，且 task_id/generation/turn_id 字段原样输出；idle 场景 result_digest 非空

Scenario: 不可信投影降级 unknown
  Test:
    Package: octos-cli
    Filter: peer_list_untrusted_lifetime_degrades_to_unknown
  Given lifetime.json 的 registry_key 与 `<profile>:peer:<slug>` 不匹配（或 originator 缺失、version!=1）
  When 调用 `list_peers`
  Then execution=="unknown"（不从旧 terminal guessed-idle）且 last_outcome 仍按 turns.txt 证据输出

Scenario: idle 摘要不匹配降级 unknown
  Test:
    Package: octos-cli
    Filter: peer_list_idle_digest_mismatch_degrades_to_unknown
  Given lifetime.json 为 phase=idle 且 result_digest 非空，但 result.md 实际 SHA256 与之不匹配（或 result.md 缺失）
  When 调用 `list_peers`
  Then execution=="unknown"（摘要绑定失败不宣称 idle）

Scenario: lifetime 损坏 JSON 降级
  Test:
    Package: octos-cli
    Filter: peer_list_corrupt_lifetime_degrades_to_evidence
  Given lifetime.json 为截断 JSON，turns.txt 记录 `1 errored <ts>`
  When 调用 `list_peers`
  Then execution=="unknown"（无可信 authority）且 last_outcome=="errored"（证据保留），不 panic

### Rule: legacy-unknown — 无元数据不猜测
Scenario: legacy peer 无 turns.txt 无 lifetime（critical）
  Test:
    Package: octos-cli
    Filter: peer_list_legacy_no_metadata_reports_unknown
  Given 一个 staged peer 仅有 brief.md 与 result.md（无 turns.txt、无 lifetime.json）
  When 调用 `list_peers`
  Then status=="done" 且 execution=="unknown" 且 last_outcome==null 且 rounds_delivered==1

### Rule: robust-reads — 符号链接
Scenario: turns.txt 为符号链接被拒读
  Test:
    Package: octos-cli
    Filter: peer_list_symlinked_turns_index_is_ignored
  Given turns.txt 是指向外部的符号链接且无 lifetime.json
  When 调用 `list_peers`
  Then 读取端忽略该文件，execution=="unknown"，不跟随链接

### Rule: closed-terminal — closed 覆盖一切
Scenario: closed peer 保持 closed 优先
  Test:
    Package: octos-cli
    Filter: peer_list_closed_peer_reports_closed_execution_with_outcome
  Given 一个 closed peer（closed 标记 + result.md + turns.txt `1 errored`）
  When 调用 `list_peers`
  Then status=="closed" 且 execution=="closed" 且 last_outcome=="errored"

### Rule: derivation-precedence — execution 派生优先级
Scenario: 派生优先级 closed > 可信 lifetime > unknown
  Test:
    Package: octos-cli
    Filter: peer_list_execution_derivation_precedence
  Given 同一 data_dir 下四个 peer：closed+errored 证据、可信 lifetime running+旧 completed 证据、无 lifetime（turns 有 errored 证据）、两者皆无
  When 调用 `list_peers`
  Then 四行 execution 分别为 closed、running、unknown、unknown（无 authority 不推导）

### Rule: interface-fields — 消费端字段契约
Scenario: docs/peer-status-interface.json 契约与实现字段一致
  Test:
    Package: octos-cli
    Filter: peer_list_fields_match_status_interface_contract
  Given docs/peer-status-interface.json 声明的字段集（tracked 文件）
  When 序列化 PeerListRow
  Then 输出 JSON 的键集与契约一致（slug/status/execution/last_outcome/round/rounds_delivered/has_brief/result_versions/name/model_lane/master_session_id/task_id/generation/turn_id）

### Rule: shared-assembly — JSON 与人类表共用组装
Scenario: json 与 table 字段一致
  Test:
    Package: octos-cli
    Filter: peer_list_json_and_table_share_assembly
  Given 上述 done+errored fixture
  When 分别以 --json 与表格渲染同一 rows
  Then JSON 含全部新字段，表格行含 CURRENT 与 OUTCOME 两列呈现（errored 原样显示；表映射 unknown→?、null→- 被断言）

### Rule: real-writer-shapes — 真实 writer 形状回归（外层 probe 移植）
Scenario: invalidate 保留旧 turn_id 的 Pending 仍可信（critical）
  Test:
    Package: octos-cli
    Filter: outer_peer_projection_accepts_real_followup_pending
  Given 生产 Fixture/begin/complete 后调用真实 invalidate_peer_lifetime_for_input
  Then 磁盘呈 Pending+Some(旧turn_id) 且投影仍 Some（queued 权威保留，不降 unknown）

Scenario: finish(queued) 产 Pending+Some 仍可信
  Test:
    Package: octos-cli
    Filter: outer_peer_projection_accepts_finish_with_queued_input
  Given 生产 finish_peer_lifetime_turn 以 has_queued_input=true 结束
  Then 磁盘呈 Pending+Some 且投影仍 Some

### Rule: entry-compat — 真实 CLI 入口兼容（外层复查 #2）
Scenario: 仅 numbered result 的 legacy peer 保持 done
  Test:
    Package: octos-cli
    Filter: peer_list_numbered_only_peer_stays_done
  Given 一个 staged peer 仅有 result-1.md（无 bare result.md）
  Then status=="done"（numbered 或 bare 任一存在即 done，旧语义不变）

Scenario: 超 cap 的 bare result 仍证明交付
  Test:
    Package: octos-cli
    Filter: peer_list_oversized_bare_result_still_done
  Given bare result.md 超过读 cap（内容读失败）
  Then status=="done" 且 rounds_delivered==1

Scenario: 显式 name 等于 slug 仍保留 Some(name)
  Test:
    Package: octos-cli
    Filter: peer_list_explicit_name_equal_to_slug_is_preserved
  Given name 文件内容恰等于 slug
  Then name==Some(slug 内容)（不得无故变 None）

Scenario: 外来 slug frontmatter 不构成本 peer 证据
  Test:
    Package: octos-cli
    Filter: peer_list_foreign_slug_frontmatter_is_not_outcome_evidence
  Given result-1.md frontmatter slug 指向其他 peer 且 turns.txt 同轮 completed
  Then last_outcome==null（交叉校验失败不主张）

Scenario: 扫描 cap 触顶显式截断（scanner 级）
  Test:
    Package: octos-cli
    Filter: peer_list_scan_cap_truncation_yields_no_outcome
  Given 小 cap + 混合条目（result 文件与非 result 文件）耗尽扫描预算
  Then 扫描器返回显式截断错误（不返回部分列表）；cap 足够时返回完整命中
  And 预算计入全部扫描条目而非仅 result 命中（混合文件也耗尽预算）

Scenario: 空 identity 字符串拒绝（Pending+Some("")、空 master）
  Test:
    Package: octos-cli
    Filter: peer_projection_rejects_empty_identity_strings
  Given lifetime.json 为 Pending+turn_id="" 或 master 为空白串
  Then 投影为 None（不为其背书）

Scenario: serve gather/list 入口按调用方 profile 投影（critical）
  Test:
    Package: octos-cli
    Filter: peer_gather_entries_thread_caller_profile_for_lifetime
  Given 非默认 profile（gatherx）的 staged peer 具有 registry_key 绑定
    gatherx 的可信 running lifetime 及 round1 completed 终止证据
  When 以 profile_id=gatherx 调 raw peer/gather RPC 与 peer_gather 工具回调
  Then raw JSON execution=="running" 且
    master_session_id/task_id/generation/turn_id/last_outcome 原样透出
    （修复前默认 octos 读取会使同一盘面降级 unknown）；peer_gather 工具
    回调与 peer_list 工具文本（exec=running）经同一 profile-aware 读取组文

## Out of Scope
- lifetime.json / goal ledger 写侧语义变更（只读投影）。
- peer_gather 大改动（仅 raw_peer_gather JSON 透出新字段）。
- 仪表盘/TUI 渲染改造。
