# Submit an independent page verdict

On v2 plans, a new candidate can have a superseded verdict in the review
file. Its `page_gate` row returns `replaces_verdict_id` for the existing
authentic revision. Use this key at the top level of `page_review_submit`,
alongside `page`, `candidate_id` and `verdict`. Reuse your observations if the
submission was rejected only for its argument shape.

The reviewer uses `page_review_submit` after inspecting the exact candidate and completing its required visual/object checks. Use the opaque `candidate_id` from the publisher's handoff (also returned by `page_gate`). List the immutable evidence files actually inspected, including applicable `review_submit` receipts. Paths or existing path/hash bindings are accepted. The tool supplies the reviewer identity and all publication bindings; it does not judge pixels.

    {"action":"page_review_submit","page":"intro","candidate_id":"<from handoff>",
     "verdict":{"accepted":false,"score":91,"critical_defects":[],
               "component_scores":{"content":19,"editability":20,"typography":18,"readability":11,"fidelity":13,"sources":10},
               "open_required_findings":["F1"],
               "evidence":["reviews/intro/inspection-01.json"],
               "findings":[{"id":"F1","status":"open","description":"Legend overlaps a data label."}]}}

For per-page work, fill the unanswered `review_request_template` in the candidate handoff or get the current one with `page_gate(only_page)`. It binds the page, candidate and any previous verdict before you fill the evidence, checks and score. For `verification_mode:"per_page"`, an accepted verdict additionally needs `delivery_checks` with a passing observed basis in every domain. See [assembled delivery](assembled-delivery.md) for the schema. For instance, a reviewer who checked thirteen editable labels against the source could submit `"copy":{"status":"pass","basis":"All thirteen source labels appear verbatim in native text shapes."}` inside `verdict.delivery_checks`. Supply an independent observation in the same shape for each of the other three domains. Rejected candidates use the usual findings and repair routing.

With per-page delivery a successful submission refreshes the editable working copy at the returned `progress_path`. If it returns `progress_error`, publish the existing verdict ID and relay that error to the coordinator; the coordinator can use `page_progress` to recover the working copy.

Keep the plan's review path stable for new tasks. The tool writes the gate-readable verdict there and preserves immutable revisions. Identical retries return the same verdict ID. To revise, read the current verdict and pass its returned `verdict_id` as `replaces_verdict_id`. Report the publication with `peer_publish`. Refused, stale and malformed submissions cannot replace the current verdict. Existing legacy review files stay readable; to adopt native submission for an unfinished legacy page, the coordinator assigns a fresh review destination after preserving the previous verdict and findings. Pin the reviewer per page with optional `reviewer_session`.

New plans use schema version 2 and require a `rubric` mapping task-specific category names to maximum points totaling 100. Each verdict carries `component_scores` with the exact same categories and a score equal to their sum. For a version 1 native verdict under a previously published plan with no frozen rubric, any supplied `component_scores` must still sum to the reported score. Any supplied `component_maxima` must total 100 and bound each category. Retain historical scalar-only decisions. Record the user's permitted raster/vector scope in the review brief. For a below-threshold verdict, identify the concrete repair or the unresolved interpretation/tool limitation with its owner and evidence. The reviewer owns judgments and revisions. Correct a ledger using that reviewer's existing evidence and candidate.


A rejection with no critical or required fix needs a `disposition`, for example
`{"owner":"tool_maintainer","reason":"A font face cannot be addressed","resume_when":"A rendered fixture resolves that face"}`.
Use `reviewer` for clarification by the current reviewer, `arbiter` for an
independent interpretation of the existing rubric, and `tool_maintainer` for a
technical peer to investigate a capability. Open findings still block assembly.
For a previously recorded `task_owner` verdict the gate derives a peer phase on
read. A critical or required finding or a recorded technical secondary owner
routes to diagnosis; a score-only dispute routes to arbitration. The original
verdict remains intact. The coordinator follows the current dispatch phase.
With native peer handoff support, publish the
result's `verdict_id` as the `peer_publish.handoff_id`.

When the assigned specialist exhausts permitted remedies, they publish a JSON
report containing `outcome:"blocked"`, `phase` (`arbitrate` or `diagnose`),
`page`, `candidate_sha256`, `peer` (their session), `attempted_remedy` and
`remaining_blocker`. The coordinator verifies the native publication identity.
The original reviewer can attach the published report in one call. Use the
candidate and verdict IDs returned on the same `page_gate` row:

    {"action":"page_review_submit","page":"intro","candidate_id":"<candidate id>","replaces_verdict_id":"<verdict id>","blocked_report":"reviews/intro-specialist.json"}

