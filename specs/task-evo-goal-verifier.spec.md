spec: task
name: "完成校验错误分类、有界恢复及证据账本 (evo-goal-verifier)"
tags: [goal, verifier, autonomy, octos-cli, error-classification]
---

- **task**: evo-goal-verifier
- **status**: draft
- **worktree**: /Users/zhangalex/.local/tmp/octoloop-evolution-20260909/octos-goal-verifier @ `fix/evo-goal-verifier`
（allowed files 见 ## Boundaries / ### Allowed Changes）

## Intent

Native goal completion verifier 的 `NotDone { reason }` 把基础设施故障（调用失败、空答复、无效格式）与语义判断（证据不足）混为一谈，导致：调用失败被当作"目标未完成"驱动无意义的继续循环；无法区分"该重试"与"该报告缺失证据"；宽松解析可能把脏答复（如 `DONE: but ...`、DONE 后另起 NOT_DONE）当作 Done 采认。本任务给 verifier 引入结构化错误分类、有界恢复（transient 最多重试 1 次，总计最多 2 次调用，且受预算与同证据去重约束）与持久证据账本，并保证每次调用（含失败重试）的 token usage 都计入 goal 账目。

## Decisions（v3 — 合并 GLM/k3 设计审查，双方 APPROVE-WITH-CHANGES 收编）

