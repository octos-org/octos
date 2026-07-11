# Octos UI Protocol Change Request: §6 catalog guard covers the FIRST_SERVER registry

## Header

- Request id: `UPCR-2026-028`
- Title: Extend the §6 catalog guard to the `UI_PROTOCOL_FIRST_SERVER_METHODS` registry
- Author: UI Protocol spec-consistency audit (codex follow-up)
- Date: 2026-07-10
- Target protocol: `octos-ui/v1alpha1`
- Status: accepted
- Sibling UPCRs: `UPCR-2026-027` (introduced the §6 catalog + drift guard)

## Summary

Strictly a test-tooling change. It adds **no** new method, notification, payload,
enum variant, capability bit, or protocol identifier — the wire surface is
unchanged. It closes a coverage gap in the drift guard added by `UPCR-2026-027`.

## Motivation

The guard `spec_section6_catalog_lists_every_advertised_method` asserts that §6 is
a superset of the advertised method constants. `UPCR-2026-027` chained three
lists: `UI_PROTOCOL_COMMAND_METHODS`, `UI_PROTOCOL_NOTIFICATION_METHODS`, and
`APPUI_EXTRA_METHODS`.

But the running server advertises its method surface from a different pair:
`ui_protocol_server_supported_methods` builds
`UI_PROTOCOL_FIRST_SERVER_METHODS ∪ APPUI_EXTRA_METHODS`. The guard omitted
`UI_PROTOCOL_FIRST_SERVER_METHODS`.

Today the registries overlap — `FIRST_SERVER ⊆ COMMAND`, so no method is
currently unguarded (verified: chaining the list in leaves `missing` empty and
the test green). But a future **server-only** method added to
`UI_PROTOCOL_FIRST_SERVER_METHODS` (and not to `UI_PROTOCOL_COMMAND_METHODS`)
could be advertised without a §6 entry and slip past the guard — exactly the
drift class the guard exists to stop. codex review flagged this on the merge
commit of `UPCR-2026-027`.

## Changes

- Chain `UI_PROTOCOL_FIRST_SERVER_METHODS` into the guard's `missing` computation
  so §6 must stay a superset of the full advertised surface
  (`COMMAND ∪ NOTIFICATION ∪ FIRST_SERVER ∪ APPUI_EXTRA`).
- Update the test's doc comment and assertion message to name the fourth list.

## Compatibility

- Strictly test-only: no existing method, notification, payload, enum variant,
  capability bit, or protocol identifier changes; no client or server behavior
  changes. Zero §6 edits (the catalog was already complete for this set).

## Implementation anchors

- Guard: `octos-cli/src/api/ui_protocol.rs`
  `spec_section6_catalog_lists_every_advertised_method`
- Advertised registry: `octos-cli/src/api/ui_protocol.rs`
  `ui_protocol_server_supported_methods`
