# UPCR-2026-R6982BEFC — Durable Chat / Code work mode on sessions

Status: proposed (implemented for review)
PR: TBD

## Decision record

`UPCR-2026-R6982BEFC`.

Octoscode Web is adding a Chat mode next to today's Code mode (two-modes
proposal, task T-A). A client must be able to tell which mode a session
belongs to on open, list and hydrate, and get the same answer after a
reconnect, a server restart, or from another client. Inferring the mode from a
title or cwd is not acceptable. This record adds that durable metadata only.
Per-chat workspaces (T-B) are a separate record.

### Feature

`session.work_mode.v1` (`UI_PROTOCOL_FEATURE_SESSION_WORK_MODE_V1`). It is in
the known-feature registry. Like `session.append_message.participant.v1`, it is
strictly opt-in:

- It is not part of the no-header `first_server_slice` baseline.
- It is not a stdio default.

A connection that does not request it sees byte-identical replies.

### Wire

`WorkMode` is `"chat" | "code"`.

- **`session/open`** gains optional `work_mode`.
  - It requires `session.work_mode.v1`. Without the feature the open fails with
    `invalid_params`, `data.kind = "feature_required"`.
  - The open that creates the session records it.
  - When the session already exists, the request must match the recorded mode.
    Otherwise the open fails with `invalid_params`,
    `data.kind = "work_mode_mismatch"`, and `data.requested` / `data.work_mode`.
    A session's mode never changes after creation.
  - Omitting `work_mode` opens any session and creates new ones as Code.
  - A value other than `"chat"` or `"code"` fails parameter decoding
    (`invalid_params`).
- **`session/create`** (UPCR-2026-060) gains the same optional `work_mode`, with
  the same rules.
- **`SessionOpened`** gains `work_mode`. It is present exactly when the connection
  negotiated the feature, and it is always the session's recorded mode. Only the
  opening connection's reply carries it. The `session/open` ledger record and its
  broadcast to other connections are unchanged.
- **`session/hydrate`** results gain `work_mode`, under the same presence rule,
  whatever `include` asks for.
- **`session/list`** rows (`SessionInfo`) gain `work_mode`.
  - With the feature, every row carries it. A row the server cannot attribute,
    such as one merged from a gateway listing, says `"code"`.
  - Without the feature, no row carries it.
  - `session/history/list` rows do not carry it.
  - SessionRef catalog rows (`workspace.session_refs.v1`) do not carry it yet.

### Durability

- The mode is stored as `work_mode` in the meta line of the session's JSONL
  record. It is written once, by the first-open record
  (`SessionManager::ensure_session_record_in_mode`).
- Every later meta write keeps it:
  - rewrites;
  - title updates;
  - seal rolls;
  - strict-mode recovery;
  - a writer whose in-memory copy predates the record. The disk value wins when
    the writer has none.
- Clearing a session (`/clear`, `/new`) drops its history but not its mode. A
  chat's record is re-created holding only the mode.
- A forked session inherits its parent's mode.
- A record without `work_mode` reads as Code. That covers every session that
  predates this record, so none is ever moved into Chat. A stored value this
  build does not recognise also reads as Code. It is kept verbatim across
  rewrites, so a newer server that wrote it still finds it.

### Compatibility

- Additive and opt-in. Old clients and old servers are unaffected.
- A client that did not negotiate the feature cannot set a mode. It still opens
  chats as ordinary sessions.
- `turn/start`, approvals, the queue, recovery and the ledger's persisted
  records are unchanged.

### Web fixture

Octoscode Web's mock server already serves this shape for its two-modes e2e
token: `work_mode` on `SessionOpened` and `session/list` rows, and
`session.work_mode.v1` in `supported_features`. To use a real server, the web
client must add `session.work_mode.v1` to the features it requests.
