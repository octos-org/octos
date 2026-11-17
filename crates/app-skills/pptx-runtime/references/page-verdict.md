# Submit an independent page verdict

The reviewer uses `page_review_submit` after inspecting the exact candidate and completing its required visual/object checks. Use the opaque `candidate_id` from the publisher's handoff (also returned by `page_gate`). List the immutable evidence files actually inspected, including applicable `review_submit` receipts. Paths or existing path/hash bindings are accepted. The tool supplies the reviewer identity and all publication bindings; it does not judge pixels.

    {"action":"page_review_submit","page":"intro","candidate_id":"<from handoff>",
     "verdict":{"accepted":false,"score":91,"critical_defects":[],
               "open_required_findings":["F1"],
               "evidence":["reviews/intro/inspection-01.json"],
               "findings":[{"id":"F1","status":"open","description":"Legend overlaps a data label."}]}}

Keep the plan's review path stable for new tasks. The tool writes the gate-readable verdict there and preserves immutable revisions. Identical retries return the same verdict ID. To revise, read the current verdict and pass its returned `verdict_id` as `replaces_verdict_id`. Report the publication with `peer_publish`. Refused, stale and malformed submissions cannot replace the current verdict. Existing legacy review files stay readable; to adopt native submission for an unfinished legacy page, the coordinator assigns a fresh review destination after preserving the previous verdict and findings. Pin the reviewer per page with optional `reviewer_session`.

For new plans freeze a `rubric` mapping task-specific category names to maximum points totaling 100. Each verdict then carries `component_scores` with the same categories, and the score must equal their sum. Record the user's permitted raster/vector scope in the review brief. For a below-threshold verdict, identify the concrete repair or the unresolved interpretation/tool limitation with its owner and evidence. The reviewer owns judgments and revisions. An unchanged score or a schema correction does not open a new visual repair cycle.


A rejection with no critical or required fix needs a `disposition`, for example
`{"owner":"tool_maintainer","reason":"A font face cannot be addressed","resume_when":"A rendered fixture resolves that face"}`.
Other supported owners are `reviewer` and `task_owner`. A disposition can also
keep a required finding open when a specific dependency prevents repair.
`page_dispatch.needs_disposition` exposes that owner and condition; the page
continues to block assembly. With native peer handoff support, publish the
result's `verdict_id` as the `peer_publish.handoff_id`.
