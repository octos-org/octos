# UPCR-2026-R4946304A — Turn interrupt also stops in-flight tool execution

Status: proposed
PR: TBD

## Decision record

**Problem (T-0241, evidenced by T-0236/T-0283 dogfood probes):** every turn
abort path (`turn/interrupt`, work-secret revocation/expiry,
`abort_connection_turns`) only dropped the agent task's `JoinHandle`. Tool
calls live in their own `tokio::spawn` workers, so an already-running
`shell` command kept running to its own timeout (up to 600s) and mutated
files AFTER the client was told the turn had stopped.

**Decision:** a per-turn `tokio_util::sync::CancellationToken` is created
at turn admission, stored on the `ActiveTurn` registry entry, and threaded
into the turn's `Agent`. Every abort path fires the token BEFORE aborting
the agent task and then grants a bounded grace
(`TURN_TOOL_CANCEL_GRACE = 2s`, well under the 5s interrupt ack budget) so
in-flight tools can terminate honestly:

- `shell` spawns its child in its own process group (`process_group(0)`,
  Unix) and watches the token; on cancel it SIGTERMs the group, probes
  after 500ms, SIGKILLs survivors — the same routine the command-timeout
  arm already used (now extracted as `terminate_process_tree`). Windows
  keeps `taskkill /F /T` as the documented equivalent. Docker sandbox
  relies on `docker run --rm` signal forwarding (client-side SIGKILL is
  best-effort; documented).
- The tool worker (`spawn_tool_task`) races the tool future against the
  token with a 1.5s grace (`TOOL_TURN_CANCEL_GRACE`); after the grace the
  future is dropped and the transcript records a synthetic cancelled
  Tool message — no dangling tool_call, and the result text never claims
  completion for non-cancellable (MCP/remote) side effects.
- The agent loop checks the token at the top of each iteration and ends
  the turn instead of issuing another LLM call.
- Synchronous spawn children receive `token.child_token()`, so the cascade
  reaches nested tools and model calls (T-0283 P2-4). Detached background
  tasks are excluded by design — they are cancelled through the T-0222
  supervisor graph instead.

**Wire compatibility:** no protocol message, field, or codec changes. The
client-visible change is behavioral only: after an interrupt, tool results
arrive as cancelled Tool messages before the terminal event, and started
subprocesses no longer outlive the stop acknowledgement.

**Overlap (documented division):** T-0108 R-16 gave `manage_skills` its
own abort flag + subprocess kill for install mutations. That flag is
install-scoped; bridging it to this turn token is a later cleanup, not
part of this change.

Cite this record elsewhere as `UPCR-2026-R4946304A`.
