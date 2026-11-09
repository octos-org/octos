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
Maker initializes with:

```json
{"action":"evidence_init","task_id":"deck-task","stagnant_attempts":3,
 "deliverables":["exports/editable.pptx"],"verification_mode":"independent_visual",
 "reviewer_agent":"EXACT-REVIEWER-SESSION-FROM-HOST"}
```

Omit `owner_agent` under Octos: the host binds Maker identity. Freeze the real,
different Reviewer identity. Do not replace an existing policy or retarget the
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
alongside reviewer_agent. It must be an actual host session distinct from Maker
and Reviewer; the existing Supervisor can serve. No extra vision service is
introduced. This Arbiter can submit a current review that dismisses a registered
false positive using `state:"dismissed", arbitration:{"basis":"..."}`. Host
attestation and original/current evidence are required. Maker and Reviewer cannot
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
