# Octos UI Protocol Change Request: model continuation and media transport

## Header

- Request id: `UPCR-2026-055`
- Date: 2026-10-27
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented in PR #2877; pending merge
- Scope: additive LLM profile configuration, persistence and connection testing

## Problem

Model reasoning continuation and provider image-upload behavior must survive
profile persistence and provider reconstruction. A model catalog entry alone
cannot express an operator's custom route policy. The protocol's strict field
validation previously rejected these settings or omitted them when creating a
provider for a connection test.

## Contract

The existing `profile/llm/upsert` selection accepts the following optional fields:

- `selection.model_hints.reasoning_history`: `omit`, `preserve`,
  `preserve_with_tools`, or `preserve_or_placeholder`.
- `selection.model_hints.lacks_video` and `retain_tool_images`: booleans matching
  the provider model-hint configuration.
- `selection.route.media_transport`: `auto`, `inline`, `moonshot_files`, or
  `deepseek_files`.
- `selection.model_hints.image_file_references`: boolean controlling whether the
  model may receive image file references on a capable route.

Absent/null model hints inherit the provider/catalog policy. Absent/null media
transport defaults to `auto`. Auto selects a Files adapter only for recognized
official HTTPS origins and paths; unknown endpoints retain inline image data.
Explicit adapters allow operators to configure compatible proxy endpoints.
An explicit false image-file-reference hint keeps images inline even when the
route supports uploads. True does not make an unsupported route capable.

These fields persist with the LLM selection and are returned in profile
readback. Each fallback selection retains its own route and hint settings.
`profile/llm/test` constructs its provider with the same supplied model hints
and media-transport policy. Its existing text-only connection probe is not a
claim that image upload or vision support has been verified.

The existing strict selection-field validation remains in place. No RPC method,
event, authorization scope or capability-negotiation requirement is added.

## Compatibility and isolation

Existing clients may omit every new field. New clients talking to an older
server must omit unsupported fields if that server rejects them; this change
does not introduce automatic field negotiation.

No tool permission, model identity, endpoint access or host-owned authorization
is granted by these settings. Provider upload references are scoped by endpoint,
credential fingerprint and adapter. They are held in memory, not serialized into
profile configuration. Credentials, image bytes and file IDs are not persisted
by the transport cache. Cancellation, tool-call pairing and tenant/profile
ownership continue to follow the existing contracts.

## Verification

- `octos-llm` Kimi contract tests cover catalog declarations, context limits,
  reasoning continuation through tool results, and resumed context.
- Media transport unit and integration tests cover endpoint/account isolation,
  concurrent upload reuse, changed image bytes, chat/stream reuse, inline
  opt-out, cancellation, bounded refresh and unsupported-reference fallback.
- CLI profile serialization and provider-construction tests cover route/hint
  round trips and independent primary/fallback settings.
- The protocol specification documents the fields alongside the existing LLM
  profile methods. `scripts/check-ui-protocol-upcr.sh` checks coverage for the
  corresponding transport and protocol-test changes.