1. **结构化分类（分层 seam，k3 §7.1 + GLM §3 一致采纳）**：新增 `GoalVerifierFailureKind { CallFailed, EmptyResponse, InvalidResponse, InsufficientEvidence }`（goal_loop_runtime.rs）。**单次调用+解析分类保持自由函数** `run_goal_completion_verifier_with_usage`（解析拆为同步纯函数 `classify_verifier_reply(content, reasoning_present) -> (GoalCompletionVerdict, Option<GoalVerifierFailureKind>, missing_evidence)`）；**新增 orchestrator 方法 `verify_goal_completion_bounded(session, profile, snapshot, provider, evidence) -> GoalVerifierOutcome`** 封装 digest gate → attempt → per-attempt charge → 预算状态检查 → 有界重试 → 落账。`GoalVerifierOutcome { verdict, kind: Option<GoalVerifierFailureKind>, attempts, missing_evidence, replayed: bool, usage }`（`kind=None ⟺ Done`，GLM 修正 A），提供 `is_done()` 与 `Display`（k3 建议）。`GoalCompletionVerdict` 保持 Done/NotDone 不变（`maybe_complete_goal_from_model` 全家零改动）。
2. **严格 DONE 解析（k3 §2 最终规则 + GLM §2 边界，合并采纳）**：现状 first-token 解析已拒绝 `DONEgarbage`；真实活漏洞是 `DONE: but ...` 与 `DONE\nNOT_DONE: ...`（first token 均为 DONE → 误判 Done）。最终可判定规则（纯函数实现）：(a) `content` 为 None/trim 后空 → `EmptyResponse`（纯 reasoning 同此态）；(b) 剥至多一层 markdown fence（``` 开头且 ``` 结尾，仅成对时）→ (c) 剥成对反引号（首尾均为 ` 且长度≥2，可重复；**trim_matches 不保证成对——未配对 `DONE 或 DONE` 一律不剥 → InvalidResponse**）→ (d) 取非空白行集合 L；(e) `|L|==1` 且 `L[0]` 忽略大小写 == `DONE` → Done（`DONE\n` 尾随空行也 Done）；(f) 否则 `L[0]` 剥成对反引号后忽略大小写以 `NOT_DONE` 开头 → `NotDone{InsufficientEvidence}`，missing_evidence=冒号后余文（前 200 字符）；(g) 其余（多行残余、DONE 带正文、`Done.`、散文式否定如 "The build is still failing"）→ `InvalidResponse`，reason 内嵌原始答复截断（前 120 字符）供模型自我纠正（GLM 边界 2）。大小写不敏感**保留现状**（`done`/`Done` 均判 Done，回归面）。
3. **有界重试（外层 + 双 peer 收敛）**：仅 `CallFailed`（且 provider 错误 `is_retryable()`，k3 建议）与 `EmptyResponse` 重试，单次验证内最多 1 次额外重试（总计 ≤2 次 `provider.chat`；每次 chat 内部含 lane RetryProvider 最多 4 次 wire 尝试，表述如实）。`InvalidResponse`/`InsufficientEvidence` 不自动重试。**第二次 chat 前置条件：goal.status == "active"**（状态式判据；`charge_goal_tokens_gated` 返回 `Option<Value>` 有五种 None 分支，不可作判据——k3 §7.2）。跨调用去重由持久 gate 承载（Decision 5）：**语义类（Done/InsufficientEvidence/InvalidResponse）同 goal_id+同 digest 永久阻断重放**（幂等）；**infra 类（CallFailed/EmptyResponse）不永久阻断**——同 digest 重放带 `replayed=true` 与 `replayed_of_ts` 标记并加 10 分钟冷却（冷却期内重放旧判词，冷却期满放行新调用），防 provider 恢复后 goal 被陈年 transient 记录楔死（k3 待验证项 + GLM 漏洞 5/6 的并集裁决）。
4. **失败也计 usage + charge 时点迁移（双 peer 必改项 + 外层细化）**：每次尝试（含失败尝试）的 `TokenUsage` 逐字段（input/output/reasoning/cache_read/cache_write）**saturating_add** 累加（`semantic_checkpoint` 不加，后写者胜并注释）；**每次 attempt 返回立即 charge 该次 usage，然后再做预算判断**（外层裁决：per-attempt 即时收账，attempt 2 的发起条件是 charge 后 goal 仍 active）；**最终合计 usage 仅作报告/落账，不得再收第二遍**。`charge_goal_verifier_usage` 函数签名与 allow_budget_limited=true 语义不动，仅调用位置收敛进 wrapper，4 个调用点的直接 charge 调用与 "Callers must charge BEFORE…" doc comment 删除。gate 重放（replayed=true）不发新调用、不 charge、返回 usage=0/attempts=0 并显示历史来源。provider `Err` 时 octos_llm 错误类型不携带 usage（接口强制），如实按 0 记。usage 不对称注明：charge 只收 input+output（reasoning/cache 只入账本镜像，权威计数在 goals row tokens_used）——与 turn 计费一致，保持。
5. **持久证据账本（v5 — VG-SCOPE 修订：完整真实 scope 绑定路径与记录）**：`<data_dir>/goal-verifier-ledgers/<scope指纹>.jsonl`（追加式、create_dir_all、一行一记录；独立兄弟目录，不进 goal-ledgers/ 防目录扫描器）。**scope 指纹 = sha256("octos-goal-verifier-scope-v1" + 长度前缀(data_dir 存储身份, cwd-scoped session, profile, goal_id))**——域标签 + 长度前缀序列化，无 sanitize 碰撞/分隔符注入/路径别名错认；data_dir 存储身份取最深存在祖先 canonicalize + 缺失尾段（解析 /var↔/private/var 等符号链接别名，且 preflight 创建目录前后稳定）。记录 schema **v3** 新增必填 `scope` 字段（同指纹）；读侧逐行校验 version==3、record.scope==本 scope 指纹、record.goal_id==本 goal_id，任一不符 fail-closed（旧 v2/缺 scope/未知版本一律 fail-closed，不接受外来 Done）。**同 scope 同 evidence 可跨重启重放；不同 session/profile 同 goal 编号同 evidence 不得重放**（不同文件、不同指纹）。字段 `outcome` **五值**（done/call_failed/empty_response/invalid_response/insufficient_evidence）+ scope、goal_id、ts_ms、attempts、usage（audit mirror，权威是 goals row tokens_used；**cache 重放返回 usage=0/attempts=0** 并在回显标注历史来源 ts，不是沿用历史计数）、missing_evidence、error（call_failed 时截断 provider 错误）、evidence_digest、replayed（重放不追加行、不 charge）。digest = `sha256("octos-goal-verifier-evidence-v1\0" + objective + "\0" + evidence + "\0" + revision)`（域分离标签 + \0 边界分隔；用有边界的序列化而非裸拼接；**诊断/重放事件不得写进 completion_evidence，防止证据流自激变 digest**）。goal resume/reopen 保留历史且同 goal_id；账本判词只用于去重省调用，不得绕过 maybe_complete_goal_from_model 的 snapshot 复查直翻状态。无 data_dir 内存路径用同一完整 scope 指纹做 cache 键 + 同样 10 分钟 infra TTL。
   **VG-PREFLIGHT（外层 2RED 裁决）**：有 data_dir 时，wrapper 在任何 `provider.chat` 之前（同 scope single-flight 锁内）先做持久化可用性预检：create_dir_all(data_dir)+is_dir 校验+账本目录创建+以 create+append 打开账本文件并 sync——即 append 将要做的真实操作。不可用 storage（如只读目录/只读账本文件）**两次调用均 0 chat / 0 attempts / 0 charge**，fail-closed 诊断且 goal 保持未完成。真实调用后 append/flush/sync 仍失败 → 组合结局 NotDone + 保留真实 usage，并把该 (scope,digest) infra 失败写入进程内 overlay（10 分钟冷却内重放、0 chat），防止对同坏 storage 无限反复花费；冷却期满放行一次真实重试（恢复的 storage 不被楔死）。
   **IO 语义 fail-closed（外层裁决，覆盖 v3 fail-open）**：有 data_dir 但账本**读失败**（IO 错误、不完整尾行、未知版本字段）→ 保守处理：不回退很旧的 Done、不发新 chat，返回显式诊断类失败（goal 保持未完成）；账本**写失败** → 显式诊断并保持 goal 未完成，不得把已验证 Done 缓存为成功返回。无 data_dir 的 legacy ephemeral 会话 → 明确用进程内 single-flight + 内存 cache，回显标注"无法跨重启恢复"，不得假称已持久化。同 digest 短期冷却（10 分钟）保留为明确瞬态恢复策略（冷却期内重放判词）。
   **并发 single-flight（外层裁决）**：去重不只是 append 时 Mutex——需要 per-goal in-flight reservation：并发两个 gate miss 不得双调 provider；single-flight guard 不得持有整个 orchestrator state 锁跨 await（用独立 per-goal 锁/reservation map）。**恢复语义**：resume/reopen 后证据补齐（digest 变化）应允许重新验证；严禁引导模型仅改写 reason 文本绕过同证据限制。
   **gate 判定次序（k3 Round 3 建议 A，实现注释钉死）**：读账本（失败→fail-closed 诊断）→ 查记录（无→放行）→ 语义类→永久重放 / infra 类→TTL 冷却比较——读失败 fail-closed 优先级高于冷却判定。坏尾行 fail-closed 是**有意的一次性人工介入点**（不自愈；操作路径=修复或删除该 jsonl），写入 doc comment 防 hang 误报。
   **复合结局表达（k3 Round 3 建议 B）**：`GoalVerifierOutcome` 增加诊断能力（`diagnostic: Option<String>` 或诊断类 kind 变体），使"Done 判定已得但账本写失败"可表达——此时返回诊断失败、goal 保持未完成，但该次 attempt 的 usage 已真实 charge（正确代价，如实呈现）。
6. **全部调用点迁移（含 k3 §6 实勘补充）**：goal_tool.rs（goal_update）、session_actor.rs（sentinel）、ui_protocol_transport.rs×2（sentinel）共四处真实调用点**全部改调 `verify_goal_completion_bounded` wrapper**（只换一行，gate/charge/落账不再各抄一份）；另 goal_tool.rs 两个 `#[ignore]` live-capture 测试（2016/2074）因返回类型变更**编译必改**（`--all-targets` 会编译它们）。NotDone 输出由 `GoalVerifierOutcome::Display` 统一（含 kind+attempt+截断原文），不再输出裸 `verifier returned: `。不改变既有 snapshot guard（TOCTOU 防护）语义。
7. **max_tokens=2048 保留**：非已证实根因，不动（测试 `goal_completion_verifier_uses_reasoning_headroom_max_tokens` 继续通过）。
8. **语义错误不触发盲重试**：verifier 返回 NotDone(InsufficientEvidence) 时 goal 留 Active 并向模型回显 missing evidence（含 NOT_DONE 余文），而不是让 orchestrator 立即再跑一轮 verifier。
9. **测试落位（k3 §6）**：gate 类场景（dedupe/release/scoped）测 wrapper 层；唯 budget_exhaustion 场景走 goal_update 跨层集成；`run_interactive_sentinel_completion` 现无单测，首测先确认可独立构造（参数已注入，可行）。

