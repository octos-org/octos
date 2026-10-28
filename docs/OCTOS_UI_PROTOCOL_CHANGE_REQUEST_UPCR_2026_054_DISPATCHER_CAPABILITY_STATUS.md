# Octos UI Protocol Change Request: dispatcher capability status

## Header

- Request id: `UPCR-2026-054`
- Date: 2026-10-24
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented in PR #2862; pending merge
- Scope: tool availability reporting and transport context projection for the
  default fixed tool dispatcher

## Problem

With fixed tool dispatch, a capability can be authorized and callable without
having its own declaration in the model request. Reporting only directly
advertised tools incorrectly marks these capabilities unavailable. Transport
context projection must also preserve provider-required reasoning and avoid
repeating unchanged runtime snapshots on every turn.

## Contract

The existing tool status and `coding_tool_contract.required_tools[]` responses
describe model-reachable capabilities. A registered, authorized native tool
reachable through `run_tool` is available even when its schema is absent from
the directly advertised list. Host exclusions, profile restrictions, hidden
tools, provider policy and backend availability still apply. The dispatcher
does not authorize a capability that the host denied.

The default request advertises four coding tools and the two dispatch gateways.
Discovery does not activate tools or grow that declaration list. Consequently,
dispatch-only capabilities are available, not deferred; there is no activation
step. No response field, method name, or status enum is added.

Transport context projection uses the provider-specific reasoning policy and
includes retained reasoning in the compaction budget. Unchanged peer, memory,
and goal snapshots are deduplicated; an initially empty snapshot can be omitted,
but a later clearing snapshot is recorded. One-shot notices retain their
existing event behavior. These are durable context events, not additional
user-authored chat messages.

## Compatibility

Existing clients can consume the unchanged response shape. Clients must use
capability availability rather than assuming one directly advertised schema per
available capability. Native approval, cancellation, tool-call identity and
host-owned authorization remain in force. The historical assistant transcript
retains the original gateway call.

## Verification

- Agent dispatcher tests cover stable declarations, denied and hidden tools,
  host allowlists, cancellation, native approvals and hooks.
- CLI dispatcher capability status tests cover gateway-reachable availability
  and exclusion after a provider-policy denial.
- Context manager and transport tests cover provider reasoning projection,
  snapshot deduplication, and context persistence.
- The UI protocol spec documents the meaning of `available` with dispatch.
