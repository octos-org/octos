spec: task
name: "octos serve 关闭期 BrokenPipe 双重 panic / SIGABRT 修复"
tags: [serve, shutdown, broken-pipe, panic-hook, console-output, octos-cli]
estimate: 0.5d
requirement_id: REQ-SERVE-BP-001
---

## Intent

REQ-SERVE-BP-001：`octos serve` 进入关闭流程后，`ServeCommand::run_async` 中
`println!("{}", "Stopping gateways...".yellow())` 向已断开的 stdout 写入触发
`BrokenPipe` panic；随后 `color_eyre::PanicHook` 用 `eprintln!` 写 stderr 再次
panic，形成双重 panic → `std::process::abort()` → SIGABRT core dump。这不是
OOM（崩溃时 17 GiB available），也不是 SIGSEGV。

本任务将 serve 控制台输出改为 fallible I/O，加固 color-eyre panic hook 的
stderr 写入，确保 `BrokenPipe`（观察者离开）不阻断
`process_manager.stop_all().await` 清理路径。

## Decisions

- 新增 `serve_console` helper 模块（`crates/octos-cli/src/commands/serve_console.rs`），
  提供 fallible 控制台写入函数，核心函数签名
  `fn write_line(w: &mut impl io::Write, msg: &str) -> io::Result<()>`，
  加 stdout/stderr 薄封装 `print_stdout` / `print_stderr`。
  内部用 `write_all`，不 panic。
- `ErrorKind::BrokenPipe` 视为观察者离开的正常信号：吞掉、记 tracing debug、
  继续后续逻辑；其他写错误记 tracing warn、不覆盖 shutdown 结果。
- `serve.rs` 中 `ServeCommand::run_async` 启动段与关闭段的
  `println!`/`eprintln!` 全部替换为 helper 调用。
- `main.rs` 的 `color_eyre::install()` 改为 `color_eyre::config::HookBuilder::into_hooks()`，
  手动安装 EyreHook + 自定义 PanicHook：panic report 写入抽为
  `fn write_panic_report(w: &mut impl io::Write, msg: &str)`，
  用 fallible I/O 吞掉 BrokenPipe；Eyre report hook 能力保留。
- 关闭顺序不变：graceful shutdown → `process_manager.stop_all().await` →
  `std::process::exit(0)`。
- 不引入 nightly `unix_sigpipe` / `on_broken_pipe`；不散布 `let _ = println!`；
  不把 SIGABRT 改成另一个信号。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/commands/serve.rs
- crates/octos-cli/src/commands/serve_console.rs
- crates/octos-cli/src/commands/mod.rs
- crates/octos-cli/src/main.rs
- specs/task-s-broken-pipe-shutdown.spec.md
- docs/requirements/REQ-SERVE-BP-001.md

### Forbidden
- 不改变 `process_manager.stop_all()` 语义或调用顺序
- 不改变 axum graceful shutdown 机制
- 不改变 tracing 日志的既有 rolling sink 配置
- 不引入新的后台任务或阻塞点

## Out of Scope

- 其他 CLI 子命令的 println! 加固（仅 serve 路径）
- SIGPIPE 信号处理策略变更

## Acceptance Criteria

### Rule: console-helper — serve_console helper 提供 fallible 写入，测试机械覆盖 production wiring

Scenario: BrokenPipe 写入不 panic 且返回 Ok
  Test:
    Package: octos-cli
    Filter: serve_console_write_line_broken_pipe_returns_ok
  Level: unit
  Targets: crates/octos-cli/src/commands/serve_console.rs write_line
  Given 一个实现 io::Write 的 writer 在 write_all 时返回 Err(ErrorKind::BrokenPipe)
  When serve_console::write_line 以该 writer 被调用
  Then 函数返回 Ok(())
  And 不 panic

Scenario: 其他 IO 错误降级为 tracing warn 不 panic
  Test:
    Package: octos-cli
    Filter: serve_console_write_line_other_error_returns_ok
  Level: unit
  Targets: crates/octos-cli/src/commands/serve_console.rs write_line
  Given 一个实现 io::Write 的 writer 在 write_all 时返回 Err(ErrorKind::Other)
  When serve_console::write_line 以该 writer 被调用
  Then 函数返回 Ok(())
  And 不 panic

