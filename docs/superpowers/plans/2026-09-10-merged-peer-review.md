# merged-peer-review 修复计划（2026-09-10，TASK=peer）

审计源：merged-review-audit-20260910/REVIEW-AUDIT.md（#2272）。基线 329566d3。

## 步骤
1. 设计+测试计划 → 修复工作目录下 peer-design.md（会话 scratch 交接件，不入库） ✅
2. 行为修复（实际历史：实现与测试同 turn 连续完成，非测试先行；RED 为步骤 4a 的事后补做可执行验证）：closed 分支改走 trusted_lifetime_projection 同一校验，通过保留身份、否则 null ✅（5 新测试 + 1 既有测试修改）
3. 文档同步：interface（identity note/idle_no_turns round≥1/cli --profile/spec_revision+versioning_note/serve 超集说明）、spec（generation 1 对齐/closed 条款/serve 超集）、peer.rs 注释 ✅
4. Cargo 窗口授予后的真实 RED/GREEN 顺序（ROOT review 2 修正）：
   a. RED（基线文件替换法——GLM 审查指出：生产与测试同在 mod.rs，stash 整文件会连测试一起撤走）：**先 `cp` 保存当前候选 mod.rs 到修复工作目录下 mod.rs.candidate**，再从 `git show 329566d3:crates/octos-cli/src/peers/mod.rs` 取基线文件，把本分支的 closed 测试段（5 新 + 1 修改）（含 helper 不变）拼入基线文件的测试模块 → 编译运行 → identity Some 断言真实执行并红；negative 用例在基线即绿，如实记录其性质为回归钉而非 RED 项
   b. GREEN：从 4a 保存的候选文件 `cp` 恢复 mod.rs（不使用 git stash）→ 同一测试集真实执行全绿
   c. fmt --check + clippy -D warnings（获租约后）
   d. 如实记录：本任务实际历史是先写实现（同 turn 连续完成），RED 步骤为事后补做的可执行验证，不以 git show 静态推断替代测试
5. GLM-5.3 + K3-256K 只读独立 diff 审查 → 互读 → disposition → 最终报告 peer-result.md

## Dispositions
- 性能重复读消除：wontdo（无测量基线；禁缓存/过期语义）
- 孤儿 Running 恢复写入者：wontdo（破坏 begin/finish 单写者；维持已知限制）
- schema version bump：不做（行为语义澄清不改输出字段集）

## 实际验证结果（2026-09-10，Cargo 窗口期）
- RED（基线生产 + 候选测试，真实执行）：closed 6 测试 → 2 failed / 4 passed，EXIT=101——identity Some 断言在基线恒 null 行为上真实红；4 个负例在基线即绿（回归钉性质，如实记录）。日志 red-run.log
- GREEN（候选恢复后）：closed 6/6 passed，EXIT=0（green-run.log）；peer_list_ 全套 35/35 passed，EXIT=0（green-peerlist.log）
- fmt --check：首次 EXIT=1（一处 assert 换行），`cargo fmt -p octos-cli` 后已修复（纯格式，语义零变化）
- clippy -p octos-cli --features api --all-targets -- -D warnings：EXIT=0（clippy.log）
- fixture 顺序修正（ROOT 确认）：closed_after_failed_round 改为先 round1 completed 后 round2 interrupted（terminal helper 重写 result.md，倒序会破坏最高轮绑定）
