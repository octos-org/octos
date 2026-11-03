# UPCR-2026-072 — UI test-support extraction without protocol changes

Status: proposed (implemented for review)
Task: T-0114 / A-04
Capability: none (no protocol change)

## Problem

Shared UI protocol test helpers and a provider stub live inside the giant transport test module, forcing a sibling test module to depend on it. Extracting test support touches protocol-owned test files but not the runtime contract.

## Contract

No production code, RPC method, notification, payload, schema, capability, or permission changes. Test helper bodies, channel capacity, receive semantics, and stub failure behavior remain identical. Test count and fully qualified test names remain unchanged.

## Validation

Compare the sorted transport test-name lists before and after extraction, run the complete transport test suite and CLI library tests, and run formatting and diff checks.
