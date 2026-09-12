spec: task
name: "build.rs 监听真实 Git 元数据路径（普通/linked/detached/packed/无 Git/归档）"
tags: [build, cargo, git, octos-cli]
---
## Intent
 crates/octos-cli/build.rs 的 rerun-if-changed 必须指向真实存在的 Git 元数据路径：linked worktree 不再因监听不存在的 ../../.git/HEAD 而无改动重复编译；同分支新提交仍更新 OCTOS_GIT_HASH；归档源码不得误采外层仓库。

## Decisions
- 路径解析：git rev-parse --path-format=absolute --git-path（HEAD / refs/heads/<branch> / packed-refs），逐个存在才输出 rerun-if-changed。
- 同分支提交触发：refs/heads/<branch> 文件内容随提交改写（HEAD 文本不变）→ 必须监听该 ref 文件。detached HEAD 场景 HEAD 文件本身含 commit。
- packed refs：存在 packed-refs 时监听之；提交/分支操作改写它或生成 loose ref，build script 重跑后重解析路径集合。
- 无 Git/归档防误采：包目录祖父为项目根，要求根有 .git 且 Git 解析的 canonical toplevel 与根相等；Git 自行处理相对或绝对 gitdir。无 Git 时版本 hash 为空，仅监听 build.rs。
- 禁止硬编码本机路径；零新增依赖。

## Boundaries

### Allowed Changes
- crates/octos-cli/build.rs
- crates/octos-cli/tests/build_git_metadata.rs
- specs/task-build-git-metadata-watch.spec.md
- docs/superpowers/plans/2026-09-12-build-watch.md

### Forbidden
- 硬编码绝对路径或假造存在性
- 新增任何 crate 依赖
- 监听不存在路径
- 以字符串包含代替 -vv rustc 计数断言

## Completion Criteria

### Rule: git-metadata-watch — 元数据监听与构建新鲜度
Scenario: 普通仓库二次无改动 Fresh
  Test:
    Package: octos-cli
    Filter: build_git_watch_regular_repo_second_build_fresh
  Given 一个普通 git 仓库 fixture（复制生产 build.rs）
  When cargo build --offline -vv 连续两次（同一 fixture target）
  Then 第二次 probe 为 Fresh 且 rustc 调用 0 次

Scenario: linked worktree 二次无改动 Fresh
  Test:
    Package: octos-cli
    Filter: build_git_watch_linked_worktree_second_build_fresh
  Given git worktree add 的 linked worktree fixture
  When cargo build --offline -vv 连续两次
  Then 第二次 Fresh 且 rustc 0 次（修复前 Dirty：监听 ../../.git/HEAD 不存在）

Scenario: 同分支提交后 hash 更新
  Test:
    Package: octos-cli
    Filter: build_git_watch_same_branch_commit_updates_hash
  Given 预热后的 fixture 仓库
  When 同分支新 commit 后第三次 build
  Then probe Dirty 且 OCTOS_GIT_HASH == 新 commit 短 SHA

Scenario: detached HEAD 提交切换触发
  Test:
    Package: octos-cli
    Filter: build_git_watch_detached_head_change_triggers
  Given linked detached worktree fixture 预热
  When checkout 到另一 commit
  Then 重建且 hash 更新

Scenario: packed refs 二次 Fresh 且提交触发
  Test:
    Package: octos-cli
    Filter: build_git_watch_packed_refs_fresh_and_commit_triggers
  Given git pack-refs --all 后的 fixture
  When 二次无改动 build（Fresh/rustc0）；随后同分支提交
  Then 先 Fresh；提交后 Dirty 且 hash 更新

Scenario: 无 Git 目录仅监听 build.rs 且构建成功
  Test:
    Package: octos-cli
    Filter: build_git_watch_no_git_dir_builds_without_rerun
  Given 无 .git 的 tempdir fixture
  When cargo build
  Then 成功且 build.rs 输出仅含 build.rs 的 rerun-if-changed 行、hash 为空

Scenario: 归档不误采外层仓库
  Test:
    Package: octos-cli
    Filter: build_git_watch_archive_inside_outer_repo_not_adopted
  Given 外层 git 仓库内以 git archive 解出的源码归档（无 .git）
  When cargo build
  Then 成功、仅监听 build.rs、hash 为空（不采用外层仓库 hash）

Scenario: 分支切换触发重建
  Test:
    Package: octos-cli
    Filter: build_git_watch_branch_switch_triggers
  Given 预热 fixture
  When git checkout 另一分支后 build
  Then 重建且 hash 更新

Scenario: 被父仓库 track 的归档不误采
  Test:
    Package: octos-cli
    Filter: build_git_watch_tracked_archive_inside_outer_repo_not_adopted
  Given 外层 git 仓库内解出归档且外层仓库已 track 归档文件（ls-files 会命中）
  When cargo build
  Then 成功、hash 为空、二次 Fresh（owning-repo 校验：项目根 .git 与 canonical toplevel 一致）

Scenario: pack-refs --prune 后同分支提交刷新 hash
  Test:
    Package: octos-cli
    Filter: build_git_watch_packed_pruned_same_branch_commit_refreshes
  Given pack-refs --all --prune 后预热的 fixture（无 loose ref）
  When 同分支新 commit（仅创建 loose ref，HEAD/packed-refs 不变）
  Then 重建且 hash == 新短 SHA（refs 目录监听发现创建）

Scenario: 相对 gitdir 与 linked worktree 提交更新
  Test:
    Package: octos-cli
    Filter: build_git_watch_relative_gitdir_and_linked_commit_refresh
  Given linked worktree 的 .git 使用合法相对 gitdir 路径
  When 连续构建两次后在当前分支提交并再次构建
  Then 无改动构建不调用 probe rustc,提交后版本 hash 等于新 HEAD

Scenario: fixture 清理不修改父仓库
  Test:
    Package: octos-cli
    Filter: build_git_fixture_cleanup_preserves_ancestor_worktrees
  Given fixture 位于含过期 worktree 登记的自有外层测试仓库
  When 清理无 Git 的子 fixture
  Then 仅删除该 fixture,外层仓库登记保持不变
