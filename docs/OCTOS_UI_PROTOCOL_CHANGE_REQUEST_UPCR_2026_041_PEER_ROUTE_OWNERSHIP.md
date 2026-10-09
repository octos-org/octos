# UPCR-2026-041: peer route ownership on tool registration, unregister, and session tool-list set

- **Request id**: UPCR-2026-041
- **Date**: 2026-10-07
- **Status**: implemented (this PR)
- **Implements**: the #2660 register-hijack fix (this PR's route-ownership gates)

## Problem

A live host-tool route (peer tools or a host session's kernel-tool surface) could be silently taken over by any other connection holding the shared mint-once host token: the registering connection's identity was never checked against the route's current owner. The consequence (SAFE-T1008 tool-shadowing class): another consumer's `peer/tool/call` payloads flow to the taking-over connection, and fake `peer/tool/result` injections reach the model context.

## Contract

1. `peer/tools/register` (peer path): if a live route exists for the peer and belongs to a different connection, refuse with `peer_route_not_owner`. Release there (unregister) or purge first.
2. `peer/tools/unregister`: only the owning connection may release the route; a different connection gets `peer_route_not_owner`.
3. `peer/tools/register` (the host's own-connection session path) and `session/tool_list/set`: the session's tool route belongs to the connection that registered it; another live connection registering gets `peer_route_not_owner`. Same-connection re-declaration (the #2600 re-declare-on-open pattern) is unaffected.
4. Purge already carries `peer_purge_not_owner` (unchanged).

All four entry points now enforce: **the route follows the connection that registered it; taking it over requires releasing it there first.**

## Non-goals

- The mint-once/no-rotation property of the host token (#2562) and the bearer-replay face — credential lifecycle, not route ownership.
- The external-connection gate (#2665) — orthogonal (external connections were always refused).
