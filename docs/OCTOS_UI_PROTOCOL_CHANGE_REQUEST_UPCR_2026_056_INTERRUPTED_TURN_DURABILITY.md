# UPCR-2026-056 — interrupted-turn durability: dispatched prompt commits to durable history

## Summary

When a turn is interrupted (explicit cancel, connection-bound shutdown, kernel error unwind) after the kernel has dispatched the user's prompt to the model, the dispatched prompt MUST be committed to the session's durable history before the turn is torn down. On reconnect, a client resuming the session MUST observe the turn's input in history exactly once — never a silent drop.

## Motivation

Before this change, an interrupted turn's dispatched prompt existed only in the in-flight turn state: a disconnect between dispatch and completion lost the user's message from history entirely (octos#2831, filed by octoscode-web as P1 #78). The client saw its own message vanish; retrying duplicated work or dropped intent.

## Contract

1. **Commit-before-teardown**: the interrupt path commits the dispatched prompt (preserving assistant output already durably committed) to durable history synchronously before releasing turn resources.
2. **Exactly-once replay**: reconnect/replay reads history, not in-flight state — the prompt appears once regardless of how many times the client reconnects.
3. **Cancel is still recorded**: an explicit client cancel records the prompt with the turn's terminal reason, so the audit trail shows asked-and-cancelled, not asked-and-vanished.

The dispatched-prompt commit also precedes `connection_closed`, provider-error,
and `terminal_lost` terminal events (#2881). Connection cleanup cancels and joins
the agent task before checking whether its prompt is already durable. Internal
master continuations retain their existing do-not-persist behavior; interrupted
media turns retain prompt text. The server-owned dispatch path below also commits materialized attachments before execution.

## Acceptance

octoscode-web#261 probe (`e2e-live/interrupted-turn-durability`): fails (red) on kernels without the contract, passes (green) once this change lands.

## Server-owned WebSocket execution (T-0060 / dogfood F-01)

Ordinary accepted WebSocket turns continue through refresh, network loss and
closed tabs. This is a lifetime correction, not an opt-in flag or a new wire
method. A closing connection releases its writer, subscribers and remembered
approval scopes; it does not cancel the admitted orchestration/agent tasks,
pending ordinary approvals/questions, or their execution slots. Detached task
completion removes only the exact old dispatch from the global registry.

Before the model call, commit the finalized user prompt and materialized media.
The end-of-turn append skips that initial user row, preserving separate steers
and assistant/tool rows. Internal continuation prompts remain excluded. A
failure to commit the request fails the turn before model/tool execution.

Shared-history lanes (UPCR-2026-034) deduplicate only the precommitted request;
its presence does not mean the turn finished. The other lane still sees the
live draft and approval/question status until response rows commit or the
running-turn registration ends. Match the dispatch's thread and start time
so unrelated rows and an earlier use of the same turn ID cannot hide it.

Reconnect through the normal authenticated profile/workspace and session/open
cursor replay, then hydrate / query turn state. Pending approvals and questions
remain answerable under the existing checks; explicit turn/interrupt still
cancels the work, including from an authorized replacement socket.

Session-ingress work secrets, stdio shutdown, registered tool hosts and
host-managed external clients retain connection-bound cleanup. Ingress was
omitted from this classification in #2882: outside host-managed mode it is
not an external client, but its turn must still be interrupted when its grant
fails revalidation and the socket closes with `1008 session_ingress_revoked`.
A still-valid ingress grant also keeps connection-bound lifetime: ordinary
socket disconnect interrupts its turn without revoking the grant. The existing
abort path prevents subsequent tool calls, cancels pending approvals/questions,
and evicts remembered authorization scopes. Already-started native tools may
continue to finish or reach their configured timeout, including side effects
after turn termination.
This change does not provide cancellation of those tool tasks or processes.
Revalidation occurs before client text frames, not ping/pong control frames;
this does not add background grant polling or change ordinary browser detach.
Their private tool/control/prompt ownership is not
transferred to another connection. The existing #2881 fallback saves dispatched
prompts before connection_closed/error/terminal_lost, and the connection_closed
message tells the caller to reconnect, review the retained request and resend.
This does not resume tasks after server process exit.

T-0020 / #2881 remains the abnormal-exit persistence safety net. T-0060 adds
server-owned WS execution and prompt visibility during a running turn. The
web #78 / T-0021 probe must require continued execution on refresh, while its
explicit-stop variant must still require a retained interrupted prompt.

Verification includes two simultaneous foreground/background turns, closing
before the first lifecycle delivery, duplicate-free hydration while running
and after completion, approval/question recovery, exact-dispatch cleanup, and
the existing external/host ownership and stdio cancellation regressions. The
browser acceptance uses the same compiled frontend as the dogfood site with
an isolated core and deterministic offline provider.

### Session-ingress background work after invalidation

When text-frame revalidation fails for a revoked, expired or replaced work secret,
connection teardown also cancels surviving spawn-only tasks and pending terminal/
failure continuations across every workspace of the granted profile/session,
including after the parent turn has finished. Already-admitted automatic
continuations are interrupted. The session-wide cancellation intentionally
includes browser-originated background tasks in that same profile/session;
foreign sessions and profiles are not cancelled. A valid-grant ordinary
connection close retains background work. A newly issued secret is required
following invalid-grant 1008/401 responses. No eager revocation watcher or
cancellation of already-running native tool processes is introduced.

### Recursive background cancellation and admission generations

Observed work-secret invalidation retires a profile/session background-work
generation before new interactive work is admitted. Every private child
supervisor inherits the cancellation lease, including descendants created after
the original parent ended; terminal/failure producers close before queue purge.
Ingress bind and interactive admission observe grant revisions. A stale socket
holds its original generation and cannot cancel later grant/browser work.
There is no polling watcher: a close without a subsequent text frame does not
observe revocation, and a silent sibling's foreground turn is not closed by
another socket. Synchronous child calls and already-running native tools retain
their existing completion/timeout limitation. Connection cleanup releases the global turn-registry lock before waiting on
gateway or session-manager operations.
