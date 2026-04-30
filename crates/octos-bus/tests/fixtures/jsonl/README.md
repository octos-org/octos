# JSONL Replay Fixtures

This directory holds session JSONL fixtures replayed by the
`jsonl_replay_thread_binding` integration test in
`crates/octos-bus/tests/jsonl_replay_thread_binding.rs`.

The harness asserts the thread_id binding invariant on every record:

> `assistant.thread_id == originating_user.client_message_id`

for every assistant + tool record. Originating user is determined as:

1. The user record whose `client_message_id` matches the
   `response_to_client_message_id` field on the assistant/tool record,
   when present and pointing at a known user cmid.
2. Otherwise, the most recent user record before this record.

## Current fixtures

- `issue-649-three-user-overflow.jsonl` — INTENTIONALLY broken. Mirrors
  the production session `web-1777402538752` from issue #649, where the
  deep-research tool result and final assistant for the stock query were
  tagged with `cmid-3` (voices) instead of `cmid-2` (stock). The test
  asserts this fixture exposes at least three violations at specific
  record indices.

- `correct-three-user-overflow.jsonl` — Same shape with correct
  thread_id bindings. The test asserts zero violations.

## Importing a real production session as a fixture

After issue #649 is fixed and a re-run captures a clean JSONL on a mini,
run:

```bash
scripts/import-session-fixture.sh <mini-host> <remote-jsonl-path> <local-fixture-name>
# e.g.
scripts/import-session-fixture.sh mini3 \
  /var/lib/octos/sessions/web-1777402538752.jsonl \
  fixed-three-user-overflow.jsonl
```

The script copies the JSONL into this directory. To wire the new
fixture into the regression suite, add a corresponding `#[test]` to
`jsonl_replay_thread_binding.rs` that calls `check_jsonl` on it and
asserts whichever expectation applies (zero violations for a fixed
session, or specific violations for a captured regression).

## Schema expected by the harness

Each line is a JSON object with at least these fields:

- `role` — `"user"`, `"assistant"`, `"tool"`, or `"system"`.
- `content` — string body.
- `thread_id` — string (may be empty for legacy records).
- `timestamp` — RFC3339 UTC, used for human-friendly violation reports.
- `client_message_id` — required on `user` records.
- `response_to_client_message_id` — optional on `assistant`/`tool`
  records. Where present, must point to a `client_message_id` of a
  preceding user record; this lets the harness disambiguate which
  user message the assistant is actually replying to when other user
  messages have arrived in the meantime.

## Privacy

Production sessions may contain user content. Before committing a
fixture that originated from a real session, scrub PII and replace
`content` strings with synthetic placeholders. The harness only cares
about `role`, `thread_id`, `client_message_id`, and
`response_to_client_message_id` — so content can safely be redacted to
short summaries.
