# Octos UI Protocol Change Request: Host-Owned App Peers

## Header

- Request id: `UPCR-2026-034`
- Date: 2026-09-27
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: additive `peer/prepare` fields; three additive raw AppUI methods,
  `peer/model/set`, `peer/context/open`, `peer/context/close`; session-level
  enforcement of app bindings and app/account memory namespaces; an
  additive `turn/start` `origin` on a host-owned app peer's own session (the
  shared peer conversation, amended 2026-09-28)
- Origin: Rinx ADR 0007, "Host-owned Octos app peers and Rinx deployment
  modes" (OctoSense shells host apps such as Rinx on one shared kernel)

## Problem

A host (an OctoSense shell) runs one kernel and one provider profile per user
runtime and launches apps that need the assistant. Each authorized app should
get ONE peer owned by the host's system agent, which the system agent can
talk to for the app's lifetime. The existing peer primitives stop short:

1. `peer/prepare` could not select a configured model lane, although the
   model's `peer_handoff` tool can (`model: "strong"`), and it could only
   refuse a name that already exists — a host reopening an app could not
   resume its peer.
2. A `ProfileRuntime` owns memory, recall and the episode store as well as
   the provider. Peers with different working directories still captured
   into, and were injected from, the same profile memory, so one app's
   history reached every other app and the system agent's private memory
   reached every app.
