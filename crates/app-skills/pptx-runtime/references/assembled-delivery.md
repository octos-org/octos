# Finish a page-assembled deck

## Compile independently approved pages

When a version 2 plan already lists multiple pages, omitting `verification_mode`
at initialization also selects `per_page`. The coordinator needs a different
reviewer session for every listed page before it can freeze the roster.

For new separable decks, stage one maker and one dedicated independent reviewer per page. When the host supplies only a slug or task queue ID, ask every staged peer in its first brief to run `pptx_tool({"action":"doctor","check_renderer":false})` and publish its exact `continuation.host_identity`. Gather the replies in parallel before initialization. A slug addresses peer messages; the authenticated session owns the candidate or verdict. Write their real sessions, ordered page IDs, unique candidate and verdict paths, and the rubric into the version 2 page plan. Before production call `evidence_init` with `"verification_mode":"per_page"` and the intended delivery paths, for example `["final/deck.pptx","final/qa.json"]`. This freezes that exact roster and page order. With limited capacity, retain the distinct sessions and run ready pages in waves.

Start the source work, asset generation, slide construction and final-size DeckCraft rendering on each page's own lane. Its reviewer examines the bound one-slide PPTX and PNG against the reference, checks the editable elements, and sends any defects straight back to that maker. Use `page_review_submit` on the current candidate. An accepted verdict includes the usual rubric, score, evidence files and findings plus four observations under `verdict.delivery_checks`.

```json
{"copy":{"status":"pass","basis":"<verified wording and data>"},
 "artwork":{"status":"pass","basis":"<observed illustrations and diagram detail>"},
 "readability":{"status":"pass","basis":"<observed typography, clipping, contrast and spacing>"},
 "editability":{"status":"pass","basis":"<verified editable content in this PPTX>"}}
```

After the last page passes, the coordinator calls `page_assemble(output)` once, then supplies the returned receipt to `{"action":"evidence_assembled","assembly":"<receipt path>","qa":"final/qa.json"}`. The runtime reopens the compiled package, checks its integrity, and binds ordered page artifacts, reviewed previews and independent verdicts to the delivery evidence. The QA lists each page's reviewer, score, observations and source preview. Read `evidence_status` on the next turn. The visual judgments apply to the exact single-slide inputs. A separate inspection of the compiled pixels requires the whole-deck procedure below.

## Independently inspect an assembled deck when requested

For version 2 page plans, the declared coordinator runs `page_assemble` in its own host session. After `page_assemble`, the coordinator gives the independent final reviewer the returned native assembly receipt path, final PPTX and its final DeckCraft `render.json`. A passing page verdict covers its individual slide. The final reviewer checks page order, agreement with the approved design, editability and actual rendered output. Reuse valid evidence for unchanged pages and report remaining limitations. Unknown lettering in a reference is not by itself a correctness or readability failure.

Carry the user's image-processing constraints into the final review brief. When the user prohibits image scripts, the reviewer opens each supplied reference and corresponding DeckCraft PNG with the host's image reader. When it reports `shown_to_model:false` due to size, call the native `review_preview` action on those exact files and open the returned views. Each whole-page PNG retains the original aspect ratio and binds the source SHA-256. For small lettering open native `crop` results from the full-resolution file. Group reads as needed. Record the original paths and reconcile the inspected files with the completed render receipt before signing. If the reviewer cannot open a required image, record that page as unknown until it can be inspected.

The reviewer can request an unanswered observations file before writing the human-readable report. Call `assembly_review_submit` with `assembly`, `render_receipt` and `template_output` such as `review/final-observations.json`. The native action fills the actual receipt hashes and page IDs. Each domain begins as unknown and requires the reviewer's own basis. Submit the completed observations, report and verdict output in a separate call.

The observations JSON has this structure. Include every plan page ID. Use the hashes of the actual native assembly and render receipts that were inspected.

```json
{
  "assembly_sha256": "<assembly receipt SHA-256>",
  "render_receipt_sha256": "<final render receipt SHA-256>",
  "pages": {
    "<page ID>": {
      "copy": {"status":"pass","basis":"<what was checked>"},
      "artwork": {"status":"pass","basis":"<what was checked>"},
      "readability": {"status":"pass","basis":"<what was checked>"},
      "editability": {"status":"pass","basis":"<which essential elements remain editable>"}
    }
  },
  "critical_defects": [],
  "open_required_findings": []
}
```

