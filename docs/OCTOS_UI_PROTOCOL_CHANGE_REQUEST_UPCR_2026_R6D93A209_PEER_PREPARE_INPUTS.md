# UPCR-2026-R6D93A209 — Declare first-turn inputs when staging a peer over OUP

Status: proposed

An OUP caller staging a peer for a page had to hand over file paths for the
maker to reopen. UPCR-2026-R6D93A209 exposes the existing first-turn input
contract to ordinary `peer/prepare` calls through the optional `inputs` field
and the `peer.prepare.inputs.v1` capability. The existing `peer_handoff`
resolver, fenced checkout checks, peer read permissions, budget and image
handling also apply here. An empty or omitted list preserves profile-only
staging. A nonempty list requires an open owning session with a read scope
and its own workspace. Host-owned and resumed app peers reject such requests
before provisioning. The response and turn formats stay compatible.

The request-boundary test checks that the first completion request contains
the declared page data and image bytes. The refusal test checks that invalid
ownership, a different workspace and missing required input leave no peer.
