# Evidence before repair, independent review, and safe reuse

These operations preserve observations; they do not prove the model understood
an image. Use the task's selected vision model. Maker edits, Reviewer judges,
Supervisor routes work and arbitrates using the original evidence.

## Register a defect when it is found

Before Maker changes it, Reviewer calls:

```json
{"action":"finding_register","cycle":"attempt-a/cycle.json","findings":[
  {"id":"label-clearance","page":"source-page-id","object_id":"label-id",
   "check":"readability","state":"open","note":"Describe the observed defect and its impact"}
]}
```

The tool resolves the object to a contextual viewport and saves the exact current,
blank, and available design crops. A page plus canvas/image-appropriate `box` may
be used for a non-object defect (registration boxes are actual rendered pixels).
Keep the stable finding ID. With `previous_cycle` and `previous_review`, Rust
inherits the registered viewport and receipt automatically; current observations
only need the ID, state and new observation. Omitted old findings stay open.
Explicitly changed identities, boxes or receipts are rejected. Without a bound
predecessor, supply the returned registration. Do not replace before evidence.
`review_submit` also registers new open findings automatically, but immediate
registration allows repair to begin before the whole review is finished.

Missing scope fails at registration. A resolved claim with unchanged local
pixels stays open; editing an unrelated label cannot close it. Changed pixels
are only evidence that a change occurred. Reviewer must still inspect the actual
result and surrounding objects before claiming resolution.

## Choose the verification route before work

Default `evidence_init` is strict. Where the intended acceptance combines native
measurements with independent visual verification of unsupported boundaries,
the task policy owner initializes with (the coordinator for a shared-workspace
page fleet; the maker for an independent single-maker task):

```json
{"action":"evidence_init","task_id":"deck-task","stagnant_attempts":3,
 "deliverables":["exports/editable.pptx"],"verification_mode":"independent_visual",
 "reviewer_agent":"EXACT-REVIEWER-SESSION-FROM-HOST"}
```

Omit `owner_agent` under Octos: the host binds the actual policy-owner identity.
Freeze a different Reviewer identity; in a page fleet it must also be separate
from every page maker. Do not replace an existing policy or retarget the
route after encountering failures. A legacy strict task stays strict.

Reviewer reads original/blank/current crops and complete object boundaries,
including neighboring labels. Surface/OCR candidates can identify shadows,
glows, or the wrong side of a ribbon; they do not establish semantic ownership.
Use `view-index.json` measurements as questions to check against all three images.

With this route, `review_submit` accepts a relation response with
`status:"visual_acceptance"` and a concrete `basis` from that Reviewer.
This differs from `intentional`, which still requires a measured `support_id`.
Use visual acceptance only when independent inspection resolves an ambiguous
candidate. Keep actual defects or uncertainty as `defect`/`unknown`.

`view-index.json.native_review_candidates` lists native unknowns eligible for a
separate visual route. The returned `observations_template` already contains
these IDs in `measurement_reviews`, initially unknown with an empty basis.
Reviewer fills each decision and observation; do not transcribe hashes/IDs from
another report or apply one blanket pass to every row:

```json
{"id":"EXACT-NATIVE-FINDING-ID","decision":"accepted_visual",
 "basis":"Describe the actual visible border, glyph clearance and original comparison"}
```

The alternatives are `defect` and `unknown`. Only
`OWNER_UNVERIFIED`, `OWNER_BOUNDARY_UNVERIFIED`, `COMPOSITE_INK_UNVERIFIED`, and
`ARTWORK_CONTACT_UNVERIFIED` **unknowns** qualify; all five object checks must
also pass. The host records Reviewer identity and exact evidence. Maker cannot
self-approve. Native failures, incompatible references, altered constraints,
missing objects or unresolved copy cannot be overridden. Native QA retains
`unknown`; record/status separately report independent visual acceptances.
Object pass observations do not replace `measurement_reviews`: every eligible
unknown requires its own decision. `evidence_record.verification_feedback` and
`evidence_status.verification_route.feedback` list remaining IDs and the next
action. The categories in the frozen plan remain authoritative, including on
older tasks. An unsupported category is a blocker, not permission to declare
acceptance. A new plan must be a separate, explicitly identified task.
A positive artwork-contact count samples nontransparent glyph pixels, not spaces
between letters. Inspect the reported sample points on the actual glyphs. The
2-render-pixel contrast probe can reflect benign proximity or texture, so it
does not alone prove occlusion; explain that distinction using original, blank
and current pixels. Do not dismiss glyph samples as whitespace.