## Boundaries

### Allowed Changes

crates/octos-cli/src/autonomy/goal_loop_runtime.rs
crates/octos-cli/src/autonomy/agent_orchestrator.rs
crates/octos-cli/src/goal_tool.rs
crates/octos-cli/src/session_actor.rs
crates/octos-cli/src/api/ui_protocol_transport.rs
specs/task-evo-goal-verifier.spec.md
docs/superpowers/plans/2026-09-09-evo-goal-verifier.md

### Forbidden

- 不改 max_tokens=2048、profile/sub-provider 解析、charge 的 allow_budget_limited 语义、TOCTOU snapshot guard。
- 不删既有测试断言换绿；不改权限/模型/凭据；不写主树或其他任务目录；不 push/PR。

## Completion Criteria

### Rule: verifier-call-failure-classification — 基础设施失败与语义判断分离

Scenario: 调用失败一次后重试成功（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_retries_transient_and_skips_auth_error
  Given mock provider 第一次返回 Err、第二次返回 `DONE`
  When 调用 verifier
  Then 判定 Done，attempts==2，usage 为两次尝试之和

Scenario: 调用失败两次后如实失败并封顶
  Test:
    Package: octos-cli
    Filter: goal_verifier_caps_attempts_at_two_on_persistent_call_failure
  Given mock provider 两次都 Err
  When 调用 verifier
  Then NotDone + kind=CallFailed + attempts==2，provider.chat 恰好被调 2 次

