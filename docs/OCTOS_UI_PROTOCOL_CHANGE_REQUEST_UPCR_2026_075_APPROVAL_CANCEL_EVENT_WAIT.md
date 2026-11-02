# UPCR-2026-075 — Approval-cancellation test waits for the store event

Status: proposed (implemented for review)
Task: T-0183
Capability: none (no protocol change)

## Problem

The approval-cancellation regression used a fixed 100ms sleep before asserting that the pending-approval response sender had closed. A busy Windows runner could schedule the driver later and fail the test before the real cancellation, removing an unrelated PR from the merge queue.

## Contract

No RPC method, notification, event payload, schema, capability, permission, lifecycle, or client-visible behavior changes. The production path already cancels the pending approval synchronously before awaiting a full durable-send queue. The test now waits for that existing oneshot closure with a failure-guard timeout, then retains the full-queue park, abort, and bounded-join assertions.

## Validation

The complete stalled-consumer file passes 17/17 tests, and the exact full-path target passed 100 consecutive Unix iterations with one test executed per iteration. A PR-only Windows step runs the exact target by its full path so a zero-test filter cannot masquerade as coverage. No production code or wire behavior changes.
