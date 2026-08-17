spec: task
name: "octos serve 不再长期持有 /proc/*/stat 句柄"
tags: [octos-cli, sysinfo, resources, admin-metrics]
estimate: 0.5d
---

## Intent

事故排查时发现运行 26 小时的 `octos serve --stdio --solo` 持有约 1200 个
`/proc/<pid>/stat` 与 `/proc/<pid>/task/<tid>/stat` 句柄，其中不少指向早已退出
的进程。根因是 `sysinfo` 在 Linux 上为每个索引过的进程/线程缓存一个打开的
`stat` 文件（预算为 `RLIMIT_NOFILE` 的一半，且会把软限制抬到硬限制）；
`serve` 启动时用 `System::new_all()` 把全机进程快照进内存并持有句柄，之后只有
admin dashboard 轮询 `system_metrics` 时才 `refresh_all()` 清理死进程——stdio
模式下没人轮询，句柄就永久开着。本任务把 `sysinfo` 的句柄缓存关掉、启动时不再
快照进程、`system_metrics` 只刷新它真正渲染的数据。

## Decisions

<!-- lint-ack: verification-metadata-suggestion — fd 计数场景读的是本进程 /proc/self/fd，属进程内自检，非外部 I/O -->

- 在构造 `AppState.sysinfo` 之前调用一次 `sysinfo::set_open_files_limit(0)`（封装为
  `octos_cli::sysinfo_budget::new_metrics_system()`——模块位于 crate 顶层，sysinfo 相关函数
  以 `#[cfg(feature = "api")]` 门控，结构检查测试不门控；`serve.rs` 与 `api/mod.rs`
  两处构造均改用它），
  使 `sysinfo` 每次刷新都是打开→读取→关闭，不保留任何 `/proc` 句柄。
- 启动构造用 `System::new()`（不快照进程），不再使用 `System::new_all()`。
- `system_metrics` 改为按需刷新：`refresh_cpu_usage()` + `refresh_memory()`；仅当
  `procs=1` 时以 `ProcessesToUpdate::All`、`remove_dead_processes = true`、
  `ProcessRefreshKind::nothing().with_cpu().with_memory()`（不含 tasks/线程）刷新进程；
  封装为 `sysinfo_budget::refresh_metrics(sys, include_procs)`。
- 语义不变：端点输出的字段集与数值来源不变（CPU 平均、内存、磁盘、可选 top 进程）。
- Linux 专属行为：fd 断言测试仅在 `target_os = "linux"` 编译。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/lib.rs
- crates/octos-cli/src/sysinfo_budget.rs
- crates/octos-cli/src/api/mod.rs
- crates/octos-cli/src/api/admin.rs
- crates/octos-cli/src/commands/serve.rs
- specs/task-sysinfo-proc-stat-fd-budget.spec.md

### Forbidden
- 不改变 `system_metrics` 响应的字段名或含义。
- 不新增 crate 依赖，不升级 `sysinfo` 版本。
- 不引入周期性后台刷新任务来"清理"句柄（治标）。

## Completion Criteria

### Rule: no-retained-proc-handles — 刷新后不保留 /proc 句柄
Scenario: 进程刷新两轮后本进程没有任何 /proc/*/stat 句柄（critical；需 --features api）
  Tags: critical
  Review: human
  Test:
    Package: octos-cli
    Filter: metrics_refresh_retains_no_proc_stat_fds
  Given 通过 `new_metrics_system()`（内部已调用 `sysinfo::set_open_files_limit(0)`）构造的 `System`
  When `refresh_metrics(sys, true)` 连续调用两次（进程刷新用 `ProcessRefreshKind::nothing().with_cpu().with_memory()`）
  Then `/proc/self/fd` 中不存在指向 `/proc/<pid>/stat` 或 `/proc/<pid>/task/<tid>/stat` 的链接（`target_os = "linux"`）
  And `sys.processes()` 非空（进程数据确实被刷新了）

Scenario: 启动构造不快照进程（需 --features api）
  Review: human
  Test:
    Package: octos-cli
    Filter: metrics_system_does_not_snapshot_processes_at_startup
  When 调用 `new_metrics_system()`
  Then `sys.processes()` 为空
  And `/proc/self/fd` 中不存在指向 `/proc/<pid>/stat` 的链接

Scenario: 不请求 procs 时不刷新进程表（需 --features api）
  Review: human
  Test:
    Package: octos-cli
    Filter: metrics_refresh_without_procs_leaves_process_table_empty
  Given 通过 `new_metrics_system()` 构造的 `System`
  When `refresh_metrics(sys, false)` 被调用（只走 `refresh_cpu_usage()` + `refresh_memory()`）
  Then `sys.processes()` 仍为空
  And CPU 数量与总内存已被填充（大于 0）

### Rule: no-new-all — 代码中不再有启动期全量快照
Scenario: serve 与 api 状态构造不再调用 System::new_all（结构检查）
  Test:
    Package: octos-cli
    Filter: sysinfo_budget_module_owns_all_system_constructions
  When 扫描 `crates/octos-cli/src` 中的 `sysinfo::System::new` 调用
  Then 只有 `sysinfo_budget.rs` 直接构造 `System`
  And 源码中不出现 `System::new_all()`

## Out of Scope

- 把 dashboard 的进程列表改为分页/流式。
- 其他 `/proc` 读取路径（tool sandbox、bwrap 探测）——本次事故的句柄全部来自 `sysinfo`。
