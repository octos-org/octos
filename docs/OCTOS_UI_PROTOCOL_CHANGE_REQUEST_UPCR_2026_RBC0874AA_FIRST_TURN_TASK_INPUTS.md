# UPCR-2026-RBC0874AA — A peer's declared inputs arrive in the first request of their task

Status: proposed

## Decision record

No wire shape changes. `UPCR-2026-RBC0874AA` records what the server adds to the
first message of a staged peer's conversation, and to the message a
`peer_send_input` call delivers to an existing peer.

The `peer_handoff` tool takes an optional `inputs` list: the files the peer
needs to begin, each a path as the peer would name it to `read_file`, with an
optional `kind` (`text`, `data`, `image`), `required` (default true) and
`sha256` pin. A fleet task in `goal_plan` takes the same list. Both go through
one resolver (`octos_agent::task_inputs`).

At staging the server checks the declaration with the calling session's
`SessionScope` when one is available and records it beside the brief, before
`brief.md` makes the peer visible. A required input that is not there refuses
the handoff, and nothing is staged. A hidden folder, denied path, or unavailable
session scope is left to the peer session's own read access; the note says only
that its first request resolves it, never whether the file exists, its size, or
its digest.

At the first `turn/start` of the peer's session — the conversation has no
message yet — the server reads the declared files under that session's own read
access and appends them to the turn's text before the message is committed:

- text and structured data, each whole, in a fenced block with its path and
  SHA-256;
- an image as an attachment of the message, frozen as a private snapshot, when
  the session's provider takes images;
- a manifest naming every declared input and what became of it: delivered, or
  left out and why (missing, denied, changed, over budget, unsupported,
  unreadable, invalid, duplicate).

An input is delivered whole or not at all. When a required input cannot be
delivered, none are: the message gains a notice listing each input and the
reason instead, and the turn still runs, because a peer reports back only by
running. The decision is also recorded in `peers/<slug>/inputs-report.jsonl`,
one line per turn that was due inputs.

A delivered image stays delivered, or the report says otherwise. When an
endpoint refuses image parts, the OpenAI-compatible provider sends a request
again without its images so the turn can go on. A request that carries a
required input image is not sent again without it: it fails with the refusal,
and the peer's turn ends errored with that reason, so a peer never does its task
without an image its task requires. An optional input image is dropped by that
retry like any other, and the request that goes out without it says so: the
manifest line that called the image delivered reads `NOT delivered
(unsupported)` in that request, while the committed message is left as it was.
When the turn ends, a further line in `inputs-report.jsonl` repeats the turn's
report with that image as left out (unsupported) and names it under
`images_dropped`.

Because the inputs are part of the committed first message, later turns replay
them from the transcript as they were first read, and are not given them again.

### A message to an existing peer

The `peer_send_input` tool takes the same optional `inputs` list: the files the
task in its message needs. The server checks it with the live peer session's
`SessionScope` when that runtime is available; otherwise it carries the
declaration to the owning turn without learning whether the files exist, their
type, size, or digest. A required input that is not there refuses the send, with
every input's state, and nothing is queued. A sha256 mismatch reports only that
it does not match, never any part of the file's actual digest.

The declaration is queued and persisted with the message, as continuation
metadata, and moves with it when a reopen re-homes the message to the peer's new
wire. The turn that runs the message reads the files then, under the peer
session's own read access, and appends them to the message exactly as above,
before it is committed. A repair round therefore gets the files as they are when
the peer starts on it, while the files an earlier message brought stay in the
transcript as they were read then. When that message is also the first of the
conversation, the inputs the peer was staged with and the message's own go in
together, under one budget and one manifest.

Only the serve continuation queue carries a declaration. A `peer_send_input`
that declares inputs for a host-owned app peer (whose host starts its turns) or
for a peer served by the gateway's in-process inbox is refused with that reason
instead of being sent without them.

What a client observes:

- `turn/start`, `peer/staged`, `session/open` and every other method,
  notification and field are unchanged. A client keeps sending the brief as the
  peer's first turn and attaches nothing.
- The first user message of a peer staged with inputs is longer than the text
  the client sent, and may carry image media the client did not attach. The
  transcript and `session/hydrate` return the message as committed.
- The user message a `peer_send_input` with inputs becomes is likewise longer
  than the message that was sent, and may carry image media.
- A session that is not a staged peer's, a peer staged without inputs, and every
  turn after a conversation's first that delivers no such message are untouched.

No protocol method, notification, field, session or turn identifier, or
capability is added or reinterpreted by `UPCR-2026-RBC0874AA`.