Each status is `pass`, `unknown` or `fail`. Record open findings on the relevant page and domain. Then the **independent reviewer's own session** calls:

```json
{"action":"assembly_review_submit","assembly":"<page_assemble receipt path>","render_receipt":"final/rendered/render.json","observations":"review/final-observations.json","report":"review/final-report.md","output":"review/final-verdict.json"}
```

The **continuation owner** collects this verdict:

```json
{"action":"evidence_assembled","review":"review/final-verdict.json","qa":"final/qa.json"}
```

Reserve the verdict JSON path for `assembly_review_submit` and the policy's QA output path for `evidence_assembled` from the beginning of the job. Store reviewer observations, package diagnostics and draft verdicts under their own paths. If a draft already occupies one of those destinations, preserve it under a separate path and update its writer before submitting. Keep existing native attestations at their recorded paths and follow the revision procedure below.

The tool revalidates accepted native assembly inputs, final render order and bytes, the independent review, and package integrity. The evidence snapshot binds all declared delivery paths, including the QA and verdict files when specified. Repeating unchanged inputs preserves the attempt and artifact identities. On a later tool turn, read `evidence_status`. When all required checks pass, delivery bindings are current and the native observer consumed the snapshot, end the task. Report an actual blocker or route open findings through the existing page peer. A status message does not retire or close a peer.

Large review histories use SHA-256-bound dependency manifests when the checked snapshot would exceed 256 KiB. The observer verifies each file in those one-level inventories and hashes a shared file once per verification. Every declared deliverable remains an explicit snapshot artifact. A host that predates this evidence format cannot certify the sharded snapshot.

### Repair a failed final delivery

An independent reviewer can reject the actual delivered bytes after a maker starts the next draft. Octos checks the unchanged plan and canonical delivery, reproduces the package from the recorded single-slide artifacts, and validates the final renders. A passing verdict additionally requires the current page sources and independent approvals. Preserve the delivered page artifacts while repairing their drafts.

The final reviewer submits the unresolved check with its observed evidence. The affected page reviewers release their accepted candidates, page makers publish corrected native slides, and the page reviewers independently accept the new candidates. The coordinator then calls:

```json
{"action":"page_assemble","output":"final/deck.pptx","repair_review":{"path":"review/final-verdict.json","sha256":"<digest returned by assembly_review_submit>"}}
```

The pinned verdict identifies the original assembly, deck, render receipt and intended preview files. The operation checks the coordinator, task, current verdict, original delivery and approved pages, then publishes the repaired deck and actual DeckCraft renders under the same paths. It leaves byte-identical previews in place and keeps the previous receipt metadata in the returned repair history. Retry the same request after a transport interruption. Preserve the rejected verdict and its source observations/report until the repair has returned. The final reviewer checks the new bytes, submits new observations bound to the returned assembly and render receipts, and supplies the original verdict digest as `previous_review`. The coordinator records the new verdict with `evidence_assembled`.

If the approved DeckCraft renderer runs separately from the orchestration runtime, add `"stage_repair_render":true` to the repair request. Run the returned `render_request` using the approved deployment. Repeat the original repair request without the stage flag to verify the staged render receipt and promote the verified output. Staging preserves the canonical delivery, and all review, version and render-binding checks also apply during promotion.

With no verified staged render, the repair action checks its local picture renderer and requests the separate render automatically if that check fails. A host can also set `OCTOS_PPTX_REQUIRE_CLEAN_RENDER=1` on the registered runtime to apply this preflight to every newly rendered deck. Existing source-bound render receipts remain available for verification.

In per-page mode each native verdict refreshes a single rolling editable package at `.octos/pptx-progress/ready.pptx`. It contains only signed current pages, in plan order. Withdrawing the last page removes the working copy. The coordinator can run `page_progress` to recover after interruption and read the included page IDs. Final delivery still uses the complete `page_assemble` receipt.
