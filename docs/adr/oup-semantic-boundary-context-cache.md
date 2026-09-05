# OUP Semantic-Boundary Context and Prompt-Cache Design Record

- Date: 2026-09-02
- Updated: 2026-09-05
- Status: OUP/OctosCode and chat/ACP implemented; final review, local acceptance and mini3 cloud OUP/tmux soak passed with explicit limits
- Scope: OUP (`octos serve --stdio/--ws`), OctosCode, and local chat/ACP adapters
- Octos base revision reviewed: `5ea987813de4fd2afdd1d78f2106ad2868f0d923`
- OctosCode base revision reviewed: `60376702272c41e024ebcecfbd0a580759c12363`
- Pi revision reviewed: `5cd93f688aaab89dbb6dfa4aca535f21796ae185`
- Primary research reference: [FreeToken §3.1, Semantic-Aware State Caching](https://arxiv.org/html/2608.16157v1#S3.SS1)

## Decision summary

### Follow-on authorization: full frontend convergence (2026-09-04)

The operator requested complete chat/ACP migration and removal of unused code.
This supersedes the historical deferral below. The implementation and acceptance
work is:

- [x] Local chat and ACP submit through the existing OUP dispatcher, including
  session/open, turn/start, cancellation, replay and permission/question replies.
  They do not own a second model-history vector or context manager.
- [x] Reuse profile/session bootstrap with explicit local configuration; retain
  provider routing, credentials, tools, sandbox, hooks and embedding contracts.
- [x] Select implicit iteration limits by execution intent, not transport;
  interactive turns are unlimited, explicit limits and autonomous safeguards
  remain. Convergence checkpoints still reflect and continue.
- [x] Eliminate fabricated Session Summary assistant replies in OctosCode;
  recover canonical output or expose the missing answer as a diagnostic.
- [x] Cover default/minimal feature builds, adapter protocol integration tests,
  strict lint/format checks and real multi-turn chat/ACP/OUP soak runs.
- [x] Repair the confirmed terminal-integrity and local-peer shutdown findings
  and rerun their adversarial cases on the final binary pair. Final local
  acceptance, supplemental mini3 cloud OUP/tmux soak and remaining limits are
  recorded below; this is not a historical-data migration claim.

Acceptance evidence for the earlier OUP milestone remains historical evidence,
not proof of these new adapters. Final migration acceptance is recorded separately
below; development runs that exposed defects are not counted as acceptance.

The deleted implementations include the separate chat/ACP Agent loops, ACP
history/bootstrap/replay machinery, chat-specific peer host and parking helpers,
and the orphaned chat pipeline-tool assembler. Local peers now open child OUP
sessions; profile/session bootstrap owns tool policy, project plugin discovery
(including child pipeline factories), permissions and provider assembly.

Chat/ACP use an in-process NDJSON connection to the existing OUP dispatcher,
without opening a network listener. Both require the default `api` feature;
minimal builds return an explicit unsupported-feature error instead of retaining
another execution loop. `SessionAgentFactory::build` remains an explicit bare-Agent
embedding API backed by canonical SessionRuntime; custom factories used as ACP
transports must provide `oup_state`. ACP v1 maps typed tool approvals to
`session/request_permission`; it does not negotiate the nonstandard OUP user-question
extension. Chat supplies the terminal question responder.

Interactive OUP turns default to unlimited iterations. Local CLI flags retain
their explicit `0` default; nonzero CLI limits remain effective. Autonomous turns
without an explicit configuration retain the shared finite safeguard. Reflection
checkpoints are not terminal events or permission to fabricate an answer.

The event ledger and commit observer are now runtime-owned/storage-root scoped.
Independent embedded AppStates no longer inherit the first runtime's ledger path.
This does not claim that every pre-existing process-global contract or autonomy
registry supports unrelated embedded instances reusing identical wire keys.

OUP must not treat append-only storage as sufficient for model-side cache reuse.
It has adopted the following end-to-end contract:

> Keep the durable history as an append-only semantic ledger; derive the model
> prompt as a semantic-block projection; permit deletion, truncation, and
> compaction only at declared semantic boundaries; and preserve the longest
> byte/token-identical cache-relevant input prefix within an explicit cache
> epoch.

The design separates four concerns that were previously conflated:

1. **Durability** — whether historical facts are overwritten on disk.
2. **Prompt projection** — which historical facts are visible to the next model
   call.
3. **Prefix reuse** — how much of two provider-normalized, cache-relevant input
   streams is exactly identical from the beginning.
4. **Model-state reuse** — whether a serving engine can restore full-attention
   KV and non-KV recurrent state at a surviving prefix boundary.

Append-only durability helps (1). It does not, by itself, guarantee (3) or (4).
Semantic boundaries connect the four layers without pretending that
semantically similar text can share KV state.

## Implementation outcome (2026-09-03)

The OUP milestone described by this record is implemented in the current
Octos and OctosCode worktrees. The semantic path is the default for
`octos serve --stdio/--ws`; it is not an optional client-side compactor.
OctosCode consumes OUP lifecycle state and renders diagnostics, while OUP
remains the only policy authority.

The rollout selector is:

| Value | Effective behavior |
| --- | --- |
| unset or `on` | Semantic-block compaction and stable-prefix epochs are active. This is the default. |
| `shadow` | Legacy projection remains model-visible while the semantic candidate is calculated and compared through redacted hashes/counts. |
| `off` | Legacy item-boundary compaction remains available as an operational rollback. |

Set it with `OCTOS_OUP_SEMANTIC_CONTEXT_MODE`. Automatic compaction defaults
to 70% of the provider context window and targets two thirds of that threshold
(about 46.7% of the window, before summary-budget reservation). Tests and
operators can override those values with
`OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS` and
`OCTOS_CONTEXT_COMPACT_TARGET_TOKENS`.

### Delivered work by phase

| Phase | Status | Delivered behavior |
| --- | --- | --- |
| 0 — observability | Complete | Provider-normalized, body-free input manifests; exact-prefix comparison; request/usage correlation; JSONL observer; offline diff example. |
| 1 — semantic ledger | Complete | Append-only `ledger_items`, independently selected active projection, deterministic semantic blocks, source-head validation, v1 rebuild, and atomic snapshots. |
| 2 — semantic compaction | Complete | Token-budgeted complete-block cuts, discarded-only summary input, typed tool preservation, disjoint manifests, budget outcomes, idempotence, and heuristic fallback. |
| 3 — stable prefix epochs | Complete | Stable instruction/tool prefix, volatile typed tail events, explicit epoch identity, and invalidation reasons for route/system/tools/compaction changes. |
| 4 — provider context | Complete | Provider-neutral `PromptCacheContext`; capability-gated OpenAI fields; Anthropic semantic breakpoints; normalized manifests for OpenAI, Responses, Anthropic, Gemini, and OpenRouter; usage attribution through retries/failover. |
| 5 — recurrent-state hints | Contract complete | Closed-boundary hints and restored-checkpoint reports are typed and capability-gated through provider wrappers. No bundled hosted provider claims to materialize recurrent checkpoints; a capable local engine must opt in. |
| 6 — client adoption | Shared OUP path | Negotiated `context.semantic_cache.v1`, optional lifecycle fields, and the OctosCode `/context` diagnostic pane are shipped. The follow-on above also routes chat/ACP through OUP and records their separate acceptance. |

The wire change is specified separately by
[UPCR-2026-029](../OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_029_SEMANTIC_CONTEXT_CACHE_DIAGNOSTICS.md).
The four optional fields are `cache_epoch_id`,
`last_cache_invalidation_reason`, `semantic_head_id`, and
`semantic_head_kind`. Old clients ignore them; new clients display them only
after capability negotiation and never place them in the transcript or model
prompt.

### Important implementation boundaries

- The immutable source is `ContextManager::ledger_items`; `items` is the
  current model-visible generation. Compaction appends its summary generation
  and changes the active ID selection without deleting the covered source.
- `SemanticBlock` is a deterministic derived index. It groups complete tool
  call/result batches and provides legal cuts; it is rebuilt rather than
  trusted when loading a snapshot.
- The System/tool prefix is fingerprinted separately from conversation
  blocks. Peer results, background results, goal progress, monitor events, and
  mutable usage state are typed tail data rather than System mutations.
- Provider manifests retain hashes, kinds, ordering, and normalized sizes, not
  prompt bodies. Cache availability affects performance only.
- OctosCode owns reconnect presentation and queue safety, but it does not infer
  semantic heads, rotate epochs, or choose compaction boundaries.

### Post-review amendments (2026-09-03, independent review)

An independent read-only review of the implemented worktree found defects in
the delivered milestone. The fixes below are part of the accepted record; where
they change a contract stated elsewhere in this document, this subsection wins.

Provider layer (`crates/octos-llm`):

- Every wrapper (`SwappableProvider`, `RetryProvider`, `MiddlewareStack`,
  `ContextWindowOverride`, `ProviderRouter`, `FallbackProvider`) forwards
  `provider_metadata()`/`provider_metadata_for_index()` to the lane that
  served the response, and `FallbackProvider` tags `provider_index`
  (0 = primary, `i + 1` = fallback `i`) on chat and stream responses exactly
  like `ProviderChain`. Cache usage is therefore attributed to the serving
  lane through any composition, not to the wrapper's construction-time lane.
- Observer relation chains are keyed per `(session, provider, model)` lane.
  Legitimate failover/hedge alternation inside one session no longer reads as
  `route_changed`; a genuine route switch appears as a new lane initialization
  in the observer while the authoritative `model_route_changed` reason remains
  the ContextManager epoch rotation record. Usage that matches no manifest is
  written as an explicit `usage_unmatched` row instead of being dropped, so
  manifest/usage accounting stays auditable. Manifest emission still requires
  the `octos.prompt_cache=trace` target; this is a documented contract, not an
  accident.
- Session/turn identifier hashes in observer output use schema
  `octos.prompt-cache-observer-identifier.v2`, mixed with a per-process random
  salt. Correlation is stable within a process and deliberately not comparable
  across restarts, so low-entropy channel identifiers cannot be enumerated
  offline from a captured JSONL.
- The Anthropic manifest now includes `config:thinking` and
  `config:context_management` stable segments when those request fields are
  present, because they change the provider-side cache mapping. The
  marker-wrapper collapse in `without_cache_markers` applies only to
  `system` and `messages[*].content`; a literal single-element text array
  elsewhere (for example inside a tool schema) keeps its own hash.
- The Anthropic rolling breakpoint is placed only on a user boundary with no
  outstanding `tool_use` ids anywhere before it; a trailing plain-text user
  message no longer counts as complete while a parallel tool group is open.
- The `zai`, `zai-coding`, and `r9s` registry lanes opt in to explicit
  `cache_control` breakpoints; unknown custom Anthropic-compatible base URLs
  remain off by default.
- `OpenAIProvider` keeps an explicit affinity override across builder-call
  order, treats `https://api.openai.com/v1/` (trailing slash) as official, and
  evaluates the prompt-cache kill switch per request like the Responses
  adapter. An explicit opt-in remains subject to the operator kill switch.
- Transport errors name the concrete lane, never a vendor: every adapter
  formats `failed to send [streaming] request to <label>/<model>
  (api_style=<anthropic_messages|openai_chat_completions|openai_responses|gemini_generate_content|openrouter_chat_completions>)`,
  where `<label>` is the configured provider label (for example `zai-coding`
  for an Anthropic-Messages-compatible endpoint). `FallbackProvider`,
  `ProviderChain`, and `AdaptiveRouter` wrap the returned error with a
  self-sufficient summary that lists every lane that failed with its
  `api_style`, while the typed cause chain (`LlmError`, HTTP status,
  failover classification) is preserved underneath. A K3 → Anthropic-compatible
  fallback failure therefore surfaces both lanes instead of the misleading
  string "failed to send streaming request to Anthropic".
- Operational errors follow the same rule as transport errors. Request
  serialization/build failures, response read/parse failures, empty
  `choices`/`candidates`, and HTTP-status mappings in all five adapters are
  rendered through one helper as
  `<stage> … <label>/<model> (api_style=<style>)`, and `LlmError` carries an
  optional `api_style` that its Display includes. A vendor word appears in a
  runtime message only when the configured lane really is that vendor (the
  default labels `openai`, `anthropic`, `gemini`, `openrouter`) or when it
  names a protocol dialect the request must satisfy (the Gemini structured
  schema constraint). The Responses and Vertex lanes now report
  `<provider>/<model>` instead of the former hardcoded
  `openai-responses/…` and `gemini/…` labels.

Durable UI ledger privacy (`octos_core::secret_redaction`):

- Tool-call arguments are passed through a centralized secret redactor before
  they reach the live `tool_started` notification and the durable UI protocol
  ledger. Credential-named keys (`api_key`, `token`, `authorization`,
  `password`, `*_secret`, and similar, matched after `-`/`_`/camelCase
  normalization) have their string values replaced by `[REDACTED]`;
  `Authorization: Bearer/Basic` content, env-style assignments, secret flags,
  and recognizable key formats (`sk-…`, `AKIA…`, GitHub/Slack/Google/GitLab
  tokens, JWTs, private-key blocks) are replaced inside command text and inside
  JSON embedded in strings. Ordinary arguments (paths, queries, numbers, prose
  containing "token" or "key") stay byte-identical, and redaction is
  idempotent. Existing on-disk ledger files are never rewritten; an operator
  who finds a raw credential in an older ledger must rotate that credential.
- The same redactor guards every raw durable AppUI evidence sink, not only the
  typed ledger: `append_appui_evidence_jsonl(_at)`,
  `write_appui_evidence_json(_at)`, `append_evidence_jsonl_to_dir`, the
  `artifact-index.json` merge, `append_appui_server_log(_at)` (text lines),
  and the client-writable free-text fields of the approval audit record. A raw
  client or server frame therefore cannot bypass the scrub by being persisted
  as evidence; ordinary frames remain byte-identical after the scrub.

OUP server (`crates/octos-cli/src/api`):

- Canonical ContextManager writes survive active turns. The per-turn prompt
  scratch records the canonical source watermark when it is cloned at turn
  start, and before every copy-back it adopts the durable rows the canonical
  manager merged after that watermark (`adopt_source_items_after`): a
  prompt-equivalent source-less twin already recorded by the in-flight turn is
  stamped with the durable seq instead of duplicated; any other row is copied
  at its durable-order position (after the newest stamped row whose seq is not
  greater than the incoming one, and never ahead of an installed compaction
  summary). Background, send_file, and steer merges made while a turn is
  running therefore remain in memory and in the next snapshot, and the next
  turn start no longer falls back to a full rebuild.
- Background result delivery resolves the live session manager at delivery
  time through a session-keyed registry; when no turn is live it performs
  load → merge → persist under the per-session writer lock. A sender captured
  by an earlier turn can no longer persist a stale manager over the current
  snapshot.
- Snapshot persistence has one writer discipline per session: every
  load+persist sequence takes `appui_context_persist_lock(session)`, and pure
  read paths do not persist while an in-memory live manager exists.
- Manual `session/compact` during an active turn is refused rather than
  queued: it is not applied to the on-disk snapshot behind the live bridge and
  returns an explicit `compaction_deferred_active_turn` error so the client
  can retry after the turn.
- Compaction budgets derive from the target, not the threshold:
  `summary_budget = clamp(target/3, 256, 4096)` capped below the target, and
  `semantic_target = max(target - summary_budget, target/2, 1)`. Invalid
  environment overrides (threshold below 1, target at or above the threshold)
  fall back to the derived defaults with a warning. The earlier formula
  collapsed the retention target to one token for every threshold at or below
  about 6.1K tokens, including the 6,000/2,000 soak configuration.
- Epoch invalidation reasons now include `branch_selected` (the ledger
  carries a fork boundary, durable across snapshots) and `ledger_rebuilt`
  (the loader discarded a stale or unreadable snapshot and rebuilt from
  canonical history; an append-only rebase keeps the persisted epoch and a
  first boot without a snapshot stays `initialized`). `provider_serializer_changed`,
  `security_policy_changed`, `provider_cache_policy_changed`, and
  `operator_forced` remain reserved names with no trigger site yet.
- A `GoalSnapshot` context event whose only change is a volatile counter
  (`tokens_used`, `time_used_seconds`, `continuations_used`) coalesces with the
  previous snapshot instead of appending a new model-visible item.
- An infeasible or failed automatic compaction pass is suppressed while the
  compactable candidate set is unchanged (fingerprint of the target, the
  keep-recent budget inputs, and the candidate item ids that a retry could
  summarize), so a long tool-heavy turn produces one failed record and one
  lifecycle pair, not one per iteration; a new user turn re-arms it.
- Prompt-equivalent merging searches only the durable-order window (after
  the newest stamped row whose seq is not greater than the incoming seq and
  before the first stamped row with a greater seq), so a crash-leftover
  unstamped twin ahead of that window is never stamped, while two legitimately
  identical consecutive replies still stamp in durable order.
- v2 `FileAttached.attachment_owner` uses the same phase-based assistant
  segment index as the delta/persisted envelopes, so attachments bind to the
  bubble that announced them and never to a segment id no payload carries.
- `context.semantic_cache.v1` is strictly opt-in: the stdio pre-negotiation
  defaults do not advertise it, and `first_server_slice` (the no-header
  baseline and the serde default for a decoded `SessionOpened`) excludes it
  exactly like `projection.envelope.v2`. `octos doctor`'s structural skew check
  applies the same exclusion.
- `turn_error` messages built from a typed provider error append the lane
  summary (`[lanes: …]`) when the outermost error context carries
  `api_style=`, so the client sees every failed lane for HTTP-status failures
  as well as transport failures.

Agent loop (`crates/octos-agent`):

- The convergence checkpoint reflection is re-injected each iteration as a
  User-role typed tail event
  (`<context_event kind="convergence_checkpoint" authority="background">`),
  never as a System row, and it is never persisted. The checkpoint request
  itself appends its instruction as the final User row and sends the same tool
  schemas as the action call with `tool_choice = none`, so its cache-relevant
  prefix is byte-identical to the action call instead of an empty-tools
  variant. Only the reflection text is used; tool calls on a reflection are
  dropped.
- Prompt fingerprints are position-aware: only the leading run of System rows
  forms the stable prefix; a System row that follows any non-System row is a
  conversation segment and cannot move the stable-prefix hash.
- Convergence thresholds count COMPLETED action calls and action tokens only.
  The controller increments its action counter after each successfully
  completed action call and evaluates `due()` before the next one, so a
  call-based checkpoint with interval N fires only after N completed action
  calls (its reflection runs before action call N+1), consecutive call-based
  checkpoints are exactly N completed actions apart, and the checkpoint
  message reports the true number of completed action calls. The reflection
  call and its usage never count toward the next checkpoint; token, time, and
  forced (file-churn) checkpoints and fail-open re-arming are unchanged.
- The agent-side cache affinity key is derived from the session identity
  alone, and the non-OUP fallback epoch from session plus stable-prefix hash.
  Neither includes the per-call `provider_name()/model_id()` of a dynamic
  wrapper, so router lane flaps cannot change `prompt_cache_key`. The fallback
  epoch is observability-only; route attribution comes from the manifests.
- The durable per-turn output log is never mutated after the fact: the
  tool-carrier row keeps its full text even when the final answer repeats it,
  so session JSONL, the ledger, and the prompt the model saw agree
  byte-for-byte across restart. Any presentation-level de-duplication belongs
  to a display projection, not to durable history.
- Unattended lanes (gateway and session actors) fall back to
  `UNATTENDED_MAX_ITERATIONS_FALLBACK = 50` when `gateway.max_iterations` is
  unset; `AgentConfig::default().max_iterations = 0` (unlimited) remains the
  interactive default for chat and ACP.

OctosCode client (reconnect presentation and queue safety):

- Reconnect reconciliation is connection-epoch aware. When a replacement
  stdio child connects, the transport queues a `BackendConnectionEpoch` marker
  before any frame from the new child; the store stamps every live-reply latch
  with the current epoch and, on `BackendRelaunched`, fails only latches from
  an older epoch. A continuation the new daemon resumed before the scoped
  `session/opened` is therefore never tombstoned, and a replayed
  `turn/started` for an already-terminal turn can no longer re-latch or
  re-arm the Working state.
- A definite `session/open` RPC rejection releases the scoped-affinity
  barrier, surfaces the error, flushes the deferred commands, reverts the
  reconnect reopen target to the last confirmed session, and keeps the
  connection (and stdio child) alive. Only ambiguous outcomes (mismatched
  session id, malformed success, timeout) still recycle the transport.
- A coverage record from a turn that ended in an error never de-duplicates a
  later commit through direct prefix evidence; interrupts keep ordinary
  de-duplication because they commit their streamed text.
- v1 assistant bytes are held out of immutable scrollback while the
  capability set is still unknown, so a later v2 canonical takeover cannot
  re-emit them.
- The hydration queue never evicts a `session/open` on overflow; locally
  generated `session/open` requests without a cwd are scoped to the captured
  workspace root at the transport choke point.

### Final-review amendments (2026-09-04, post-fix acceptance)

A final independent review of the worktree that already contained every
2026-09-03 amendment re-read the complete diffs of both repositories, ran a
fresh live smoke against the rebuilt binaries, and then re-ran the real
`tmux` soak. It found the defects below; each was fixed in the same worktree
with a regression test that was observed failing before its fix. Where a
statement here changes an earlier one in this record, this subsection wins.

Provider layer and epoch identity (`crates/octos-llm`, `crates/octos-cli`):

- The prompt-cache epoch compared two different lane identities. The
  pre-call reconciliation used `provider_name()` — the router label, which
  `OpenAIProvider::with_base_url` tags as `<label>@<host>` for every
  non-official endpoint (`moonshot-coding@api` for K3) — while the post-call
  observation used the serving lane's `ProviderMetadata.provider`
  (`moonshot-coding`). Every request therefore rotated the epoch twice with a
  spurious `model_route_changed`, which the live smoke showed as
  `Last invalidation: model route changed` on a fresh session with one
  route. The epoch now derives its route identity from `ProviderMetadata`
  (`prompt_cache_lane_identity`), i.e. the same `{provider, model}` the
  serving lane reports through `provider_metadata_for_index`.
- For the same reason the OpenAI-compatible and Responses manifests carried
  a label (`moonshot-coding@api`, hardcoded `openai-responses`) that never
  matched the usage rows attributed through `provider_metadata_for_index`,
  so every usage row on those lanes was written as `usage_unmatched` and the
  "correlated usage" accounting was empty for the K3 route. Manifests now
  carry `provider_metadata().provider`.
- `provider_index` was a flat, single-level slot number: a `ProviderChain`
  inside a `FallbackProvider` slot (or the reverse) reported the inner
  slot's number, which the outer composite then resolved against its own
  slot table — the wrong lane, contradicting "through any composition".
  `provider_index` is now a flat index over the LEAF lanes of the whole
  composition tree: every provider reports `provider_lane_count()` (1 for an
  adapter, the slot sum for a composite, the inner count for a wrapper);
  a composite tags a response with `lane_offset(slot) + inner_index`,
  translates a nested stream's `ProviderIndex` events into the same flat
  space, and `provider_metadata_for_index` maps a flat index back to
  `(slot, inner)` and forwards the remainder. Regressions:
  `should_resolve_serving_lane_when_fallback_wraps_provider_chain` (chat and
  stream) and `should_resolve_serving_lane_when_chain_wraps_fallback_provider`.
- `ChatConfig.tool_choice` was never serialized by any adapter, so the
  convergence reflection's `tool_choice = none` had no wire effect. All five
  adapters now emit the provider form (`"none"` / `{"type":"none"}` /
  `toolConfig.functionCallingConfig.mode = NONE`, and the `required` /
  specific-tool equivalents) only when tools are present and the choice is
  not the default, so ordinary request bodies stay byte-identical. On
  Anthropic a `tool_choice` change invalidates message-level cache entries
  (system and tools stay cached); the Anthropic manifest therefore records
  `config:tool_choice` as a stable segment, and a checkpoint reflection on an
  Anthropic-style lane costs one message-cache miss for that call. This is
  the accepted trade-off against a reflection that can call tools.

Agent loop (`crates/octos-agent`):

- The checkpoint request derived from the bare `AgentConfig` rather than the
  action call's `ChatConfig`: it dropped the tier-2 `context_management`
  payload and capped `max_tokens` to 1,024, which on Anthropic removes the
  derived `thinking` budget — a stable-segment change inside one epoch. The
  checkpoint now clones the exact action config, changes only `tool_choice`,
  and applies the output cap only when no reasoning effort is configured.
- The single budget-grace call (#1691) could be spent on a convergence
  reflection: when the grace iteration coincided with a due checkpoint the
  reflection ran, `continue`d into the exhausted budget, and the turn ended
  with the stop message instead of the deliverable. The checkpoint is now
  skipped on a grace iteration.
- A delegated child (`delegate` tool) inherited `AgentConfig::default()`
  when no worker config was set, i.e. `max_iterations = 0` (unlimited, the
  interactive default) on an unattended lane. Delegate children now fall
  back to the spawn cap (`DEFAULT_SPAWN_MAX_ITERATIONS`) when the worker
  config is unset or says unlimited.

OUP server (`crates/octos-cli/src/api`):

- A drained `turn/steer` duplicated every pre-steer row of its turn,
  durably, and — because the host persisted it at drain time — gave the
  steer a LOWER durable sequence than the prompt/answer rows the model had
  already seen. The duplicate came from the turn-end rows' durable-order
  window starting after the stamped steer, so they never found their
  in-flight twins; the ordering meant a context ledger rebuilt from session
  history (missing, stale or corrupt snapshot) showed `steer → prompt →
  answer` instead of the chronology the model saw, contradicting the
  "snapshot is a rebuildable index" contract. Both are fixed at the
  representation: a drained steer now stays in the agent's chronological
  `turn_output_log` and is persisted by the end-of-turn loop at its
  model-visible position (after every row the model had seen), so durable
  sequence order equals chronology, a rebuild reproduces it, and twins stamp
  in order. Defense in depth remains for any lower-sequence durable row
  merged into an in-flight turn: rows recorded by the in-flight turn carry a
  non-persisted `in_flight` mark and are stamped in place when the window
  search fails (a source-less conversation row never survives a snapshot
  load — see the committed-only load rule below — so no crash leftover can
  be stamped from behind a newer row), and the source-head hash orders
  stamped rows by sequence. An interim variant
  that relocated the stamped twin to the window front was rejected by the
  live soak (it moved each turn's prompt ahead of the preceding goal-snapshot
  tail row and made every turn boundary `old_history_changed`) and by the
  end-to-end ordering test. Regressions:
  `turn_steer_end_to_end_injects_before_next_llm_call_and_persists_once`
  (now also asserts the durable order, exactly-once ledger rows, and that a
  ledger rebuilt from session history alone keeps answer-before-steer),
  `should_stamp_pre_steer_turn_rows_at_turn_end_without_duplicates`.
- The scratch watermark was a scalar: two merges landing out of sequence
  order (persist seq 4, yield; persist and merge seq 5; sync; merge seq 4)
  dropped the late row from every later copy-back and snapshot.
  `adopt_source_items_after` now adopts by presence (durable sequence plus
  kind), keeping the watermark as a diagnostic only.
- A snapshot persisted mid-turn carries the turn's source-less conversation
  rows (prompt, tool calls/results, reply fragments) because every prompt
  sync copies the scratch back and persists. After a daemon crash the
  durable history never received those rows, yet the reload accepted them
  as context: coverage validation ignores source-less rows, so the ghost
  prompt/tool rows stayed model-visible and a later compaction could even
  summarize them. Snapshot loading is now committed-only: source-less
  conversation rows (`SessionLog`/`AgentLoop`/`ToolRuntime` sources, i.e.
  uncommitted work of an interrupted turn) are dropped before coverage
  validation and before any rebase, and a snapshot whose compaction
  generation already consumed such rows is discarded as a whole and the
  ledger is rebuilt from canonical history (`ledger_rebuilt`). Supervisor
  context events, compaction summaries and fork/checkpoint rows are
  source-less by design and are unaffected. This also aligns an internal
  master-continuation prompt (never persisted as a user row) with rebuild
  semantics: it is model-visible for its own turn and not resurrected from
  the snapshot afterwards. Regressions:
  `should_drop_uncommitted_conversation_rows_when_reloading_after_a_crash`,
  `should_not_retain_uncommitted_rows_when_rebasing_over_appended_history`,
  `should_rebuild_from_history_when_a_snapshot_compaction_depends_on_uncommitted_rows`.
- A snapshot persisted mid-turn carries the turn's source-less conversation
  rows (prompt, tool calls/results, reply fragments) because every prompt
  sync copies the scratch back and persists. After a daemon crash the
  durable history never received those rows, yet the reload accepted them
  as context: coverage validation ignores source-less rows, so the ghost
  prompt/tool rows stayed model-visible and a later compaction could even
  summarize them. Snapshot loading is now committed-only: source-less
  conversation rows (`SessionLog`/`AgentLoop`/`ToolRuntime` sources, i.e.
  uncommitted work of an interrupted turn) are dropped before coverage
  validation and before any rebase, and a snapshot whose compaction
  generation already consumed such rows is discarded as a whole and the
  ledger is rebuilt from canonical history (`ledger_rebuilt`). Supervisor
  context events, compaction summaries and fork/checkpoint rows are
  source-less by design and are unaffected. This also aligns an internal
  master-continuation prompt (never persisted as a user row) with rebuild
  semantics: it is model-visible for its own turn and not resurrected from
  the snapshot afterwards. Regressions:
  `should_drop_uncommitted_conversation_rows_when_reloading_after_a_crash`,
  `should_not_retain_uncommitted_rows_when_rebasing_over_appended_history`,
  `should_rebuild_from_history_when_a_snapshot_compaction_depends_on_uncommitted_rows`.
- Turn start, `session/open`, hydrate/inspection reads and manual compaction
  loaded, compacted and persisted the snapshot without holding the
  per-session writer lock, and the turn path registered its live manager
  only after releasing it, so an idle background merge could land between
  the load and the persist and be overwritten by the first copy-back (the
  next turn then rebuilt the ledger and lost every compaction generation).
  All four paths now hold `appui_context_persist_lock(session)` across
  load → compact → persist, the turn path registers its live manager under
  the same lock, and the manual-compact live-turn check shares that critical
  section. Lock order stays manager → persist: read paths release the writer
  lock before touching a live manager.

Durable UI ledger privacy (`octos_core::secret_redaction`):

- The scrub was keyed on `tool_started`/`ToolStart` only. `approval/requested`
  repeated the raw shell command in `body` (`Run command: …`) and in the typed
  `command_line`; `approval/decided` kept the client-writable `client_note`
  and `scope` raw in the UI ledger (only the audit file was scrubbed); and
  tool RESULTS (`tool/completed.output_preview`, the v1/v2 `ToolEnd`
  `output_preview`/`error`) persisted the first 2 KB of whatever a tool
  printed (`.env`, `printenv`). One shared scrub
  (`redact_ui_notification_secrets`) now runs at the wire boundary
  (`send_notification_durable`) and at ledger append for all of these
  shapes, so no notification producer can bypass it.
- Redactor coverage: bracketed unquoted values (`token=[…]`, `password={…}`,
  `secret=(…)`) previously scanned as an empty, exempt value and passed
  through; `curl -u user:password` / `--user` / `--proxy-user` were not
  credential keys. Both are scrubbed now (the user-name half of a basic-auth
  pair stays visible). Two false positives were removed: an identifier
  immediately followed by a call (`let token = tokenizer.next();`) and the
  literals `None`/`nil`/`undefined` under a credential-named key stay
  byte-identical. Unresolved and documented: prose of the form
  `the secret: nobody knows` is still redacted (fail closed), `foreign_key`/
  `primary_key`-style suffix matches, bare `Basic <b64>` outside an
  `Authorization` header, and `{"name": "DB_PASSWORD", "value": …}` pairs.

OctosCode client (stdio reconnect):

- The new `client_hello` (3 s) and scoped `session/open` (10 s) barriers
  measured from spawn, while a real `octos serve --stdio` cold start takes
  10–15 s on this host (profile store ~6 s, plugin verification ~6 s). The
  live smoke reproduced the consequence: every cold launch reported
  "negotiation timed out; using legacy server defaults", the first restart
  after a daemon kill recycled a child that was one second from serving, and
  the prompt typed during the restart never reached any daemon (`S04 …
  seen=01,02,04`). Barriers now use a startup grace
  (`STDIO_CHILD_STARTUP_GRACE`, 90 s) until the child's first frame, then
  restart their clocks with the steady-state deadlines; a child that exits
  during bootstrap still fails fast through the reader.
- The prompt itself was lost on the store side: a prompt submitted directly
  from the composer to an idle session never armed the staged-submit gate
  (only the staged drain did), so the transport's explicit
  `request_cancelled` had nothing to re-stage and the optimistic bubble
  stayed in the transcript with no turn behind it. Direct submits now arm
  the same in-flight gate; a cancelled or refused `turn/start` is re-staged
  and resubmitted exactly once after the replacement child's scoped open
  (regression: `cancelled_scoped_submit_is_restaged_and_resubmitted_exactly_once_after_relaunch`,
  and the real-child `prompt_submitted_during_slow_child_bootstrap_reaches_the_child_exactly_once`).
- A command deferred behind a dead child's barriers used to survive
  `mark_disconnected` and was flushed to the NEXT child after its scoped
  open, on top of the store's own relaunch re-stage — a duplicate
  `turn/start`. Deferred commands are now cancelled explicitly on disconnect.
- The staged-submit staleness backstop distinguishes an in-flight gate
  (120 s, above the startup grace, so a prompt legitimately deferred behind
  a booting child is not resubmitted while it waits) from a backoff-only
  gate (10 s retry cadence, unchanged).
- Two local session switches inside one input batch queued their opens in
  reverse order (`[Open(C), Open(B)]`) and bypassed the hydration-queue cap;
  only the latest local open is now queued, at the front, through a bounded
  path.

Reviewed and left as documented limitations (not fixed in this pass):

- Anthropic prompt caching defaults off for any non-official base URL with
  no configuration knob to opt a corporate proxy back in; the `zai`,
  `zai-coding` and `r9s` registry lanes opt in.
- `AdaptiveRouter::api_style()` and `provider_name()` consult the selector
  and can log a lane change; no production caller today.
- `ledger_rebuilt` is an in-memory flag: a rebuild performed by a
  hydrate/inspection read persists a covering snapshot, and the next turn
  reports `initialized`. The in-loop `OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS`
  path applies no clamp/warning.
- Sub-agent affinity keys: spawn/delegate workers hash `"anonymous"`, so all
  sub-agents in a process share one `prompt_cache_key` (routing efficiency
  only). Checkpoint bodies consume iteration indices (a cap of M yields
  M − ⌊M/(N+1)⌋ action calls). Any User row that mimics the checkpoint
  envelope is stripped from the prompt.
- OctosCode: a `HydrateSession` becomes the confirmed reopen target at send
  time; the connection-epoch marker can in principle be outrun by a frame
  read inside the same reconnecting poll (microseconds); commands deferred
  for a session whose `session/open` was definitively rejected are still
  flushed.
- The OUP `turn/start` agent runs with the unattended fallback of 50
  iterations when `gateway.max_iterations` is unset even though OctosCode
  can interrupt a turn; an explicit `0` disables the backstop silently.

## Scope and routing decision

The canonical execution path used by all three frontends is:

```text
OctosCode / local chat / ACP bridge
  -> OUP connection (stdio/WebSocket or in-process NDJSON)
  -> session/open
  -> turn/start
  -> AppUiPromptContextBridge
  -> Agent loop
  -> LLM provider
```

Chat and ACP are presentation/transport adapters, not context authorities.
Their old execution paths have been removed; neither owns a second
ContextManager or model-history vector.

## Terminology

### Append-only ledger

An immutable history of events or blocks. Corrections, branches, compactions,
and removals are represented by new entries; old entries remain recoverable.

### Exact prefix

The leading serialized bytes/tokens shared by two cache-relevant model inputs
after provider normalization. This is not necessarily a literal prefix of the
whole HTTP JSON body: a JSON array closes differently when another message is
appended, and fields unrelated to prompt caching may follow it. Provider
adapters must fingerprint the ordered System/instruction, tool-schema, and
message/content stream in the same semantic order used by that provider.
Provider prompt caches and radix KV caches reuse exact model-input prefixes,
not text that is merely semantically equivalent.

### Semantic boundary

A stable boundary around an agent operation that is normally edited as one
unit: a conversation turn, reasoning segment, complete tool interaction,
background result, peer result, or compaction generation.

Semantic boundaries do not make changed tokens cache-equivalent. They place
edits and checkpoints where the unchanged prefix is most likely to survive.

### Cache epoch

A maximal interval in which the stable request prefix contract remains valid.
Changing the model route, provider serialization, stable system instructions,
tool schemas, security policy, or installed compaction summary starts a new
epoch. Adding ordinary semantic blocks does not.

### Full-attention KV versus recurrent state

Full-attention KV can be reused up to the longest exact prefix, commonly via a
radix tree. Hybrid models may also carry a recurrent state that represents the
entire prefix and cannot be partially reused. That state needs explicit
checkpoints. OUP can preserve and describe useful boundaries; only a capable
serving engine can materialize the recurrent-state checkpoint itself.

## What FreeToken establishes

FreeToken does not replace exact prefix matching with semantic matching. Its
state reuse has two layers:

1. Full-attention KV uses a radix prefix tree.
2. A small pool of recurrent-state checkpoints is attached to selected nodes
   in that prefix tree.

Because recurrent checkpoints are expensive, FreeToken places them at special
token boundaries that delimit thinking segments, tool calls and outputs, and
conversation turns. Agent harnesses usually delete or replace those complete
blocks. The prefix before the edited block therefore survives exactly, and the
engine restores the deepest surviving checkpoint and re-prefills only the new
suffix.

The lesson for OUP is not “understand two texts as similar.” It is:

- retain an exact prefix whenever possible;
- edit complete semantic blocks rather than arbitrary message/item counts;
- make high-value boundaries explicit to provider adapters and capable local
  serving engines;
- spend a limited checkpoint/cache-breakpoint budget where edits are likely.

FreeToken's semantic expert cache and bandwidth-adaptive MoE execution are
separate mechanisms and are not part of this OUP context design.

## Reviewed implementation map

The comparison in this record is based on these concrete implementation
surfaces rather than README-level descriptions.

### OUP and Octos

- `crates/octos-cli/src/api/context_manager.rs`
  - `TranscriptItemKind`
  - `record_item_with_source_ref`
  - `record_tool_output_with_source_ref`
  - `for_prompt`
  - `compact_context`
  - `load_or_rebuild_context_manager`
- `crates/octos-cli/src/api/ui_protocol_transport.rs`
  - `AppUiPromptContextBridge`
  - OUP turn-start context loading and per-turn Agent construction
  - peer, goal-progress, monitor, and active-goal prompt injection
  - compaction lifecycle and final response persistence
- `crates/octos-agent/src/compaction.rs`
  - heuristic and LLM summary input rendering
- `crates/octos-agent/src/agent/compaction.rs`
  - prompt-context-manager early return from legacy tier-1 compaction
- `crates/octos-llm/src/config.rs`
  - shared `ChatConfig`
- `crates/octos-llm/src/openai.rs`, `anthropic.rs`, and `gemini.rs`
  - final provider request shape and cache usage parsing
- `specs/kv-cache-friendly-compaction.spec.md`
  - the earlier cache-friendly work and its explicit exclusion of the OUP
    ContextManager/AppUI path
- OctosCode `src/cli.rs`
  - default backend command `octos serve --stdio --solo`

### Pi

- `packages/coding-agent/src/core/session-manager.ts`
  - append-only tree, active leaf, compaction entry, and context projection
- `packages/coding-agent/src/core/compaction/compaction.ts`
  - token/turn-aware compaction preparation and split-turn handling
- `packages/coding-agent/src/core/sdk.ts`
  - stable session ID propagation
- `packages/ai/src/api/openai-responses.ts`
  - OpenAI prompt-cache key and retention
- `packages/ai/src/api/anthropic-messages.ts`
  - Anthropic rolling cache-control placement

## Comparison

| Dimension | Pi | FreeToken | OUP baseline before this ADR | Implemented OUP |
| --- | --- | --- | --- | --- |
| Durable history | Append-only JSONL tree; entries are immutable | Serving runtime, not an agent transcript authority | Canonical session history is append-oriented, but the ContextManager persists a replaceable JSON snapshot | Append-only semantic block/event ledger plus rebuildable snapshot/index |
| Branching | Parent-linked entries and an active leaf | Radix prefix tree over token prefixes | Session/thread identity exists; active ContextManager items are a flat vector | Parent-linked semantic blocks with an explicit active head |
| Prompt construction | Resolves active path; latest compaction summary plus retained entries | Restores deepest surviving prefix node | Projects flat `TranscriptItem`s | Projects complete semantic blocks from the active path |
| Between-call growth | Normally appends messages | Adds a token suffix | Mostly append-like inside one OUP turn | Exact serialized prefix invariant inside an epoch |
| System prompt stability | Stable by default; extensions may intentionally alter it | Stable prefix is required for state reuse | Rebuilt per turn; memory, peer, goal, monitor, and usage data may change it | Stable instruction block per epoch; volatile data becomes tail context blocks |
| Tool schema stability | Stable tool list is part of the request prefix | Not the focus | Tool ordering is intended to be deterministic, but there is no epoch/hash contract | Canonical tool-schema hash; a tool-set change rotates the epoch |
| Compaction representation | Appends a `CompactionEntry` with `firstKeptEntryId`; old entries remain | Not an agent summarizer | Replaces active `items` with a summary plus retained items | Appends a compaction generation referencing an exact discarded block set and first retained block |
| Compaction cut | Token/turn-aware; handles a split-turn prefix separately | Checkpoints at semantic special-token boundaries | `keep_recent_items`, currently item-count based | Token-budgeted selection over complete semantic blocks |
| Summary input | Discarded prefix, plus explicit handling for split-turn prefix | N/A | Baseline OUP callers summarized the full projected prompt before retaining a tail | Only the disjoint discarded block set |
| Tool structure in summaries | Serializer preserves tool and reasoning structure | Special tokens expose tool/thinking boundaries | LLM summary rendering primarily uses role/content and can lose tool-call arguments | Typed semantic serialization including tool names, arguments, results, reasoning class, and artifact refs |
| Provider cache identity | Stable `sessionId`; OpenAI cache key/retention and provider affinity; Anthropic breakpoints | Engine-owned radix/checkpoint keys | Anthropic breakpoints exist; OpenAI `ChatConfig` has no session/cache identity | Stable session key + cache epoch + retention/capability policy |
| Semantic checkpoint hints | No engine checkpoint placement contract | Core mechanism | None | Optional internal boundary hints for capable local providers |
| Cache observability | Provider-specific usage and tests | TTFT/re-prefill evaluation | Cache-read usage is parsed for several providers | Eligible-prefix, reused-token, epoch, and invalidation-reason telemetry |

## Pi: useful properties and limits

Pi's `SessionManager` is an append-only tree stored in JSONL. Each entry has an
ID and parent ID; branching moves the active leaf instead of rewriting history.
`buildSessionContext()` resolves the active path.

Compaction is also appended. A `CompactionEntry` records a summary and
`firstKeptEntryId`; prompt construction emits the latest summary, the retained
entries beginning at that ID, and entries appended afterward. The source
history remains available.

Pi also carries cache identity through the provider layer:

- the agent receives the stable session ID;
- OpenAI Responses requests can include `prompt_cache_key` and cache retention;
- Anthropic requests can place cache-control markers on the system prompt,
  tool definitions, and the rolling conversation boundary;
- provider-specific session-affinity headers are supported.

These mechanisms improve routing and cache retention but do not remove the
exact-prefix requirement. A modified system prompt or deep historical rewrite
still invalidates the prefix from its first changed token. Pi extensions can
also intentionally modify prompts, so append-only is a default discipline, not
a proof that every extension preserves cacheability.

## Pre-implementation OUP behavior (baseline)

### What is already strong

- `ContextManager` preserves typed items for user input, assistant output,
  reasoning, tool calls, tool outputs, context injections, child summaries,
  checkpoints, forks, and compactions.
- New transcript items are ordinarily appended with stable item IDs.
- Tool outputs retain content-addressed evidence and can expose a bounded
  model-visible projection without losing the raw artifact.
- `AppUiPromptContextBridge` records and projects context before each model
  call.
- The runtime System message is captured once at OUP `TurnStart`, preventing
  repeated summary concatenation during later iterations of the same turn.
- Anthropic prompt caching is enabled by default and marks the system prompt,
  last tool definition, and rolling last user/tool-result boundary.
- Context generation, transcript hashes, checkpoints, compaction records, and
  lifecycle notifications already provide a foundation for migration.

### Why “ContextManager is append-only” is incomplete

The old KV-cache-friendly specification intentionally left the
`octos-cli`/AppUI ContextManager path unchanged and treated observed append-like
behavior as sufficient. That conclusion only covered message-history behavior
inside the examined loop. It did not compare provider-normalized,
cache-relevant input streams across OUP turns.

OUP currently rebuilds a per-turn Agent and refreshes its system-prompt
segments. It can append the following volatile data to the first System
message:

- newly available peer results;
- read-and-clear goal-progress notes;
- read-and-clear monitor events;
- the active goal and its changing `tokens_used/token_budget` values;
- refreshed memory/prompt segments.

Any change near the beginning of the request invalidates the entire following
prefix even when every conversation item was appended correctly.

Furthermore, a ContextManager compaction assigns a replacement vector to
`self.items`. This is acceptable for a projection snapshot, but it is not an
append-only semantic ledger and cannot by itself represent branches or explain
which exact blocks a summary replaced.

## Gap and risk register

### G1 — Volatile first System message (P0 performance, P1 semantics)

Changing peer, monitor, goal-progress, memory, or token-usage text in the first
System message destroys the reusable prefix from token zero. Treating
untrusted peer/tool data as System content also gives data a stronger
instruction role than necessary.

### G2 — Summary/retained-tail overlap (P0 correctness)

The OUP compaction caller currently generates its summary from the entire
pre-compaction prompt and then asks ContextManager to retain recent items. The
result has the logical shape:

```text
summary(discarded + retained) + retained
```

The retained facts occur twice. This can cause repetitive answers, stale-plan
resurrection, inconsistent tool state, and unnecessary post-compaction tokens.

The required shape is:

```text
summary(discarded complete blocks) + retained complete blocks
```

with an enforced empty intersection between the two source block sets.

### G3 — Item-count cuts are not semantic cuts (P0 correctness)

`keep_recent_items` can split a conversation turn or a parallel tool-call
group. It can retain a result without enough call context, retain a call whose
result was summarized, or move the current instruction into background
summary text.

### G4 — Lossy compaction serialization (P1 correctness)

The LLM compaction renderer emphasizes role/content. Tool calls carry important
structure outside ordinary message content: call ID, name, arguments,
result/artifact relation, and sometimes reasoning metadata. A summarizer must
receive a typed rendering or it cannot reliably preserve unfinished work and
tool effects.

### G5 — Missing cross-provider cache identity (P1 performance)

Anthropic receives explicit cache-control markers, but the shared `ChatConfig`
does not carry a stable session cache key, cache epoch, retention preference,
or invalidation reason. The OpenAI-compatible request has no equivalent of
Pi's `prompt_cache_key`; Gemini currently supplies no explicit cached-content
resource.

### G6 — No serialized-prefix invariant (P1 performance)

Existing tests validate ContextManager state and prompt content. They do not
prove that two actual requests emitted by each provider adapter share the
expected cache-relevant input prefix. Provider normalization can invalidate a
cache even when the source `Message` vector looks unchanged. Comparing whole
HTTP JSON bodies would also be wrong; tests need the provider-normalized prompt
element/token stream or boundary-level wire hashes.

### G7 — Snapshot freshness can be misclassified (P1 correctness)

Context snapshot coverage is checked using a high watermark against the
provided history length. The OUP turn-start path may load only a bounded tail.
A stale snapshot with a high source sequence can therefore appear current.
Background assistant persistence also has paths that invalidate caches without
merging the new item into ContextManager immediately.

Semantic ledger migration must fix source-head identity, not only item shape.

### G8 — No boundary/checkpoint contract for local engines (P2 performance)

OUP does not expose semantic boundary IDs, prefix hashes, recompute lengths, or
checkpoint priorities to an engine capable of preserving recurrent state.
Hosted APIs may ignore such information; a FreeToken-class local engine can use
it.

## Implemented architecture

```text
                  append-only
Canonical session events ---------> Semantic Context Ledger
                                        |
                                        | resolve active head
                                        v
                                Semantic block path
                                        |
                         policy / budget / capabilities
                                        |
                                        v
                              Prompt Projection Plan
                         /              |              \
                        v               v               v
                 OpenAI adapter   Anthropic adapter   Local engine adapter
                 cache key/epoch  cache breakpoints   KV + recurrent hints
                        \               |               /
                         +------- cache telemetry -----+
```

The ledger is the durable source of truth. A snapshot is a rebuildable index,
not the sole copy of the active history. The projection is allowed to omit or
summarize blocks, but it records exactly what it did and never mutates the
source blocks.

## Semantic ledger model

### Block identity

Introduce an internal `SemanticBlock` with at least:

```rust
struct SemanticBlock {
    id: SemanticBlockId,
    parent_id: Option<SemanticBlockId>,
    turn_id: Option<String>,
    group_id: Option<String>,
    kind: SemanticBlockKind,
    source_refs: Vec<TranscriptSourceRef>,
    content_hash: String,
    prefix_hash_after: String,
    estimated_tokens: usize,
    instruction_authority: InstructionAuthority,
    stability: BlockStability,
    checkpoint_eligibility: CheckpointEligibility,
}
```

`content_hash` covers the canonical semantic content. `prefix_hash_after`
commits to the active path through this block. Provider cache-input segment
hashes are tracked separately because different adapters serialize the same
semantics differently.

### Required block kinds

| Block kind | Atomic contents | Boundary after block | Default authority |
| --- | --- | --- | --- |
| `StableInstructions` | Base system/developer policy and stable workspace instructions | Yes; epoch root | System/developer |
| `UserTurn` | User content and media references | Yes | User |
| `AssistantReasoning` | One complete retained reasoning/thinking segment | Yes when separately retained | Assistant/background |
| `AssistantFinal` | Assistant response without tool calls | Yes | Assistant |
| `ToolInteraction` | One assistant tool-call batch plus all completed/aborted results | Yes only when group is closed | Assistant + tool |
| `ContextEvent` | Memory update, goal snapshot, monitor event, or injected runtime fact | Yes | Typed data, not System by default |
| `PeerResult` | Bounded peer capsule and artifact references | Yes | Untrusted data/background |
| `BackgroundResult` | Bounded task capsule and artifact references | Yes | Untrusted data/background |
| `CompactionGeneration` | Summary, discarded IDs, first retained ID, policy and hashes | Yes; starts new epoch | Background context |
| `BranchBoundary` | Parent head, sanitizer and fork policy | Yes | Metadata |

Parallel tool calls belong to one `ToolInteraction` group until every call has
a terminal result. An interrupted call receives an explicit synthetic aborted
result so the group can close deterministically.

### Append-only mutation rules

- New user/model/tool events append blocks or extend only an explicitly open
  block builder that has not yet been committed.
- Once committed, a block is immutable.
- Deletion or truncation appends a projection/tombstone event referencing whole
  block IDs; it does not alter the source block.
- Branching appends a branch boundary with a parent head.
- Compaction appends a new generation; it does not erase the summarized blocks.
- Raw tool artifacts remain content-addressed and out of band.
- Snapshots may be atomically replaced because they are derived indexes. Their
  schema and source-head hash must prove which ledger prefix they cover.

## Prompt cache epoch

### Epoch identity

Define an internal `PromptCacheEpoch` whose identity is a hash of:

```text
provider route + model ID
+ provider serializer/version
+ stable instructions hash
+ ordered tool-schema hash
+ security/policy hash
+ active compaction generation
+ model-relevant modality/config shape
```

Sampling temperature and output-token limits do not necessarily change input
KV, but provider routing behavior differs. The provider capability table must
declare whether each request field participates in its cache identity.

### Intentional invalidation reasons

Every epoch rotation records one reason:

- `model_route_changed`
- `provider_serializer_changed`
- `stable_instructions_changed`
- `tool_schema_changed`
- `security_policy_changed`
- `compaction_installed`
- `branch_selected`
- `provider_cache_policy_changed`
- `operator_forced`

Ordinary user turns, tool interactions, peer results, and monitor events append
inside the current epoch and do not rotate it.

### Stable versus volatile prompt content

The first System/developer block contains only instructions that are expected
to remain stable for the epoch. Volatile state moves to typed tail blocks:

- The behavior “use `goal_update` when complete” remains a stable instruction.
- The goal objective is user-provided data in a goal context block.
- `tokens_used`, progress notes, and task status are volatile context blocks.
- Peer and background-agent results are untrusted result blocks.
- Monitor output is an external event block.
- A memory-bank update is appended as a memory context block unless it changes
  actual governing instructions; the latter intentionally rotates the epoch.

This preserves instruction priority without promoting untrusted data into the
System message.

## Semantic compaction algorithm

### Selection

1. Resolve the active semantic block path.
2. Calculate provider-aware token estimates for complete blocks.
3. Pin the stable instruction block, newest user instruction, open tool group,
   active goal identity, and any other policy-protected block.
4. Choose the oldest-to-newest discarded prefix whose cut lands on a legal
   semantic boundary and leaves the required output/headroom budget.
5. If a single tool result is too large, keep its bounded envelope and artifact
   reference; do not split arbitrary text or the call/result relation.
6. Never split the current user turn or an open parallel tool group.

### Summary input

Serialize only discarded blocks. The typed compaction input includes:

- block kind and turn/group identity;
- user instructions and their temporal order;
- assistant conclusions, not hidden reasoning verbatim unless policy permits;
- tool name, canonical arguments, terminal result, side effects, and artifact
  refs;
- files modified/read and unresolved errors;
- active plan/goal state at the cut;
- explicit boundary between discarded background and retained current work.

The retained block set is never included in the summary request.

### Installation

The compaction result records:

```text
summary
discarded_block_ids
first_retained_block_id
input_head_id + input_prefix_hash
summary_input_hash
summary_output_hash
token_estimate_before/after
policy_id
model/provider or heuristic generator
```

Before commit, enforce:

- discarded and retained IDs are disjoint;
- every discarded block is covered exactly once;
- retained blocks preserve their order and hashes;
- no tool interaction is divided across the cut;
- the newest user instruction remains raw;
- projected tokens after compaction are below the target, not merely below the
  model's absolute maximum;
- repeated installation with the same input/policy is idempotent.

Installing the compaction starts a new cache epoch. Subsequent calls append to
the new `stable instructions + summary + retained tail` prefix.

## Provider integration

### Shared request contract

Extend the internal LLM request configuration with a provider-neutral cache
context, for example:

```rust
struct PromptCacheContext {
    session_affinity_key: String,
    epoch_id: String,
    retention: CacheRetention,
    stable_prefix_hash: String,
    last_boundary: Option<SemanticBoundaryHint>,
}
```

This is internal model-call metadata, not sampling parameters, and must not be
blindly forwarded to providers that do not support it.

### OpenAI-compatible adapters

- Send a bounded stable session-affinity key as `prompt_cache_key` where
  supported by the
  [OpenAI Responses request contract](https://developers.openai.com/api/reference/cli/resources/responses/methods/create).
  Keep `epoch_id` separate for validation and telemetry: changing
  an affinity key on every compaction/branch can unnecessarily route the
  request away from a server that still owns a reusable earlier prefix.
- Let an explicit provider capability/policy decide whether the epoch is part
  of its key; do not assume one policy fits every OpenAI-compatible endpoint.
- Send retention/session-affinity fields only under explicit provider
  capabilities.
- Preserve canonical message and tool ordering.
- Continue parsing `cached_tokens`, but associate usage with epoch and boundary
  IDs.
- For Kimi/DeepSeek-compatible endpoints without explicit keys, retain the
  exact-prefix invariant and do not invent unsupported request fields.

### Anthropic adapter

- Keep the stable System and ordered-tools breakpoints.
- Move the rolling conversation breakpoint to the latest complete semantic
  boundary that can legally carry a marker in Anthropic's wire schema, not
  merely whichever user-role message happens to be last.
- Use the limited breakpoint budget deliberately: stable instructions, tools,
  prior compacted generation, and newest high-value completed boundary.
- Make TTL/retention capability-driven.

### Gemini adapter

- Continue automatic prefix reuse where available.
- Add explicit cached-content resources only behind a capability and lifecycle
  policy; creation/deletion must not become a correctness dependency.

### Local serving engines

Define an optional capability for semantic checkpoint hints:

```text
boundary_id
boundary_kind
provider_wire_prefix_hash
prefix_token_count
estimated_recompute_tokens
checkpoint_priority
```

A capable engine may attach full-attention and recurrent-state checkpoints to
these boundaries. An incapable provider ignores the hints. OUP never assumes a
checkpoint exists unless the provider reports it.

## OUP protocol and observability

The first implementation can remain server-internal, but OUP lifecycle state
should eventually expose backward-compatible optional fields under a negotiated
feature such as `context.semantic_boundaries.v1`:

```text
context_generation
semantic_head_id
cache_epoch_id
last_boundary_id/kind
last_compaction_id
eligible_prefix_tokens
cache_read_tokens
cache_write_tokens
prefix_reuse_ratio
last_invalidation_reason
```

Do not expose raw hidden reasoning or full peer/tool payloads in telemetry.
OctosCode should display a compact cache/context diagnostic view; it does not
own compaction or boundary decisions.

## Implementation plan and disposition

The phase descriptions below are retained as the implementation contract.
Their disposition reflects the verified worktree on 2026-09-03; “complete” is
scoped to OUP and OctosCode, not the deferred legacy frontend routing.

### Phase 0 — Baseline and invariant instrumentation — complete

Goal: measure the current OUP behavior without changing prompts.

Changes:

- Add a provider-neutral canonical prompt fingerprint plus ordered,
  boundary-level hashes over stable System, tools, and projected messages.
- Have each provider adapter report hashes and lengths for its cache-relevant
  normalized input segments. Use those manifests to calculate the longest
  common prefix without retaining prompt bodies.
- Record invalidation source when System, tools, model route, or an old history
  item changes.
- Correlate provider cache-read/write usage with OUP session and turn IDs.
- Add an offline analyzer for captured, redacted request manifests.

Primary files:

- `crates/octos-agent/src/agent/llm_call.rs`
- `crates/octos-llm/src/config.rs`
- provider request builders under `crates/octos-llm/src/`
- `crates/octos-cli/src/api/ui_protocol_transport.rs`

Exit criteria:

- A multi-turn OctosCode run identifies every prefix invalidation reason.
- No prompt bodies, secrets, or hidden reasoning are written to diagnostics;
  only hashes, counts, roles, and boundary metadata are retained.

### Phase 1 — Semantic blocks and shadow mode — complete

Goal: derive semantic blocks while preserving the existing prompt byte-for-byte.

Changes:

- Add semantic block/group IDs and boundary metadata to ContextManager state.
- Group parallel tool calls and their results into closed interactions.
- Derive user-turn, assistant, peer, background, and context-event blocks.
- Persist a schema-v2 snapshot with the canonical session source-head hash.
- Load v1 snapshots by rebuilding semantic metadata from canonical session
  history; never fabricate coverage from a bounded tail.
- Compare legacy and semantic projections in tests and optional shadow logs.

Primary file:

- `crates/octos-cli/src/api/context_manager.rs`

Exit criteria:

- Semantic shadow projection is content-equivalent to legacy projection for
  sessions without compaction.
- Every committed tool-call group is complete or explicitly aborted.
- Restart/hydration reconstructs the same semantic head and hashes.

### Phase 2 — Correct semantic compaction — complete

Goal: remove the correctness defects before optimizing provider caching.

Changes:

- Replace `keep_recent_items` selection with token-budgeted semantic-block
  selection.
- Generate summaries from discarded blocks only.
- Use typed compaction serialization that preserves tool structure and
  artifact references.
- Enforce post-compaction budget, disjoint coverage, current-instruction, and
  tool-group invariants.
- Append compaction generations to the semantic ledger and retain existing raw
  history.
- Keep heuristic fallback, but feed it the same discarded block set.

Primary files:

- `crates/octos-cli/src/api/context_manager.rs`
- `crates/octos-cli/src/api/ui_protocol_transport.rs`
- `crates/octos-agent/src/compaction.rs`

Exit criteria:

- No `summary(A+B) + raw(B)` projection is possible.
- No compaction splits a complete turn/tool interaction.
- A second immediate compaction is unnecessary unless new input crossed the
  threshold.

### Phase 3 — Stable OUP prefix epochs — complete

Goal: make cross-turn OUP requests append-only inside a semantic epoch.

Changes:

- Separate stable system/developer instructions from volatile context.
- Convert peer results, goal progress, monitor events, memory updates, and
  changing usage counters into typed tail blocks.
- Freeze stable System and ordered tool definitions for the epoch.
- Introduce explicit epoch rotation with reason codes.
- Ensure per-turn Agent reconstruction cannot silently mutate the stable
  prefix.

Primary files:

- `crates/octos-cli/src/api/ui_protocol_transport.rs`
- Agent prompt-segment assembly under `crates/octos-agent/src/`

Exit criteria:

- With unchanged model/tools/policy and no compaction, request N's eligible
  serialized prefix is exactly preserved in request N+1.
- Peer/background/monitor delivery changes only the tail.
- A genuine instruction/tool change rotates the epoch rather than masquerading
  as an ordinary append.

### Phase 4 — Provider cache context — complete

Goal: make the prefix contract actionable across providers.

Changes:

- Add `PromptCacheContext` to `ChatConfig` or the internal call envelope.
- Implement OpenAI cache key/retention and capability-gated affinity.
- Make Anthropic breakpoint selection semantic-boundary aware.
- Define Gemini cached-content policy where supported.
- Add capability declarations and strict omission for incompatible endpoints.

Primary files:

- `crates/octos-llm/src/config.rs`
- `crates/octos-llm/src/openai.rs`
- `crates/octos-llm/src/anthropic.rs`
- `crates/octos-llm/src/gemini.rs`

Exit criteria:

- Provider JSON golden tests prove correct fields, ordering, and omission.
- Unsupported Kimi/DeepSeek routes never receive reserved/unknown cache fields.
- Cache usage is attributed to the correct session epoch.

### Phase 5 — Optional recurrent-state boundary hints — contract complete

Goal: permit FreeToken-class local engines to reuse hybrid-model state.

Changes:

- Add an internal provider capability for semantic checkpoint hints.
- Emit hints only at complete, surviving semantic boundaries.
- Accept provider reports for restored boundary/checkpoint and re-prefill
  length.
- Keep all behavior optional and correctness-neutral.

Exit criteria:

- A mock local provider restores the deepest surviving boundary after a tool
  output/thinking block edit.
- Hosted providers produce identical cache-relevant prompt inputs to Phase 4
  when the capability is absent; unrelated HTTP envelope details may differ.

Disposition note: the provider-neutral hint/report types, deepest-shared
boundary logic, closed-tool-group eligibility, wrapper capability propagation,
and unsupported-provider omission are implemented and tested. Materializing a
KV/recurrent checkpoint belongs to a future local-engine adapter; no hosted
provider is presented as having done so.

### Phase 6 — OUP client adoption and frontend convergence

Goal: keep context ownership in OUP.

Changes:

- Add optional context/cache diagnostics to OUP capability negotiation and
  lifecycle notifications.
- Render them in OctosCode without moving policy into the client.
- Route `octos chat` and ACP entry points through OUP rather than copying
  the semantic ledger or compaction implementation.

Exit criteria:

- OctosCode, chat, and ACP frontends observe the same OUP context
  generation and cache epoch for equivalent sessions.

Disposition note: the initial OUP milestone covered OctosCode only. The
2026-09-04 follow-on removes the separate chat/ACP execution paths and tests
the real adapters against this same lifecycle.

## Test plan

### Unit tests

- Committed semantic blocks are immutable.
- Branching changes the active head without modifying ancestors.
- A parallel tool-call group becomes eligible only after every result is
  terminal.
- Interrupted tools close with explicit aborted results.
- Semantic selection never cuts a user turn or tool interaction.
- Discarded and retained block sets are disjoint and exhaustive.
- LLM and heuristic summaries receive exactly the same discarded block IDs.
- The newest user instruction remains raw after compaction.
- Summary token output is capped and post-compaction projection meets budget.
- Stable System/tool hashes do not change on ordinary turns.
- Each declared policy/tool/model/compaction change rotates the epoch once.
- v1 snapshot migration reconstructs exact source coverage.
- A stale snapshot cannot pass coverage validation against a newer canonical
  session head.

### Provider golden tests

- Two same-epoch OpenAI requests retain an exact cache-input prefix and the same
  bounded affinity key.
- Epoch rotation changes validation metadata and records a reason; the OpenAI
  affinity key follows the declared provider policy instead of changing
  implicitly.
- Anthropic breakpoints land on stable instructions, final tool definition,
  and a complete semantic conversation boundary.
- Dynamic peer/monitor/goal blocks do not alter the Anthropic System field.
- Providers without explicit cache support receive no unknown fields.
- Cache read/write token accounting remains disjoint and correct.

### OUP integration tests

- Multi-round tool loop without compaction.
- Long tool output with artifact sidecar and semantic envelope.
- Automatic and manual compaction during a tool-heavy turn.
- Parallel tool calls followed by interruption.
- Peer agent completion delivered between user turns.
- Background task completion delivered while the main session is idle.
- Goal progress and monitor events delivered without System mutation.
- Model/tool/profile switch with intentional epoch rotation.
- Session restart, hydrate, branch, and continue.
- Compaction failure and heuristic fallback without ledger mutation.

### Real OctosCode tmux soak

Run the actual `octos serve --stdio --solo` backend, not a fixture, with:

1. At least 30 foreground turns in one session.
2. Repeated read/search/shell tool interactions.
3. Several large tool outputs that create artifact sidecars.
4. At least one automatic and one manual compaction.
5. One peer-agent result and one background-task result.
6. One monitor/goal progress wake.
7. Backend restart followed by session hydration and continuation.
8. One deliberate model or tool-set change to prove epoch rotation.

Capture:

- redacted OUP transcript and lifecycle events;
- semantic ledger/snapshot and source-head validation;
- per-call request fingerprints and common-prefix measurements;
- provider cache-read/write token usage;
- compaction discarded/retained block manifests;
- TTFT and prefill measurements where the provider exposes them;
- final TUI capture proving a coherent, non-repetitive answer after compaction.

The soak passes only if no secret or hidden reasoning content appears in the
diagnostic artifacts.

## Verification record (2026-09-03 and 2026-09-04)

### Automated checks

The final worktree passed these commands after the last reconnect fix:

| Repository | Command | Result |
| --- | --- | --- |
| OctosCode | `cargo test --all-targets --quiet` | No failures. Main unit suite: 1,967 passed, 1 ignored; every integration suite also passed. |
| OctosCode | `cargo clippy --all-targets -- -D warnings` | Passed. |
| OctosCode | `cargo build --bin octoscode` | Passed. |
| Octos | `cargo test -p octos-core -p octos-llm -p octos-agent --quiet` | No failures. Principal library suites included 2,624 passed/3 ignored, 359 passed/1 ignored, and 539 passed/3 ignored; credentialed network tests remained explicitly ignored. |
| Octos | `cargo test -p octos-cli --quiet -- --test-threads=1` | No failures. Main CLI suite: 1,582 passed, 3 ignored; all following integration/doc suites passed. |
| Octos | `cargo clippy -p octos-core -p octos-llm -p octos-agent -p octos-cli --all-targets -- -D warnings` | Passed. |
| Octos | `cargo build -p octos-cli` | Passed. |
| Both | formatter check and `git diff --check` | Passed. |

An earlier workspace-wide all-features build reached the optional
`llama-cpp`/CUDA binding and could not continue because this macOS test host
does not have `cmake`. The production OUP/OctosCode packages and all relevant
feature paths above compile and test; this is recorded as an environment
limitation rather than silently classified as a passing all-features build.

### Post-review re-verification (2026-09-03, after the amendment fixes)

The worktree containing every fix listed under "Post-review amendments" was
re-verified from scratch. `octos-cli` compiles the OUP transport only under
`--features api` (`default = []`), so both feature sets were run.

| Repository | Command | Result |
| --- | --- | --- |
| OctosCode | `cargo fmt --all -- --check`, `git diff --check` | Clean. |
| OctosCode | `cargo clippy --all-targets -- -D warnings` | Passed. |
| OctosCode | `cargo test --all-targets --quiet` | No failures. Main unit suite: 1,977 passed, 1 ignored; every integration suite passed. |
| Octos | `cargo fmt --all -- --check`, `git diff --check` | Clean. |
| Octos | `cargo clippy -p octos-core -p octos-llm -p octos-agent -p octos-cli --all-targets -- -D warnings` | Passed. |
| Octos | `cargo clippy -p octos-cli --features api --all-targets -- -D warnings` | Passed. |
| Octos | `cargo test -p octos-core -p octos-llm -p octos-agent --quiet` | No failures. Core: 387 passed/1 ignored; LLM: 587 passed/3 ignored; agent: 2,630 passed/3 ignored. Every integration target passed. |
| Octos | `cargo test -p octos-cli --quiet -- --test-threads=1` | No failures. Main suite: 1,591 passed, 3 ignored. |
| Octos | `cargo test -p octos-cli --features api --quiet -- --test-threads=1` | No failures. Main suite: 3,117 passed, 6 ignored. Every integration target and doctest passed. |
| Both | `cargo build` | Passed with the default feature sets. |

The 2026-09-03 `tmux` soak below predates the amendment fixes. It was
repeated on 2026-09-04 against the final binaries after the final-review
amendments; that run is recorded under "Post-fix real `tmux` OUP soak
(2026-09-04)" further down, and the regression tests named in the
implementation record were each observed failing before their fix.

### Final re-verification (2026-09-04, after the final-review amendments)

The worktree containing every fix listed under "Final-review amendments" was
re-verified from scratch after the last edit. `octos-cli` still compiles the
OUP transport only under `--features api`, so both feature sets were run.

| Repository | Command | Result |
| --- | --- | --- |
| OctosCode | `cargo fmt --all -- --check`, `git diff --check` | Clean. |
| OctosCode | `cargo clippy --all-targets -- -D warnings` | Passed. |
| OctosCode | `cargo test --all-targets --quiet` | No failures. 2,166 passed, 4 ignored across 33 test binaries. |
| OctosCode | `cargo build --bin octoscode` | Passed. |
| Octos | `cargo fmt --all -- --check`, `git diff --check` | Clean. |
| Octos | `cargo clippy -p octos-cli --features api --all-targets -- -D warnings` | Passed. |
| Octos | `cargo clippy -p octos-core -p octos-llm -p octos-agent -p octos-cli --all-targets -- -D warnings` | Passed. |
| Octos | `cargo test -p octos-core -p octos-llm -p octos-agent --quiet` | No failures. 3,949 passed, 49 ignored across 64 test binaries. |
| Octos | `cargo test -p octos-cli --features api --quiet -- --test-threads=1` | No failures. 3,266 passed, 13 ignored across 22 test binaries. |
| Octos | `cargo test -p octos-cli --quiet -- --test-threads=1` | No failures. 1,644 passed, 9 ignored across 21 test binaries. |
| Octos | `cargo build -p octos-cli --features api` | Passed. |

Regressions observed RED against the pre-fix behavior:
`should_not_spend_budget_grace_call_on_convergence_reflection` (re-run with
the grace gate disabled), `cancelled_scoped_submit_is_restaged_and_resubmitted_exactly_once_after_relaunch`
(failed before the direct-submit gate), `prompt_submitted_during_slow_child_bootstrap_reaches_the_child_exactly_once`
(produced two `turn/start`s before the in-flight staleness split), the
steer end-to-end test (failed under the interim twin relocation), and the
Responses manifest test whose old `openai-responses` expectation no longer
held. The remaining regressions were added together with their fixes and
verified GREEN; the live smoke and the two rejected soak attempts are the
observed pre-fix failures for the epoch-identity, negotiation-timeout,
prompt-loss and cross-turn-prefix defects. The optional `llama-cpp`/CUDA
all-features build remains unavailable on this host (no `cmake`) and is not
claimed.

### Post-fix real `tmux` OUP soak (2026-09-04)

The acceptance run was repeated against the final binaries built by the
verification chain above (daemon 2026-09-04 02:29, client 2026-09-04 01:20),
with the same real PTY harness as the 2026-09-03 run:

- tmux 3.7b (prefix build), 210×58 pane; OctosCode in protocol mode;
- `octos serve --stdio --solo` with a fresh, isolated instance directory
  (`/tmp/octos-oup-final-20260904.epW6om/instance`) and an isolated workspace;
- OUP session `oup-final-20260904f`; semantic mode `on`, 6,000-token test threshold,
  2,000-token target;
- live K3 (`moonshot-coding`) and GLM-5.3 (`zai-coding`) routes;
- redacted provider manifests in `/tmp/octos-oup-final-20260904.epW6om/cache-finalpass.jsonl`.

Five earlier attempts on 2026-09-04 were rejected and are not counted. The
first (01:20) ran an interim ContextManager variant (twin relocation, since
replaced) and had two harness defects: the manual-compaction step used a
fixed sleep and typed the next prompt into the confirmation menu, and the
ad-hoc "create a goal" prompt never entered the goal keeper (`goal_plan` is
only callable on a GOAL turn, so the goal was never planned). The second
(02:31, on the final binaries) reached `FAILS=0` but was invalidated on
inspection: the objective let the keeper choose an acceptance command with
literal quotes, which the fleet validator ran verbatim (exit 1), so the goal
ended `blocked` while a prose regex and a prefix-only `T21` check still
passed. The third (02:52) was stopped at T09 because its reply detection
searched the terminal pane, where the expected text also appears inside the
echoed user prompt. The fourth (02:54) was stopped at T15 because its reply
detection counted assistant rows in the workspace session file
`dev/sessions/<session>.jsonl`, which is not append-only: the background
completion is persisted through the per-user session layout, whose first
open migrates a flat-only session (the per-user file kept all 66 rows; the
legacy flat projection was re-created with only the rows appended
afterwards), so a row-count watermark could never pass. `SessionManager`
reads the merged flat + per-user view, so no authoritative reader was shown
to lose data; the split is recorded as an observation, not a defect. The
fifth (03:15) failed T20 through its own objective text: it described the
acceptance as whitespace-split arguments, so the keeper planned four
one-token acceptance commands (`grep`, `-q`, the marker, the path), and the
command targeted `src/sample.rs` in the parent workspace while a fleet
worker owns an isolated scratch workspace; the validator evidence shows the
four commands failing individually. The goal objective now asks for one
task with `grant.fs = host`, tells the worker to read the absolute source
file and write `verified.txt` into its scratch workspace, and states the
single acceptance string `grep -q PROOF-SOURCE-1788422955 verified.txt`
without quotes or trailing punctuation.

The harness uses only the append-only OUP ledger
(`ui-protocol/<session>/ledger-*.log`): before each send it records the
highest durable `seq` for the session, and a turn passes only when a later
`envelope_v2` record whose payload type is `assistant_persisted` carries the
expected text and the client is back at Done/Idle (the pane is never
searched for reply content). The T15 background completion must appear as a
later `background/spawn_complete` envelope whose content carries both
`final-bg-marker` and the source marker. Expected replies are full values
(`T05` the complete workspace path, `T18` exactly `monitor_01`, `T22` both
file markers, `T30` nonce, README marker, source marker and model; `T17`
is its required prefix followed by the returned monitor id). The harness
also waits for the `context_compaction_completed` ledger record plus an
idle client before sending the post-compaction prompt, fails the run when
either compaction path leaves no completed record, enters the goal flow
through `/goal` (`session/goal/set`), asserts that the keeper's own GOAL
turn plans and dispatches a fleet task (`fleet-work/goal_01-*`), polls the
server's goal record through `/goal` (`session/goal/get`, rendered with the
fixed `Goal <status>:` template) until `complete` and fails on
`blocked`/`failed`, requires the exact `T21 OK goal=complete` reply, and
resets the workspace-scoped session store between runs. After the run an
independent verifier reads only the main session's ledger records
(`event.session_id` equal to the soak session): every label must appear as
exactly one user prompt (T20 as the goal objective), every full expected
reply in exactly one `assistant_persisted` segment, and the T15 completion
in exactly one `background/spawn_complete` envelope; echoed prompts, peer
and worker sessions never count. The per-user and legacy flat session files
are checked separately for duplicated sentinels and are never summed.

Result: **FAILS=0 — all 30 turns answered with their exact expected replies.** In the main-session records of the append-only UI ledger, every label T01–T30 appears as exactly one user prompt (T20 once as the `/goal` objective), every full expected reply appears in exactly one `assistant_persisted` segment, and the T15 completion appears in exactly one `background/spawn_complete` envelope; echoed user prompts and other sessions never count. The workspace session files are a separate consistency check: the per-user file holds 28 assistant rows and the legacy flat projection 29, with no sentinel duplicated inside either file.

| Turn | Result | Time to reply | Reply prefix / event |
| --- | --- | ---: | --- |
| T01 | pass | 9 s | `T01 OK stored` |
| T02 | pass | 6 s | `T02 OK prior=FINAL-NONCE-20260904F` |
| T03 | pass | 9 s | `T03 OK readme=PROOF-README-1788422955` |
| T04 | pass | 9 s | `T04 OK source=PROOF-SOURCE-1788422955` |
| T05 | pass | 15 s | `T05 OK cwd=/private/tmp/octos-oup-final-20260904.epW6om/workspace` |
| T06 | pass | 21 s | `T06 OK large-1` |
| T07 | pass | 18 s | `T07 OK large-2` |
| T08 | pass | 21 s | `T08 OK large-3` |
| T09 | pass | 18 s | `T09 OK raw-large-1` |
| T10 | pass | 18 s | `T10 OK raw-large-2` |
| T11 | pass | 18 s | `T11 OK raw-large-3` |
| T12 | pass | 9 s | `T12 OK prior=FINAL-NONCE-20260904F` |
| T13 | pass | 15 s | `T13 OK prior=FINAL-NONCE-20260904F` |
| T14 | pass | 30 s | `T14 OK peer=PROOF-README-1788422955` |
| T15 | pass | 24 s | `T15 OK background-started` |
| T16 | pass | 15 s | `T16 OK background=PROOF-SOURCE-1788422955` |
| T17 | pass | 27 s | `T17 OK monitor-created` |
| T18 | pass | 15 s | `T18 OK monitor=monitor_01` |
| T19 | pass | 9 s | `T19 OK monitor-deleted` |
| T21 | pass | 12 s | `T21 OK goal=complete` |
| T22 | pass | 9 s | `T22 OK parallel=PROOF-README-1788422955+PROOF-SOURCE-1788422955` |
| T23 | pass | 15 s | `T23 OK resume-prior=FINAL-NONCE-20260904F` |
| T24 | pass | 39 s | `T24 OK restart-prior=FINAL-NONCE-20260904F` |
| T25 | pass | 18 s | `T25 OK model=glm-5.3 prior=FINAL-NONCE-20260904F` |
| T26 | pass | 18 s | `T26 OK model=k3 prior=FINAL-NONCE-20260904F` |
| T27 | pass | 18 s | `T27 OK shell=workspace` |
| T28 | pass | 9 s | `T28 OK prior=FINAL-NONCE-20260904F` |
| T29 | pass | 18 s | `T29 OK monitor=absent` |
| T30 | pass | 12 s | `T30 PASS nonce=FINAL-NONCE-20260904F readme=PROOF-README-1788422955 source=PROOF-SOURCE-1788422955 model=k3` |

- automatic compaction during T06–T11: 1 `context_compaction_completed` record(s) before T12 (mandatory check).
- manual `/compact` before T13: completed records 1 → 2 within 3 s (mandatory check).
- T15 background task: `background/spawn_complete` envelope carrying `final-bg-marker` and the source marker observed 15 s after the prompt.
- T20 `/goal`: the keeper's own GOAL turn planned and dispatched a fleet task after 10 s (`goal_01-01a06bfa-9aa4-7b40-a1ce-ca0bd96f6a65`).
- goal wake: `session/goal/get` reported `complete` after 30 s (polled statuses: active@10s, complete@30s).
- daemon restart #1: external kill of the daemon with T24 submitted immediately; the client relaunched the daemon, the deferred prompt reached it once, and the answer `T24 OK restart-prior=FINAL-NONCE-20260904F` arrived 39 s after the send.
- daemon restarts #2/#3 after the model switches: new pids 72522, 72679; the client had already reconnected to each new daemon at the harness's first status poll after the kill.
- epoch invalidation reasons recorded in the durable ledger: {'initialized': 19, 'compaction_installed': 19, 'model_route_changed': 7}.

Live cache evidence from `cache-finalpass.jsonl`:

| Observation | Result |
| --- | ---: |
| manifest observations | 69 |
| usage records correlated with a manifest | 69 |
| `usage_unmatched` records | 0 |
| `append_only` relation records | 52 |
| `old_history_changed` relation records | 4 |
| `epoch_rotated` / `initialized` / `stable_prefix_changed` | 3 / 7 / 3 |
| usage records with non-zero provider cache reads | 68 / 69 |
| total reported cache-read tokens | 1,661,952 (max 30,720 in one response; 13 epoch identities) |
| live routes observed | `moonshot-coding/k3` (68), `zai-coding/glm-5.3` (1) |
| epoch invalidation reasons recorded in the durable ledger | {'initialized': 19, 'compaction_installed': 19, 'model_route_changed': 7} |

A literal scan of the manifest found 0 occurrence(s) of `FINAL-NONCE-20260904F`, 0 occurrence(s) of `PROOF-README-1788422955`, 0 occurrence(s) of `PROOF-SOURCE-1788422955`, 0 occurrence(s) of `T30 FINAL SENTINEL`, 0 occurrence(s) of the proof directory path. The file contains hashes, segment kinds, normalized lengths, epoch/affinity hashes, relation codes, and provider usage only.

### Real `tmux` OUP soak

The acceptance run used the actual debug binaries and a real PTY, not a mock
transport:

- tmux 3.7b, 210×58 pane;
- OctosCode in protocol mode;
- `octos serve --stdio --solo` with an isolated instance directory;
- OUP session `oup-final-20260903b` and an isolated workspace;
- semantic mode `on`, 6,000-token test threshold, and 2,000-token target;
- live K3 and GLM-5.3 provider routes;
- redacted provider manifests written to
  `/tmp/octos-oup-semantic-proof.sPDred/cache-finalpass.jsonl`.

The append-only OUP UI ledger contains exactly one user prompt for every
label from T01 through T30. The final post-fix capture reached `Done`, and T30
returned the nonce plus both file markers exactly once.

The run exercised:

1. ordinary no-tool turns and exact recall across the entire session;
2. README/source reads, grep, shell execution, parallel tool calls, and large
   outputs stored behind content-addressed sidecars;
3. automatic compaction from about 6.9K to 2.0K tokens (4 blocks retained,
   54 dropped), manual compaction from about 2.2K to 304 tokens (3 retained,
   9 dropped), and another goal-keeper automatic compaction from about 6.2K
   to 1.5K;
4. nonce recall immediately after both automatic and manual compaction;
5. one foreground peer result, one background sub-agent result, the automatic
   continuation after each result, and a scatter-join terminal;
6. monitor create/list/delete plus post-restart proof that the deleted monitor
   stayed absent;
7. goal create/plan/dispatch, fleet completion, and the goal-progress wake that
   re-entered the keeper and observed `complete`;
8. OctosCode process restart against the same durable session;
9. two external stdio-daemon terminations with a prompt submitted immediately
   during recovery; scoped reopen, hydration, and FIFO drain completed without
   a duplicate or lost prompt;
10. K3 → GLM-5.3 → K3 changes, each applied by a daemon restart; the same
    pre-compaction nonce remained available and `/context` reported
    `model route changed` as the deliberate cache invalidation.

The no-tool recall sentinel
`FINAL-NONCE-20260903B` survived automatic/manual compaction, client restart,
both daemon restarts, and both model-route changes. T24, the prompt submitted
during a daemon death, appears once as a user prompt and once as an assistant
answer in durable/session evidence and once in the rendered capture.

### Defects found by the soak and fixed

The soak was intentionally diagnostic: failed runs were not relabelled as
passes. It found two client integration defects after the server-side semantic
context work was already correct.

1. **Repeated background continuation rendering.** A server-initiated
   continuation reuses the latest user prompt as its semantic anchor.
   OctosCode inferred that every such continuation belonged to the first
   assistant after that prompt, rejected the correct live-prefix coverage, and
   re-flushed the answer when `assistant_persisted` arrived. The coverage
   reducer now lets direct canonical-prefix evidence win when the inferred row
   cannot contain the live reply. Regression:
   `server_continuation_reuses_prompt_anchor_without_reflushing_live_prefix`.
2. **Working forever after daemon restart.** A new daemon can resume a
   process-wide durable continuation before the client's workspace-scoped
   `session/open` completes. OctosCode previously reconciled dead-child state
   at socket connect, then accepted the startup turn; switching to the scoped
   stream could hide that turn's terminal and strand the queue. Relaunch
   reconciliation is now delayed until the expected scoped `session/opened`,
   queued before deferred scoped commands are released. Regression:
   `stdio_reconnect_waits_for_scoped_open_before_releasing_background_continuation`.

The first defect also tightened canonical v2 segment handling so finalized v2
answers do not trigger the old partial-answer heuristic. The second preserves
all existing staged-submit restaging, pre-token, capability-negotiation, and
session-affinity barriers.

### Live cache evidence and privacy audit

The final manifest contains 77 manifest observations, 75 of which carry a
correlated usage record (independently re-audited 2026-09-03):

| Observation | Result |
| --- | ---: |
| `append_only` relation records | 124 |
| records with matching stable prefix | 131 |
| usage records with non-zero provider cache reads | 73 / 75 |
| total reported cache-read tokens | 1,404,160 |
| maximum cache-read tokens in one response | 25,088 |
| maximum mechanically reusable normalized prefix | 101,882 bytes |
| live routes observed | `moonshot-coding@api/k3`, `zai-coding/glm-5.3` |

The other relation records were explicit initialization, epoch rotation, or
stable-prefix change events; no miss was inferred from semantic similarity.
The manifest records 11 epoch identities across the master, peer, background,
and goal-worker calls exercised by the run.

A literal scan of the manifest found zero occurrences of the nonce, either
proof marker, the T30 prompt, or the workspace path. The file contains hashes,
segment kinds, normalized lengths, epoch/affinity hashes, relation codes, and
provider usage only. Provider TTFT was not asserted because these routes did
not expose a correlated TTFT field in the response usage contract.

## Acceptance criteria

### Correctness

- Every model-visible fact after compaction is sourced either from exactly one
  summary-covered discarded block or one raw retained block, never both.
- No semantic tool interaction is split.
- The current user instruction is never represented solely by background
  summary text.
- Durable source history remains recoverable across compaction and branching.
- Restart and background completion cannot produce a falsely current context
  snapshot.

### Prefix/cache behavior

- Inside one epoch, earlier eligible provider cache-input content remains
  byte/token-identical; only a suffix and provider-approved rolling cache
  marker may advance.
- An edit at a semantic boundary preserves the provider cache-input prefix
  through that boundary.
- Every non-append prefix change has a recorded epoch rotation and reason.
- On a cache-capable live provider, the second eligible request reports cache
  reads or an equivalent provider proof. Performance ratios are reported
  against the Phase 0 baseline rather than hidden behind a fixed universal
  threshold.

### UX and operations

- Context/cache diagnostics are understandable without exposing implementation
  noise in the normal OctosCode transcript.
- Cache misses never affect correctness.
- Operators can disable provider-specific cache features without disabling
  semantic context management.
- Old session snapshots migrate or rebuild automatically.

## Rollout and compatibility

- The production default is `on`; retain `shadow` for request-equivalence
  diagnosis and `off` as an operational rollback. An invalid value fails safe
  to `on` with a warning rather than silently disabling the correctness fix.
- Preserve canonical session history throughout migration.
- Treat ContextManager snapshots as rebuildable and version them to v2.
- Do not overwrite a valid v1 snapshot until v2 reconstruction and validation
  succeed.
- Keep provider cache fields capability-gated and absent by default on unknown
  compatibility routes.
- Roll back projection behavior independently from ledger recording so evidence
  collected in shadow mode remains usable.
- Introduce any OUP wire additions as optional fields behind capability
  negotiation; old OctosCode builds must continue to function.

## Relationship to existing contracts

- This record **amends the assumption and the OUP behavior** described by
  `specs/kv-cache-friendly-compaction.spec.md` that the ContextManager/AppUI
  path could remain out of scope because observed history was append-like.
- It extends the M16 ContextManager work: existing typed tool-output envelopes,
  artifact evidence, generations, checkpoints, forks, and compaction records
  remain useful and should be migrated rather than discarded.
- It preserves M17 parent/child isolation. Child transcripts stay out of the
  parent prompt; only bounded child/peer result capsules become semantic blocks
  in the parent ledger.
- OUP `context_state` additions are governed by accepted `UPCR-2026-029`, with
  feature negotiation and replay behavior. Internal semantic recording and
  shadow validation remain independent of the wire addition.
- Chat/ACP fixes belong in their OUP adapters or the common runtime; their
  deleted context/execution loops must not be restored as another authority.

## Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| Moving dynamic data out of System weakens instruction priority | Keep stable behavioral rules in System/developer instructions; represent objectives and events with typed authority and explicit untrusted-data framing |
| An incomplete tool group blocks compaction | Close interrupted/failed calls with explicit terminal envelopes; allow artifact-only reduction within the group |
| Semantic schema migration loses coverage | Rebuild from full canonical session history and verify source-head/prefix hashes before installing v2 |
| Provider serializers normalize messages differently | Test the final wire representation, not only `Message` vectors |
| Cache key causes affinity hot spots | Hash bounded session+epoch identity and follow provider capability/retention limits |
| Too many semantic checkpoints consume local VRAM | Emit candidates and priorities; let the engine enforce capacity/LRU policy |
| Summary generation exceeds budget or times out | Hard output limit, validation, bounded retry, then deterministic heuristic fallback without mutating the active generation on failure |
| Dynamic tools change unexpectedly mid-turn | Freeze tool schemas for an epoch/turn; rotate deliberately at the next safe boundary |

## Explicit non-goals

- Semantic similarity caching of different token sequences.
- Treating a semantic boundary as permission to reuse invalid KV.
- Reimplementing a serving engine's KV or recurrent-state allocator in OUP.
- Coupling correctness to provider cache availability.
- Maintaining separate semantic context implementations for OctosCode, chat,
  ACP, gateway, or individual providers.
- Including FreeToken's expert LRU or bandwidth-adaptive MoE scheduling in the
  OUP context layer.

## Implemented work-package order

The implementation followed this dependency order:

1. Request fingerprint/cache observability with no prompt changes.
2. Semantic block metadata and source-head validation in shadow mode.
3. Disjoint, boundary-safe compaction.
4. Stable System and explicit cache epochs.
5. Provider cache-context integration.
6. Optional local-engine recurrent checkpoint hints.
7. OUP lifecycle diagnostics and OctosCode adoption.
8. Chat/ACP routing convergence and obsolete-code removal (the follow-on above).

Correctness work precedes provider optimizations. In particular, do not add an
OpenAI cache key and declare success while OUP still mutates the first System
message or summarizes retained content twice.

## Six-finding follow-up (2026-09-04)

The independent current-worktree re-review identified two P1 and four P2
findings. The following changes supersede earlier descriptions of rejected
opens, goal-counter coalescing, and prefix-only live-coverage matching:

- **P1 — scalar credentials:** credential-named JSON fields redact numeric
  and boolean leaves as well as strings, including embedded JSON and argv
  values. Non-secret numbers/booleans and null remain unchanged. A durable
  ledger regression checks both disk bytes and replay after reopening;
  examples use synthetic credentials only.
- **P1 — rejected scoped open:** a definite RPC rejection keeps the transport
  alive but latches a successful-open requirement. Global discovery and
  profile/auth correction remain available. Deferred and subsequent scoped
  requests fail locally until a corrective/reconnect open succeeds; queued
  corrective opens re-arm the normal FIFO barrier. Rejected turn/start uses a
  session/turn-correlated `session_open_rejected` terminal, not the generic
  `request_cancelled` path that automatically re-stages prompts. The prompt
  and failure remain visible and unrelated sessions' submit gates survive.
- **P2 — fresh goal counters:** counter-only changes append a canonical
  revision and return a changed result so the OUP caller refreshes and persists
  its model prompt. The active projection keeps only the latest semantically
  equivalent goal snapshot. Canonical ancestors are never rewritten;
  hydration and post-compaction reconstruction perform the same selection.
- **P2 — manifest stream identity:** the JSONL diff example uses the observer's
  recorded `previous_sequence` and `comparison`, never physical adjacency or
  a global sequence lookup for runtime records. This preserves peer/request
  boundaries and daemon-local sequence resets. Both `usage` and
  `usage_unmatched` are skipped. Only legacy raw manifests compare adjacent
  rows. Parse errors report line numbers without quoting input values.
- **P2 — continuation identity:** committed assistant rows retain their
  turn/thread id. Coverage requires matching session/turn identity (including
  the wire/local v2 mapping); legacy rows require a unique exact prompt anchor.
  Shared anchors or common text prefixes alone never authorize deduplication.
- **P2 — persist-lock lifetime:** the per-session registry stores weak
  references and prunes expired entries. Every writer/waiter retains a strong
  reference, so pruning cannot create two live locks for one session. The
  regression churns 2,048 retired sessions while preserving one held lock.

Focused regressions include
`should_redact_numeric_credentials_before_durable_replay`,
`session_open_rpc_error_keeps_connection_but_requires_scope_before_turns`,
`rejected_scope_terminal_settles_only_its_submit_without_retrying_it`,
`should_restore_latest_goal_counters_after_interleaved_turns_and_compaction`,
the three `prompt_cache_manifest_diff` example tests,
`continuation_coverage_never_consumes_another_turn_with_the_same_prefix`,
`legacy_reply_with_shared_anchor_retains_ambiguous_prefix`, and
`should_reclaim_expired_context_persist_locks_without_splitting_live_writers`.
OctosCode's scope-barrier and assistant-projection specs carry the revised
contracts. Markdown fixtures now supply the same identities as production
commits; their prefix/suffix and fence-separator assertions are unchanged.

Follow-up verification on `Mrandi5.local` (arm64):

| Target | Verification | Result |
| --- | --- | --- |
| OctosCode | `cargo test --all-targets` | 2,169 passed, 4 ignored, no failures (33 test binaries). |
| OctosCode | strict all-targets clippy, build, fmt, diff check | Passed. |
| Octos core/LLM/agent | `cargo test -p octos-core -p octos-llm -p octos-agent --all-targets` | 3,944 passed, 49 ignored, no failures (65 test binaries, including the manifest example). |
| Octos CLI API | `cargo test -p octos-cli --features api -- --test-threads=1` | 3,262 passed, 13 ignored, no failures (22 test/doc-test summaries). |
| Octos CLI default | `cargo test -p octos-cli -- --test-threads=1` | 1,644 passed, 9 ignored, no failures (21 summaries). |
| Octos | strict all-targets clippy for CLI API and core/LLM/agent; API build; fmt; diff check | Passed. |

The first parallel CLI library run was **not** green: two
`peer_awaiting_wake_tests` failed while inspecting the shared default
orchestrator (3,119 passed, 2 failed, 6 ignored). Both passed in the full
serial run, and the isolated nine-test peer suite also passed with eight
test threads. The declining continuation-count assertion and shared singleton
are consistent with cross-test state interference; this pass does not claim
to have fixed that parallel-test isolation issue. The normal macOS debug
link emitted an unwind-table-size warning; strict clippy passed. Optional
llama-cpp/CUDA all-features coverage remains unclaimed.

A fresh real-tmux attempt at
`/tmp/octos-six-fixes-soak-20260904.uuSpus` was rejected at T15, after T01–T14
and two automatic plus one manual compaction passed. The background child
delivered `PROOF-SAMPLE-RS-8734` instead of the unchanged source fixture's
marker. Its task text was correct, but no actual child file-read evidence
was found and only one child model request was observed. This is consistent
with an ungrounded model answer, not proof of a transport or cwd defect.
The failure is retained in `REJECTED.md` and is not counted as acceptance.
The follow-up harness requires the child to call a file-reading tool on the
absolute fixture path, without changing the expected marker or production
binaries.

### Follow-up soak acceptance

The same final binaries completed all 30 scenarios in the fresh instance
`/tmp/octos-six-fixes-soak-20260904.yoXB2G`, session
`oup-six-fixes-20260904j` (2026-09-04 14:43–14:56 PDT). The driver reports
`FAILS=0`; the independent durable-ledger verifier reports `ledger gate OK:
True`, no violations, exactly one full expected assistant segment per reply
sentinel, and one task-correlated T15 completion. Every user prompt/goal
objective appears once; the goal itself was observed `complete` through the
typed goal-status UI. There were five durable compaction completions, including
the required automatic and manual phases, a client restart, three externally
terminated daemon restarts (one with an immediately queued prompt), and the
verified K3 → GLM-5.3 → K3 route sequence. No sentinel duplicates were found
in either session-file projection, checked separately.

The harness paused at T15 to correct a false-negative predicate: this valid
`background/spawn_complete` body carried only `Status: SUCCESS` and the marker,
not the task label. Its `task_id` matched the supervisor's `child_started`
record for `final-bg-marker`; child manifests 33–34 also showed a tool call
and result before the answer. The checker now joins those typed identities
instead of requiring a label in prose. The driver resumed after the existing
T15 with its original sequence watermark 1102; the client and daemon stayed
running, no prompt was replayed, and production binaries were unchanged.
The corrected verifier still rejects the first attempt's wrong marker.
`HARNESS-IDENTITY-CORRECTION.md` records the details; this is not represented
as an uninterrupted, unmodified-harness run.

Cache evidence: 73 manifests and 73 usage reports; 70 usage reports had nonzero
cache reads, totaling 1,774,080 cached tokens. Routes were 72 K3 requests and
one GLM-5.3 request. The fixed manifest-diff tool emitted 66 comparisons,
exactly the 66 runtime observations carrying a recorded predecessor/comparison,
without cross-stream or cross-restart adjacency reconstruction. The manifest
privacy scan found no nonce, fixture markers, sentinel prompt, or workspace
path in plaintext. SHA-256 verification confirmed both product binaries were
unchanged throughout the accepted attempt. Test clients/daemons were stopped;
the isolated proof data was retained.

Evidence files in that directory: `evidence.txt`, `soak-driver.log`,
`cache-finalpass.jsonl`, `manifest-diff.jsonl`, `binary-sha256.txt`, terminal
captures, and the corrected local harness/verifier. Full test/clippy logs are
under `/tmp/octos-six-fixes-*.log` and `/tmp/octoscode-six-fixes-*.log`.
No commit was created by this follow-up.

## Frontend migration validation and open review (2026-09-04)

Chat and ACP now use the same OUP dispatcher as OctosCode. The old chat/ACP
Agent loops, independent ACP history/bootstrap/replay, chat-specific peer host,
and orphaned pipeline-tool assembler have been removed. The three legacy files
`commands/chat.rs`, `commands/acp.rs`, and `peers/host.rs` account for 5,005 deleted
lines; this is not a net line-count reduction claim, because replacement adapters
and regression tests were added.

Development tests exposed and fixed runtime-owned ledger isolation, tool-policy
reapplication, cwd plugin discovery and child pipeline-factory rebinding, named
ACP channel/profile resolution, duplicate terminal text when canonical persistence
precedes queued deltas, and ACP stdin-EOF shutdown. RED/GREEN logs and rejected
development runs remain under `/tmp/octos-oup-migration-*.log`,
`/tmp/octos-migration-*.log`, and
`/tmp/octos-oup-migration-soak-20260904.gd4djh`. That development soak is not the
immutable-build acceptance: target binaries changed while defects were fixed.

### Pinned-build validation

The final binaries were copied to the isolated proof directory
`/tmp/octos-oup-migration-final-20260904.cWjayX/bin`. SHA-256 checks against
`binary-sha256.before` passed before and after the tests. This run was performed
on the current arm64 Mac mini host; no remote login credentials were
added to this record.

- CLI: 3,241 tests passed across 22 suites, 13 ignored, zero failed
  (`/tmp/octos-migration-cli-full-v4.log`, serial execution).
- Agent/LLM: 3,551 passed across 62 suites, 48 ignored, zero failed
  (`/tmp/octos-migration-agent-llm-v1.log`).
- Core/bus: 644 passed across eight suites, one ignored, zero failed
  (`/tmp/octos-migration-core-bus-v1.log`).
- OctosCode all targets: 2,170 passed across 33 suites, four ignored, zero failed
  (`/tmp/octos-migration-client-all-v5.log`).
- Strict all-target clippy passed for CLI/Agent/bus and OctosCode; both builds,
  both format checks and both diff checks passed. The final CLI no-default-feature
  check passed (`/tmp/octos-migration-no-default-final.log`). Optional workspace-wide
  llama-cpp/CUDA coverage is not claimed. The macOS debug linker emitted its
  unwind-table-size warning.

The 51-scenario normal real-provider matrix passed: OctosCode/OUP 30, chat REPL
10, ACP 10 and chat JSON one. OUP ran in real tmux from 17:04 to 17:12 PDT with
six compaction completions, automatic/manual compaction, peer and background
delivery, goal wake/completion, monitor lifecycle, parallel tools, a client
restart, three daemon terminations/reconnections and K3 → GLM-5.3 → K3.
`soak-driver.log` reports `FAILS=0`; the independent append-only ledger gate in
`evidence.txt` reports `True`, with exactly one expected assistant segment per
reply and one task-correlated background completion. Each distinct user prompt
or goal objective occurs once. The flat and per-user session projections have
no duplicate reply sentinels; they contain different portions of this run and
are not individually claimed to be complete replay authorities.

Chat and ACP each completed ten real prompts and four compactions. ACP additionally
closed and restarted its process, loaded canonical replay, cancelled one turn,
and successfully answered the next prompt. The JSON one-shot returned valid JSON
with the expected answer and actual usage. These tests use private fixture data,
not a user's artwork or workspace content. Test tmux/client/daemon processes were
stopped, and proof data was retained.

OUP cache evidence contains 72 request manifests and 71 usage reports; 70 usage
reports have nonzero cache reads, totaling 1,332,736 cached tokens. Manifest routes
are 71 K3 and one GLM-5.3 request. The chat/ACP files separately contain 28
`usage_unmatched` reports, all with nonzero cache reads, totaling 276,736 tokens:
those runs did not enable trace-level request-manifest capture and do not prove
request/usage pairing. A separate trace-enabled JSON probe recorded one manifest
and one matched usage. Scans of these manifests found no test nonce, fixture
marker or proof/workspace path in plaintext. No full credential-history audit is
implied by this manifest-only scan.

### Additional probes are not all clear

The requested independent empty-answer review reproduced two backend defects
against the same pinned binary using localhost OpenAI-style and Anthropic-style
fixtures, without external model calls or image generation:

1. **P1: reasoning-only output is accepted as successful completion.**
   `agent/detection.rs::is_retriable_response` excludes responses containing
   reasoning from empty-response recovery. The Agent's EndTurn path returns
   successful empty content, and OUP emits Completed with no final assistant row.
   The migrated chat adapter reports a missing-answer error, and OctosCode no
   longer fabricates the empty Session Summary, but neither fixes this backend
   misclassification. There is no canonical answer for reopening to recover.
2. **P2: output truncation is accepted as successful completion.** The
   `StopReason::MaxTokens` branch returns an ordinary successful response and
   loses the incomplete status. A fixture containing an unfinished sentence
   exits chat with status zero and emits Completed. An unlimited iteration count
   does not remove the provider's per-response output-token cap.

Evidence is retained in `/tmp/octos-empty-answer-review.rRmwnO/`: `results.jsonl`,
`anthropic-results.jsonl`, the localhost reproduction fixture and isolated ledgers.
These mechanisms are independently reproduced, not a claim about the exact
cause of the operator's historical image-design turn without its provider trace.
The bare PNG path is text, not an attachment (`@path` is the client attachment
syntax); this alone does not explain a falsely successful empty terminal.

The extra real chat-peer probe did start a child OUP session, observe its actual
`read_file` result and persist the correct peer blackboard result. It is not
counted as end-to-end acceptance: the parent tried a workspace-external
`read_file` rather than gathering through the peer API, and goal completion was rejected.
The follow-up inspection below corrects the assumption that `peer_gather` was
actually exposed by chat's opt-in tool surface.
A follow-up process using the existing peer state exited with
`OUP dispatcher did not shut down after EOF`; its output and trace are retained in
`chat-peer-gather.out` and `chat-peer-gather.stderr`. The embedded client's ten-second
deadline covers the entire dispatcher, while the dispatcher itself permits ten
seconds of active-turn drain before subsequent cleanup. The outer abort can
therefore preempt normal cleanup. The one-shot chat adapter propagates this close
error before inspecting the turn result, hiding the original result/error.
The failing follow-up has no new persisted assistant answer; it must not borrow
the previous turn's PASS as evidence. A restored continuation started before
SessionOpened; that continuation causing the new turn's admission to fail is a
timeline-supported inference, because the original RPC error was not captured.
These failures are not hidden by the passing 51-scenario normal matrix. No source
fixes or commits were made as part of the diagnosis-only independent review.

## Terminal-integrity repair and deployed-client diagnosis (2026-09-04)

The operator authorized repair after the diagnosis above. The earlier failing
fixtures and peer attempt remain historical evidence, not silently relabeled passes.

The repeatedly reported “partial live answer” card has a separately verified
deployment cause. The active design window was still running the installed
OctosCode `0.3.0-rc.9 (02e6816 2026-09-01)` and Octos
`2.0.3-rc.9 (5ea98781 2026-08-24)` from `~/.cargo/bin`; executable inode and
SHA-256 checks matched those installed files. The old client contains the exact
reported card text. Its successful-tool-activity heuristic classifies a single
line of at least 32 characters as partial unless it ends with a small set of
English punctuation. A normal Chinese sentence ending in `。` therefore triggers
a false card. This is not evidence that the provider truncated that historical
answer. The current client has removed the heuristic; a new regression sends
successful tool events, a long Chinese sentence ending in `。`, and completion,
and requires exactly the real answer with no synthetic Summary. Historical
Summary rendering remains available for old stored content.

Backend repairs are independent of that stale-client diagnosis:

- Reasoning without non-whitespace assistant text or tool calls is an empty
  response, subject to bounded recovery. Exhaustion is an error, not Completed;
  internal reasoning is never promoted into a user-facing final answer.
- Conversational `MaxTokens` returns a typed incomplete-response carrier. OUP
  persists its actual partial body, then emits `output_truncated` / Errored,
  preserving content for hydration and connection reopening. The task-mode
  continuation policy is unchanged; no automatic conversational continuation
  or removal of provider output-token limits is claimed.
- Rejected-response usage includes the last attempt and non-streaming fallback,
  including mixed rejected-response/transport-error paths. A single failed-exit
  settlement and successful-response merge avoid both omission and double charge.
- Interactive goal charging includes consumed failed/truncated work. Both goal
  completion paths require a genuine Completed terminal and the current turn's
  committed final-answer event, never a partial/history/background row. Truncated
  voice replies have internal control markers stripped but do not execute them.
- Embedded close cooperatively stops dispatch and performs owned-turn cleanup;
  it no longer races an outer ten-second abort against the dispatcher's own drain.
  Explicit writer closure and abort-on-drop forwarding cleanup avoid relying on
  detached sender clones. One-shot chat preserves its primary answer/error if
  shutdown also reports a warning.
- Embedded boot reserves foreground admission until the first turn is accepted.
  Background admission is enabled during a turn or an idle listener, restricted
  to opened sessions. This does not implement a general waiting queue for a new
  foreground request when an already-admitted background turn is still active.

Adversarial RED/GREEN evidence is under `/tmp/octos-terminal-*.log`. The original
four backend response tests failed before repair; three shutdown/outcome tests
and the unopened-session continuation test also demonstrated their original
failures. CLI terminal-integrity tests then passed 9/9. Agent terminal-integrity
tests passed 12/12, including six mixed-error/adaptive-recovery usage regressions.
The partial-output persistence regression closes and opens a new connection
against the same AppState; it is not described as a cold-process restart test.

New pinned-binary proof directory:
`/tmp/octos-terminal-integrity-final-20260904.XEjkc9`. Unlike the earlier harness,
this run also isolates `OCTOS_HOME` and the profile registry so model switching
does not modify the operator's active profile. The operator's design window has
not been terminated or restarted, and no installed binary has been replaced.

The additional chat-peer probe returned an answer and a second process reopened
the same state and returned `CHAT-PEER-REOPEN-PASS` without the old shutdown error.
That validates only the reopen/outcome edge, not the requested gather workflow:
chat's retained `CHAT_PEER_TOOLS` allow list exposes handoff/list/respond and
explicitly excludes `peer_gather`. The model's final prose claimed a gather call,
but the actual trace instead contains shell attempts. No gather acceptance is
claimed and no new tool-surface authorization is inferred from that prose.

Known broader accounting limitations are not claimed fixed: usage reported inside
a stream and followed by a stream error can still be lost before a ChatResponse
exists; cancellation can miss pending retry usage in the interrupt tracker; and
ordinary failed turns do not yet persist all priced usage to the global usage
ledger. These are distinct from the repaired rejected-response totals and the
typed truncated-response persistence path. Autonomous post-terminal accounting
also remains exposed to cancellation of that later cleanup tail.

### Repair validation results and remaining soak failure

- CLI: 3,250 passed, zero failed, 13 ignored, 22 suites
  (`/tmp/octos-terminal-cli-full.log`). The final focused terminal-integrity
  rerun passed 9/9, including exact single-charge assertions.
- Agent/LLM: 3,563 passed, zero failed, 48 ignored, 62 suites
  (`/tmp/octos-terminal-agent-llm-full.log`).
- OctosCode: 2,171 passed, zero failed, four ignored, 33 all-target suites after
  the explicit Chinese regression (`/tmp/octos-terminal-client-all-chinese.log`).
- Both strict all-target clippy checks, format/diff checks, the backend build,
  and CLI no-default-feature check passed. The macOS debug linker retained its
  unwind-table-size warning; optional workspace-wide llama-cpp/CUDA is not covered.
- Actual CLI + localhost HTTP: 12 cases passed across OpenAI-style and
  Anthropic-style adapters (68 local requests, not live-provider calls). Each
  has exactly one durable terminal: four successful answers, six bounded
  empty-answer errors, two `output_truncated` errors with one real persisted
  partial body each. The CLI `--no-retry` cases still used the bounded empty-answer
  recovery ladder; they are not evidence of whole-ladder FailFast behavior.
- Real ACP: ten prompts, nine Completed and one Interrupted; process restart
  and canonical replay, real tools and cancel-then-fresh passed. No compaction
  occurred in this ACP run. Cancellation preceded tool start, so running-tool
  termination is not proved. Real chat REPL completed ten prompts and exited
  cleanly; the peer-state JSON reopening probe also returned its new answer.
  Nonce-bearing prompts repeat the expected nonce, so these checks do not prove
  independent recall of a nonce absent from the current input.

The new OUP/tmux run lasted 17:55–18:05 PDT and exercised seven compactions,
peer/background work, monitor and goal lifecycle, parallel tools, one client
restart, three daemon restarts and K3 → GLM-5.3 → K3. The driver reports `FAILS=0`,
but **strict independent acceptance failed**: T14's expected reply appears in
three different main-session assistant segments, not one. Ledger records 1163,
1201 and 1288 have distinct turn/thread IDs and canonical message identities
(message sequences 55, 56 and 57); the latter two continuation turns generated
identical follow-up answers. Their text deltas and Completed events are real,
and the TUI capture also displays all three. This is redundant continuation
execution, not duplicate replay of one persisted message, a tool carrier or a
benign quotation. The original turn already had two pending continuations before
completion. Supervisor and serve logs identify the two sources: `peer_close`
queued `child_completed` (continuation 1) and `scatter_join_complete`
(continuation 2) for the same peer work. Both come from
`enqueue_agent_terminal_continuations` in `autonomy/agent_orchestrator.rs`.
Simply merging the two wakeups would not by itself prove that a peer result
already consumed by the foreground needs any additional answer.
The strict gate remains failed; it was not relaxed to accept the run.
This is a remaining review finding separate from the stale-client Summary card.

Cache diagnostics from this **non-accepted** run contain 79 manifests and 75
usage records; 73 usage records have nonzero cache reads, totaling 1,432,832
cached tokens. The manifest-only privacy scan found zero nonce, fixture marker
or proof/workspace path occurrences. Binary SHA-256 checks before/after match.
The test tmux/client/daemon processes were stopped; the operator's active design
processes and shared profile were left unchanged. No commit or installation was
performed. These results do not support an overall clean-review or full-soak-pass
claim while the redundant peer continuations remain unresolved.
The four private `dev.json` profile copies used to seed the test runtimes were
removed after shutdown; the original profile is intact. Re-running the live
harness requires reseeding those private profiles, not recovering credentials
from the retained logs or source tree.

### Peer lifetime wake repair (2026-09-04, follow-up)

The redundant T14 continuations above had a second cause beneath the terminal
guard: `TaskSupervisor::register("peer_handoff", ...)` allocates a child session
key, just as an ordinary spawn does. `background_task_backend_kind` checked that
key first and therefore stamped actual peer lifetimes `spawn_child_session`.
Tests using manually typed peer records alone missed the production behavior.
The classifier now prioritizes the exact registered tool identity, not nickname,
summary text, or a guessed session name.

Closing a peer now persists its terminal lifecycle without queuing ordinary
`child_completed` / `scatter_join_complete` turns. An abnormal lifetime
(`failed` / `interrupted`, including an orphan with no result file) retains one
child diagnostic but no scatter join. Ordinary background workers retain their
existing notifications; peer lifetimes neither block their joins nor inflate
their terminal-child count. Peer turn results continue to use the fleet round
gate. Its master-idle hook now accepts the actual turn runtime's resolved profile
at all three terminal call sites: deriving that profile from a bare TUI session
key previously stranded results that arrived while the master was busy.

TDD evidence: the four initial regressions failed before these repairs
(`/tmp/octos-peer-wake-red-v2.log`). The first typed terminal guard alone still
failed the real register/gather/close test; correcting the classifier made it
pass. Five final focused tests pass (`/tmp/octos-peer-wake-green-final.log`),
covering real registration and master liveness, close through both supervisor
hooks, persisted terminal plus new-store reopening, ordinary-worker joins, and
deferred unread results on a bare master session. The error-result variant uses
a staged result file, not a live provider error. Independent read-only review
found no new blocker within this fresh-path scope.

Upgrade boundary: already queued legacy continuations are **not retroactively
purged**. The old supervisor metadata lacks authoritative original tool identity;
the separately persisted per-session task ledger has it, but a safe cross-ledger
scope-checked migration is not part of this change. Names alone are insufficient
grounds to discard work. The pre-existing crash window between in-memory fleet
synthesis enqueue and durable synthesized marks is also not claimed repaired.

The next live run, `/tmp/octos-peer-wake-final-20260904.uvgEr7`, used an isolated
`OCTOS_HOME`, a new store, and newly built pinned backend/client binaries. It ran
20:30–20:39 PDT through all 30 scenarios, five compactions, client restart, three
daemon restarts and K3 → GLM-5.3 → K3. The driver again reported `FAILS=0`, but
the unchanged independent gate **failed T14=2**, with every other expected reply
and the task-correlated background completion appearing once.

This time the actual foreground trace called `peer_handoff`, gathered a still
running peer, waited, gathered its finished result and answered without closing
the peer. Ledger seq 708 is that answer; seq 746 belongs to a distinct
`peer_fleet_synthesis` turn that explicitly says the result was already gathered
and repeats the T14 reply. Thus fixing explicit-close notifications is not a
complete consumed-result fix. The newly functioning bare-master idle edge also
needs to distinguish already answered results from unread results; suppressing
the idle edge would merely reintroduce lost wakeups. This run remains failed.

Both pinned binary hashes and the shared original profile hash match after the
run. Cache evidence has 68 manifests, 67 usage records, 65 with nonzero cache
reads and 1,264,128 cached tokens; the manifest-only privacy scan has zero test
nonce, fixture marker or proof/workspace path matches. The test processes were
stopped and its private profile copy removed; the original profile and active
operator session remain unchanged. No installation or commit was performed.

The first default-parallel CLI run additionally exposed test isolation failure:
`closed_peer_park_produces_neither_wake_nor_escalation_row` expected its baseline
plus one wake (2), but found 0. Multiple tests, including the newly added embedded
boot test, reset the entire process-global orchestrator while other tests use
it. The exact winning reset is not identifiable from that log. All six unsafe
reset calls and their now-unused test-only helpers have been removed; affected
tests use dedicated profiles/sessions. Default-parallel CLI all-targets then
passed 3,255 tests, zero failed, nine ignored, 21 suites
(`/tmp/octos-peer-wake-cli-full-isolated.log`); OctosCode all-targets passed 2,171,
zero failed, four ignored, 33 suites (`/tmp/octos-peer-wake-client-full.log`).
This isolation cleanup does not change production behavior. Neither passing
suite overrides the remaining live consumed-result failure.

### Successful foreground peer-result consumption

The open-peer duplicate now has its own repair, distinct from lifetime terminal
notifications. A turn's `peer_gather` callback captures only complete, owned
result snapshots, using the round in the exact returned result header plus a
SHA-256 digest. `result.md` is published before its version index, so consulting
that later index could acknowledge the wrong round. Readiness now also prefers
the published header round. Sibling reads remain allowed but cannot acknowledge
the owner's work; capped or legacy/unidentifiable results are not guessed complete.

A successful actual terminal, nonempty answer and canonical persisted-final
identity are all required before committing the captured receipts. The terminal
and receipt write share the existing continuation-admission registry lock; no
provider call is awaited under it. Error, interruption, empty output and missing
final persistence leave results eligible. A later unseen result differs from the
captured snapshot and still wakes the master. The new bounded, atomically merged
per-master `.consumed-*` record stores rounds and digests, not result bodies;
it does not reinterpret `.synthesized-*`, whose existing meaning is enqueue state.

An already queued fleet synthesis is retired through the existing durable
continuation-completion API only when every specifically requested peer is
authoritatively owned and either its current result is consumed or its explicit
close marker names that owner. Unknown/missing ownership, missing request identity,
new fleet members, live peer turns and pending input prevent this no-op. Ordinary
background notifications are unaffected.

TDD evidence: the initial foreground-gather case failed before the repair;
review probes separately failed for gather-then-close with synthesis already
queued and for an exact current result after lower-round restaging. All nine
new cases pass, including an actual interrupt winning `try_emit_terminal`,
concurrent receipt merges, unseen newer results with a lagging version index,
nonowner/truncated reads, and boot evaluation after rereading receipts. The queued
runner test asserts zero provider calls and an empty queue; it does not itself
reopen SupervisorStore. Focused peer tests pass 154/154 and terminal-integrity
tests 9/9. Logs are `/tmp/octos-peer-consumption-{red,review-red,final-green,
peer-regressions,terminal-regressions}.log`. A second independent read-only
review found no new blocker in the repair. Production and tests were frozen
before the next pinned build.

The earlier limitations of `.synthesized` enqueue/crash recovery and legacy
misclassified continuation migration remain explicit. Also not newly claimed
fixed: externally deleting/recreating a same-slug peer at a lower round while
another peer preserves old `.synthesized` marks. Normal named staging rejects an
existing slug and close does not delete that directory.

The next pinned-binary validation, preserved at
`/tmp/octos-peer-consumed-final-20260904.BB5vx6`, **was not accepted**, even
though the unchanged 30-scenario driver and once-only reply gate passed.
T14's foreground turn gathered two pending snapshots, then an argument-only
preexecution guard blocked its third `peer_gather` and persisted a fabricated
`[DOOM LOOP DETECTED]` answer as Completed (assistant seq 759). A later distinct
fleet turn actually gathered the completed result and supplied the expected
T14 answer once. The receipt mechanism correctly left unread completed work
eligible; the remaining defect was the Agent's interpretation of asynchronous
polling. A supplemental terminal-integrity gate rejects controller-generated
doom/cycle/Session Summary answers, errors, and foreground completions without
a persisted answer after their final tool. It rejects this trace, without
relaxing the existing expected-reply gate.

That failed run still supplies bounded evidence: five compactions, client and
three daemon restarts, K3 → GLM-5.3 → K3, 68 usage reports with 67 nonzero cache
reads totaling 1,321,472 cached tokens, and zero fixture nonce/marker/workspace
matches in the cache manifest. The independent localhost HTTP fixture passed
all 12 cases (68 requests): normal text, reasoning-only responses, output
truncation, bounded empty-output recovery/exhaustion in both API styles.
Those checks do not turn the live soak into a pass. Additional peer probes were
not run on this failed binary. Isolated processes were stopped, private profile
credentials removed, and original profile/binary hashes verified unchanged.

### Asynchronous peer polling and ACP test lifecycle follow-up

`peer_gather` and `peer_list` now bypass arguments-only preexecution doom/cycle
decisions. Their actual returned snapshots still enter result-aware history:
three unchanged reads request a waiting-aware, tools-disabled convergence
reflection, not a fabricated final or an abort. Changed output resets the
unchanged-result threshold. The checkpoint remains transient working memory,
with its real usage counted, and normal action calls resume afterward. Mixed
mutating-tool cycles remain protected because peer result hashes are retained
in cycle history. Generic non-peer synthetic hard-stop paths are explicitly
outside this bounded repair; this is not a claim that all guards were migrated.

Behavior-first tests using the real peer tools with scripted provider calls
failed 3/3 before the repair and pass afterward for changed third reads,
unchanged-result reflection followed by a genuine final, and progress resetting
the threshold. Including detector protection/reset tests, focused GREEN is 5/5;
existing loop-detector tests pass 24/24 and doom-guard tests 3/3. Evidence:
`/tmp/octos-peer-polling-{red,final-green,loop-detector-regressions,doom-regressions}.log`.

The preceding CLI all-target run also exposed two ACP integration test lifecycle
failures when reopening the same episode database. The tests used an SDK helper
whose foreground completion dropped the server actor before its normal async
cleanup; these were same-runtime transport simulations, not proof of a normal
process-exit leak. The two tests now own a separately spawned server transport
and await its JoinHandle before constructing the next factory. History and
replay assertions remain, with no sleeps or retries. Seven parallel integration
tests passed, followed by ten repeated seven-thread runs (70/70), strict scoped
clippy, fmt and diff checks. Only the integration test file changed for this
finding; detached-pump lifecycle guarantees beyond this evidence are not claimed.

The frozen polling repair was subsequently validated with pinned binaries in
`/tmp/octos-peer-polling-final-20260904.Pj9pLl`, using the unchanged 30-scenario
soak plus both independent gates, four stricter peer lifetime probes (including
a delayed peer and a cold daemon restart), and the 12-case localhost fixture.
The additional probes rejected that build, as documented below.

Frozen-build automated validation now passes: Agent/LLM 3,568 tests (48 ignored,
62 suites), CLI all targets 3,264 tests (9 ignored, 21 suites), strict
Agent/CLI all-target clippy, CLI no-default-features check, fmt and diff checks.
CLI doctests have four pre-existing ignored examples and no failures. The
unchanged OctosCode binary matches the separately validated client build
(2,171 tests, 4 ignored, 33 suites). The macOS debug build emits only the known
oversized DWARF unwind-table linker warning. Workspace-wide optional llama/CUDA
targets are not covered by this scoped build matrix.

The fresh localhost HTTP run passed all 12 cases (68 requests). An independent
audit of actual OUP ledgers and session rows, ignoring the runner's pass flags,
verified four genuine Completed terminals, six reasoning-only/exhaustion
runtime errors, and two output-truncation errors. The two truncated text cases
persist the exact provider partial once before Error, never Completed. Successful
recoveries have one final and account for both provider calls. Terminal identity,
persist-before-terminal order and committed-row references all passed; these
fixtures do not claim broad streaming-error/cancellation accounting coverage.
Evidence is `provider-ledger-audit.json` in the fresh proof directory.

Independent inspection of the fresh real T14 trace confirms the behavioral
checkpoint, not merely a matching sentinel. In thread
`01a06fc8-0256-77a3-8c93-cd73e12804ad`, the first three `peer_gather` executions
returned pending snapshots (completed seqs 681, 692, 703), followed by checkpoint
1 at seq 707. The model made one further fresh pending read, then executed
`bash {"cmd":"sleep 10"}` (10,021 ms), gathered a completed result (seq 745),
and persisted its genuine final once at seq 770 before terminal seq 771. The peer
itself read the actual README file and completed once. The round-1 consumed
receipt digest matches its current result bytes. Thus this run proves a real
reflection followed by bounded waiting and fresh evidence, not a claim that
the controller enforces a sleep or eliminates every possible busy-poll pattern.

### Additional real UI and cold-restart rejection (2026-09-04)

The fresh `Pj9pLl` run finished the original 30 scenarios at 21:22:03 PDT,
with both then-current ledger gates passing: all expected replies once, no
controller-generated final, no error, and no foreground final missing after
its final tool. Five compactions completed. Cache evidence at that boundary:
72 manifests, 71 usage reports, all 71 with nonzero cache reads totaling
1,405,760 tokens; zero fixture nonce/marker/workspace plaintext matches in the
manifest. A later independent TUI capture audit, excluding wrapped echoed user
prompts, also confirmed all 29 expected ordinary-turn replies were visible.

**The overall run was nevertheless rejected by its extra probes.** P31
deliberately delayed a peer by 15 seconds, gathered its actual result, kept it
open, then cold-restarted the daemon. Both the pre-restart capture
`captures/212316-P31.txt` and post-restart quiet-window capture
`captures/212338-P31-quiet-window.txt` lack the genuine assistant final; its
sentinel appears only inside the echoed prompt. The backend had persisted
three distinct assistant messages at seqs 2352–2354 with different message IDs
but the same `assistant_segment_id`, ending in `:assistant:4`. The first
preamble finalized that identity, causing the client to ignore the real final.
The commit observer had assigned every end-of-turn batched row the latest
stream phase instead of that row's own producer identity. This is a producer
contract violation, not a reason to disable client duplicate suppression.

P32 exposed a separate cold-restart defect. The transport remembered only the
most recently opened peer and reopened it without the main session. The next
main turn was admitted without its former cwd binding, writing a new bare-key
ledger from seq 1 instead of continuing the prior scoped ledger after seq
2356. The peer's second result was genuinely read; its parent final landed at
the wrong stream's seq 249, followed by terminal 250. The client remained
Working because it correctly rejected the lower-sequence stream. The test
driver was stopped after this confirmed failure; P33/P34 were not run.

Evidence is retained in `Pj9pLl/FAILED.md`, `tui-answer-audit-FAILED.json`, and
`terminal-integrity-FAILED.txt`. The strengthened gates now also reject reused
segment identity across distinct canonical messages, a split main stream,
nonmonotonic per-stream cursors, and missing visible TUI answers. They reject
this proof without counting prompt text as an answer. The isolated processes
and private profile credential copy were removed; original profile and pinned
binary hashes were verified unchanged. Repair and fresh acceptance remain in
progress; the preceding main-soak success must not be reported as an all-clear.

### Producer identity and multiplexed scope recovery repair

Assistant identity now originates beside the Agent's append-only output log:
each assistant message index carries its actual producer iteration. Live token
events preserve that iteration, and the canonical observer receives an explicit
per-message projection override. Neither dropped progress, hidden reflection,
skipped user rows nor end-of-turn batching can renumber these identities.
Tool-free steer answers, marker-filtered voice deltas and typed partial responses
use the same correlation. An equal-text earlier answer cannot suppress a later
final. A controller-authored final following a visible same-iteration preamble
gets a distinct final suffix; this does not remove the separately documented
generic synthetic-stop behavior. Uncorrelated canonical writers receive unique
durable-row identities, not a guessed live segment. Native attachment ownership
is captured durably and recovered from thread watermarks after ring eviction or
restart; an attachment without evidence of an assistant owner has none.

The client now retains every acknowledged session scope and reopens them in
order after reconnect, including the master and open peers. Deferred scoped
requests resume only after every required acknowledgement. Hydrate preserves
the confirmed profile/cwd/topic/sandbox; an unknown hydrate first performs an
explicit scoped open at its original FIFO position. Failed opens do not confer
scope authority, and failed multiplex recovery remains blocked until a
corrective open succeeds. No cursor rejection or duplicate barrier was weakened.

The backend independently rejects a cold turn with known scoped ledger history
but no live runtime binding using `session_open_required`, before provider calls
or new bare-ledger writes. An implicit no-cwd open cannot bypass this check.
Only an explicit authorized open restores the original scope: the server does
not infer cwd or reconstruct ephemeral sandbox restrictions from historical
metadata. Fresh unopened sessions with no scoped history remain supported.
Already-corrupted ledgers and other raw review/headless entry points are not
claimed migrated by this bounded guard.

Behavior-first evidence: the actual Agent/bounded reporter/forwarder/observer
batch cases failed before the identity fix. The expanded CLI `should_` set now
passes 862/862, attachment cases 9/9, Agent terminal integrity 12/12, and steer
and typed-partial provenance cases pass. Strict all-target CLI clippy passes.
Cold-scope cases pass 5/5 and existing session-open cases 23/23. The client's
complete matrix passes 2,176 tests (4 ignored), with strict clippy, fmt and diff
checks. These are scoped results, not yet fresh combined live acceptance.

A separate real tmux/stdio TUI fixture reproduces the old failure with only a
localhost provider and harmless `read_file` tools. OpenAI and Anthropic
streaming and their two nonstream fallback lanes each persist three exact nonblank
answers with colliding IDs: the genuine final is absent live but appears once
on cold replay without another provider call. Alphanumeric answer sentinels
avoid Markdown formatting ambiguity; echoed prompts are excluded. Evidence is
`/tmp/octos-segment-tui-preflight-20260904.O3CIGT/red-v2/results.json` and
`red-anthropic-fallback/results.json`. All four lanes pass against the frozen
`TlCAyv` pair: live and cold replay final counts are each one, replay makes no
provider calls, streamed IDs match committed IDs, and terminal references name
the exact committed final. `green-v1/results.json` records 48 fake-key localhost
requests, 12 actual successful tools, four Completed and zero Error terminals.
Both binaries' hashes remain unchanged. The separate 12-case HTTP terminal
fixture and independent durable-ledger audit also pass on this build.

Full 30+4 remote-provider acceptance was **not launched** on `TlCAyv`:
independent review identified a concrete attachment-owner crash window between
the durable assistant append and its second watermark write, and the full CLI
matrix found a serve-lock guard lifetime failure during parallel execution.
These are being addressed before a fresh pinned build. The next reserved proof
is `/tmp/octos-canonical-crash-final-20260904.K1xP8B`; no new all-clear is asserted.

The serve-lock failure was reproduced deterministically without fork timing:
holding a duplicate file descriptor across the guard's drop kept the exclusive
flock alive, preventing immediate reacquisition. The guard now explicitly calls
the fully qualified `fs2::FileExt::unlock` on drop. Its new regression also proves
that closing the stale duplicate does not release a subsequently acquired guard.
RED is 0/1, GREEN 1/1, and all 23 serve tests pass; an independent read-only
review found no new blocker. Evidence is `/tmp/octos-serve-lock-{red,green,
regressions}.log`. Descriptor inheritance during parallel subprocess creation
is consistent with the original matrix failure, not a claim that a particular
child was traced. No retries, sleeps, global serialization, lock-marker changes
or teardown-order changes were introduced.

Cold attachment-owner recovery now reconciles acknowledged watermark metadata
with actual retained native assistant sources from disk, even when the session
is absent from the hot ring. A separate optional source sequence distinguishes
newer durable evidence from old retained rows. The allocator's reserved next
sequence and completed barrier remain authoritative; an append that never
landed cannot invent an owner. Older watermark files remain readable. This disk
scan occurs only on recovery of an existing watermark, not per token or for
brand-new turns. Owner regressions pass 24/24, watermark cases 7/7, and strict
clippy passes; independent read-only review and six directly rerun recovery
cases found no blocker.

### Fresh combined build: visible answers fixed, peer restart still rejected

The next immutable pair at
`/tmp/octos-canonical-crash-final-20260904.K1xP8B` passed the same four real-TUI
localhost lanes again (`green-final-v2/results.json` under the preflight proof),
and all 12 HTTP terminal cases plus independent ledger audit. The automated
matrix passes: core 392 tests (1 ignored), Agent/LLM 3,570 (48 ignored), CLI all
targets 3,288 (9 ignored), OctosCode 2,176 (4 ignored), strict all-target clippy,
no-default CLI check, builds, fmt and diff checks. CLI doctests have four
existing ignored examples. Optional workspace llama/CUDA targets remain outside
this scoped matrix; the known macOS debug unwind-table linker warning remains.

Real tmux testing ran all original30 and four extra prompts. Both drivers
exited zero, all 37 expected assistant-only TUI answer captures passed, native
stream/canonical identities matched, and no main scope split or cursor rollback
occurred. P31's delayed peer actually slept 15,047 ms, read README, and supplied
one visible final before and after cold restart; the main and peer scopes both
reopened. P32 actually reused that peer for a new src/sample.rs round and its
consumed receipt matches the new result bytes. A brief upper Working label in
immediate captures was independently traced to the nominal two-second whole-job
sampler; the 20-second quiet captures and final frame clear it. No persistent
Working or blocked queue was observed on this build.

**The run nevertheless failed semantic acceptance.** Original T14 and P31
`peer_handoff` lifetime descriptors remained nonterminal while their peers were
kept open, even after real peer work completed. The liveness lease exists only
inside one process. On restart the generic orphan sweep lost that lease and
classified the descriptors failed, queuing spurious child-completed warnings:

- after T23 client restart, completed T14 peer task
  `01a06ffe-45f4-75b2-a811-81ec20212b12` generated an extra main answer at seq1706;
- after P31 cold restart, completed `audit-open` task
  `01a07005-27b5-7000-91ba-693c34582f0b` generated an extra main turn starting at
  seq2283 and an orphan warning at seq2327, inside P32's quiet window.

The strict P32 gate correctly rejects two starts/two terminals instead of one.
The original expected-answer and structural gates had missed the first extra
warning because it was a model-written answer with valid identity and terminal
order. Thus original30 must not be called a full semantic pass either. A new
acceptance-only check inspects typed supervisor failed-child terminals for this
all-success workload, rather than keyword-matching warning text. It rejects
both failures on this preserved proof. Real orphan/failed-child reporting must
not be globally suppressed to satisfy the test.

Evidence is `K1xP8B/FAILED.md`, `peer-probe-strict-FAILED.txt`,
`terminal-integrity-v2-FAILED.txt`, `peer-probe-all-windows.json`, supervisor
events, and immediate/quiet captures. Five compactions and model switching did
run. Full-run cache evidence: 86 manifests, 85 usage reports, 81 nonzero cache
reads totaling 1,612,032 tokens, with zero fixture privacy needles in the
manifest. These bounded successes do not override the semantic rejection.
Isolated processes were stopped, the private provider profile copy removed,
and shared profile/pinned binary hashes verified unchanged. The next reserved
proof is `/tmp/octos-peer-restart-final-20260904.JQzadg`; durable peer-lifetime
restart recovery and fresh acceptance remain in progress.

A shorter independent localhost regression now reproduces this same lifecycle
failure through real tmux/stdio OUP without remote provider credentials:
`/tmp/octos-peer-restart-preflight-20260904.LSa0Pn/red-v1/result.json`.
Both actual peer rounds read their expected files and return the correct
parent answers, but restart creates a third unsolicited main turn and typed
`peer_handoff` failure. Its fake provider returns a distinct diagnostic for
unexpected autonomous input, preventing the extra wake from hiding inside
matching final text. The pinned old pair remains hash-identical. This RED will
be rerun against the fresh repair before the full live-provider soak.

### Typed peer lifetime repair and retained-client replay regression

The backend now persists a generation-bound peer lifetime separately from a
worker lifetime. Only an exact owned binding with a completed turn and matching
durable result digest can restore an idle lease before the generic orphan sweep.
New input invalidates that receipt before queueing; stale completion, unfinished
work, real failure and legacy unproved state cannot certify idle. Ordinary worker
orphan reporting is unchanged. Eight focused restart/factory regressions pass,
as do the fresh Agent/LLM matrix (3,570 passed, 48 ignored) and CLI all-target
matrix (3,296 passed, 9 ignored).

The immutable intermediate backend at
`/tmp/octos-peer-lifetime-candidate-20260904.jKk9nV` passes the independent real
tmux peer-restart lifecycle probe in `LSa0Pn/lifecycle-candidate-v1`: exactly two
parent turns and two peer turns, two real file reads and owned result gathers,
zero typed failures and zero unexpected wakes. Fake authentication and unchanged
binary hashes are verified. **The combined probe is still rejected:** its old
client duplicates the first answer after retained-client daemon reconnect.

A new client lifecycle regression reproduces that duplicate. Live rendering
aggregates multiple assistant segments into one message, while hydration
returns separate canonical rows, invalidating the immutable scrollback prefix.
The same probe also exposes a duplicated user prompt during live tool/final
flush, before restart. The next acceptance captures full available scrollback
with an isolated 50,000-line tmux history and counts every matching user anchor
and assistant answer, not only the last matching prompt or a 200-line tail.
These client repairs and the fresh complete soak remain in progress; backend
lifecycle success alone is not overall acceptance.

The live prompt duplicate is now traced to two delayed empty-history hydrates
arriving after optimistic submit and the first tool start. Hydration removed
both the local user row and its unconfirmed tracking; the later canonical user
echo then printed it again. The exact recorded ordering has a failing unit
regression. Review also confirms that an old running-turn hydrate can reach the
store after a newer terminal, so completed-answer preservation needs its own
race test. These are not resize artifacts or provider-generated duplicates.

The new full-scrollback audit additionally checks every earlier sentinel in
later captures, not only the newest turn. Historical absence after a fresh
client is allowed; duplicate known prompts or answers are not. On the new
backend, core 392/1 ignored, strict clippy, no-default CLI check, build, fmt and
diff checks pass; CLI doctests remain four ignored. The 12 fake-auth localhost
HTTP cases pass with 68 requests and an independently passing durable-ledger
audit under `JQzadg`. The hydration-only client matrix is 2,178 passed / 4 ignored,
but this predates the delayed-hydrate prompt repair and is not final acceptance.

Applying the stronger history gate to the preserved `K1xP8B` captures finds 15
bad frames: for example P32's own current-answer check passes while older T30
and P31 each appear twice as prompts and answers. Thus the earlier 37-capture
success was only the old last-anchor/window check, not proof that the entire
visible history was duplicate-free. Evidence is
`JQzadg/tui-audit-prior-proof-red.json`; five gate regressions pass, including an
old answer duplicated without its prompt anchor and wrapped prompt text that
must not count as assistant output. The short peer probe uses new `audit_v2.py`
and `peer_restart_v2.py`, preserving its original audit/report files unchanged.

The hydration/scoped optimistic/late-terminal candidate then passed 2,181
client tests (4 ignored), strict clippy, build, fmt and diff checks. Its
post-terminal repair permits one automatic refresh per contradicted scoped turn
and connection epoch; repeated plain/context stale results cannot form a request
loop. Explicit rollback and terminal pruning do not resurrect withdrawn input.
The guard requires turn metadata; omitted-turn snapshots are not covered by a
blanket stale-snapshot claim. Independent source review found no functional
blocker, with a nonblocking current-epoch latch-size limitation recorded.

**Real TUI still rejects that candidate.** Client SHA-256 `7c71a1a8...9591`,
pinned in `JQzadg`, passes every original short peer-restart gate in
`LSa0Pn/final-peer-restart-v1`: 14 fake-only HTTP calls, exactly two parent and
two peer turns, correct file reads/gathers/results, zero typed failure or extra
wake, stable scopes/cursors, and each actual final once through daemon restart.
But all five strengthened prompt gates fail: the first prompt appears twice
adjacently (first-terminal lines 62/63), while the second prompt appears once.
Restart adds no further copies. The new wire orders both empty hydrates after
the first assistant delta but before tool start; this remaining cadence is
being reproduced separately, not assumed to be the already-fixed ordering.
The 30-turn remote-provider driver was never started, the isolated profile copy
was removed and hashes verified unchanged. The next reserved proof is
`/tmp/octos-native-hydrate-final-20260904.Y5PjxL`; its client is not yet pinned.

A fake-only temporary diagnostic build then proves the adjacent duplicate is a
logical reflush, not physical terminal retention. The native tracker first sees
`[system,user]` and inserts one user row; after hydration it receives an explicit
committed-only reset and sees `[user]`, inserting that row again. Terminal size
is unchanged and no menu toggle occurs. The hidden system row is synthetic
connection text inserted by `protocol_snapshot_from_launch`, not canonical
conversation. Diagnostic evidence is
`LSa0Pn/adjacent-diagnostic-v1/DIAGNOSIS.md` and the temporary count/hash trace;
the instrumented binary is separately identified from its launch wrapper.

The narrow producer repair removes that synthetic conversation row while
retaining the existing snapshot connection/read-only status. A regression now
starts with the actual transport bootstrap, submits a prompt, applies both
delayed empty hydrates, and checks native scrollback once. It fails before repair
and passes afterward (`/tmp/octos-client-bootstrap-prompt-{red,green}.log`).
Temporary diagnostic instrumentation was removed. Fresh immutable real-TUI
acceptance is still required; no passing unit result relabels the rejected runs.

### Full-history and repeated-summary failures exposed by the next long run

The final bootstrap candidate (client `963d1b39...94d76`, backend `41dad949...275d`)
passes all 51 exact-byte short peer-restart checks and all four streaming/
fallback TUI lanes, with fake-only localhost providers. The four lanes perform
12 actual reads, show each final once live and after cold replay, and make zero
provider calls during replay. Client all-targets passes 2,182 / 4 ignored.
These component successes remain valid but are not full acceptance.

The next real-provider run at `/tmp/octos-final-acceptance-20260904.zRLtcY`
started 23:26 PDT and was stopped at 23:32 after the following failures:

- Opening and closing manual compaction menus clears the visible screen and
  re-flushes the entire native transcript on each edge. The full-history gate
  finds every T01–T12 prompt and answer three times in
  `233008-after-manual-compact.txt`. T01 anchors are at lines 50/476/938. Earlier
  immediate turn captures are clean. This is a genuine reflush defect, not a
  duplicate server answer; it affects ordinary menu geometry, not a physical
  terminal resize.
- T12's actual final at seq1040 explicitly refuses to claim recall and says
  FAIL. The older substring gate incorrectly accepts a counterfactual quotation
  of the requested reply inside that refusal. T13's final at seq1266 likewise
  refuses; both turns completed normally. New `canonical_final_audit.py` follows
  the terminal's exact `session_result.message_id` and validates that committed
  final, never an earlier iteration or quoted substring. Five gate regressions
  pass and this preserved proof correctly fails. T05 also carries a harmless
  explanatory note about logical `/tmp` versus physical `/private/tmp`; the
  next fixture will request `pwd -P` to remove that command/path ambiguity.
- The refusal has a real context-loss cause. Summary `ctxitem_000041` preserves
  T01 and its nonce. Re-compaction replaces that body's contribution with only
  `> User: [Conversation summary]` in `ctxitem_000062`; manual summary
  `ctxitem_000070` repeats the loss. `ContextManager::for_prompt` wraps an
  installed typed summary as a User message; the extractive compactor takes
  that message's first line. Original data remains in append-only items/session
  rows but is missing from active model context. This conclusion follows typed
  durable item sets and deterministic projection, not an unrecorded raw
  provider-prompt claim. The nonce-bearing recall prompt is still not an
  independent memory measurement.

The driver, test client and daemon were stopped, copied provider configuration
removed, and shared profile plus immutable binary hashes checked unchanged.
T14–T30 and the four additional turns were not run. Ongoing repairs address
menu-only viewport clearing with native history preserved, and internally typed
prior-summary carry-forward with hard budget bounds, spoofed-user-marker tests
and repeated auto/manual/recovered compaction tests. Final acceptance remains
open; no failed proof or old gate result is relabelled a pass.

### Frozen menu and typed prior-summary repairs awaiting fresh acceptance

The menu fix now clears only the current viewport; it does not invalidate the
already committed native scrollback tracker. Real resize and genuine canonical
rewrite handling remain separate. The actual-draw regression reproduces seven
copies before the repair and one afterward. The final fake-only real-tmux proof
`/tmp/octos-menu-tui-preflight-20260904.DRPazW/green-verified` passes 15/15 checks:
23 full-history captures, nine tool-backed turns, three visible context/confirm
menu cycles, and bounded vacated space that subsequent output fills. Every old
prompt, final, and tail remains exactly once. The corresponding preserved old
binary run has seven copies of every old history item. Client all-targets is
2,183 passed / 4 ignored, with strict clippy, fmt, diff check and build passing.

The backend carries prior summary bodies using internal typed provenance:
`PriorCompactionSummary` indexes the post-repair prompt projection, and its
`PromptFrame` sidecar is serde-skipped. Only actual CompactionSummary source
items receive it; a user-authored summary-looking marker is ordinary input.
All AppUI auto/manual/loop and SessionActor summarizer paths share this helper.
The hard budget and Unicode clipping remain enforced. The soft target is
relative to the carried body plus remaining allowance, so a substantial old
summary does not starve newly fitting evidence. Repeated auto/manual compaction
with snapshot recovery, marker spoofing, nested headings, tiny budgets and new
evidence all have regressions. Agent compaction 39/39 and CLI compaction 34/34
pass. The full fresh matrix passes Agent/LLM 3,572 / 48 ignored, CLI 3,298 /
9 ignored, strict clippy, CLI no-default check, fmt, diff check and build;
CLI doctests have four pre-existing ignored cases.

The fresh final candidate is
`/tmp/octos-summary-menu-final-20260904.oK2bDV`, with backend SHA-256
`37e38378240a495648ca66e0cff8252bacf653606640cbc90306791847914907`
and client SHA-256
`a0dd3432cfdddfdc540e0ce3755b8de5888e3290efe7ddc1a74f14068ba937cc`.
Its exact-byte short proofs and long real-provider run are still in progress.
This is local macOS evidence, not a cloud Mac mini run. Separate fixes for
already-merged PRs #2239 and #2240 are being developed in isolated branches;
they are not silently included in these primary-worktree binaries.

### Singleton background wake and mixed-history replay still reject oK2bDV

The frozen candidate's real-provider driver ran the original 30 scenarios from
2026-09-04 23:59:35 to 2026-09-05 00:08:50 PDT, then four peer probes through
00:12:30. Original labelled reply/background/goal gates pass. T12 and T13 now
have exact successful terminal-referenced finals, and manual menu full-history
captures remain clean. Automatic and manual compaction, monitor lifecycle,
goal fleet acceptance, client restart, three model/reconnect restarts and
K3 → GLM-5.3 → K3 all execute. Latest typed compaction summary after reopening
still contains T01's nonce body; this is not a claim that raw historical
provider prompts were captured or that nonce-bearing prompts measure recall.

Broader acceptance still **fails**, for independently observed reasons:

- One `final-bg-marker` child queues ChildCompleted (supervisor seq3) and a
  ScatterJoinComplete with `terminal_children=1` (seq7). Processing both yields
  main admissions950/1010, finals997/1048 and Completed1007/1115 without new user
  input or another result. `000403-after-background.txt` shows two model-written
  announcements, the latter incorrectly calling the soak complete. This is a
  producer singleton-join duplication, not a duplicated canonical assistant ID.
  New `background_once_audit.py` rejects both redundant queue reasons and the
  extra autonomous admission. Six gate controls pass; preserved real evidence
  correctly fails. A narrow singleton-join repair is in progress, retaining
  per-child notifications and genuine two-or-more-child aggregate joins.
- P31's labelled final2259 says the peer is still running and promises to
  gather later. A normal later peer-result continuation gathers and returns
  AUDIT1. The old substring waiter accepts it, but exact labelled-final and
  one-main-turn-per-probe gates correctly fail. This is not an orphan-recovery
  failure. P32 also adds explanatory prose before its requested exact reply,
  so the stricter final gate rejects it; it is not a missing-final transport
  error. No failed output is relabelled a pass.
- After P31's daemon restart, `001031-P31-quiet-window.txt` contains T01–P31
  history twice. The second hydrated copy places background completion beneath
  T01, unlike its live T15 position. All later full-history captures retain the
  duplicate. This mixed-history recovery cadence needs its own reproduction;
  prior menu and fake-only peer component successes do not cover it.

The independent ordinary terminal/segment/scope gate has zero typed failures,
identity collisions, split main streams or failed supervisor children. That
does not negate the above failures. There are 85 provider-usage records, 83
with nonzero cache reads, totaling 1,642,496 cached tokens. Manifest privacy
needles (nonce, fixture markers, prompt sentinel and workspace path) all have
zero matches. Both drivers finished; isolated client/daemon stopped; the
private profile copy was removed; shared profile, user process and immutable
binary before/after checks are unchanged. No install or primary commit.

### Singleton enqueue repaired; combined reconnect proof finds a second client cause

The backend now queues ChildCompleted but skips ScatterJoinComplete for fewer
than two matching children. Genuine multi-child joins and peer/scope exclusion
remain covered. Selective durable recovery still tests two independent pending
singleton groups, completing one and restoring the other. Existing persisted
legacy singleton-join entries are not migrated. Independent source review and
the actual backend TUI proof pass; CLI all-targets is 3,300 passed / 9 ignored,
with strict clippy, no-default check, fmt, diff and build passing.

The first client repair separates three sequence domains: session message
position, child-thread envelope sequence and ledger cursor. Exact canonical
message identity anchors a background projection to its hydrated row; unmatched
envelopes retain arrival order without deleting distinct equal-text rows. Five
new RED/GREEN regressions pass, and all-targets is 2,188 passed / 4 ignored with
strict clippy, fmt, diff and build passing.

The resulting immutable candidate is
`/tmp/octos-background-order-final-20260905.MCTTLk`, backend SHA-256
`fb09be1043117b69c138907f6974f05bc642a851e2986c56e17e7ce1c2b5d31c`,
client SHA-256
`f2a22ec16ed798bf0a0152e02779a90b80291c6db2e9b6fe68f179e0f717610f`.
Its exact-byte peer restart passes 51/51, four streaming/fallback lanes pass,
and 12 HTTP fault cases pass an independent durable-ledger audit. However,
combined real-tmux background/history/restart acceptance **fails 19/20** at
`/tmp/octos-singleton-bg-preflight-20260905.64Lyz1/combined-final-v1`.

The same background event is delivered live and after reconnect with identical
child thread, thread seq1, cursor148 and canonical message ID. The reducer adds
it twice. The second card appears at lines158–162 in `restart-quiet.txt`, then
canonical hydration removes that duplicate suffix, causing a full native-history
reflush from line163. The cold background card is now in the correct position
after its parent (line238), confirming that ordering was a real but incomplete
repair. Every prior prompt/final appears twice and the child value three times.
Six canonical main starts/completions and the single ChildCompleted queue remain
correct; there is no additional singleton model wake. Further identity-safe
client replay repair is in progress. This candidate is rejected before copying
any private provider profile or starting another real-provider 30+4 run.

Separately, fixes for already-merged #2239 and #2240 are published in isolated
follow-up PRs [#2261](https://github.com/octos-org/octos/pull/2261) and
[#2262](https://github.com/octos-org/octos/pull/2262). They are open, not merged,
and not silently included in these primary-worktree binaries. Their PR reports
distinguish focused passing tests, upstream full-suite failures, and platform
validation limitations.

### Client replay ownership repair and broader acceptance preparation

The follow-up client fix remains client-local: shared `octos_core::Message` has
no canonical background message ID, so a sidecar records the exact typed
`(session, message_id)` plus its current displayed row index and full row
signature. This is ownership of a currently retained row, not an ever-seen
receipt cache or content-based dedupe. Both native and legacy background paths
consult the same ownership record. Hydration rebuilds it from typed canonical
rows/envelopes, duplicate exact IDs inside one hydrate are inert, and empty IDs
or distinct equal-text IDs are not globally collapsed. Known optimistic
insert/remove operations shift ownership alongside their real message rows;
rollback and session removal release obsolete ownership.

Independent review also found a snapshot restoration edge before final freeze:
pruning against a snapshot that temporarily omits an optimistic prefix drops a
retained background owner before the prefix is restored. Merely moving pruning
later would shift an old index twice. A separate RED regression now covers
restoring the optimistic prefix before rebinding old exact-slot ownership. The
full matrix and immutable combined real-TUI proof must still pass; this design
description does not supersede the rejected MCTTLk result.

Fresh acceptance preparation is at
`/tmp/octos-background-replay-final-20260905.mjh35K`; its private provider profile
has not been copied and no real-provider long run has started. An additional
`all_admissions_audit.py` now checks every main admission, including unlabelled
goal/background continuations, for canonical final/terminal identity, explicit
turn-identity consistency, and ordered tool start/completion pairs. Independent
review produced four false-pass counterexamples, retained as RED tests, before
repair. Its 18 controls and the existing 25 acceptance controls pass. Preserved
oK2bDV correctly passes only this narrow terminal gate with 38 admissions while
remaining rejected by wake-count, exact labelled-final and native-history gates.
Valid extra autonomous turns still require an independent audit of their durable
cause; this helper does not infer that a successful terminal authorizes a wake.

### Activity archival remains a separate native-history replay cause

The frozen client replay-identity repair passes 41 focused tests and all-targets
2,197 / 4 ignored, plus strict clippy, fmt, diff and final build. Its SHA-256 is
`cb6438fef001f0e6be94e292c1794125c61213d44d57d16cc7cec266b51979d4`,
paired with the unchanged `fb09be104...b5d31c` backend in mjh35K. Exact-byte peer
restart passes 51/51, all four streaming/fallback lanes pass, and the fresh 12
HTTP fault cases pass independent durable-ledger audit (68 fake provider calls).

Combined real-tmux proof
`/tmp/octos-singleton-bg-preflight-20260905.64Lyz1/combined-replay-final-v1`
nevertheless **fails 19/20**. The exact background projection really is replayed
twice; the second pre-hydrate card is now suppressed, proving the intended repair
is exercised. Yet `restart-quiet.txt` has T01 anchors at61/158 and the background
value at156/254: unchanged dialogue still reflushes as a whole.

Independent capture/source review identifies another layer: the parent already
has an archived Spawn tool record when its background child finishes. That late
completion leaves a Progress item bound to the parent. A replayed ToolEnd can
mutate the last item sharing the tool-call ID regardless of activity kind, and
hydrate's completed-turn capture replaces the archived parent log with residual
late activity, losing the original Spawn row. The committed fingerprint includes
all archived activity data; the same message count and same activity-log count
with changed activity content falls through to a full native-history reset.
The old late-activity exception only recognizes log-count growth and is limited
by eight retained live-turn coverages, so removing that one count comparison is
not sufficient for the long-history workflow.

mjh35K is rejected without copying private provider configuration or starting a
real-provider long run. The next scoped repair must preserve archived activity,
distinguish Tool and Progress identities sharing a call ID, and display genuine
late activity without repeating unchanged dialogue. Actual message rewrites,
rollback and session-switch behavior must remain functional. Unit or component
successes do not supersede this failed combined proof.

### Scoped activity archival and native-history repair (2026-09-05)

The rejected `mjh35K` client was also exercised with ten additional tool-backed
foreground turns between background completion and daemon restart. This moves
the parent beyond the eight-entry completed-live cache while retaining its
archived activity. The independent fixture
`/tmp/octos-singleton-bg-preflight-20260905.64Lyz1/aged-replay-red-v1`
fails 25/26 gates: 43 native-history markers are each present once before the
restart and are duplicated afterwards. All 16 canonical main admissions remain
correct. This is additional RED evidence, not an acceptance pass.

The client repair now merges archived activity by typed identity, retains the
original Spawn record, and scopes tool updates and terminal activity capture to
the owning session and turn. Typed background activity uses its canonical message
ID; untyped equal text is not a global deduplication key. Message-prefix hashes
detect real dialogue rewrites independently of late activity. Retained archived
activity coverage survives the shorter completed-live cache, while separately
tracking whether the summary footer was actually displayed. Moving an activity
anchor when its first assistant answer arrives must not reprint the old tool or
footer, and running tool output must not be acknowledged before rendering.

The first complete test run exposed a valid unanchored-activity integration
contract; the assertion was preserved and exact session/turn-owned rendering
restored. Unknown unanchored logs and zero-history frames must not acknowledge
content that has not been displayed. The final independent bounded review
approved these slices. Full client all-targets v2 passes 2,207 tests with 4
ignored across 33 suites; strict clippy, fmt and diff checks pass. Logs include
`/tmp/octos-client-activity-final-all-targets.log` (rejected first run) and
`/tmp/octos-client-activity-final-all-targets-v2.log` (successful final run).

Fresh immutable-pair acceptance is being prepared in
`/tmp/octos-activity-replay-final-20260905.If6Q18`. The unchanged backend is
`fb09be104...b5d31c`; the final client build, original and aged background real-TUI
proofs, peer restart and streaming/fallback checks, then the original real-provider
30+4 run remain required. No private provider profile has been copied to this
candidate and no new real-provider long run has started at this checkpoint.
All earlier rejected candidates and broader accounting limitations remain in
effect; source-level repair and passing component tests are not final acceptance.

Subsequent immutable-pair checkpoint: client SHA-256
`2ac7cb1d3ce7df5182980e3ff6ffa52f7b576cd2bb04208d63167f45344c41ba`
was built last and source/spec manifests verified unchanged. The unchanged
original background fixture now passes 20/20 (`combined-activity-final-v1`),
and its separately extended aged-parent fixture passes 26/26
(`aged-activity-final-v1`). Each actually receives the same canonical background
projection twice after reconnect, yet retained Spawn, prompt, final and child
card markers remain once. The aged proof preserves all 43 markers through
restart and 45 after its final foreground ping. Six and sixteen canonical main
admissions respectively pass independent final-pointer/tool-pair audit. Both
fixtures completed their quiet windows and cleaned up their own processes.

The same bytes pass peer restart 51/51 and four streaming/fallback lanes under
`activity-replay-final-v1` in the established LSa0Pn/O3CIGT fixture roots. The
four lanes perform 48 fake HTTP calls and 12 actual file reads, with zero provider
calls during replay. These are real local tmux/client/daemon tests backed by fake
providers, not real-provider soak evidence. After all short gates passed, the
isolated real-provider 30-round run in If6Q18 started at 2026-09-05 08:36:45 UTC;
the four additional peer rounds and full independent acceptance gates remain
pending. Only a permission-restricted private profile copy is used; the installed
programs and existing user sessions are not replaced or restarted.

### If6Q18 real 30+4 result: durable gates pass, native history still rejected

The exact `fb09be1043117b69c138907f6974f05bc642a851e2986c56e17e7ce1c2b5d31c` /
`2ac7cb1d3ce7df5182980e3ff6ffa52f7b576cd2bb04208d63167f45344c41ba` pair ran locally from
2026-09-05 08:36:45–08:46:31 UTC for the original 30 rounds, then
08:48:16–08:52:03 UTC for the four peer probes. Both drivers report `FAILS=0`.
Independent durable audit passes all 36 main admissions and 33 exact labelled
terminal-pointer answers. The only three unlabelled main turns are one actual
ChildCompleted update, the initial goal planning turn, and the actual successful
fleet-task wake. Peer lifetime/result/consumed-receipt digests match; close and
the measured restart/quiet windows do not introduce another model admission.

The final native-history gate nevertheless **rejects this candidate**. After
P31's daemon-only restart, full capture `014943-P31-quiet-window.txt` contains
T01 prompt anchors at lines49/219 and P31 anchors at199/380, with their genuine
assistant replies also twice. This repeats through P32–P34 and the final cleanup
capture: eight FULL captures fail. The second copy loses the recent T27/T29/P31
tool groups while keeping their final replies; background card ordering remains
correct. Thus correct canonical persistence and zero driver failures are still
not sufficient to accept the client rendering path. Both independent reviewers
confirmed the same failure, and all rejected bytes/captures are retained.

An audit-only repair made the capture contract explicit before this verdict.
`snap(..., 0)` captures only the visible pane and can start in a wrapped user
prompt; `snap(..., back > 0)` captures full native history with `-S -`. The old
helper misclassified the initial user-prompt suffix in the visible-only
client-relaunch snapshot. Its original helper, failed row and screenshot remain
preserved. The repaired classifier uses exact driver-declared labels, never
content-based guesses: all 37 required current-turn/peer-quiet slots must be
FULL, unknown labels fail closed, and known viewport snapshots are separately
reported as nonacceptance diagnostics. The strict full-history parser is
unchanged, including rejection of duplicate old answers without their user
anchor. Twelve TUI controls / 50 total helper controls pass, while the real
runtime correctly remains RED: 44 FULL captures, 14 viewport diagnostics,
zero unknown classifications.

Cache observations are 83 manifests and 82 usage records, with 79 nonzero cache
reads and 1,570,560 reported cached tokens. All nonce, fixture marker, prompt
sentinel and workspace-path manifest scans are zero. The unpaired manifest is
the goal-update completion verifier request (sequence50), not post-terminal
cancellation: that verifier calls the provider directly, outside the agent
cache-usage enrichment hook. Its tool still accounts returned usage directly;
missing cache telemetry is not proof of uncharged goal tokens. Two main
`old_history_changed` observations are also accurately reported, not hidden:
the transient internal continuation User tail disappears at the next foreground
request. Preceding stable segments and unchanged assistant/tool payload hashes
match. This is evidence of preserved reusable prefixes, not 100% append-only
normalized requests.

Summary evidence has a separate boundary. Before the goal flow, typed summary
43→64→72 carried the full prior body (1,173→1,215→1,561 characters, excluding
generated headings). Later safety rebuilds at1132 (`Invalid`, after internal-goal
compaction) and1386 (`Stale`, first client reopen) created fresh summary lineage;
the final snapshot has summary143, not the earlier three summary generations.
An ad hoc requirement that the final rebuildable snapshot still contain all
three was invalid. The internal-prompt/compaction invalidation is consistent
with the committed-only safety policy documented above; the exact Stale field
was not logged and the replaced snapshot is unavailable. No full prior-body
preservation across every rebuild or independent nonce-recall result is claimed.
Later daemon/model/peer reopens loaded the snapshot successfully.

Owned tmux/client/daemon processes stopped and the private profile copy was
removed at08:55:19 UTC. Original provider configuration, installed binaries,
four existing user processes, and pinned source/spec/binary hashes are unchanged.
No primary commit or installation was made. The next bounded investigation is
the combination of cold-client hydrated history, new live tool turns and a
subsequent daemon/peer restart; passing the earlier short fixtures does not waive
this new long-run rejection.

### Parallel PR fixes: final CI checkpoint

The original #2239/#2240 were already merged, so their fixes remain separate
open follow-ups, not changes silently integrated into these dirty worktrees:

- [#2261](https://github.com/octos-org/octos/pull/2261), head
  `6aae774d87a913ef12139121aa0849865634b59a`: authentication-store safety and
  relocation/error-handling fixes, plus correctly serialized Windows readiness
  test fixtures. All scheduled checks pass; native Windows completed at
  2026-09-05 09:06:20 UTC. Its test-only Windows file-store override does not
  implement or claim a production Windows credential backend.
- [#2262](https://github.com/octos-org/octos/pull/2262), head
  `b26702dcb5f36a936aefcd98b54b2954dc8d114d`: fenced peer cache/config publication,
  valid TOML strings and safe exclusion updates. All scheduled checks pass;
  Windows completed at07:59:49 UTC. Unix no-follow/symlink/hardlink checks remain
  Unix-specific; Windows validates the portable/reparse-check path, not Unix FDs.

Neither PR is merged. Skipped workflow jobs are not represented as executed
platform builds, and successful PR CI does not supersede the primary If6Q18
native-history rejection above.

### Whitespace-only segment drift reproduced; cold tool anchoring separated

The next isolated controls in
`/tmp/octos-cold-history-preflight-20260905.FV1oaK` distinguish lifecycle from
answer representation. V4 performs a real cold-client relaunch, ten new
tool-backed foreground turns, then a same-client daemon restart: 29/29 passes
on the rejected If6 bytes. V5 differs only in one tool-call response's assistant
content being a single space instead of empty. V5 fails 28/29: all durable
admissions/tools/finals remain correct, but native history replays twice after
the daemon restart. Both raw wire streams and unchanged fixture scripts remain
preserved for the fixed-byte repeat.

This reproduces the exact If6 cause: T29's iteration1 emits one space and P31's
iterations2/4 each emit one space; those whole whitespace-only iterations have
no canonical persisted assistant row. The live client aggregate retained their
spaces, so hydrate's canonical answer changes invisible bytes and correctly
trips the strict prefix guard. The repair targets closed whitespace-only native
segments, not global string trimming or a relaxed history-rewrite check.
Meaningful indentation, same-segment whitespace before later text, compatibility
V1 streams and delayed/out-of-order canonical segment authority have dedicated
regressions; the bounded whitespace slice has independent approval, pending the
combined final matrix and runtime repeat.

V4 also exposes a separate cold-history presentation issue: all three older read
previews are absent even though hydrate returns eight tool envelopes. Actual
hydrated user rows have a thread ID but no turn ID; the hydrated turn table
contains the exact thread→turn mapping. Without restoring prompt anchors from
that typed mapping at projected message positions, archived tool logs have no
request/anchor and are not rendered. A bounded identity-safe repair is in
progress. Equal prompt text is not sufficient ownership, ambiguous mappings or
userless continuations must not guess the latest user, and optimistic insertions
must preserve real projected positions.

Preparation for the next pair is at
`/tmp/octos-cold-live-replay-final-20260905.eKcjvT`: unchanged backend only, no
final client or private provider profile yet. In addition to the 50 established
gate controls, a separate eight-control cold-tool audit requires those three old
tool results exactly once within their owning prompt-to-next-user interval in all four
full captures spanning cold launch and later daemon restart. That audit rejects
the preserved V4 result; V4/V5 scripts are not weakened or retroactively changed.
Independent review caught an audit-only false rejection: cold history renders
assistant content before its archived activity, so tool-before-final is not an
existing product contract. A positive control reproduces that gate failure in
`cold-tool-owned-turn-red.log`; the corrected ownership-bounded audit passes all
eight controls, including missing, duplicated and wrong-turn negatives. No
renderer ordering was changed to satisfy the audit.
All earlier failed candidates remain rejected until the next exact pair passes
these new controls and a fresh original30+4 real-provider acceptance run.

### Current review and runtime rejection checkpoint (2026-09-05)

Current worktree acceptance is **not complete**. The latest real-provider run
at `/tmp/octos-cold-live-replay-final-20260905.eKcjvT` completed the original
30 turns and four peer probes with zero driver failures, but is rejected by
17 of 44 full native-history captures. Background identity changed from row52
to row55 after the legacy flat/per-user timestamp merge, so later hydrates moved
the background card and reprinted prior history. The captured raw failure is
retained; a successful driver or valid final pointer cannot override it.

The server-side repair reconciles committed identities using exact persisted
timestamp and typed owner, validates full role/content/media, and rejects
ambiguous or contradictory mappings. Retained canonical ledger references are
read independently of the requested replay window and hot-ring capacity, capped
at the captured scoped head. The client strict-prefix check is unchanged; no
equal-text deduplication is introduced. Fourteen focused backend controls pass,
including actual mixed-store reindexing, tiny hot-ring/cold replay, rotated
retained logs and rejected wire fallback. This is source-level evidence, not
acceptance of a newly built runtime pair.

Actual Claude Code `fable[1m]` resolved to `claude-fable-5-1` and completed two
read-only review invocations, with reports and invocation/coverage evidence at
`/tmp/octos-fable-review-20260905.ofETxe`. Combined source access covers all
34,177 backend diff lines, 13,745 client diff lines and 26 new files. The first
report prematurely claimed complete coverage while a backend tail remained
unread; that original report is preserved, and the follow-up closed the gap.
This is an access audit, not a claim of defect-free code. New fixes are outside
that original snapshot and still require post-fix review.

The review confirmed three additional findings. Trusted synthetic tool-abort
outputs were misclassified as uncommitted real tool output, invalidating compacted
snapshots; the producer now records explicit `Synthetic` authority. All92 context
tests pass, including three new controls. Ambiguous legacy source-less rows still
rebuild conservatively, and real ghost rejection is unchanged. Non-OUP gateway,
specialist and FFI conversational consumers lost structured truncated partial
responses. Their bounded repairs now preserve failure semantics and actual
partial output; gateway/specialist actual truncation controls pass6/6, API tests
pass89/89, and FFI/UniFFI all-target tests pass38 with2 real-provider tests ignored.
The C ABI still returns NULL; an additive consume-once partial-result accessor
and the structured UniFFI error were exercised through actual C exports and
generated Python against localhost HTTP. Successful FFI `iterations` retains its
preexisting assistant-row count (a one-round tool-free success can report0);
only incomplete-response iteration counts use the producer's final iteration.
No unrelated successful-counter contract change is claimed.

Ephemeral chat now retains shared profile memory, tools and skills while placing
session transcripts and OUP ledger in its temporary store. Actual OUP, database
lock-contention and session-root tests pass. Explicit memory/file/cron/goal tool
writes remain shared: the persistence flag is not a filesystem sandbox. Reasoning
token breakdowns are also retained through agent retries/fallback and the OpenAI
nonstream/shared-SSE parser; they are not added again to output tokens or pricing.
Actual parser RED and C/Python HTTP counter checks are retained in the focused
proofs. These component passes still require a final source-exact combined matrix.

The final JSON review remains open: typed OUP failure now retains actual partial
output and per-turn usage instead of deleting both with the ephemeral transcript.
Cold durable replay exposed a missing error-terminal usage carrier, now addressed
by an optional backward-compatible field. Independent review then reproduced a
further negative: a final empty-text MaxTokens response with a parsed tool call
can cause a late persisted pre-tool preamble to be mistaken for a partial final.
The repair must use producer-authoritative final-message identity, not arrival
order, text matching or segment-number inference. Its actual RED is preserved;
no final source freeze or acceptance is claimed until this control passes.

The rejected eK run has36 valid main admissions and a passing peer lifecycle
gate, but T17 actually used cron, not native monitor_create. Its genuine cron
ID does not satisfy the existing native-monitor final matcher. The next candidate
at `/tmp/octos-identity-reload-final-20260905.Q8eNRW` explicitly requires native
monitor_create/list/delete in T17/T18/T19/T29 and adds an identity-correlated
full-tool-output gate. All95 independent gate controls pass:58 retained controls,
14 native-monitor,21 background-identity and2 real recorder controls. The old
cron run remains RED. The deterministic background-before-foreground persistence
fixture also reproduced row12→15 with the old binary:16 healthy admissions and
finals did not prevent original identity loss and full-history failure. Its
strengthened recorder correlates every full hydrate request/response by launch
and RPC identity, rejecting omitted fields or unanswered requests. No new
candidate backend binary/soak acceptance is claimed yet.

The unchanged client component was freshly tested (2217 passed/4 ignored), linted,
formatted and built, pinned at SHA256
`95e33d7df51ea60b3d489760273b4e657787be49f65d63ae0b76d292c6e5cd7c`.
It still resolves its git-pinned core at5ea9878, not the local workspace core;
the additive backend error carrier must remain wire-compatible with that client.
The latest foundation all-target pass (3964 passed/49 ignored) predates the final
parser/error-carrier edits and is not claimed as final-version validation.

The rejected run's final cache evidence is84 manifests/83 usage records,
80 nonzero cache reads and1,592,320 cached tokens, with six literal manifest
privacy checks passing. One direct goal-verifier manifest is unpaired with usage;
this is not a blanket no-loss usage claim. Its private copied profile and owned
client/daemon were cleaned at10:33:31UTC. Existing user processes and installed
binaries were unchanged; no primary commit or installation was made. All runtime
soaks reported here are local macOS, not cloud Mac mini. An explicit authorized
cloud SSH target is still awaiting operator input.

### Final combined candidate checkpoint — 2026-09-05 18:35 UTC

This checkpoint supersedes the still-open component status immediately above,
but is not yet a completed runtime acceptance. The rejected eK candidate and
all original failing evidence remain unchanged.

Actual restricted Claude Code, requesting `fable[1m]` and reporting
`claude-fable-5-1`, completed three post-fix read-only passes. The access audit
covers all8,443 delta lines (7,889 original,535 supplemental,19 test-lock lines),
with123 Read,87 Grep and13 Glob calls and no tool errors or outside-snapshot
reads. The first pass's omitted1,664-line generated-binding range was explicitly
closed, not inferred from its initial all-read claim. The final combined source
comparison has no differences. Verbatim reports and manifests are retained in
`/tmp/octos-fable-postfix-review-20260905.p76wz6`. Fable confirms the two latest
fixes and reports no new confirmed blocker; this is static review, not soak.

F1 now uses the same effective media for the canonical background envelope and
its durable assistant row. Actual one/two-file Unix plugin tests reproduce the
old identity loss and pass after the fix, including shifted and cold-reopened
history. Strict owner/time/content/media identity validation remains unchanged.
Already-written contradictory split-media rows are not migrated. F2 suppresses
an empty outbound when a streamed incomplete answer has already been finalized
but final persistence fails; actual RED and both valid/failed-persistence GREEN
controls are retained. The final additional edit only scopes a test mutex guard
before an async await; no production behavior or assertion was weakened.

Typed JSON failure is now producer-authoritative: it preserves an actual final
partial answer and current-turn usage, but cannot promote a late pre-tool preamble
when the terminal points to no final answer. Optional error carriers remain
compatible with the client's separately git-pinned core. Unknown usage still
falls back to a default object, not a guarantee of measured zero usage.

The final expanded compile-input matrix v3 passed all six stages,18:19:27 to
18:24:01UTC: affected all-target tests7,838 passed/60 ignored/95 suites; minimal
CLI tests1,625 passed/6 ignored; strict all-target and minimal clippy; workspace
fmt; and diff check. It fingerprints all tracked/nonignored crate inputs,
including embedded assets, the root catalog and the external frpc template,
before and after each stage. Earlier v1/v2 lint failures are preserved and are
not final-version passes. Nine subsequent library/binding/header/actual-ABI/build
stages passed against exactly that matrix baseline; the Python binding matched
fresh generation byte-for-byte, and C/Python exercised four localhost requests.
The backend was built LAST, then copied without installing it:

- backend SHA256 `ec10ce3ec843887d61ec343aeb2eda3ba7a7df59daf35ade6cb6f766c43b3078`;
- client SHA256 `95e33d7df51ea60b3d489760273b4e657787be49f65d63ae0b76d292c6e5cd7c`.

The fresh deterministic V6 run on this pair passes34/34. Its actual background
row shifts12 to15 while all four post-commit full hydrates retain the original
ID and background source; all six full request/response pairs and raw traces
match. Sixteen admissions, final/tool ordering and native full-history checks
pass. The12 actual CLI/HTTP provider fault cases also pass, including68 provider
calls and independent durable-ledger checks, without a fabricated Session Summary.
Remaining short fixtures, the additive actual two-file TUI fixture, and the new
real-provider30+4 are still running at this checkpoint. The latter started at
18:32:54UTC, is local-only, and uses an isolated0700 home/0600 copied profile.

The separately read-only pinned-web check establishes that
`octos-web@1e985386a4dff3dddcee409157f6d36fe2a462c8` uses WS/OUP rather than the
legacy SSE completion bridge. It does not establish deployed versions or external
old SSE consumers. Other explicit limits remain: retained-ledger hydrate reads
are not constant-time; ephemeral explicit tool writes share profile state; FFI
successful iteration counting is unchanged; generated Python exceptions may
include their structured partial result in repr. No hand-edited generated-binding
or historical-row migration claim is made.

### Final local acceptance — 2026-09-05 18:48 UTC

The final candidate above is now locally accepted. This supersedes its in-progress
runtime status, not the preserved rejections of earlier candidates. No production
source changed after the final matrix/build/Fable snapshots; later ADR additions
are coordinator-authored acceptance documentation, not additional Fable reads.

The real provider/tmux run used the exact `ec10ce3e…` backend and `95e33d7d…`
client: original 30 rounds ran 18:32:54–18:41:21 UTC, followed by four peer rounds
through 18:44:38 UTC. All nine orchestration stages passed, including seven
independent final gates. A separate reviewer reran those seven gates read-only
in `/tmp/octos-independent-real-final-20260905.x9bNoj`, with unchanged original
ledger/capture/result hashes and matching binary/harness hashes:

- 36 unique admissions and 36 successful canonical completions: 29 labelled
  foreground turns, four peer turns, one background notification, two goal turns;
- 33/33 labelled answers matched their exact terminal-referenced final identity;
- all 44 FULL native captures passed, plus 37 required current-turn checks;
  14 viewport snapshots remain diagnostic-only, with zero unknown classifications;
- 34 ordered tool start/completion pairs; no protocol errors, fake Session Summary,
  controller answer, segment-ID collision, split stream or typed failed child;
- one actual background child/continuation/answer, without a redundant join;
- native monitor create/list/delete and post-restart absence all passed, using
  the actual returned `monitor_01`, not a cron substitute;
- client restart, immediate-submit daemon restart, K3 → GLM-5.3 → K3 with daemon
  restarts, and kept-open peer restart/reuse/close/quiet-window checks all passed;
- six actual compaction completions, including automatic and explicit manual
  compaction. Nonce-bearing recall prompts are not independent memory measurement.

The goal worker actually read the scoped source file, wrote `verified.txt`, and
received an executor-created `acc_0` completion receipt with exit code 0. Supervisor
sequence 26 recorded complete before the accepted fleet wake at sequence 27.
A redundant later `goal_update` at main sequence 1414 was rejected for insufficient
evidence in that request; subsequent `goal_get` retained complete. This rejection
is preserved, so the 34 tool pairs are not described as all successful.

The seven fresh short-fixture groups also passed: ordinary/aged background
20/20 and 26/26; cold-client/whitespace variants 29/29 each; shifted-identity V6
34/34; peer lifecycle 51/51; four streaming/fallback lanes 4/4. Supplemental cold
tool-ownership checks passed 12/12 in each of V4/V5/V6. These are actual new-pair
runs, not reused old runtime evidence.

The additive actual two-file TUI fixture `media-final-v2` passed 21/21 checks
and its independently reviewed gate passed 26 controls. It verifies four real
admissions, one canonical background card, two exact attachment owners in all
four post-commit full hydrates, and zero provider calls during both restart quiet
windows. The existing client renders the completion body, not separate attachment
widgets: native captures prove the card, wire/hydrate prove attachment ownership.
The parent follows the proven spawn-only policy with its real preamble and no
fabricated acknowledgement; ordinary turns still require exact final pointers.
V1's missing plugin-discovery configuration remains a rejected, cleaned fixture
run. V2 uses an explicitly isolated supported plugin path; no product change was
needed. A later ad-hoc diagnostic indexing error did not affect the frozen gate.

Three actual JSON/text partial-failure HTTP executions passed 29/29 checks, with
six provider calls, actual file reads, exact current-turn input/output usage 38/22,
one actual partial per output, nonzero exits, and ephemeral-store cleanup. The
original gate's 27/29 result remains preserved: two expectations incorrectly
required explicitly serialized optional zero counters. The bounded audit-only
correction follows the existing skip-zero contract, passes 12 boundary controls,
and was independently reviewed; neither product nor captured runtime changed.
The separate 12 provider fault cases and durable-ledger audit also passed.

Final cache evidence is 83 manifests / 82 usage records, 82 exact pairs,
79 nonzero cache reads and 1,572,864 cached tokens. There are 16 non-null usage
epochs across K3 and GLM-5.3. One manifest (line 99, sequence 50) lacks usage and
epoch metadata; it is explicitly unpaired, not inferred to have zero usage or
assigned an unproven cause. No orphan usage, duplicate keys or malformed counters
were found. Eight independent audit controls pass. Six specified literal privacy
checks (nonce, both markers, SOAK prompt, proof path and workspace path) pass;
this is not a fresh credential/history scan or a blanket billing-completeness claim.

Cleanup completed at 18:44:39 UTC. The copied private profile was deleted, the
isolated tmux server/client/daemon exited, and existing user process identities
and start times were unchanged. Installed binaries and the original profile
hashes match the pre-run baseline. Final source verification matches all 855
compile inputs plus the tracked deletion; client source/spec hashes also match.
No commit, installation, user-process restart or PR integration was performed.

Primary proof: `/tmp/octos-identity-reload-final-20260905.Q8eNRW/ACCEPTANCE.md`.
Actual Fable reports remain under `p76wz6`; the JSON proof is under `0fGUYY`.
Cloud Mac mini soak still requires an explicit authorized SSH target. Historical
contradictory split-media rows remain unmigrated, and optional llama/CUDA builds
are outside this final affected-package matrix. The other review limitations
listed above remain explicit; local acceptance does not erase them.

### Supplemental mini3 cloud acceptance — 2026-09-05 20:58 UTC

The operator supplied and authorized the cloud target after local acceptance.
This resolves the earlier cloud-target prerequisite; it does not turn prior
local evidence into cloud evidence. This is an actual remote macOS 15.7.9 arm64
OctosCode/OUP/tmux execution, not a cloud rerun of the entire local unit, ABI,
chat/ACP or provider-fault matrix. Passwordless SSH uses the local `mini3` alias;
no login password, private key or host address is recorded in this repository.

The exact final candidate was copied into an isolated remote proof, without
installing or rebuilding it: backend SHA256 `ec10ce3e…`, client `95e33d7d…`,
and portable tmux 3.7b `bc2a8bac…`, matching the local accepted artifacts.
Original 30 rounds ran 20:40:48–20:48:22 UTC; four additional peer rounds ended
20:51:18 UTC, approximately 10 minutes 30 seconds total. All nine orchestration
stages passed, including seven final gates. After downloading a hash-verified
evidence archive, the coordinator reran all seven gates read-only locally:
all passed, with unchanged original ledger/capture/result hashes and exact
binary/harness hashes. This replay is not a new Fable or external-agent review.

- 36 unique admissions / 36 canonical completions, with no turn errors or
  interruptions; 33 exact labelled final answers and 37 required current-turn
  TUI checks passed.
- All 44 FULL native captures passed. Fourteen viewport captures remain
  diagnostic-only; no unknown classifications, fabricated Session Summary,
  duplicate answer, segment collision, split stream or failed child was found.
- All 35 tool start/completion pairs were ordered. One redundant `goal_update`
  was rejected, so this is not an all-tools-success assertion.
- Six real compaction completions covered automatic and explicit manual
  compaction. Peer reads, one background completion/continuation, parallel
  tools, native monitor create/list/delete and post-restart absence passed.
- Client restart, immediate-submit daemon restart, K3 → GLM-5.3 → K3 with
  daemon restarts, and kept-open peer daemon restart/reuse/close/quiet windows
  passed. There were three deliberate daemon restarts in the original driver
  and one more in the peer probe. Nonce-bearing prompts are not independent
  memory measurement.

The goal worker actually read the scoped source and wrote the exact marker to
`verified.txt`; its executor-created `acc_0` receipt has exit code 0. Supervisor
sequence 16 recorded goal completion before the accepted fleet wake at sequence
20. The later `goal_update` at main sequence 1318 was rejected for insufficient
evidence in that request; subsequent `goal_get` at 1346 and 1495 retained complete.
The additional read-only extractor initially assumed a trailing newline from
the local run; cloud tool arguments and the actual artifact both omit it. Its
original failure and source are preserved. Only this run-specific extractor's
expected bytes were corrected; the product, captured evidence, acceptance
command and seven mandatory gates were not changed.

Cloud cache evidence contains 84 manifests / 83 usage records, 83 exact pairs,
80 nonzero cache-read records and 1,598,976 provider-reported cached tokens.
K3 has 82 usage records; GLM-5.3 has one, reporting zero cache read. One manifest
(line 101, sequence 51) remains unpaired, with no inferred usage or assigned
cause. There are no duplicate keys, orphan usage, malformed counters or bad
pairs. All six specified literal privacy checks pass. A separate in-memory
scan of 208 exported evidence files (excluding binaries/bytecode) found no
literal or JSON-escaped matches to six known provider-secret values. This is
not a fresh Git-history scan or a guarantee against arbitrary secret encodings.

The remote preflight's initial 72-test invocation had two missing historical
negative-fixture path errors. Both original fixtures were copied unchanged and
only their test lookup paths were made portable; the preserved subsequent
72/72 run passed on mini3. Cache logical/physical-path controls passed 9/9
(eight overlap the 72-test suite; counts are not additive). Failed preflight
and extractor records remain available, rather than relabelled as passes.

The detached controller completed cleanup at 20:51:18 UTC. Both temporary
provider-profile copies were deleted, and a fresh SSH check at 20:56:53 UTC
confirmed their absence, no owned runtime processes, and an unchanged existing
remote profile. The evidence archive excludes profile directories and private
input. Temporary SSH monitoring timeouts did not abort the detached run; their
cause was not established. Local installed binaries/profile remain unchanged;
all 855 final backend compile inputs plus the expected deletion still match.
No production-source changes, commits, installations, user-service restarts or
PR integration were made for this cloud test. Earlier Fable and local acceptance
limits, including unmigrated historical split-media rows and optional llama/CUDA
build coverage, remain in effect.

Cloud proof, mirrored locally:
`/tmp/octos-mini3-soak-20260905.EOQbPu/ACCEPTANCE.md`.
Independent replay, credential-scan and cleanup records:
`/tmp/octos-mini3-coordinator-20260905.qZGJb4/`.
Original evidence archive SHA256:
`e8713ee60aac8666590a890c5a3b268ed37e55f79b41f13ab17b40618869c0c4`.

## RC integration checkpoint (2026-09-05)

The reviewed changes were transplanted onto current upstream main in isolated
worktrees. The original dirty worktrees and their unrelated root README edits
were preserved. The prior Fable review and mini3 soak above cover the earlier
candidate, not this integration or the forthcoming release artifacts.

Integration preserves upstream sampler defaults, cache-aware cost attribution,
lazy ledger recovery, peer-owned result receipts, streaming route selection and
stdio/reconnect barriers. Chat and ACP obtain their agent defaults from the same
profile resolver as OUP. Truncated model responses remain typed failures carrying
their real partial answer and usage, rather than synthetic successful answers.
OpenAI sampler extensions cannot override reserved cache or tool-choice fields.
The client includes the previously committed paste-editing and mixed-projection
dedup fixes on which the reviewed worktree depended.

Local integration evidence is retained under
`/Users/ychen/.octos/outer/oup-rc-20260905.Igc2Dj/`:

- `octos-tests-3.log`: 8,354 passing tests across 94 suites, 62 ignored,
  covering CLI, agent, core, LLM, bus and services with all targets.
- `octos-clippy-workspace-1.log`: strict workspace/all-target clippy passed.
- `octoscode-tests-runtime-2.log`: 2,281 passing tests across 43 suites,
  four ignored. Linux-only `olp_evo_*` and `olp_watch_board_harvest_*`
  cases were filtered on this Mac because GNU flock/stat are unavailable;
  unfiltered Ubuntu CI remains required before merge.
- `octoscode-clippy-1.log`: strict all-target clippy passed.
- Subsequent `octos-minimal-tests.log`: 1,682 passed, six ignored.
  `octos-release-matrix-clippy.log`: strict combined channel/release-feature
  clippy passed. The isolated FFI build exposed API-gated peer-registry helpers
  that default-feature and unit-test builds masked. Recovery now keeps its
  shared registry available to non-API gateway actors, and required matrix CI
  separately compiles/lints the minimal production library.
  `octos-minimal-lib-clippy-2.log` and `octos-abi-tests-2.log` passed after this
  fix. The C header compiled with warnings denied; `uniffi-python-test.log`
  passed real localhost C/Python partial-answer and success controls.
- The additional reserved-sampler and hydration-queue regressions have separate
  red/green logs. The release PR records subsequent checks of the final commit.
- Known mini credential byte-pattern scans found no password or host matches in
  staged changed files. The backend secret scanner flagged eleven deliberate
  redaction-test fixtures (synthetic alphabet/placeholder tokens, example JWT,
  short invalid key bodies and example curl credentials); these were inspected,
  not suppressed by excluding the redaction implementation. Client scan passed.

This checkpoint is not a claim that CI, PR merges, RC publication or a new
artifact-pair soak has completed. Optional llama/CUDA coverage remains outside
the default/release feature proof. Removing the implicit goal budget and making
chat a daemon-only thin client are not part of this release change.

## Definition of done

The original OUP/OctosCode milestone required the following properties. Current
worktree acceptance and its explicit rejections above take precedence; this list
is not a new all-clear:

- the semantic ledger is the single OUP context authority;
- compaction operates on disjoint complete semantic blocks;
- same-epoch provider-normalized cache-input prefixes are mechanically
  verified;
- cache epoch rotations are explicit and observable;
- provider caching is capability-gated and measured;
- the real OctosCode stdio/tmux soak passes across compaction, peer/background
  delivery, restart, and one deliberate invalidation.

The follow-on frontend convergence routes `octos chat` and ACP through the same
OUP dispatcher. Its separate acceptance must cover real adapters and shutdown,
not merely reuse the earlier OctosCode milestone's test counts.
