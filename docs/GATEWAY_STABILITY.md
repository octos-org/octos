# Gateway Stability Issues

Identified via deep code review of the agent loop, gateway dispatch, and message bus.
All issues cause the gateway to hang silently or lose messages without logging.

## Issue Tracker

### CRITICAL

| # | File | Description | Status |
|---|------|-------------|--------|
| 1 | `octos-llm/src/{openai,anthropic,gemini,openrouter,embedding}.rs` | **No HTTP timeout on LLM requests.** All providers use `Client::new()` with no timeout. A hung API server blocks the agent loop forever. | Fixed |
| 2 | `octos-agent/src/agent.rs` `execute_tools()` | **No timeout on tool execution.** `join_all(futures)` has no outer timeout. A hung tool (MCP, web_search) blocks the agent loop indefinitely. `WebSearchTool` also has no HTTP timeout. | Fixed |
| 3 | `octos-cli/src/commands/gateway.rs` spawned task | **No timeout on session processing.** `process_session_message()` holds the session lock and semaphore permit for the entire duration. If the LLM hangs, the lock is held forever. 10 stuck calls deadlock the entire gateway. | Fixed |
| 4 | `octos-cli/src/commands/gateway.rs` lines 1251-1259 | **Shared tool context race.** `MessageTool`, `SendFileTool`, `SpawnTool`, `CronTool` are shared `Arc` across all concurrent sessions. `set_context()` mutates routing state — concurrent sessions can overwrite each other, delivering messages to wrong recipients. | Fixed |

### HIGH

| # | File | Description | Status |
|---|------|-------------|--------|
| 5 | `octos-bus/src/channel.rs` lines 116-156 | **Outbound dispatcher panic kills all delivery.** The dispatcher runs in a fire-and-forget `tokio::spawn`. If `channel.send()` panics, the dispatcher dies silently. All subsequent messages are lost. | Fixed |
| 6 | `octos-bus/src/channel.rs` line 112, `gateway.rs` lines 317-320 | **`recv_inbound()` can never return None.** `BusPublisher.in_tx` survives inside the outbound dispatcher task, plus 4 clones held by cron/heartbeat/spawn/collect. Even if every channel dies, the main loop hangs forever. | Fixed |

### MEDIUM

| # | File | Description | Status |
|---|------|-------------|--------|
| 7 | `octos-cli/src/commands/gateway.rs` lines 1051,1078,1102,1368,1384 | **Silent error on outbound sends.** All `out_tx.send()` use `let _ =`. If the channel is closed, the response is silently lost with no log. | Fixed |
| 8 | `octos-cli/src/commands/gateway.rs` lines 1316,1329 | **Silent error on session persistence.** `add_message()` failures are silently discarded — conversation history is lost. | Fixed |
| 9 | `octos-agent/src/agent.rs` lines 1237-1309 + `retry.rs` | **Double retry stacking.** Agent-level retry (3x) wraps provider-level retry (3x). Worst case: 16 LLM calls with minutes of delay. | Documented |
| 10 | `octos-bus/src/channel.rs` lines 166-171 | **`stop_all()` short-circuits on first error.** Uses `?` — if one channel fails to stop, remaining channels are never stopped. | Fixed |
| 11 | `octos-cli/src/commands/gateway.rs` lines 959,1148; `chat.rs` line 343 | **Wrong memory ordering.** Shutdown flag loaded with `Ordering::Relaxed` but stored with `Release`. On ARM (Apple Silicon), the load can miss the store. | Fixed |

### LOW

| # | File | Description | Status |
|---|------|-------------|--------|
| 12 | `octos-bus/src/channel.rs` lines 102-108 | **Channel tasks are fire-and-forget.** Only Telegram has a reconnect loop. Other channels die permanently on error. | Documented |
| 13 | `octos-cli/src/commands/gateway.rs` lines 265-276,861-867,923-929 | **Unmonitored background tasks.** Metrics exporter, Ctrl+C handler, persona updater have no panic logging. | Documented |
| 14 | `octos-cli/src/commands/gateway.rs` line 962 | **Config changes only applied on next message.** Idle gateways never pick up config changes. | Documented |
