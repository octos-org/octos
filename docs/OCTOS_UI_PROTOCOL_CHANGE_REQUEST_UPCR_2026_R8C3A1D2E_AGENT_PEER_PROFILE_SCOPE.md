# UPCR-2026-R8C3A1D2E — AgentUpdated / PeerStaged / PeerClosed join profile-scoped delivery

Status: proposed (implemented for review)
PR: #3271

## Decision record

`UPCR-2026-R8C3A1D2E`.

### Problem (octos#2081)

`agent/updated`, `peer/staged` and `peer/closed` are durable notifications
that carry tenant data (`output_tail`, `summary`, `cwd`, `brief`,
`brief_path`). #2075 scope-filtered the other durable autonomy
notifications (`session/goal/*`, `loop/*`, `monitor/*`, `session/open`,
`background/activity`) but deliberately left these three unfiltered: their
profile stamps are turn-side (the agent record's / peer session's runtime
profile), and on an unauthenticated admin connection with a header-routed
profile the turn profile can diverge from the connection scope, so a blind
filter could starve the very connection that produced the event. Meanwhile
the events leak across profiles on a shared wire session key.

### Decision

Add the three arms to the existing profile-scope delivery filter
(`ledger_event_matches_profile_scope`), which both the reconnect replay and
the live forwarder already share:

- On a connection whose scope is a tenant boundary (an authenticated
  connection pinned to profile `P` at upgrade, #2067), the three
  notifications are delivered only when their stamped profile equals `P`.
  Emitter and receiver agree by construction there: the open ran
  `validate_authenticated_session_scope`, so the session runtime, its
  background-task mirrors and its peer staging all stamp `P`.
- Unauthenticated (admin / routed-header) and stdio connections keep the
  deliver-everything boundary: their `profile_scope` is `None`, so nothing
  is filtered. This preserves the deployment mode #2075 was protecting —
  the turn-side stamp can never be filtered away from the connection that
  produced it, because such a connection is never filtered at all.

The stamps are required wire fields on all three events (no legacy
unstamped rows exist), so the comparison is a plain equality — no
`_main`-normalization fallback.

### Compatibility

- **Wire shape**: unchanged. No field, method, capability or cursor change.
  Clients that already ignore foreign-profile frames are unaffected.
- **Behavior**: a profile-scoped connection opening a shared session key no
  longer replays or live-receives `agent/updated` / `peer/staged` /
  `peer/closed` stamped with another profile. This is the intended tenant
  isolation #2075 established for the other autonomy events.
- **Unscoped/stdio connections**: unchanged (deliver everything).
- Two same-pass alignments with no independent contract weight:
  - `resolve_autonomy_profile_id` now rejects a whitespace-only
    `profile_id` (`trim().is_empty()`), matching the delivery filter's
    normalization instead of minting a whitespace pseudo-profile.
  - The M9 `TaskOutput` test fixture stamps its mirrored agent record with
    the turn's resolved profile instead of a hard-coded `_main`, so the
    fixture still works on a profile-scoped connection.
