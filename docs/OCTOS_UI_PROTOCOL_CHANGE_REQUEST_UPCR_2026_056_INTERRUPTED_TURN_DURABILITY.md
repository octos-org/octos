# UPCR-2026-056 — interrupted-turn durability: dispatched prompt commits to durable history

## Summary

When a turn is interrupted (client disconnect, explicit cancel, kernel error unwind) after the kernel has dispatched the user's prompt to the model, the dispatched prompt MUST be committed to the session's durable history before the turn is torn down. On reconnect, a client resuming the session MUST observe the turn's input in history exactly once — never a silent drop.

## Motivation

Before this change, an interrupted turn's dispatched prompt existed only in the in-flight turn state: a disconnect between dispatch and completion lost the user's message from history entirely (octos#2831, filed by octoscode-web as P1 #78). The client saw its own message vanish; retrying duplicated work or dropped intent.

## Contract

1. **Commit-before-teardown**: the interrupt path commits the dispatched prompt (and any assistant output already produced) to durable history synchronously before releasing turn resources.
2. **Exactly-once replay**: reconnect/replay reads history, not in-flight state — the prompt appears once regardless of how many times the client reconnects.
3. **Cancel is still recorded**: an explicit client cancel records the prompt with the turn's terminal reason, so the audit trail shows asked-and-cancelled, not asked-and-vanished.

## Acceptance

octoscode-web#261 probe (`e2e-live/interrupted-turn-durability`): fails (red) on kernels without the contract, passes (green) once this change lands.