This preserves the existing rejection, score, findings and evidence. Retrying
the attachment returns the same verdict ID. Reports may identify the page by
`page` or `page_id`; if both are present they must agree. An in-progress
report belongs to the current investigator until it publishes a conclusion.
Publish the finished report once and keep its bytes immutable while it is
bound. Subsequent status updates belong in the task status, outside that file.
If a reviewer redirected the same open finding while the specialist worked,
the original reviewer can bind the finished result from the earlier referral.
The runtime checks the immutable verdict chain, candidate, reviewer and finding
IDs. A changed candidate or different finding requires its own evidence.
The gate validates the report scope and bound bytes. Changed evidence reopens
review. A changed capability can trigger a fresh referral or a new candidate.
When changing only the render of the identical editable PPTX, the assigned v2
reviewer can first use `page_publish` with `refresh_render_only:true`, the page,
current candidate ID and native replacement `render_receipt` path. It checks
the same PPTX, engine, fonts, PNG hash and dimensions. It archives the previous
candidate and returns the new ID and the previous verdict ID. Review the new
pixels before submitting the decision. Unchanged PPTX bytes need no rebuild.

### Verified repair requests

New per-page verification plans freeze the repair gate at initialization. An optional roster-level `repair_requirements` object maps stable IDs to specific requirements agreed before production. Keep subjective measurements and unknown typefaces in review notes unless a concrete presentation impact or a frozen requirement applies.

In a rejected verdict without a specialist disposition, list all actionable findings together in `critical_defects` and `open_required_findings`. Each needs a unique ID and one of these forms.

- `{"id":"qa-…","kind":"native_qa"}` refers to a failed check for this page in the native QA report bound to the exact PPTX.
- `{"id":"year","kind":"visible","box":[40,80,60,25],"impact":"The year label cannot be read against the light background."}` uses pixel coordinates on the current render.
- `{"id":"asset","kind":"requirement","rule":"approved-art-only","impact":"The illustration was replaced."}` refers to a named frozen requirement.

When the host receives a signed rejection, reproduced native failures queue the maker. Manual findings queue the assigned confirmation peer. The coordinator can recover that assignment with `page_assignments`; an unassigned single-page referral stays in `specialist_pages` until the coordinator arranges independent confirmation. Read `repair_dispatch` and `next` after either publication. The peer inspects the cited region at presentation size and the reference or frozen requirement, then calls `page_repair_decide` with the exact candidate_id, verdict_id, decision (`confirm` or `dispute`) and `finding_observations` keyed by every manual finding ID. Confirm only if all actionable findings are supported. On dispute the original reviewer revises the verdict with `replaces_verdict_id`, retaining supported findings. A decision never awards a score. The maker receives a repair assignment only after confirmation or a verified native failure.

For missing or incorrect editable labels with known object IDs and reference wording, include a map on the frozen requirement finding, for example `{"id":"legend-copy","kind":"requirement","rule":"editable-legend","impact":"The legend labels must be editable","native_text":{"label_a":"Increase","label_b":"Decrease"}}`. Use this for named Octos-native text shapes with stable IDs. Check coverage, placement, visibility and typography separately on the render. Each label must occur exactly once under its ID. The tool verifies the exposed defect against the reviewed PPTX and freezes the measured values in the verdict. The existing independent confirmation covers all required findings together.

On a repair publication, the page tool returns the IDs that changed since the last distinct proposal and those still unresolved. Until all declared native text targets match, the maker keeps the assignment. On success the page returns to its assigned independent reviewer. For an audit, read this verdict's `repair_observations_dir` from its page gate row. The numbered receipts bind the original verdict and proposed PPTX hashes. Join a subsequent independent page verdict by page and artifact SHA-256 to determine whether that version passed. Each exact proposal has one receipt, including after a retry.

On a new image-reconstruction assignment, fill the `source_detail_review` section of the template. Keep the original and current render bindings in evidence. A whole-page verdict covers measured source regions, marks discovered by the reviewer and prior open findings. For a detail inside an illustration, inspect a magnified reference/render pair. Use `representation: "missing"` on a rejected verdict for visible strokes missing from the render, even when their wording is unknown. Use `"artwork"` for visually retained pixels and `"native"` with the actual text object ID for an editable label. A scoped pass cannot sign the page.

For each native label compare the original glyphs and line direction separately. For example, a rotated italic label might have `{"box":[20,30,80,25],"source_glyph":"italic","source_angle_deg":20,"rendered_glyph":"italic","rendered_baseline_basis":"Both lines descend along the face edge and the rendered stems lean relative to that line.","representation":"native","object_id":"face-label","basis":"Compared the enlarged word on the same face in both images."}`. Use `glyph_span` for a uniquely identifiable word within a mixed-format text box. Give any unresolved finding its stable ID; retain it in later rejections or give a current closure observation in `resolved_findings`. See the independent reviewer assignment for the full fields.
