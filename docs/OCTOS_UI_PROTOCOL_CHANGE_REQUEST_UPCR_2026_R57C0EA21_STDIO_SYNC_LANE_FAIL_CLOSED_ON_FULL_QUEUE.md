# UPCR-2026-R57C0EA21 — Stdio sync lane fails closed on a full writer queue

Status: proposed (implemented for review)
Task: T-0108 (audit R-04)
Capability: none (no protocol change)

## Problem

The SYNC stdio lane of `WsConnection` (`enqueue_durable_or_lifecycle`, serving
RPC replies via `send_rpc_result`/`send_rpc_error` and ledger notifications
via `send_notification_durable`) performed a blocking `SyncSender::send`.
With a stalled host reader the bounded writer queue fills and the send parks
an executor worker indefinitely — freezing dispatch, cancellation, and
cleanup for the connection. Enough stalled workers freeze the runtime.

## Contract

No RPC method, notification, event payload, schema, capability, permission,
compatibility behavior, JSONL format, or persistence format is added,
removed, or changed. The only behavioral change is the failure mode on a
FULL stdio writer queue for the sync lane: instead of blocking until the
host reader drains, lifecycle sends now fail closed with `LifecycleFailure`
plus the existing `mark_failed` latch (read loop tears the connection down,
unchanged #922.2 mapping), and durable sends report `BackpressureDrop` and
remain recoverable through the existing ledger replay-lossy path. The async
offloaded lanes (#2065/#2807) keep their never-drop cooperative park; wire
bytes on a healthy host are unchanged. No consumer migration is required.

## Validation

Regression tests assert both sync sends return promptly (fail closed) on a
full stdio queue with a stalled host reader; the prior test pinning the
blocking wait is rewritten to the fail-closed contract. The existing
ui_protocol suites (1319 tests), canonical-feature clippy, fmt and typos all
pass.