For pages with complete bound whole-page comparison observations, review_submit
also fills missing inspected-page coverage arrays. It carries observed coverage,
not a pass verdict. Incomplete comparisons and object checks remain blocking.
An attestation proves provenance, not visual correctness or hostile-write security.

## Review in bounded page batches

An unscoped multi-page `review_pack` now returns `mode: "page_batches"`,
`pack_created: false`, a bound plan, and executable per-page `batches` requests.
It generates no crops or observations. Dispatch those requests through the
already frozen independent review route; do not create a second full-deck pack.
`pages` / `page_numbers` scope images, comparisons and native measurement
candidates together. Deck-wide findings remain visible. `all_pages: true` is an
explicit opt-in for a combined pack, not the default final-review workflow.
Final coverage must still include every page.

Reviewer reads each page's actual evidence and calls `review_submit` for that
page. After every required page is submitted, the coordinator combines the
bound results without re-authoring observations:

```json
{"action":"review_merge","cycle":"final/cycle.json",
 "reviews":["reviews/page-001-bound.json","reviews/page-002-bound.json"],
 "output":"reviews/final-bound.json"}
```

Then call `evidence_record` with that merged review, followed by `evidence_status`
on a subsequent tool turn. A deck score or `accepted: true` report does not
replace structured object, comparison, relation or measurement observations.
Do this evidence handoff on the first page before producing the full deck, so
interface failures surface early rather than after all pages have been scored.

`review_merge` accepts only direct `review_submit` outputs for the exact same
cycle from the frozen independent Reviewer/Arbiter. It preserves provenance and
unknowns, rejects conflicting overlapping rows, stale/tampered inputs and scores,
and cannot turn missing checks into passes. Source reviews and receipts remain
required artifacts; keep them with the final QA evidence. Nested merges are not
supported. Merge receipts do not impersonate an Arbiter for new arbitration.
Reviews from earlier cycles require the existing local evidence reuse route,
not a cross-cycle merge. Do not change identities or reset the policy to make
aggregation succeed. After repair, resubmit a replacement page review and merge
the current set once; do not combine contradictory old and replacement rows.

## Reference fidelity is separate from readability

New tasks freeze `fidelity_contract: reference-fidelity-v1`. Older task policies
and histories remain unchanged. Each referenced object gains a `fidelity`
section in the existing observations template. Rust binds original/blank/current
images and their matched viewport. Keep `evidence` or omit it for Rust to supply;
altered/stale evidence is rejected. After reading those images and neighbors, fill:

```json
{"comparison_reliable":true,"layout_relation":"preserved",
 "reference_interference":"absent","current_interference":"absent",
 "interference_change":"equivalent",
 "basis":"Describe the actual source/current spatial relationship and background differences"}
```

- `layout_relation`: `preserved`, `changed`, or `unknown`.
- Both interference fields: `absent`, `present`, or `unknown`.
- `interference_change`: `equivalent`, `reduced`, `increased`, or `unknown`.
- `comparison_reliable`: true, false, or null; uncertainty remains blocking.

Rust derives the outcome. A changed relationship or added/increased interference
is a defect even if foreground strokes remain readable. Preserved slanted type,
an intentional crossing or texture already in the approved design is not an
automatic defect. Conflicting facts stay unknown. Font rasterization differences
alone do not establish a defect; inspect owner, direction, clearance and neighbors.
A readable/whole-page pass or `accepted_visual` cannot waive failed fidelity.
The three review/evidence tools return `reference_fidelity` with blocking codes.
They do not verify the truth of model observations or subjective aesthetics.

Confirmed defects receive stable `fidelity-...` finding IDs and immutable before
crops. Omitting them or changing later prose to pass/optional does not close them.
Resolve through the existing registered-finding workflow after an actual local
change and independent re-review. Reuse requires passing fidelity evidence plus
unchanged local pixels and neighbors; legacy legibility passes cannot substitute.

