# Efficient production with evidence

Apply these rules without changing the user's chosen design or weakening QA.
No fixed page numbers, text, coordinates, or model-specific quality verdicts.

## Calibrate before scaling

Use the [bundled native builder](native-builder.md) for blank artwork plus native
text. Preserve the generated layout and canonical copy. Reuse its pixel/point
conversion, typography implementation and applicable deployment checks. Inspect
representative real output to detect font or layout differences; do not make
writing a calibration/measurement framework a prerequisite for page dispatch.
Recalibrate the affected behavior only when the environment, chosen fonts or a
demonstrated defect requires it. Batch independent generation and preparation.

For image-first tasks every page must have a complete generated design before
its conversion. Calibrating selected pages does not waive this requirement.
Choose diagrams by the relationships being explained; the calibration process
does not impose one universal two-column or poster layout.

For authorized parallel work, keep this same reusable page pipeline. Share its
implementation and verified environment, not only an interface specification;
workers own page data and local exceptions. Apply the responsibilities in
[independent page workers](independent-pages.md). Shared code must not become a
mutable global layout file that serializes all page edits.

## One production cycle

For the bundled data-driven builder, `scripts/native_cycle.py run` already combines
construction, diagnostics and full/blank rendering; use its pending/resume protocol.
The custom-build route below is for an existing different authoring backend, not
an instruction to write another pipeline.

When `tools/pptx_cycle.py` is supplied, give it a JSON plan with your own build
command, PPTX path and validator commands. It performs routine work in one
operation, never authors layout or makes visual judgments:

```json
{
  "id": "candidate-a",
  "out": "verification/candidate-a",
  "build": [".venv/bin/python", "source/build.py"],
  "pptx": "exports/candidate.pptx",
  "checks": [{"id": "copy", "argv": [".venv/bin/python", "source/check_copy.py"],
              "inputs": ["source/check_copy.py", "brief/outline.json"]}],
  "size": "3840x2160"
}
```

Run `python tools/pptx_cycle.py run <plan.json> --wait-seconds 40` using the
workspace interpreter. It builds once, checks the package, runs declared checks
and submits a render. A pending result contains the durable job receipt and a
`watch_external_job` argument object. If that native tool is available, use it
and continue independent work. Otherwise use the service's bounded early-return
wait. After completion run `python tools/pptx_cycle.py finish <cycle.json>`.
Do not rebuild while waiting or replace a fixed sleep with another fixed sleep.

On the next candidate set `previous` to the preceding `cycle.json`. The helper
compares native slide parts and transitive package dependencies to select changed
pages. Rendering still converts the whole PPTX to PDF, but rasterizes and presents
only selected pages for focused inspection. Global changes can affect all pages.
It does not assume a changed filename means changed content. Set `final: true`
for the final full render; then complete the existing full/blank and visual
coverage requirements. Empty changed-page sets are not new quality approvals.

Check scripts must return nonzero on failure. Declare their source/manuscript
dependencies in `inputs`; a changed validator cannot silently reuse old evidence.
The helper publishes `.octos/progress.json` and hash-bound reports. These establish
which check ran against which inputs, not that an arbitrary check is sufficient.
Both Pi and Octos can use the same helper and reports.

## Repair only what evidence requires

Apply the [shared reconstruction method](page-method.md). Use `native_inspect.py`
for current native/PDF measurements and batched comparison panels; run the same
helper in solo and delegated work. Copy correctness, legibility, ownership and
explicit user constraints define defects. Differences in antialiased glyph outlines
or a few pixels of otherwise correct spacing are not standalone optimization goals.

Batch independent fixes to identified objects. Inspect the corrected objects
and their affected neighbors. Keep existing finding IDs and original evidence;
do not erase unresolved defects from a new summary. A report bound to an older
PPTX cannot approve a new candidate. Distinguish an unchanged page from a page
that was never inspected.

Combine routine build, structural checks, render submission and evidence collection
in the supplied cycle where available, then inspect its images. Batch related
views with source/render context instead of opening one tiny crop per model turn.
This reduces orchestration calls, not visual coverage or the required final review.

When repeated revisions leave the same defect unresolved, stop guessing layout
constants: compare source/blank/native object/render for that object, test the
suspected cause on a small example, then make one justified correction. A change
in strategy is useful; extra reflection prose is not evidence of improvement.
Do not stop merely because a revision counter fired. Never omit content or relax
quality criteria to reduce request counts.

## Continue the same task

Keep the chosen workflow and remaining scope in the existing project record.
Consult it on resumption and after local change requests. A successful partial
build or a progress message does not complete the requested deck. Continue the
remaining authorized work while it is actionable. If blocked, name the concrete
failed operation or missing input and preserve the remaining work; do not swap
the chosen design method merely to make a file exist. Preserve completed valid
evidence so continuation does not repeat already accepted work.

## Explicitly supervised continuation

When the host has enabled an evidence-controlled task, publish each completed
repair/check cycle using [continuation evidence](continuation-evidence.md). Keep
this opt-in: the adapter reports measured/self-reviewed evidence and the generic
runtime owns scheduling. Ordinary work does not install a completion veto.
