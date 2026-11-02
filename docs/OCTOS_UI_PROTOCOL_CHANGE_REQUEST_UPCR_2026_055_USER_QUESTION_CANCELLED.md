# UPCR-2026-055 — durable user-question cancellation notification

## Summary

Adds `user_question/cancelled` so a client can retire a cancelled question after
reconnect, and the audit ledger records its terminal state (#1492).

## Wire contract

- Method: `user_question/cancelled`; params: `session_id`, optional `topic`,
  `question_id`, `turn_id`, `reason`. Topic is preserved from the session key.
- One event per pending-to-cancelled transition. Answered or previously cancelled
  questions do not generate another event. A turn drain persists the whole batch
  before awaiting a transport send, so a second abort cannot lose later events.
  Late answers return
  `user_question_stale` with the same reason.
- Question reasons have their own open registry: `turn_interrupted`,
  `request_send_failed`, `waiter_dropped`, `peer_closed`, `peer_purged`.
- Covered paths: explicit turn interrupt, connection-bound turn abort, requester
  future drop (timeout/abort/unwind), request delivery failure, peer close and its
  post-registration gate, and peer purge of the peer or its contexts.
- Under UPCR-2026-056 ordinary WebSocket detach leaves server-owned turns and
  questions alive. Stdio, external clients and connections providing host tools
  remain connection-bound. A detached server-owned question's closed transport
  therefore does not trigger `request_send_failed` cancellation.

## Capability and ownership

The existing `user_question.v1` capability gates both requested and cancelled
notifications on direct sends, live forwarding and replay. External prompt
ownership applies to the cancellation too, including direct peer-close/purge
emissions from a host connection. The ledger records the external-prompt marker
and carries it from request to cancellation even after owner-table eviction;
replay/hydrate can use the store's recorded owner, but after a process restart
external records are hidden when their owner is unknown.

## Compatibility and verification

The core notification-method registry and `from_method_and_params` decoder
recognize the additive method; clients accept unknown reasons and remove only
the identified pending question. Old records may omit topic.

Focused regression coverage exercises codec/registry/topic preservation,
interrupt and connection abort, detach/reconnect, guard drop and competing
cancellations, answered/late responses, failed sends, peer close and purge,
capability gates, and external live/replay ownership including cold recovery.