## Local repair and bounded arbitration

Build repairs into fresh cycle directories; preserve original images and the
last accepted output. Use previous_cycle/previous_review in review_pack. Inspect
changed areas and neighbors, and record candidates before replacing deliveries.
For a background repair, Rust permits a contract revision only with a previously
registered fidelity defect, unchanged design/canvas, fresh independent passing
observations, and pixel changes confined to its registered viewport(s). An edit
outside those regions or a resolution change remains a constraint blocker. Other
constraints remain frozen. The revision and pixel proof are recorded; adoption
requires all existing gates to pass. No automatic artwork rewriting occurs.

If arbitration may be needed, freeze an optional `arbiter_agent` at evidence_init
alongside reviewer_agent. It must be an actual host session distinct from the
policy owner and Reviewer. An existing Supervisor can serve only when it is not
that policy owner. The coordinator owning a shared-workspace fleet policy cannot
also be its Arbiter; do not invent another role or change an active policy to
work around that constraint. No extra vision service is
introduced. This Arbiter can submit a current review that dismisses a registered
false positive using `state:"dismissed", arbitration:{"basis":"..."}`. Host
attestation and original/current evidence are required. The policy owner and Reviewer cannot
grant themselves this role. Native failures and currently reported fidelity
defects remain blocking. Changed local pixels invalidate an arbitrated exception.

Without a frozen Arbiter, preserve the dispute as unresolved. Do not reset policy
or add a role after failures to clear it. Use the existing stagnant_attempts and
user/host budget: stop a stagnant repair method and return candidate plus remaining
defects, never declare full quality acceptance. Repeated same-model votes are not
independent pixel evidence. Structured checks prevent shortcuts, not shared vision
errors. Do not create another supervisor loop.

## Reuse observations without guessing

After a batch repair call:

```json
{"action":"review_pack","cycle":"attempt-b/cycle.json","output":"review/b",
 "previous_cycle":"attempt-a/cycle.json","previous_review":"review/a-bound.json"}
```

Read `view_index` and author the returned `observations_template`, preserving its
`review_id`. Rust can carry forward prior complete pass observations only if
the exact local design/blank/render pixels, viewport, declarations, neighboring
objects and relation candidates are unchanged. Defects and unknowns are not
promoted. Changed or unreviewed objects stay in `objects_requiring_review`.
Fresh authored observations take precedence. `review_submit` revalidates all
reuse inputs; changed previous files or cycles are rejected. Keep all-page
overview/sweep review and review every changed region with its neighbors.

## Use the host's actual collaboration tools

On Octos a staged peer uses `peer_publish` for an intermediate checkpoint with
exact artifact paths and readiness; it is not a final result or an instruction
to another peer. Supervisor uses `peer_gather` to read it, then `peer_send_input`
to route a review or repair to the same peer. Peers do not have master-only
`peer_send_input`, and subagent `send_input` is not an alternative peer channel.

`peer_gather(wait_ms:60000)` waits for a blackboard change or timeout, and is
cancellable. Timeout is not completion; compare task/turn and artifact versions.
Only the final returned result is marked consumed, not intermediate polling.
Publication alone does not wake an idle peer; keep the persistent supervisor
host alive. Do not resend the same assignment while its peer is still working.

For reconstructions with a registered source line inventory, initialize `independent_visual` with the independent reviewer's session. The review draft includes `source_line_reviews` for the current candidate, including lines with no native object. Use the original region indicated by `source.box` or `source.polygon`, its coordinate canvas, the current render and the linked text. Submit each observed `page`, `source_line_id`, `status` and a specific `observation`. Use `native` for verified editable text, `defect` for a visible discrepancy, and `unknown` for unresolved evidence. Only the independent reviewer can close a detected non-text mark as `not_text`, or an unreadable marking visibly preserved as artwork as `illegible_artwork`. Missing native text or an unchecked line cannot silently pass the copy check. Look in `source_line_coverage.pending_ids` for the next repair. If review discovers a new line, register it and review a new cycle containing it.
