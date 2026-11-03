# Independent page worker

Read your job JSON, canonical copy, design and blank images. Paths resolve relative
to the job. The shared contract contains the real typography/canvas requirements.
Reuse supplied preflight evidence and implementation; do not rebuild the environment
or create a private conversion/measurement framework.

Own only the assigned page data/output directory. Shared inputs, sibling pages,
root exports and the contract are read-only. Do not spawn nested workers. One ready
page is one job; there is no fixed revision quota or concurrency cap here.

Use [native builder](native-builder.md) for schema and [fit and revise](fit-and-revise.md)
for font proposals and revisions. Inspect inputs, author data, build/render/measure,
inspect actual pixels, batch necessary repairs, then publish.

Use [review evidence](review-evidence.md) for the three comparisons: manuscript
meaning, source-to-blank artwork changes, and actual final appearance. Review
unflagged regions as well as tool candidates. Inherited artwork defects remain
findings; report read-only input problems to their owner rather than calling them
acceptable because the background contains them.

- Separate labels for separate visual owners. A paragraph box with blank lines
  is not a substitute for positioning captions inside distinct cards/nodes.
- Canonical copy decides words/numbers/units. OCR supplies candidate locations.
- Review font proposals; no invented minimum size, pixel-similarity target or
  extra aesthetic requirement. Missing font/owner evidence stays unresolved.
- Batch related repairs and inspect changed objects with neighbors. Resume pending
  jobs. Reuse unchanged evidence only when source and renderer bindings match.
- Report reproducible helper failures. Never fabricate passing QA or rewrite a
  private validator merely to make a result pass.

For the initial handoff use `native_cycle.py handoff --job JOB --cycle CYCLE --review
REVIEW`. The helper packages source, editable/blank proofs, renders and hash-bound QA.
The existing review schema records truthful inspection, not automatic approval:

```json
{"inspection":{"source":true,"editable_render":true,"blank_render":true},
 "open_findings":[],"observations":["Actual observations from your inspection"]}
```

Unresolved relevant defects mean unfinished work. Return concrete blockers for
missing capabilities/inputs. Use `native_cycle.py revise` for later attempts and
publish with `handoff --revision REVISION --review REVIEW`; report its new job/result
paths. Do not overwrite accepted results. The coordinator integrates current
handoffs and reviews cross-page/final-deck behavior.

Consult [independent pages](independent-pages.md) for custom adapters or schema issues;
routine workers need not read its coordinator/custom-adapter sections.
