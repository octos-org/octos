# Font proposals and immutable revisions

Helpers calculate and manage files. They do not infer correct artwork ownership,
author copy, approve quality or call another model.

## Font proposals

`native_fit.py` uses Pillow and fontTools (`pip install fonttools` in the task
environment if missing). Supply static font files, correct weight and collection
index; no silent substitution. Instantiate variable fonts first. Contextual scripts
need Pillow RAQM support. Font-map paths resolve relative to this JSON:

```json
{"schema_version":1,"fonts":[
  {"family":"Example Sans","bold":false,"italic":false,"path":"fonts/Example-Regular.ttf","index":0}
]}
```

```sh
python skills/pptx/scripts/native_fit.py --spec source/deck.json \
  --fonts source/fonts.json --objects diagram/caption --out work/font-proposals
```

The current size is preserved unless you supply `--min-size`/`--max-size`. Choose
bounds from the user requirements/approved typography, not a universal threshold.
`--max-lines` specifies the intended line policy. Locked styles keep their size.
Identifiers/numbers are not broken merely to fit. Missing glyphs stay unresolved.

For visible-ink placement, add `--ink-align top|middle|bottom`. The current box
becomes the desired visible-ink target; horizontal alignment follows the existing
style. An optional `--targets FILE` supplies different target regions:

```json
{"schema_version":1,"objects":[{"page":"diagram","id":"caption","box":[100,200,500,160]}]}
```

Coordinates are canvas pixels. The helper uses glyph bounds and predicted font
baselines to propose a native frame; it preserves margins and the chosen font size.
It reports a constraint conflict if that frame would cross the canvas or declared
owner. Choose the correct target/owner from the artwork; widening an owner does not
prove text placement. Locked styles remain locked. `needs_style_decision` with
`wrap:false` means the explicit line plan needs a model-selected no-wrap style;
the helper does not override shared typography. Renderer shaping still needs review.

Review `report.json` and `edits.json`. No-fit calls for an allowed reflow/box change,
separate labels for separate owners, or reporting a real constraint conflict.
Never omit copy or invent a smaller minimum. Proposals include measured font hashes
and leave source untouched; actual renderer/fallback still needs visual inspection.

For a new draft apply accepted edits with `native_deck.py patch --spec SPEC --edits
EDITS --out NEW_SPEC`, then render. An empty edit list is not visual approval.

## Automatic revisions

After reviewing a ready cycle, batch changes as existing patch entries containing
`page`, `id`, `set`. A `format` update supplies the complete desired override.
Canonical copy stays protected; copy/asset revisions use the existing source workflow.

```sh
python skills/pptx/scripts/native_cycle.py revise \
  --cycle work/current/cycle.json --edits work/edits.json \
  --job work/jobs/diagram/job.json --workspace "$PWD"
```

Omit `--job` for solo work. The helper allocates a new attempt, snapshots source,
applies edits, builds/renders and returns source/before/after crops. Previous sources,
proofs and accepted handoffs are unchanged. Job revisions stay inside their owned
output directory and cannot edit siblings or the shared contract.

Save the returned revision path. Resume pending work without creating another attempt:

```sh
python skills/pptx/scripts/native_cycle.py revise --resume PATH/TO/revision.json
```

Use completion notifications or bounded waits. `--no-inspect` omits measurement
collection only; it never waives model visual inspection. Failed attempts remain
inspectable and the accepted predecessor stays intact.

After your review, publish a page and a new assembly receipt together:

```sh
python skills/pptx/scripts/native_cycle.py handoff \
  --revision PATH/TO/revision.json --review work/review.json \
  --jobs work/jobs/prepared-jobs.json
```

Use returned `updated_jobs` for assembly. The old receipt is untouched. A receipt
already pointing to another attempt is rejected, not silently merged. Without
`--jobs`, only the page handoff is published. Solo revisions use their new cycle
and artifacts directly; they have no page-job handoff.

Revision feedback includes `review.object_findings`: object IDs, measured clearances,
uncertainty and crop paths, plus persisted/new/no-longer-detected findings from the
previous inspection. Unchanged warnings remain visible. Missing prior measurements
and changed criteria are explicit; expanding an owner is reported as a changed
basis, not a repaired overflow. These are review observations, not acceptance gates.
If the compact list is truncated, its affected-page list and full report retain all
findings. Record a dismissal or repair with its object ID and concrete pixel evidence
in the existing review record; no separate form or external judge is required.

The ownership image pairs the raw render with numbered text frames (orange) and
declared regions (blue). Numbers map to `overlay_index` in the report; owner records
show shared assignments and relative area. Use this to spot whole-panel assignments
where a caption belongs to a smaller card. Local detail crops stay close to the text
even when its declared owner is large; the model determines semantic ownership.

`native_inspect.py --previous-cycle OLD --cycle NEW --out CHECK` provides the same
comparison independently. Missing owner regions are explicit. Declared parent panels
and disappearing machine findings never prove correct label ownership; view the pixels.