Scenario: 正常 writer 输出原文
  Test:
    Package: octos-cli
    Filter: serve_console_write_line_normal_writer_verbatim
  Level: unit
  Targets: crates/octos-cli/src/commands/serve_console.rs write_line
  Given 一个正常的 Vec<u8> 作为 writer
  When serve_console::write_line(writer, "hello") 被调用
  Then writer 内容为 "hello\n"
  And 函数返回 Ok(())

Scenario: print_stdout 薄封装调用 write_line 并传入 stdout lock
  Test:
    Package: octos-cli
    Filter: serve_console_print_stdout_delegates_to_write_line
  Level: unit
  Targets: crates/octos-cli/src/commands/serve_console.rs print_stdout
  Given print_stdout 是 write_line 的薄封装
  When print_stdout("test") 被调用
  Then 内部调用 write_line 并传入 io::stdout().lock()
  And 函数返回 Ok(())

### Rule: panic-hook — color-eyre panic hook 在 stderr BrokenPipe 时不二次 panic

Scenario: write_panic_report 在 BrokenPipe writer 上不 panic
  Test:
    Package: octos-cli
    Filter: write_panic_report_broken_pipe_no_panic
  Level: unit
  Targets: crates/octos-cli/src/main.rs write_panic_report
  Given 一个实现 io::Write 的 writer 在 write_all 时返回 Err(ErrorKind::BrokenPipe)
  When write_panic_report 以该 writer 被调用
  Then 函数返回（不 panic）
  And 不触发二次 panic

Scenario: write_panic_report 在正常 writer 上输出内容
  Test:
    Package: octos-cli
    Filter: write_panic_report_normal_writer_outputs
  Level: unit
  Targets: crates/octos-cli/src/main.rs write_panic_report
  Given 一个正常的 Vec<u8> 作为 writer
  When write_panic_report(writer, "panic message") 被调用
  Then writer 内容非空
  And 函数返回（不 panic）

Scenario: 子进程集成测试 stderr 断管时 panic 不 abort
  Test:
    Package: octos-cli
    Filter: subprocess_panic_stderr_broken_pipe_no_abort
  Level: integration
  Targets: crates/octos-cli/src/main.rs panic hook 安装路径
  Given 一个测试二进制安装自定义 panic hook
  And 其 stderr 被重定向到已关闭读取端的 pipe
  When 该进程触发 panic
  Then 进程退出码非 SIGABRT（非 134）
  And 进程退出码非 SIGSEGV（非 139）

### Rule: shutdown-order — 关闭顺序保持且断管不阻断清理

Scenario: 断管后 stop_all 仍被执行且 cleanup marker 可观察
  Test:
    Package: octos-cli
    Filter: serve_shutdown_broken_pipe_cleanup_marker_observed
  Level: integration
  Targets: crates/octos-cli/src/commands/serve.rs ServeCommand::run_async
  Given `octos serve` 以 pipe 模式运行且 stdout 已断开
  When SIGINT 触发 graceful shutdown
  Then `process_manager.stop_all().await` 被执行
  And tracing 日志包含 "gateways stopped" 或 "stopping all gateway child processes"
  And 进程退出码为 0（非 SIGABRT）
  And 无新 core dump 文件生成

Scenario: 关闭顺序保持 graceful → stop_all → exit
  Test:
    Package: octos-cli
    Filter: serve_shutdown_order_preserved
  Level: integration
  Targets: crates/octos-cli/src/commands/serve.rs ServeCommand::run_async
  Given `octos serve` 正常运行
  When SIGINT 触发关闭
  Then axum graceful shutdown 先完成
  And `process_manager.stop_all().await` 随后执行
  And tracing 日志包含 "stopping all gateway child processes"
  And 最终调用 `std::process::exit(0)`

### Rule: serve-startup — 启动输出在断管时不 panic

Scenario: 启动阶段 stdout 断管不 panic
  Test:
    Package: octos-cli
    Filter: serve_startup_broken_pipe_no_panic
  Level: integration
  Targets: crates/octos-cli/src/commands/serve.rs ServeCommand::run_async
  Given `octos serve` 以 pipe 模式运行
  And stdout 在启动输出前已断开
  When serve 输出启动信息（Listening/App/Admin dashboard）
  Then 不 panic
  And server 继续正常运行
