# UPCR-2026-064 — Session-bound profile resolution for workspace files

- Status: proposed (implemented for review)
- PR: #2915

## Problem

`session/files/read` resolved the session workspace from the requester's default
profile. An unscoped admin token therefore looked under `_main` even when
`session/open` had returned a named `active_profile_id` such as `coding`, and the
read failed with `session_workspace_not_found`. The same requester-default lookup
also affected session-relative `/api/files` downloads and mutations.

## Change

1. Resolve file access from the profile that actually owns the opened session:
   the embedded `session_id` profile first, then the server-recorded
   `session/open` binding for raw `web-*` ids. Use that profile's data dir for
   the legacy workspace fallback as well as the live workspace binding.
2. Apply this scope to `session/files/read`, session-relative `/api/files` and
   delivered copies, and file mutation. Upload ownership and other
   requester-scoped file rules remain unchanged.
3. WS `validate_session_scope` also honors the recorded open binding, so
   `diff/preview/get` and other session RPCs cannot bypass profile scope with a
   raw id belonging to another user.

## Authorization

- An unscoped admin connection retains the existing model and is authorized for
  every profile, including named-profile sessions.
- A user connection may read only a session whose bound profile matches its
  authenticated/routed profile. A mismatch returns
  `workspace_file_access_denied` on REST file reads and `auth_scope_violation`
  on WS session methods.
- Legacy unauthenticated local requests remain confined to `_main`.
- Explicit profile rebind of a raw session id by unscoped admin remains allowed;
  the recorded binding changes only after a successful open. No new cross-user
  rebind is permitted.

## Compatibility and limits

No new RPC method, feature token, field, or persistence format is introduced.
Existing workspace path confinement, no-follow reads, size caps, download
ownership, and typed error shapes are unchanged. Evicted sessions without an
embedded or recorded profile keep the previous requester-profile fallback;
process restart therefore does not broaden access.

## Verification

- Named-profile session read by unscoped admin and the owning user; another
  profile is denied.
- Raw-id WS binding accepts admin/owner, rejects another profile, and preserves
  explicit unscoped-admin rebind.
- Existing handlers, session-scope, and diff-preview suites.
- Real first-run octoscode-web wizard creates `coding`; an agent writes
  `notes.md`, and artifact Preview displays its contents without
  `session_workspace_not_found`.

## Follow-up: canonical session title and deletion targets

The same owner resolution also applies to `session/title.set` and
`session/delete`. The fully qualified API session key returned by `session/open`
is used unchanged, including its topic. The legacy short-id candidate expansion
must not prefix it again or proxy it through an unrelated gateway profile.
Authenticated/routed profile checks still apply before store resolution; this
change does not grant API-session mutation access to other channel namespaces.

Cold named-profile runtimes are initialized before locating the session store.
Failure to initialize the authoritative runtime or write/remove persisted data
is returned as an RPC error; the client retains its dialog and can retry. A
missing rename target remains `unknown_session`; deleting an already absent
session remains idempotent. Legacy short-id callers retain candidate lookup.
No wire method, field, capability, or transcript format changes. Deletion adds a private lifecycle sidecar beside the transcript stores.

For legacy short IDs, an authenticated user's profile is also used when no
routing header was supplied. Gateway synchronization targets only that owner,
using the credential resolved from its channel configuration and child process
environment. There is no fallback to another profile's first available gateway.
For an independent local store, a persisted rename or deletion still succeeds
if synchronization of a separate gateway copy fails. For a shared store, serve
instead delegates each exact API key to the running owning gateway, using the
endpoint, credential and data directory captured at child startup. Only the
child mutates that transcript; proxy failure never falls back to a local write.
Canonical keys retain exact addressing at the gateway too. Workspace-bound
stores remain local when their actual data directory differs from the child's.
Serve retains its SessionManager and canonical per-key persistence guards
across the delegated request; the child takes its own persistence lock while
mutating. Together these exclude both the child's actor and serve's normal,
send_file, background-result and record-only writers during the mutation. This
is coordination for the managed serve/child pair. Transcript append, rewrite,
metadata mutation and deletion also share a per-store/per-key filesystem lock;
older binaries and third-party writers that ignore that lock are not coordinated. Internal topics retain exact keys
for mutation even when hidden from sidebar listing.

Session-store lock acquisition and waiting for the delegated response each
have a three-second caller budget. A response timeout is an explicitly unknown
outcome, not successful deletion or proof of cancellation: the admitted worker
retains both serve guards until the actual gateway response, then invalidates
cache and finishes sidecar cleanup. Retrying while it is pending fails busy
before dispatching another mutation. Open, hydrate and record-only append also
bound waits for that manager and return retryable errors. A frozen child thus
requires recovery before writes can resume safely; no guard is released just
to turn a pending remote mutation into an apparently cancelled request.
Clients should refresh the observed state before retrying an unknown outcome.

