# UPCR-2026-RF2E0C984 — session_ref live subscription, missing `_main` tolerance, delete invalidation

Status: proposed (behavior fix, no contract change)
PR: TBD (T-0310)

## Decision record

`UPCR-2026-RF2E0C984`.

Follow-up fixes to #2890 (`workspace.session_refs.v1`), all behavior-only:

1. `session/open {session_ref}` now subscribes the live forwarder to the
   RESOLVED session identity instead of the empty `session_id` carried by the
   request, so title/status/message/terminal events reach ref-opened
   connections. Legacy `session_id` opens are unchanged.
2. Ref resolution skips profiles with no runtime and no store entry (e.g. an
   admin with no `_main`) instead of failing the whole lookup, and unscoped
   resolution also considers store-only profiles. `workspace/list` for such
   an absent profile returns an empty list rather than erroring. No
   authorization widening: external and user-scoped connections keep their
   existing scope checks.
3. `session/delete` removes the deleted session's `session_ref` registry
   rows, scoped to the (profile, session id) pair, so resolving or opening a
   deleted session's ref now fails with the existing `session_not_found`
   semantics instead of resurrecting it; a same-named session in another
   profile keeps its ref.

No wire shape, method, field, or permission changes.