Scenario: 首尝试耗尽预算阻止第二次调用
  Test:
    Package: octos-cli
    Filter: goal_verifier_budget_exhaustion_prevents_second_call
  Level: unit
  Test Double: scripted mock LlmProvider + 真实 orchestrator goal 记账
  Given goal 预算仅剩一次 verifier 调用的 token，同一次 goal_update 验证内第一次尝试的 per-attempt charge 已把 goal 翻到 budget_limited
  When wrapper 在发起第二次 chat 前检查 goal 状态
  Then goal.status ≠ active → 不发起第二次 provider.chat（调用计数==1），直接以第一次分类收尾（状态式判据，charge 返回值不作判据；goal.status 为 active/budget_limited 等枚举值）

Scenario: 验证前已 budget_limited 的边角
  Test:
    Package: octos-cli
    Filter: goal_verifier_skips_retry_when_goal_not_active_before_first_attempt
  Given goal 在第一次尝试前就已是 budget_limited
  When wrapper 发起验证
  Then 第一次 chat 后即不再重试（attempts==1）

Scenario: 语义 NotDone 不重试
  Test:
    Package: octos-cli
    Filter: classify_not_done_prefix_is_recognized
  Given mock 固定返回 `NOT_DONE: missing X`，其中 X 是 Given 里明示的缺口项名（本场景即字符串 "missing X"）
  When 调用 verifier
  Then kind=InsufficientEvidence + attempts==1，missing evidence 记录含 "missing X"

Scenario: 格式错误不自动重试
  Test:
    Package: octos-cli
    Filter: goal_verifier_does_not_retry_invalid_response
  Given mock 返回 `DONE: but actually not finished`（first-token DONE 带正文）
  When 调用 verifier
  Then kind=InvalidResponse + attempts==1，provider.chat 恰好被调 1 次

### Rule: verifier-strict-done-parse — DONE 必须是干净的最终判定

Scenario: 拒绝 DONEgarbage（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: classify_rejects_done_with_trailing_text
  Given mock 返回 `DONEgarbage`
  When 调用 verifier
  Then NotDone + kind=InvalidResponse（且不因重试而误判 Done）

Scenario: 接受单层 fence 包裹的 DONE
  Test:
    Package: octos-cli
    Filter: classify_accepts_single_paired_fence
  Given mock 返回以 ``` 开头并以 ``` 结尾、内部恰为 DONE 的多行答复
  When 调用 verifier
  Then 判定 Done（至多剥一层成对 fence）

