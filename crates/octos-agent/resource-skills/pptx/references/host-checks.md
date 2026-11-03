# On-demand PPTX quality tools

The producing model owns content review, visual judgment, repair planning and
its delivery decision. The harness provides reliable access to the real artifact
and useful measurements. Ordinary PPTX work does not install an automatic visual
reviewer, a completion veto or a controller that schedules another repair turn.
Use the tools below when they help answer a concrete quality question. Batch
mechanical work and choose the review scope from the task and observed changes.

## Available evidence

| Need | Tool | What the result establishes |
| --- | --- | --- |
| Package structure | `scripts/check_pptx_compat.py INPUT.pptx --out REPORT.json` | Scoped ZIP/XML/package diagnostics; native PowerPoint opening remains separate. |
| Text, notes and image inventory | `scripts/inspect_slide_layers.py INPUT.pptx --out INVENTORY.json` | Actual native text, raw formatting, speaker-note bodies (absent/empty distinguished) and embedded image identities, across all slides or `--page N`. |
| Actual rendered appearance | Installed PPTX renderer, or the deployment's checked render tool | Images tied to the candidate; read these images in the main model conversation. |
| Paired text/artwork measurement | `scripts/native_inspect.py --cycle CYCLE.json --out REVIEW_DIR` | Text measurements and source/full/blank comparison views; optional `--objects`, `--previous`, `--regions`; `--sweep-pages PAGE_ID ...` supplies separate overlapping crops even without detected findings. |
| Detailed text ownership | `scripts/audit_text_regions.py` with a bound render and model-derived regions | Glyph positions and region diagnostics for the model to interpret. See [region audit](text-region-audit.md). |
| Configured copy/style checks | `scripts/pptx_contract.py`, used by `scripts/pptx_host_check.py` | Assertions supplied by the current task, not inherited page counts or font rules. |

Use an existing builder/render route rather than rewriting infrastructure to
perform a review. The native cycle can batch build, paired rendering and inspection
with `--inspect`. Its result is evidence, not visual approval. For another builder,
inspect that builder's actual exported PPTX and render; no native-cycle adoption
is required merely to review an existing deck.

Images must reach the model, not only an OCR transcript or a separate reviewer's
summary. Inspect full pages and useful enlarged regions at a readable scale.
Source artwork helps establish ownership and visual intent; the user's current
copy is authoritative for wording and facts. Native text bounds, OCR and layout
flags can be imperfect. Resolve suspected defects against the actual pixels.

## Model-directed review and repair

Choose a useful review order, group related fixes and check affected neighbors.
When one repeated component has a defect, inspect comparable components where
that could reveal another real issue. Infer this from current content and layout;
there are no fixed page numbers, target labels or repair coordinates in the skill.
Do not invent whitespace thresholds, minimum font sizes or score targets.

After a change, inspect its actual rendered effect and relevant surrounding
regions. Reuse evidence for unchanged artifact content. The model performs
whole-deck integration review for ordering and cross-page consistency before
claiming completion; this does not require a second model to re-review every
unchanged object or a long mandatory report per object. Record concise findings,
checks and remaining limitations sufficient to support the delivery statement.

For long-running tools, use runtime job handles and completion notifications.
The notification reports completion, failure and output paths; it does not decide
what to repair. A renderer or checker outage is an infrastructure limitation,
not evidence that otherwise correct slide content must change.

## Optional second opinion

If another visual opinion is useful, the model can call the configured review
service or `scripts/pptx_host_check.py`. Read `tools/QUALITY.md`, if supplied, for
the callable endpoint or CLI. Reports include actual artifact hashes, findings,
image paths and coverage. A truncated summary is not the full set of observations.
Read relevant images and evidence; evaluate the suggestions rather than treating
an external verdict as a command to edit or an approval required for every task.
Repeatedly requesting a review of identical inputs to obtain a different answer
adds no evidence. Preserve contradictory observations and resolve them explicitly.

The adapter supports package/native-object checks, rendering, optional visual
review, optional raster-text collision proposals and before/current defect views.
See [the configuration format](pptx-contract.md) when configuring those features.
Use the current brief to supply content, typography and reference requirements.
A structure-only result does not establish visual quality; OCR can mistake icons
for text. Missing capabilities and uncertain measurements must remain explicit.
These tools provide evidence without selecting the producer's repair strategy.

## Explicit external acceptance arrangements

The default production/repair workflow uses [peer review and exit](peer-review-exit.md).
It does not turn these diagnostics into an automatic aesthetic judge. A user may
instead request a frozen benchmark, a tools-only run, or additional enforced
acceptance. Preserve that explicit policy; keep reviewer observations separate
from producer decisions and distinguish self-review from external evaluation.
Do not silently add feedback or change acceptance rules during a model comparison.

For a tools-only run, leave automatic delivery validators and observer feedback
disabled. A completion record represents the model's claim; a post-run evaluation
can assess it without automatically restarting the producer. Existing runtime
permissions and user-authorized execution boundaries still apply.

For the default Octos peer repair loop, see
[continuation evidence](continuation-evidence.md), including the configuration
and runtime-consumption checks. The adapter binds existing reports and producer
judgments; it is not an external aesthetic judge or a time/cost limiter.
