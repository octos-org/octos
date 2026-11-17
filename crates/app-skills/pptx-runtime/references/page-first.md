# Page-first admission

Use this task-local contract for a full source-data deck whose independent pages
each have their own maker and independent reviewer. It requires native page
build/cycle receipts; source-less PPTX editing can follow the ownership/review
workflow but must not fabricate these receipts or claim this gate supports that
route. It is not a scheduler or visual grader. Discover
`doctor.page_first_gate.supported` before relying on it.

Before page production, the coordinator writes `.octos/pptx-page-plan.json` in
the **actual shared task workspace**, with the complete final page order:

```json
{
  "schema_version": 1,
  "coordinator_session": "ACTUAL_COORDINATOR_SESSION_ID",
  "min_score": 95,
  "pages": [
    {
      "id": "overview",
      "maker_session": "ACTUAL_PAGE_MAKER_SESSION_ID",
      "candidate": "run/pages/overview/candidate.json",
      "review": "run/reviews/overview.json"
    }
  ]
}
```

Add one entry per page with a distinct maker session. IDs come from actual host
peer receipts, not invented labels. The default minimum is 95/100 when omitted;
an explicit user requirement determines the configured threshold. The host must
verify actual peer execution, ownership and concurrency. Declared IDs alone do
not prove that separate agents ran.

Each maker can publish an existing one-page cycle in one call:

```json
{"action":"page_publish","page":"overview","cycle":"run/pages/overview/pass-current/cycle.json"}
```

Or set `publish_candidate:true` on a new one-page `cycle`. The current host
identity must be the planned maker. The tool derives and verifies bindings,
rejects changed sources/artwork and preserves accepted candidates until the
independent reviewer releases them. Post-build changes use `artifact`,
`render_receipt` and ordered `receipts` paths; the render must match that actual
PPTX. Publication is a review request, not a pass; its `notification_sent:false`
means the host must send the handoff through its existing independent peer queue.

The published candidate JSON uses
the existing runtime convention `{"path":"WORKSPACE_RELATIVE_PATH","sha256":"ACTUAL_SHA256"}`:

```json
{
  "page_id": "overview",
  "maker_session": "ACTUAL_PAGE_MAKER_SESSION_ID",
  "source": {"path": "run/pages/overview/pass-current/page.json", "sha256": "ACTUAL_SHA256"},
  "build_receipt": {"path": "run/pages/overview/pass-current/cycle.json", "sha256": "ACTUAL_SHA256"},
  "artifact": {"path": "run/pages/overview/pass-current/postbuild.pptx", "sha256": "ACTUAL_SHA256"},
  "render": {"path": "run/pages/overview/pass-current/postbuild-rendered/slide-001.png", "sha256": "ACTUAL_SHA256"},
  "render_receipt": {"path": "run/pages/overview/pass-current/postbuild-rendered/render.json", "sha256": "ACTUAL_SHA256"},
  "dependencies": [],
  "receipts": [
    {"path": "run/pages/overview/pass-current/cycle.json", "sha256": "ACTUAL_SHA256"},
    {"path": "run/pages/overview/pass-current/postbuild-receipt.json", "sha256": "ACTUAL_SHA256"}
  ]
}
```

`source` is the actual immutable page JSON used by the build, with its own page
`id`. It must be a referenced page in the final root spec. `build_receipt` is the
build/cycle response's `receipt` binding to the actual `cycle.json` file, with
its `spec`, `sources`, `editable` and one-page `pages`; do not substitute the
outer API response for that receipt file. The gate rehashes the resolved spec and artwork;
it does not rely on a manually complete asset list. Bind additional shared inputs
in `dependencies`. Do not bind unrelated mutable drafts from other pages.
`receipts` is nonempty: include the actual cycle and post-build chain, when used.
Never refresh hashes to hide a changed file; inspect the real build chain.

Native `batch` returns its input/output bindings and a digest of the actual
ordered commands. Persist that full response as the post-build receipt. The
ordered receipt chain must connect the build's editable artifact to the final
candidate. Keep the exact command data for reproduction. `render_receipt` is the
native `render.json` whose input is that final candidate and whose one PNG is the
published render. For a source-only candidate, use the cycle's editable artifact
and its editable-render receipt instead.

`artifact` is the **one-slide final candidate**, including native post-build
repairs. Render that exact file. Its base cycle's `editable.pptx` and PNG do not
describe a different `postbuild.pptx`. Freeze the candidate and its dependencies
while review is in flight. A newer pass gets a new immutable source/output
directory and a new candidate binding; keep the current publication pointer
unambiguous. Do not compare a frozen cycle with an unrelated mutable draft.

For a new plan use [native page verdict submission](page-verdict.md). The
reviewer passes the publisher's opaque candidate ID, visual decision and
existing evidence. The tool binds the exact page and host reviewer, checks all
decision fields, preserves immutable revisions and writes the assigned review.
`cycle(publish_candidate:true, freeze_inputs:true)` can bind content-addressed
page/root snapshots. Bind an approved reference background in the page row's
optional `approved_background` when byte-for-byte reuse is required.

The following on-disk shape is also supported for existing independent reviews:

```json
{
  "page_id": "overview",
  "reviewer_session": "ACTUAL_INDEPENDENT_REVIEWER_SESSION_ID",
  "candidate_sha256": "SHA256_OF_THE_EXACT_PUBLISHED_CANDIDATE_JSON",
  "accepted": true,
  "score": 97,
  "critical_defects": [],
  "open_required_findings": [],
  "evidence": [{"path": "run/reviews/overview/observations.json", "sha256": "ACTUAL_SHA256"}]
}
```

`page_gate.pages[].candidate` returns the current candidate-file binding even
while its review is pending; use that exact digest rather than guessing it.

The reviewer must be separate from the coordinator and **every** page maker.
Evidence includes the actual observations and applicable native review receipts;
the example score is a schema illustration, not an awarded score. This admission
check does not replace `review_pack`/`review_submit`, required object observations,
the agreed rubric, or final independent review.

Call `{"action":"page_gate"}` to see current per-page reasons. With the plan
present, `build`/`cycle` automatically reject **every multi-page output** until
all entries pass, including probes. Multi-page `batch` exports are checked too,
including calls nested inside `operations`. Single-page work remains available.
Multi-page source builds must use the complete planned page order and the exact
accepted page-source bindings, resolved pages and shared canvas/styles. A root
spec that changes fonts, sizes or canvas cannot reuse unchanged page-source
hashes as approval. Batch exports must have the planned page count;
their slide identity, dependencies, editability and fidelity still need final
integration checks. A source-only rebuild refuses candidates with content-changing post-build edits. Reuse those approved slide artifacts in plan order with original formatting and verify the assembled file against them.

The check rehashes candidate/source/artifact/render/dependencies/receipts and
review evidence, checks the one-slide package and image header, and requires the
recorded score, acceptance, zero criticals and no open required findings. It
never assigns a score. `ready_for_assembly:true` is admission evidence;
`quality_approved:false` and `agent_execution_verified:false` remain explicit.

After assembly, the independent reviewer checks the actual final PPTX, native
editability, page order/continuity and its renders against the accepted pages.
Changed pages return to their existing makers and get fresh acceptance before
another assembly. Preserve valid evidence for unchanged pages.

This gate is active only after the plan exists and the loaded native runtime
advertises support. Workspaces without a plan keep existing tool behavior. It
is workflow validation, not a security boundary or authenticated peer audit.
Do not remove/rename the plan, lower its threshold, move output or switch tools
to evade it. Existing authorization and sandbox controls continue to apply.
