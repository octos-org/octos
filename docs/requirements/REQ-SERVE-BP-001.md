# REQ-SERVE-BP-001: octos serve 关闭期 BrokenPipe 不阻断 gateway cleanup

**Status**: Accepted
**Date**: 2026-08-26
**Source**: 主板 #21 迁入专属车道 w5:p2，外环(codex) 主审
**Accepted by**: 外环(codex), 2026-08-26

## Statement

`octos serve` 长驻进程在关闭流程中，向已断开的 stdout/stderr 写入控制台输出时
触发 `BrokenPipe` panic，随后 `color_eyre::PanicHook` 的 `eprintln!` 二次 panic，
形成双重 panic → `std::process::abort()` → SIGABRT core dump，导致
`process_manager.stop_all().await` gateway 清理路径被跳过。

## Rationale

- 崩溃时系统约 17 GiB available，非 OOM；非随机 SIGSEGV。
- core 内存保留原始 panic `failed printing to stdout: Broken pipe (os error 32)`。
- 主线程栈：`ServeCommand::run_async → std::io::stdio::_print → panic`，随后
  `color_eyre::PanicHook → std::io::stdio::_eprint → panic`。
- 定位到 `crates/octos-cli/src/commands/serve.rs` 中
  `ServeCommand::run_async` 关闭路径的 `println!("{}", "Stopping gateways...".yellow())`，
  发生在 `axum::serve(...).await?` 返回后、`process_manager.stop_all().await` 之前。

## Acceptance

1. serve 控制台输出（启动与关闭路径）改为 fallible I/O，
   `ErrorKind::BrokenPipe` 不 panic、不中断 server 或 gateway 清理。
2. `color_eyre` panic hook stderr 写入改为 fallible I/O，吞掉 BrokenPipe，
   不二次 panic/abort；Eyre report hook 能力保留。
3. 关闭顺序保持：graceful shutdown → `process_manager.stop_all().await` →
   正常退出。任何控制台写失败不能跳过 `stop_all`。
4. 修法集中成小型 console-output/helper 边界，不散布 `let _ = println!`，
   不依赖 nightly `unix_sigpipe`/`on_broken_pipe`，不把 SIGABRT 改成另一个
   非零信号掩盖清理缺失。

## Derived Task Contract

- `specs/task-s-broken-pipe-shutdown.spec.md` — task contract + 6 条可执行
  Acceptance Criteria selectors（agent-spec 1.4.0 lint 100%）。

## Governance

| 阶段 | 状态 | 证据 |
|---|---|---|
| requirement 创建 | done | 本文件，REQ-SERVE-BP-001，Status: Accepted |
| 接受态治理 | accepted | 外环(codex) 于 2026-08-26 逐条审阅 Statement/Rationale/Acceptance/Boundaries 后签收 |
| task contract/spec | done | `specs/task-s-broken-pipe-shutdown.spec.md`，agent-spec 1.4.0 lint 100% |
| agent-spec guard/verify 基线 | pending | agent-spec 1.4.0 只验证 task contract（`spec: task`），不解析 requirement；待 G2/G3/G4 修订后执行 |
| 实现 | frozen | 脏树冻结（serve_console.rs 新增、serve.rs/main.rs/mod.rs 改动，未 commit） |
| lifecycle 终验 | pending | 待实现解冻后执行 |
