spec: task
name: "Persist an isolated peer model selection before the first turn"
tags: [peer, model, oup, octos-cli]
---

## Intent

Support octoscode `/peer --model <id>` without changing the profile default
or racing the kickoff turn. Advertise `peer.model_override.v1` and accept
`peer/prepare.model_override: {model_id}` for new native peers. Resolve one
configured primary, fallback or sub-provider model and validate provider
construction before reserving a peer. Persist only the ID, not credentials.

## Decisions

- All fleet members receive the selection before prepare succeeds. A write
  failure rolls the staged fleet back, including worktrees and cache slots.
- Read the persisted selection at session open and turn start. A missing
  legacy record uses existing lane routing; an invalid/unavailable override
  fails explicitly. Never replace the shared profile runtime.
- Keep configured endpoints, credential selectors and context-window
  overrides with their selected model. Reject ambiguous model IDs.
- Use existing profile scoping and originator/host-token permissions.
  `peer/model/set` replaces the initial override after authorization.
- Native staging only: combining with a lane, resume or host namespace is
  rejected. Existing host-owned peer behavior remains unchanged.

## Acceptance Criteria

Scenario: The first provider resolution and reconnect use the staged model
  Test:
    Package: octos-cli
    Filter: peer_model_override_is_staged_for_every_member_without_changing_profile
  Given a profile with small primary and strong configured fallback
  When two native peers are prepared with model_override strong
  Then both records and turn-provider resolutions select strong
  And the persisted profile is unchanged

Scenario: Unknown models never create a peer
  Test:
    Package: octos-cli
    Filter: peer_model_override_rejects_unknown_model_before_staging
  Given an unconfigured model ID
  When peer preparation is requested
  Then it fails before creating the peers directory

Scenario: Model routes retain credentials and ambiguity is rejected
  Test:
    Package: octos-cli
    Filter: peer_model_override_rejects_ambiguous_ids_and_keeps_route_credentials
  Given primary and sub-provider routes with different credentials
  When a unique sub-provider model is selected
  Then it keeps its own endpoint and key selector
  And malformed or multiply configured IDs are rejected

Scenario: Corrupt or unsafe records cannot silently select the default
  Test:
    Package: octos-cli
    Filter: peer_model_override_invalid_record_never_falls_back_to_primary
  Given malformed, stale or symlinked override records
  When the turn provider is resolved
  Then resolution fails while ordinary peers retain their default behavior

Scenario: Only the owning session can replace an override
  Test:
    Package: octos-cli
    Filter: peer_model_override_replacement_requires_originator_and_is_isolated
  Given two peers with a persisted override
  When an unrelated session tries to reset one
  Then the request fails and its override stays active
  When the originator resets it through peer/model/set
  Then it returns to primary while its sibling keeps the override

Scenario: Clients can discover the additive guarantee
  Test:
    Package: octos-cli
    Filter: peer_model_override_capability_is_advertised_with_prepare
  Given a backend with peer/prepare
  When capabilities are read
  Then peer.model_override.v1 is advertised

Scenario: The first provider request names the selected model
  Test:
    Package: octos-cli
    Filter: peer_model_override_first_request_uses_selected_model
  Given a configured fallback served by a local mock endpoint
  When a peer is staged and its first model request is sent
  Then the endpoint receives model strong exactly once
  And the profile still selects small

Scenario: Cold profiles use the same inherited configuration as runtime
  Test:
    Package: octos-cli
    Filter: peer_model_override_resolves_inherited_cold_profile
  Given a child profile inheriting configured models from its parent
  When the override is validated before runtime bootstrap
  Then it resolves the inherited configured model
