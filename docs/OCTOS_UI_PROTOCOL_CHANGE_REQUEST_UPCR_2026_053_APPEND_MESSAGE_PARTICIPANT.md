# Octos UI Protocol Change Request: `participant` on `session/append_message`

## Header

- Request id: `UPCR-2026-053`
- Date: 2026-10-23
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented (#2859)
- Scope: additive optional `participant` on `session/append_message` and on
  persisted `Message` rows; memory extraction shows it; extraction and
  consolidation rules keep participant scope; negotiated feature
  `session.append_message.participant.v1`

## Problem

Some products put several participants on one side of a conversation. The
first case is Octos Learn: two preset tutors answer the same person in
lessons whose question/answer pairs are recorded through
`session/append_message` (UPCR-2026-042). Memory refresh then distills those
records into the profile's long-term memory.

The records carry no notion of who spoke. A caller can only put a label in
the body. A real-model check with Octos 413b4b68 (two lessons, one per
tutor, the label in front of each tutor reply) showed that the generic
extraction rules then:

- drop a shared experience the person valued (a metaphor they said was easy
  to remember, used by one tutor);
- widen a request the person addressed to one tutor ("you don't need
  metaphors") into a general preference, which the other tutor then receives
  and which contradicts that tutor's own style;
- merge that widened request with a request the person did make for every
  tutor ("conclusion first") into one entry.

The extractor did see the labels. The rules simply have no concept of
participants or of the scope of a request.

## Contract

`SessionAppendMessageParams` gains:

- `participant: string` (optional) — who spoke. Valid on `user` and
  `assistant` rows only. After trimming it must be non-empty, at most
  `MESSAGE_PARTICIPANT_MAX_BYTES` (64) bytes, and contain no control
  characters or `[` `]` `(` `)`. Anything else is refused with
  `invalid_params`, data `{ "kind": "invalid_participant" }`. The label is
  shown verbatim inside extraction input, so it is refused rather than
  rewritten.

`Message` gains `participant: Option<String>` with
`serde(default, skip_serializing_if = "Option::is_none")`. Turn-written rows
and legacy JSONL leave it absent, so their bytes are unchanged. The field is
not added to v2 envelopes in this UPCR.

Memory refresh input renders a user or assistant row that has a participant
as `[N:role] (participant: NAME) text`. The `[N:role]` label is unchanged, so
host-side evidence-kind derivation is unaffected.

Extraction rules (added): a shared experience the user found helpful is
durable and names the participant it happened with; a request the user
addressed to one participant applies to that participant only and is never
widened into a general preference; a request the user says applies to
everyone stays general; nothing is attributed to a participant who was not in
that exchange.

Consolidation rules (added): an entry scoped to a participant keeps that
participant; entries scoped to different participants, or a scoped entry and
a general one, are never folded into one entry.

## Negotiation and compatibility

Older servers deserialize `SessionAppendMessageParams` without
`deny_unknown_fields`, so they silently drop `participant`. A free check
against 413b4b68 confirmed it: the extraction request contained no
participant at all. Clients therefore must not rely on the field as the only
record of who spoke unless the server acknowledges
`session.append_message.participant.v1`. Like
`session.append_message.full_history_retry.v1`, the feature is advertised
only after explicit negotiation: it is not part of the no-header
`first_server_slice` baseline.

The method's gating is unchanged (UPCR-2026-042).

## Verification

- octos-core: participant serde round-trip and legacy absence; feature
  advertised only when negotiated.
- octos-cli: handler persists the trimmed label; it refuses blank,
  bracketed, multi-line and over-long labels and labels on `system` rows;
  transport acknowledges the feature only when asked; extraction input
  renders the participant; extraction and consolidation prompts carry the
  scope rules.
- Real model (Octos Learn trial script `verify-teacher-memory-refresh.mjs`,
  gemini-3.8-flash, one run per build, same two synthetic lessons):

  | | 413b4b68, label in body | this change, `participant` |
  |---|---|---|
  | Shared metaphor with tutor B | dropped | kept, names tutor B |
  | "No metaphors", said to tutor A | widened to every tutor | tutor A only |
  | "Conclusion first, all tutors" | kept, merged with the above | kept, general, separate |
  | Personal fact (basketball) | kept | kept |

  This is a single run per build; it shows the rules work, not how stable
  they are across runs.
