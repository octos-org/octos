# 2026-09-09 evo-goal-verifier — 完成校验错误分类、有界恢复及证据账本

> **历史文档（historical）**：本文记录 2026-09-09 实现轮的设计裁决与当时的 v3/29 场景
> 快照，其中步骤 6/8 曾按当时预期书写（"fail-open" 等描述与最终实现不符——账本读侧
> 实际为 fail-closed，见 spec Decisions）。**当前权威**：spec 现行文本（44 scenarios，
> 含 merged-review 2026-09-10 修复轮 4 个新绑定）与
> `docs/superpowers/plans/2026-09-10-merged-verifier-review.md`（本轮实际结果：
> 4/4 定点 + clippy/fmt EXIT0、双模型 final+互读 PASS）。本 plan 不再更新，仅作裁决
> 史存档。

- Spec: `specs/task-evo-goal-verifier.spec.md`（**历史快照时点 v3/29 scenarios**；现行 44——见上方权威指向；agent-spec lint 100%；v2 吸收外层复核 8 条，v3 吸收 GLM/k3 设计审查——双 APPROVE-WITH-CHANGES 收编）
- Native goal: `goal_01` (profile `octosfix`)
- 分支: `fix/evo-goal-verifier`（已合并 #2273）；构建/测试经共享 target 定点运行（-p octos-cli --features api），全量 all-targets 由外层集成树统一执行。

## v3 关键裁决（两 peer 分歧点）

- seam：自由函数（单次调用+纯函数解析 classify_verifier_reply）+ orchestrator wrapper `verify_goal_completion_bounded`（gate→attempt→per-attempt charge→status 式预算检查→重试→落账）。两 peer 一致。
- 解析规则采 k3 §2 最终版（fence 成对剥一层、反引号成对剥、|L|==1 且 L[0]==DONE（大小写不敏感保留现状）、NOT_DONE 前缀→InsufficientEvidence、其余含散文→InvalidResponse+原文截断回显）。
- 预算 gate 状态式（goal.status=="active" 才 attempt 2；charge 返回 Option<Value> 有 5 种 None 分支不可作判据）。
- 账本 outcome 五值（含 done）修正 v2 kind 硬伤；replay 不追加行不 charge；语义类永久阻断、infra 类 10min 冷却（防楔死）；digest=sha256 域分离+revision；fail-open。
- charge 迁 per-attempt 进 wrapper（charge_goal_verifier_usage 签名不动）。

## 步骤

1. [x] 合约：spec v3 + lint（100%，29 scenarios）。
2. [x] GLM/k3 双只读 peer 独立设计审查（design-glm.md / design-k3.md），先不互读。
3. [x] 定点互审收编：v3 合并裁决已落 spec；两 peer 结论一致度高，分歧（散文归类 InvalidResponse vs InsufficientEvidence）裁决为 InvalidResponse（k3：无法区分语义 vs 协议违规，归格式错更诚实）。
4. [x] RED：按 spec 29 个场景写失败测试（外层 runtime 反例 0pass/2fail EXIT101 → .octos/k3-rescue-logs/red-outer-probes.log）。
5. [x] GREEN：分层实现（纯函数解析 → 自由函数重构 → wrapper → 账本 → 调用点迁移；22pass/0fail/1ignored + 2 outer probes EXIT0 + clippy -D warnings EXIT0，.octos/k3-rescue-logs/）。
6. [x]（历史完成度更正）实现轮验证当时全过：clippy -D warnings EXIT0、fmt EXIT0、全量 all-targets 经 root d246b74b 合并树 10030 pass / 0 fail 采信（含两旧 CLI 时序测试）。原文"fail-open"为当时草稿措辞——最终账本读侧 fail-closed（坏行/未知版本/外来 scope 拒收），以 spec Decisions 为准。后续修复增量与全量见 2026-09-10 新 plan。
7. [x] 独立实现审查 implementation-glm-first.md（APPROVE-WITH-CHANGES）/ implementation-k3-first.md（APPROVE）→ cross-primary-on-k3.md / cross-strong-on-glm.md（含 root 纠偏追加），sha256 全冻结，token_cost.model 双证（glm-5.3 / k3-256k）。
8. [x]（历史完成度更正）修复增量（M1/M3/M4/M5 + GAP-1..8 + Filter 绑定）已完成 RED→GREEN 并经双模型互审 + root 8b4da851 全量门采信；后续 merged-review 修复（warning wire 键 / replay note 去重 / spec 44 场景）见 2026-09-10 新 plan。

## 风险

- agent_orchestrator.rs 45k 行，改动需最小侵入（新类型放 goal_loop_runtime.rs，orchestrator 只加 wrapper + 改调用点）。
- 编译破坏面（k3 §6 实勘）：4 真实调用点 + goal_tool.rs 2016/2074 ignored 测试 + ~5 verifier 直测解构适配（20269/37829/2545 等）；maybe_complete_goal_from_model 全家 + ~14 处 Done 构造零改动（verdict 枚举不动的回报）。
- 存量 mock 返回空 content 的测试会看到 2 次调用（新重试语义）——RED 阶段排查点。
- budget_exhaustion 场景是唯一跨层（wrapper+charge+goal_update）集成测试，其余 gate 场景落 wrapper 层。