Legacy full `api:<chat>[#<topic>]` keys also address exactly that key, without
short-id expansion. Only main/local or admin callers may mutate this shared
namespace; tenant users must still supply their own profiled key. Real gateway
title writes and deletion return errors when persistence fails, preserving the
distinction from missing sessions. Segment deletion errors also propagate from
explicit clears, before discarding the active transcript or in-memory session.
Best-effort segment cleanup after migration or stale-session purge logs errors.
When flat and per-user history coexist, title changes persist the per-user
metadata too; its read precedence must not resurrect an old title on reload.
Title updates read current disk metadata under the owning process's persistence lock and
preserve raw message/control rows and sealed files, rather than rewriting from
a potentially stale manager cache. Actor rewrites and segment rolls retain the
manual title committed by another writer. Mutations distinguish true NotFound
from unreadable files, untrusted metadata, and unsupported schemas; the
latter remain retryable storage errors. With trusted metadata, malformed middle
rows, torn tails and invalid body bytes are preserved verbatim on rename and
can be explicitly deleted. A valid per-user copy also permits deletion of an
obsolete flat copy with malformed metadata; rename leaves that garbage copy
untouched. I/O errors, foreign metadata keys and future schemas still fail
before either layout is changed. Legacy exact unprofiled API keys use
serve's top-level store without requiring a main LLM runtime to initialize.

Verification adds canonical-key/topic roundtrips, profile isolation, untouched
prefixed aliases, missing-title targets, and storage-error/retry regressions.
The deployed browser against an isolated real core exercises current, old, and
mobile rename across reload, and deletion of a non-current session across reload.

A successful cwd-scoped `session/list` also remembers each eligible API row's
attested workspace if it has no existing binding. This lets a sidebar mutate an
old project session after a core restart without opening it first. The binding
is limited to the authorized listing profile and never overwrites a live open;
foreign-profile and non-API keys cannot acquire a binding through this path.

### Deletion and in-flight writes

Successful deletion durably advances the session incarnation before removing
both transcript layouts. Owned handles/cache writers from an older incarnation
fail instead of recreating files. A user turn records its thread's incarnation;
that receipt survives deletion so delayed prompt recovery cannot masquerade as
a new user message. Fresh canonical deliveries must name a thread admitted in
the current incarnation. A fresh foreground user with a fresh thread ID can
reuse the session key; old completions remain refused after that reopen and
across process restart. Unthreaded system notes use the owning handle's epoch.
The private `.session-lifecycle` directory holds the generation, file lock and
thread receipts; deleting it removes the revocation guarantee. Receipts are
retained to reject arbitrarily late completions, rather than expiring by time.

The gateway removes the actor from routing while its current request can still
acknowledge its caller. Normal turn finalization appends rows without rewriting
its stale in-memory transcript over independent delivery rows. Already running
tools may finish, but their old-thread rows
cannot recreate the deleted session. Lifecycle lock acquisition is bounded to
three seconds and returns a retryable storage error; an interrupted deletion
can remain revoked and must be retried to finish removing its files. Read and
write failures of this sidecar propagate rather than assuming generation zero.
Connection cleanup keeps the session's active-turn reservation while awaiting
its worker/store, without holding the global active-turn registry lock.

The gateway also drops the pending HTTP `/chat` sender for a deleted session.
An exact topic deletion closes only that topic's stream; a base-session deletion
closes every topic and revokes each corresponding actor key, including idle
canonical and flat transcripts. Enumeration failures remain retryable errors. This ends
the original SSE response without fabricating a model result or adding a wire
event. Independent topics have separate streams; same-topic overflow keeps the
existing queued acknowledgment and stream. Only threads admitted to that stream
can deliver to it, so late output from a deleted request cannot close or write
into a fresh thread's stream. API actor replies/completions retain their exact topic and thread and bypass
the shared-chat visible-topic buffer; otherwise a freshly opened topic request
would wait for a `/s` command even after the model completed. Non-API transports
retain that buffer. Implicit topics selected by `/new` or `/s` are bound at
actual gateway routing: delivery matches the admitted thread, while precise
topic deletion matches that resolved actor scope. The request's SSE context
remains unchanged.

Admission carries a server-issued deletion generation through the inbound bus.
Before opening/enqueuing an actor the gateway checks that generation under a
per-chat dispatch lease; deletion fences only its selected scope and does no
storage I/O under the global pending-stream lock. Already queued old actor
work checks its captured durable incarnation before starting model work. A
pre-model storage error closes the admitted stream with the existing incomplete
error completion (`session_unavailable`), without a synthetic model reply.
A base deletion invalidates all earlier topic admissions; an exact topic delete
preserves unrelated topic admissions. New requests after successful deletion
can explicitly start a new incarnation. Repeated deletion and concurrent
completion remain idempotent; caller disconnect does not abandon deletion.

When canonical and flat files already coexist without a migration marker,
opening a canonical writer preserves the flat file. Coexistence cannot prove a
failed migration cleanup: the two processes may have committed independent
rows. Only the first-time migration that copied the flat data under the shared
filesystem lock removes its source; merged readers deduplicate retained copies.

Contract and completion writers strictly validate the same bounded transcript
window as ordinary readers. They do not merge sealed history outside the load
budget; legacy migration still loads the entire source before removing it.
I/O, metadata, UTF-8, schema and non-tail row errors propagate without replacing
the transcript with a partial view.

A crash-truncated JSON row at the active file's unterminated EOF is sealed before
further appends. Its original bytes remain followed by a private offset/length/hash
receipt, so a later strict writer can recognize exactly that repair after a
restart or rollover. Complete malformed rows, unsupported row schemas and UTF-8
errors are not exempt. Old already-terminated malformed rows without a receipt
still require repair; an interrupted receipt write also fails closed. The receipt
is local persistence bookkeeping, not an OUP event or an authentication boundary.