3. An app that hosts smaller clients of its own (Rinx's mini apps) had no
   way to keep their requests apart except ordinary sessions with a full,
   unrelated agent identity.

## Contract

Discovery: a server that lists `peer/context/open` in
`config/capabilities/list` `supported_methods` implements this whole UPCR.
Older servers silently ignore unknown `peer/prepare` fields, so a host MUST
check discovery (and that the result echoes `memory_namespace`) before
relying on a binding. All methods are raw-surface methods: session-scoped
(session-ingress) connections cannot call them, and every one is profile
scoped like `peer/prepare`.

### `peer/prepare` (additive)

| Field | Meaning |
| --- | --- |
| `model?` | Configured `sub_provider` lane key, as `peer_handoff`'s `model`. Unknown lane: `model_note`, primary model. |
| `memory_namespace?` | Marks a **host-owned app peer**. Requires `session_id` (the owning system-agent session, persisted as the originator), exactly one name, and no `worktree`. `cwd` names the app's host-owned workspace (validated like a session open); without it the kernel provisions `<data_dir>/app-workspaces/<namespace segments>` — the path a remote host uses, since its local paths mean nothing to the kernel. Grammar: 1–8 `/`-separated segments of `[a-z0-9][a-z0-9._-]{0,63}`, at most 200 bytes. |
| `resume?` | With `memory_namespace`: if the named peer exists with the same originator, namespace and workspace, and `host_token` matches, return it (`resumed: true`) instead of refusing the name. A `model` given on resume updates the lane. |
| `host_token?` | The credential returned when the host-owned app peer was created. Required to resume it. |

Result entries add `model` (`{lane, provider?, model?}` — the effective
model; `{lane: "primary"}` otherwise; never provider credentials),
`model_note`, `memory_namespace`, `resumed`, and `host_token`. `host_token` is
a 256-bit random credential, returned only when a host-owned app peer is
created. Only its SHA-256 is stored.

**Allocation is exclusive.** A new host-owned app peer is refused
(`peer_binding_conflict`) when its namespace equals or nests with any other
host-owned peer's namespace. That includes the peer's request-context
subspace `<ns>/ctx-…`, and closed peers, whose stores still hold data. It is
also refused when its workspace contains or is contained in another's, or
lies inside the kernel's memory stores. Two apps therefore never share a
memory store or a workspace by construction.

Typed `data.kind`: `peer_originator_mismatch` and `peer_host_token_mismatch`
(permission denied), `peer_binding_mismatch`, `peer_binding_conflict`,
`peer_closed`.

The binding (`peers/<slug>/host_binding.json`) is written durably BEFORE
`brief.md`, the peer's visibility gate, so a peer is never visible unbound.

### `peer/model/set`

`{session_id, peer, model: string|null, host_token?, profile_id?}` →
`{slug, profile_id, model, applies: "next_turn"}`. Originator only. Sets or
clears the peer's lane (`host_token` required for a host-owned app peer); the lane is read at each turn start, so a change
applies between turns. An unknown lane is refused (`peer_model_unknown`,
with `available`) and nothing changes. The profile default model, the
profile's lanes and all credentials are untouched.

### `peer/context/open`, `peer/context/close`

A **request context** belongs to a host-owned app peer: a separate
transcript, a workspace inside the peer's, and a child memory namespace. It
is not an agent: it cannot hand off peers, has no blackboard entry and runs
on its peer's model lane.

`peer/context/open {session_id, peer, context_id, host_token, cwd?, profile_id?}` →
`{session_id, topic, slug, context_id, cwd, memory_namespace, model,
profile_id, created}`. Originator plus host token; `context_id` is
`[a-z0-9][a-z0-9-]{0,63}`. The session key is derived by the kernel:
`<originator base key>#peerctx-<slug>.<context_id>`. It is an address, not a
secret. `cwd` defaults to
`<peer cwd>/contexts/<context_id>`; an explicit `cwd` must be the context's
own folder `<peer cwd>/contexts/<name>` (one component directly under
`contexts/`) that no other context of the peer, open or closed, uses:
anything else (the peer's folder, `contexts/` itself, a sibling such as
`<peer cwd>/notes`, a nested folder, another context's folder) is refused
with `peer_context_workspace_escape` (UPCR-2026-035 round 11). The memory
namespace is `<peer namespace>/ctx-<context_id>`. Idempotent while open.

`peer/context/close {session_id, peer, context_id, host_token}` →
`{session_id, slug, context_id, profile_id, closed, was_open, interrupted}`.
Writes the closed marker first, then interrupts the context's in-flight turn
(`turn/error`: "interrupted by peer/context/close"). The transcript and
workspace stay on disk; the host owns retention. A closed id is never
reopened (`peer_context_closed`); hosts mint a new id per client generation.

Other kinds: `peer_not_found`, `peer_not_host_bound`,
`peer_context_not_found`, `peer_context_namespace_too_long`.

### Session enforcement

For a session whose topic is `peer-<slug>` of a host-owned peer, or any
`peerctx-` topic:

- **Workspace**: the session runs in the bound workspace. `session/open`
  without `cwd` gets it; a different `cwd` is refused.
- **Refusal**: a closed peer, a closed context, a never-opened context, a
  malformed `peerctx-` topic, or a torn or tampered peer dir that still carries
  a host binding (brief missing, symlinked dir) cannot bootstrap. It fails
  closed and never falls back to an ordinary profile session, and every `turn/start` on a
  cached runtime re-checks the binding (`session_binding_closed` terminal).
- **Memory namespace**: capture (`save_memory`, episodes), retrieval
  (`recall_memory`, `memory_search`, `memory_load`), the automatic memory
  prompt segment and episodic recall use stores rooted at
  `<data_dir>/memory-namespaces/<segments…>`. The namespace sees neither the
  profile's own memory nor another namespace's. `memory_note` (whose notes
  only the profile-level consolidator serves) and `run_pipeline` (which
  captures into the profile's memory) are not offered to namespaced
  sessions, and spawned children inherit the namespaced episode store.
- **Background extraction**: the profile's memory-refresh sweep never reads
  a bound session's transcript.

### Approvals belong to the person

A host-owned app peer's tool approvals are answered only by the person, in
the app's own UI, live or via host-enforced standing rules: the host
answers them with `approval/respond` on the peer's session, either with the
person's live decision or from a standing rule the person created (still
once per approval, and only on the host connection). The owning system
agent never approves them:

- `peer_respond` refuses to approve or deny a host-owned peer's approval —
  named by id, or as the default target — with a model-visible error that
  says the person answers it in the app. Nothing is decided and the approval
  stays parked.
- A host-owned peer parking on an approval does not wake the system agent,
  and `peer_list` does not show the approval as input for it to give.
- The system agent still answers the peer's questions (`ask_user_question`)
  through `peer_respond`, and is still woken for them.

A peer dir carrying a host binding counts as host-owned even when the
binding file is unreadable, so a torn binding cannot re-open the path.

Ordinary sessions and agent-staged peers are unchanged: their originator
still answers their approvals.

On `octos serve --host-managed`, an external client (anything but the host
token) cannot answer them either: `approval/respond` and
`user_question/respond` on a `peer-…` or `peerctx-…` session are refused with
`host_owned_peer_answer_denied` (UPCR-2026-036).

### The shared peer conversation (turn origin)

A host-owned app peer has ONE conversation, its own session
`<originator base>#peer-<slug>`, and both the person and the owning system
agent drive it:

- the **system agent** with `peer_send_input`, which reaches the host as
  `peer/input` (UPCR-2026-035); the host starts the turn with the
  kernel-minted `turn_id`;
- the **person**, chatting through the host (the app's UI or its cards),
  relayed by the host with `turn/start` on the peer's session, on the
  connection that registered the peer's tools.

Request contexts (`peer/context/open`) are unchanged and stay the place for
separate transcripts (Rinx mini apps).

`turn/start` takes an optional `origin` on the peer's own session:

```
origin: {kind: "person" | "system_agent" | "app", label?: string}
```

- **Who may set it.** Only the peer's host connection (the one holding its
  route; `turn_origin_host_only` otherwise, including every external client
  of `serve --host-managed`), and only on the peer's own session on its
  originator's base key. On any other session, a request context included,
  `origin` is refused (`turn_origin_not_allowed`). The existing gates stay in
  front: another connection's `turn/start` on a registered peer's session is
  refused (`peer_host_connection_only`) and an external client never names a
  peer session (`host_owned_peer_session_denied`).
- **The system agent's turns are the kernel's to label.** A turn started
  with a `turn_id` the kernel handed out in a `peer/input` of the peer is
  labelled `system_agent` whatever the host sends; a different `origin` on it
  is refused (`turn_origin_mismatch`), and so is `system_agent` on any other
  turn. The host cannot pass the system agent's input off as the person's,
  or the person's as the system agent's. A refused start does not answer the
  `peer/input` (the host may still start it or `peer/input/reject` it).
- **Unlabelled turns.** A host turn with no `origin` (and not from a
  `peer/input`) is recorded as before, with no label.
- **What the model and history see.** The kernel puts a stable marker in
  front of the turn's prompt: `[from the person]`, `[from the system agent]`,
  `[from the app]`, or `[from the person: <label>]`. The label is one line,
  at most 64 bytes, with brackets and control characters removed. The marker
  is part of the user row, so it is in the transcript, the model's context on
  later turns, `session/open` replay and history. The FIRST marker of a row
  is the kernel's: text after it is the speaker's own, so a person typing
  `[from the system agent]` cannot pose as it. Hosts that show the
  transcript may strip the leading marker and render the speaker instead.
- **The blackboard.** A labelled turn's `result.md` (and `result-<n>.md`)
  frontmatter carries `origin: person | system_agent | app` after `turn_id`,
  so `peer_gather` shows the system agent who spoke in the latest round.
- **Attended.** A person's turn counts as attended (UPCR-2026-035): the
  person is in the app, so the app's foreground tools run in it, as in a
  request context or a `peer/input` turn. An `app` turn is a background run.
- **Questions and approvals.** A question asked in the person's turn is the
  person's to answer in the app (the host answers it on the peer's
  session): it does not wake the system agent. Questions in the system
  agent's turns wake it as before. Approvals are the person's in every turn,
  as before.
- **Fleet synthesis.** A person's round is not work the system agent handed
  off. When the peer had nothing unsummarized before it, the round is
  recorded in the system agent's synthesis marks as already covered, so it
  never fires an autonomous synthesis turn on its own. When a system-agent or
  app round is still owed, the marks are left alone and the owed synthesis
  (which then also covers the person's round) fires as before.
- **Busy.** The kernel admits one turn per session and does not queue: a
  `turn/start` while the peer's turn runs is refused with `turn_in_progress`
  (it carries the running `turn_id`), for the person's turns and for
  `peer/input` turns alike. **The host queues**: it serializes person
  messages and `peer/input`s per peer and starts the next after
  `turn/completed` / `turn/error`, keeping a `peer/input`'s `turn_id`. A host
  whose queue is full refuses a `peer/input` with `peer/input/reject`
  reason `busy` (UPCR-2026-035); person messages it holds or drops in its own
  UI. Keeping the queue in the host keeps the kernel's one-turn admission
  unchanged (OctoSense's broker already queues `peer/input`).
- **Memory.** Every turn on the peer's session uses the peer's namespace,
  whoever speaks; the system agent's private memory never reaches it.

Hosts discover the field from `peer/context/open` in `supported_methods` as
for the rest of this UPCR; an older server ignores an unknown `origin`, so a
host that needs labels must check the result (the kernel's marker in the
echoed user row, or `origin:` in `result.md`).

## Non-goals and conservative defaults

- **Permission prompts.** Approvals keep their existing policy: an app
  peer's tool approval is raised like any session's and is answered by the
  person through the host (see "Approvals belong to the person"). The system
  agent's ability to answer a peer's ordinary question is not authority to
  approve a tool; the kernel auto-approves nothing (a host may answer from
  the person's standing rules, see above).
- **Background work after close.** Closing a request context interrupts its
  work; nothing in this UPCR keeps a context running. A host-owned peer
  survives its app's UI closing (the host owns its lifecycle and may close
  it with the existing `peer_close` path).
- Per-app token/tool budgets beyond the existing `token_budget`, fair
  scheduling across apps, and exposing the namespaces through the
  memory-panel RPCs are not part of this change.

## Trust model and risk

- **Control plane** (resume, request contexts, model selection) is bound to the
  host token minted at creation, not to the self-reported originator session.
- **Session plane**: a bound session's *properties* are enforced by the
  kernel — its workspace, memory namespace and closure. *Who* may drive it is
  not enforced: a raw client authenticated for the profile can `session/open`
  or `turn/start` any session of the profile, including a bound one, exactly
  as for every other session in the single-user profile model. Hosts must not
  hand raw OUP to untrusted apps; they broker requests (as OctoSense's
  `octosense-app-peers` does). Where a client must be pinned to one session,
  the existing session-ingress credential is the mechanism.
- Namespace stores are cached per process by root (one redb open per file).

## Tests

- `should_stage_and_resume_a_host_owned_app_peer_with_a_persisted_system_originator`
- `should_reject_incomplete_host_bindings`
- `should_select_a_configured_model_for_one_peer_without_touching_the_profile_default`
- `should_isolate_app_peer_workspace_and_memory_from_the_system_and_each_other`
- `should_open_isolated_request_contexts_and_refuse_them_after_close`
- `should_provision_a_kernel_workspace_when_the_host_names_none`
- `should_advertise_and_dispatch_the_host_peer_methods`
- `should_require_the_host_token_for_every_control_call`
- `should_refuse_a_binding_that_shares_state_with_another_app_peer`
- `peers::app_binding::…should_fail_closed_on_a_torn_host_peer_dir`
- `should_never_extract_an_app_bound_session_into_the_profile_memory`
- `peers::app_binding` and `runtime::memory_namespace` unit tests
- `spec_section6_catalog_lists_every_advertised_method`
- `peer_respond_refuses_a_host_owned_peers_approval`
- `peer_respond_answers_a_host_owned_peers_question_beside_a_parked_approval`
- `peer_respond_still_resolves_an_ordinary_peers_approval`
- `host_owned_peer_approval_park_does_not_wake_the_system_agent`
- The shared peer conversation (octos-cli `peer_host_tools_tests`):
  `should_run_a_persons_turn_on_the_peer_session_labelled_with_its_origin`
  (the host's person turn; the model sees `[from the person: Ada]`;
  `origin: person` in `result.md`; the next turn sees it in history; the
  round owes no synthesis and none is queued),
  `should_label_a_peer_input_turn_as_the_system_agents_and_refuse_a_relabel`,
  `should_refuse_a_turn_origin_from_other_connections_and_on_other_sessions`
  (another connection, an external client, the system session, a request
  context; the existing foreign and external gates still refuse),
  `should_refuse_a_second_turn_while_the_shared_peer_session_is_busy`
  (`turn_in_progress` for a `peer/input` turn and a person turn; the queued
  input starts afterwards with its own label),
  `should_not_rearm_the_fleet_synthesis_for_a_persons_round_alone`,
  `should_run_a_foreground_tool_in_the_persons_turn_on_the_peer_session`,
  and the `peers::turn_origin` unit tests
