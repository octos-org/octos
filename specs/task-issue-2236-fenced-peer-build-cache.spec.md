spec: task
name: "围栏 peer 复用 workspace 构建缓存(issue #2236)"
tags: [peers, fence, cargo, autonomy, octos-cli]
estimate: 0.5d
---

## Intent

`peer_handoff` 的围栏是 `peers/<slug>/wt` 下的全新 `git clone --no-hardlinks`,克隆天然
没有 `target/`,Rust peer 的第一次 `cargo test` 从零编译整个 workspace(实测单次 147 s /
500 s),50 迭代预算耗尽于等编译,产出留在围栏树里未提交(2026-09-04 OLP #45 实证,
issue #2236)。本任务让围栏 peer 在不放松 `.git` 隔离的前提下复用 workspace 的热构建
缓存:围栏创建时给克隆写一份 cargo 配置指向 workspace 的 `target/`,并把这个决定写进
`model_note` 与 `peer_staged` 事件,让外环可审计。

## Decisions

<!-- lint-ack: decision-coverage — "不用环境变量"是实现约束,由 cargo 配置场景整体行使 -->

- 触发条件:围栏创建(`stage_peer` 中 `worktree == true` 分支)且 workspace 根目录存在
  `Cargo.toml`。非 Cargo workspace 不写任何文件、不改 `model_note`。
- 机制:在克隆完成后写 `wt/.cargo/config.toml`,内容恰为
  `[build]` + `target-dir` 两行,不含 CARGO_HOME/registry 等任何其它键
  (原条目续):
  `[build]\ntarget-dir = "<workspace_root>/target"\n`(绝对路径,按 TOML basic string
  编码,反斜线/引号/控制字符必须原样往返;非 UTF-8 路径保守地不共享),
  并把 `.cargo/config.toml` 追加进 `wt/.git/info/exclude`,保证 peer 的 `git status` 干净、
  普通 `git add` 不会把它带进提交(不承诺阻止 `git add -f`)。
  不用环境变量(peer 的工具进程环境不由 stage 阶段掌控)。
- 若 `wt/.cargo/config` 或 `wt/.cargo/config.toml` 已存在(含悬空符号链接),不覆盖,
  改为在 `model_note` 记 `build cache: repo has its own .cargo/config or config.toml, left untouched`。
  Cargo 的旧名称 `config` 优先级更高,不能在旁边新增 config.toml 后误报 shared。
- Unix 写入必须锚定 peer/wt/.cargo/.git/info 的 no-follow 目录句柄,拒绝符号链接;
  先成功追加 Git exclusion,再以 no-replace 原子发布配置,绝不截断项目的现有文件。
  失败返回 None,不发布未排除配置或误报 Shared。非 Unix 沿用 checked-path fallback,
  拒绝已有符号链接/目录 reparse point,不声称消除 check/open 的并发竞态。
- `model_note` 追加一行 `build cache: target-dir -> <workspace_root>/target`(与既有
  model-lane 提示同字段、换行拼接,已有内容不丢)。
- `peer_staged` 事件的 `detail` 由 `"peer staged"` 扩为 `"peer staged (build cache: shared)"`
  / `"peer staged"`(None) / `"peer staged (build cache: repo-config)"` 三种之一;
  未围栏的 peer 保持 `"peer staged"` 不变。
- 沙箱边界:本任务不改沙箱策略。共享 `target/` 在 `--danger-full-access` 与
  workspace-write 且 workspace 根可写的档位下生效;沙箱不放行时 cargo 自己报错,
  `model_note` 已告知路径,peer 可自行 `export CARGO_TARGET_DIR` 回退到克隆内。
- 测试放 `crates/octos-cli/src/peers/mod.rs` 既有 `#[cfg(test)]` 模块,用临时目录 +
  `git init` 的真实仓库夹具,不真正运行 cargo。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/peers/mod.rs
- specs/*.spec.md
- crates/octos-cli/src/obs_events.rs

### Forbidden
- 不改围栏的克隆方式(`git clone --no-hardlinks`)与 `.git` 隔离语义。
- 不改沙箱策略、权限档、`peer_handoff` 的参数面。
- 不引入新依赖;不向 peer 进程注入环境变量。
- 不改 `peer_staged` 以外任何事件的 kind 或字段。

## Out of Scope

- 非 Cargo 生态(npm、pnpm 等)的缓存复用。
- 沙箱对共享 `target/` 的写放行(另立)。
- 智能围栏(#20a)判定逻辑。
- 共享 target 的 LRU 回收与围栏拆除配对(后续 issue,对齐 #2235)。

## Acceptance Criteria

Scenario: Cargo workspace 围栏写入共享 target 配置(critical)
  Tags: critical
  Test: fenced_peer_gets_shared_target_dir_config
  Given 一个含 Cargo.toml 的临时 git 仓库作为 workspace 根
  When 以 worktree=true 为 slug "p1" 执行 stage_peer
  Then 存在 wt/.cargo/config.toml 且内容为 [build] target-dir 指向 workspace 根的 target 绝对路径
  And wt/.git/info/exclude 含 .cargo/config.toml
  And 返回的 model_note 含 "build cache: target-dir ->"

Scenario: 非 Cargo workspace 不写配置
  Test: fenced_peer_without_cargo_toml_writes_nothing
  Given 一个不含 Cargo.toml 的临时 git 仓库
  When 以 worktree=true 执行 stage_peer
  Then wt/.cargo 目录不存在
  And model_note 不含 "build cache"

Scenario: 仓库自带 .cargo/config.toml 时不覆盖
  Test: fenced_peer_keeps_repo_cargo_config
  Given 一个含 Cargo.toml 与已提交 .cargo/config.toml 的临时 git 仓库
  When 以 worktree=true 执行 stage_peer
  Then wt/.cargo/config.toml 的 sha256 与仓库中该文件相同
  And model_note 含 "left untouched"

Scenario: 未围栏 peer 不受影响
  Test: unfenced_peer_untouched_by_build_cache
  Given 一个含 Cargo.toml 的临时 git 仓库
  When 以 worktree=false 执行 stage_peer
  Then workspace 根下不新增 .cargo/config.toml
  And peer_staged 事件 detail 等于 "peer staged"

Scenario: 配置路径正确转义
  Test: should_encode_target_path_when_fenced_workspace_contains_toml_metacharacters
  Given workspace 路径包含反斜线或引号
  When 以 worktree=true 执行 stage_peer
  Then 生成的 TOML 可解析且 target-dir 精确等于原始绝对 target 路径

Scenario: 旧 Cargo 配置保留且决定真实
  Test: should_preserve_legacy_config_when_fenced_repository_owns_cargo_settings
  Given 仓库已提交 .cargo/config
  When 以 worktree=true 执行 stage_peer
  Then config 保持不变且不创建 config.toml
  And build_cache 等于 RepoConfig

Scenario: 克隆来的符号链接不允许围栏外写入
  Test: should_not_write_outside_fence_when_cloned_cargo_directory_is_symlinked
  Given 仓库已提交指向临时围栏外目录的 .cargo 符号链接
  When 以 worktree=true 执行 stage_peer
  Then 围栏外不出现 config.toml 且 build_cache 等于 None

Scenario: 排除规则写入失败不能宣称共享成功
  Test: should_decline_shared_cache_when_git_exclusion_cannot_be_written
  Given Git exclude 不是可写入的普通文件
  When 执行共享缓存配置
  Then 不发布 config.toml 且 build_cache 等于 None

Scenario: peer_staged 事件携带构建缓存决定
  Test: peer_staged_detail_reports_build_cache
  Given 一个含 Cargo.toml 的临时 git 仓库
  When 以 worktree=true 执行 stage_peer 并读取写入的 peer_staged 事件
  Then 事件 detail 等于 "peer staged (build cache: shared)"

Scenario: 围栏内 git 状态保持干净
  Test: fenced_peer_git_status_clean_after_config
  Given 已按上述方式围栏的 wt
  When 在 wt 内执行 git status --porcelain
  Then 输出为空