Scenario: 不成对反引号不剥
  Test:
    Package: octos-cli
    Filter: classify_rejects_unpaired_backticks
  Given mock 返回 `` `DONE ``（仅前导反引号，不成对）
  When 调用 verifier
  Then NotDone + InvalidResponse

Scenario: 拒绝 Done 句号变体
  Test:
    Package: octos-cli
    Filter: classify_rejects_done_with_trailing_text
  Given mock 返回 `Done.`
  When 调用 verifier
  Then NotDone + InvalidResponse

Scenario: 散文式否定归 InvalidResponse
  Test:
    Package: octos-cli
    Filter: classify_prose_negation_is_invalid_response_with_quote
  Given mock 返回 "The build is still failing, tests not run"（无 NOT_DONE 前缀）
  When 调用 verifier
  Then kind=InvalidResponse 且 reason 内嵌原始答复截断

Scenario: 拒绝 DONE 前缀带正文
  Test:
    Package: octos-cli
    Filter: classify_rejects_done_with_trailing_text
  Given mock 返回 `DONE: but the build log was never checked`
  When 调用 verifier
  Then NotDone + kind=InvalidResponse

Scenario: 拒绝 DONE 后另起 NOT_DONE 的多行答复
  Test:
    Package: octos-cli
    Filter: classify_rejects_done_with_trailing_text
  Given mock 返回 "DONE\nNOT_DONE: tests failing"
  When 调用 verifier
  Then NotDone + kind=InvalidResponse

Scenario: 接受成对反引号 DONE（回归）
  Test:
    Package: octos-cli
    Filter: run_goal_completion_verifier_accepts_backticks
  Given mock 返回 `` `DONE` ``
  When 调用 verifier
  Then 判定 Done

Scenario: 纯 reasoning 不算 DONE
  Test:
    Package: octos-cli
    Filter: classify_empty_and_reasoning_only_is_empty_response
  Given mock 返回 content=Some("") 且 reasoning_content 非空
  When 调用 verifier
  Then NotDone + kind=EmptyResponse

### Rule: verifier-usage-accounting — 失败也计费

Scenario: 重试路径 usage 累计（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_sums_billed_second_empty_attempt
  Level: unit
  Test Double: scripted mock LlmProvider
  Given 第一次 Ok(content=Some("")) usage 10/2 且第二次 Ok(usage 5/3)（两次均非零，覆盖 input/output 及非零 cache 字段的逐字段累加）
  When 调用 verifier
  Then 返回 usage==15/5（逐字段相加，cache 字段同样累加）

Scenario: 空答复但已计费
  Test:
    Package: octos-cli
    Filter: goal_verifier_sums_billed_second_empty_attempt
  Given Ok(content=Some("")) usage 10/2
  When 调用 verifier
  Then kind=EmptyResponse 且 usage 照实为 10/2

Scenario: 第二次空答复也计入合计
  Test:
    Package: octos-cli
    Filter: goal_verifier_sums_billed_second_empty_attempt
  Given 第一次 Ok("") usage 10/2 + 第二次 Ok("") usage 8/1（两次都空、都被计费）
  When 调用 verifier
  Then kind=EmptyResponse + attempts==2 且 usage==18/3

### Rule: verifier-evidence-ledger — 持久分类账本

Scenario: 账本记录分类与次数
  Test:
    Package: octos-cli
    Filter: goal_verifier_ledger_records_failure_kind_and_attempts
  Given 两次 transient 失败
  When verifier 结局落账
  Then 记录含 outcome=call_failed、attempts=2、goal_id、ts_ms（outcome 五值含 done；Done 结局也落账）

Scenario: resume 保留历史同 id（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_ledger_preserves_history_across_resume
  Given goal 有历史 verifier 记录
  When pause→resume（或 reopen）后再次 verifier 落账
  Then 同一 goal_id 下新旧记录并存，无清账

Scenario: 同证据去重 gate 挡下重复 recheck（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_dedupes_same_evidence_recheck
  Given goal g1 已有 outcome=insufficient_evidence、evidence_digest=D 的账本记录（语义类）
  When 再次以同 goal_id g1 + 同证据（digest D）请求验证
  Then 不发起新的 provider.chat，返回 replayed=true 判词（replay 不追加账本行、不 charge、Done replay 仍走完整 snapshot 复查）

Scenario: infra 类判词冷却期后放行
  Test:
    Package: octos-cli
    Filter: goal_verifier_memory_cache_infra_cooldown_releases_recall
  Given goal g1 有 outcome=call_failed、digest=D 的记录，且记录时间距本次请求已超过冷却 TTL（10 分钟）
  When 同 digest 再次请求验证
  Then 发起新的 provider.chat（provider 恢复后不被陈年 transient 记录楔死）

Scenario: infra 类判词冷却期内重放
  Test:
    Package: octos-cli
    Filter: goal_verifier_infra_cooldown_replay_and_release
  Given goal g1 有 outcome=call_failed、digest=D 的记录，距本次请求在冷却 TTL（10 分钟）内
  When 同 digest 再次请求验证
  Then 不发起新调用，返回 replayed=true + replayed_of_ts=原记录时间

Scenario: 账本读失败 fail-closed（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_ledger_read_failure_fails_closed
  Level: unit
  Test Double: 损坏的 JSONL 文件（截断尾行/未知版本字段）
  Given goal g1 有 data_dir，其 verifier 账本文件含不完整尾行或未知版本记录
  When 请求验证（gate 读账本失败）
  Then 不发起新 provider.chat、不返回 Done，返回显式诊断失败且 goal 保持未完成（不回退很旧的 Done）

Scenario: 账本写失败 fail-closed（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_ledger_write_failure_fails_closed
  Level: unit
  Test Double: 只读账本文件（读可解析、append-open EACCES）
  Given goal g1 有 data_dir，其 verifier 账本文件只读（门读侧 Miss 但预检 append-open 失败）
  When 连续两次请求验证
  Then 两次均 0 chat / 0 attempts / 0 charge（预检在任何 provider.chat 前发现不可用 storage），不缓存 Done 为成功返回，返回显式诊断失败且 goal 保持未完成

Scenario: 跨 session 不得继承持久 Done（critical，VG-SCOPE 外层探针移植）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: outer_verifier_runtime_durable_done_cannot_cross_session_after_restart
  Level: unit
  Test Double: scripted mock LlmProvider（DONE）
  Given 两个自然 fresh orchestrator 的 session A/B 同 profile 同 objective/revision 都从 goal_01 起，共用同一 provided ledger 目录，session A 已验证 Done 并落账
  When session B 以相同 evidence 请求验证
  Then B 不 replay A 的持久 Done（replayed=false），B 发起自己的 provider.chat（完整 scope 绑定：session/profile/goal_id/data_dir 身份）

Scenario: 不可写新账本在任何调用前零花费（critical，VG-PREFLIGHT 外层探针移植）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: outer_verifier_runtime_unwritable_new_ledger_spends_zero_calls
  Level: unit
  Test Double: 存在可读但 0555 不可写的 provided 目录（尚无账本文件）
  Given goal g1 的 provided data_dir 存在可读但不可写、账本不存在
  When 连续两次请求验证
  Then 两次均 0 chat / 0 attempts / 0 charge，NotDone + 显式诊断，goal 保持未完成

Scenario: 并发 gate miss 单飞（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: goal_verifier_single_flight_on_concurrent_gate_miss
  Level: unit
  Test Double: 慢速 mock LlmProvider + 并发两请求
  Given 同 goal g1 同证据的两个并发验证请求同时到达（gate 均未命中）
  When 两个请求并发执行
  Then 恰好一次 provider.chat 调用（per-goal in-flight reservation 单飞，非 append-Mutex），第二个请求等待/复用结果，全程不持 orchestrator state 锁跨 await

Scenario: 无 data_dir 的内存降级
  Test:
    Package: octos-cli
    Filter: goal_verifier_no_data_dir_uses_memory_gate
  Given legacy ephemeral 会话无 data_dir
  When 请求验证
  Then 用进程内 single-flight + 内存 cache（标注无法跨重启恢复，不假称已持久化），验证照常进行

Scenario: 诊断不污染证据流
  Test:
    Package: octos-cli
    Filter: goal_verifier_diagnostics_do_not_mutate_evidence_digest
  Given 一次验证产生 replay/诊断事件后进入下一轮验证
  When 下一轮以相同 objective 与 evidence 计算指纹
  Then digest 与上一轮相同证据一致（诊断/重放事件不写入 completion_evidence）

Scenario: 证据变化释放新验证轮
  Test:
    Package: octos-cli
    Filter: goal_verifier_changed_evidence_releases_recheck
  Given goal g1 已有 digest=D 的失败记录
  When 证据更新（digest D' ≠ D）后再次验证
  Then 发起新的 provider.chat（新 attempt 计入），历史记录保留

Scenario: 新 goal 不继承旧判词
  Test:
    Package: octos-cli
    Filter: goal_verifier_gate_scoped_to_session_and_charges_per_attempt
  Given 同 orchestrator 下 session A/B 同 profile 同 objective 的 goal，session A 以证据 D 验证 Done 并落账
  When session B 以相同证据 D（同 digest）请求验证
  Then B 不 replay A 的判词（replayed=false），正常发起新调用并独立 charge（不同 session = 不同 scope，同证据也不共享）

### Rule: verifier-call-site-migration — 全调用点结构化

Scenario: goal_update 失败回显分类
  Test:
    Package: octos-cli
    Filter: goal_update_reports_call_failed_verifier_failure
  Given verifier 两次 CallFailed
  When 模型调 goal_update status=complete
  Then 工具输出含 call failed 与 attempt，success=false，goal 保持 Active

Scenario: sentinel 路径不误判
  Test:
    Package: octos-cli
    Filter: session_actor_sentinel_reports_verifier_failure_kind
  Level: unit
  Test Double: scripted mock LlmProvider（EmptyResponse）
  Given sentinel 路径 verifier 得到 EmptyResponse
  When 完成哨兵被处理后检查 goal 状态
  Then goal 不置 Completed，NotDone reason 含分类

Scenario: session_actor sentinel 调用点绑定
  Test:
    Package: octos-cli
    Filter: session_actor_sentinel_reports_verifier_failure_kind
  Level: unit
  Test Double: scripted mock LlmProvider（EmptyResponse）
  Given scripted EmptyResponse provider 驱动 session_actor 完成哨兵路径
  When 哨兵处理完成
  Then goal 保持 Active 且 NotDone reason 含 empty_response 分类

Scenario: ui_protocol_transport 两处 sentinel 调用点绑定
  Test:
    Package: octos-cli
    Filter: ui_transport_sentinels_report_verifier_failure_kind
  Level: unit
  Test Double: scripted mock LlmProvider（CallFailed）
  Given scripted CallFailed provider 驱动 ui_protocol_transport 的两个 sentinel 验证路径
  When 验证完成
  Then 两路径均不置 Completed，NotDone reason 含 call_failed 分类与 attempt 次数

### Rule: pr-2273-storage-identity-stability — 相对 data_dir 首建前后 scope 身份稳定

Scenario: 不存在相对路径经 cwd 锚定后身份跨创建稳定
  Test:
    Package: octos-cli
    Filter: goal_verifier_storage_identity_stable_across_relative_dir_creation
  Given 真实 cwd 下不存在相对 data_dir（唯一 basename guard，不改进程 cwd；拼写矩阵含 plain、./、new/../other、a/b/../../../shallow、missing/../existing-link/fresh 及其后再 ../，unix-only symlink 探针以 #[cfg(unix)] 隔离）
  When 首次调用触发 preflight 创建目录
  Then 每种拼写创建前后 verifier_storage_identity 相同且等于其 plain 等价形；.. 回到现存目录后 symlink 经 OS 解析（.. 爬已解析 target 父级而非 symlink 父级）；同 evidence 第二次调用 durable replay 且 provider 只调 1 次

Scenario: CallFailed reason 有界 Unicode 安全
  Test:
    Package: octos-cli
    Filter: goal_verifier_call_failed_reason_is_bounded_unicode_safe
  Given provider 返回 600 个多字节 emoji 错误
  When verifier 调用失败
  Then NotDone.reason 与 call_error 同一有界字符串（chars 截断，无乱码/panic）

Scenario: InvalidResponse reason 持久化并跨重启重放
  Test:
    Package: octos-cli
    Filter: goal_verifier_invalid_response_reason_persists_and_replays
  Given InvalidResponse 判词落账（新 reason 列，非 missing_evidence）
  When 新 orchestrator（重启语义，同 goal id）读同 evidence
  Then replay 携带有界原文引述，provider 0 次调用

Scenario: 旧 v3 无 reason 行兼容回退
  Test:
    Package: octos-cli
    Filter: goal_verifier_old_v3_row_without_reason_still_replays
  Given 预 reason 列时代的 v3 invalid_response 行（无 reason/missing_evidence/error）
  When 重启后读取
  Then 反序列化成功并按 legacy 裸 outcome 回退重放

## Out of Scope

- 不改 max_tokens=2048；不重构 profile/sub-provider 解析；不动 goal 预算语义（allow_budget_limited=true 保留）；不写主树/其他任务目录；不改权限/模型/凭据配置。
