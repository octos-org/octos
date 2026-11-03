# Checked asynchronous rendering

Use this boundary when the workspace supplies the compatible JSON `/jobs`
render service configured by `tools/service.json`. Call the provided helper
instead of rewriting HTTP polling and output-directory allocation in a builder:

```sh
python <skill>/scripts/checked_jobs.py --workspace <workspace> \
  render-checked <candidate.pptx> --output-base <owned-proof-directory> \
  --attempt <attempt-label> --size 3840x2160
```

Source and output paths are workspace-relative. Dimensions must match the deck's
aspect ratio and the task's requested export size; the example is not mandatory.
`--workspace` allows the script to run directly from its installed skill folder.
The old `workspace/tools/` placement also works without this option.

The helper assigns a source/settings-bound output namespace and job ID. Changed
source bytes get a fresh directory even with the same attempt label. An unchanged
request resumes the existing job. Completed results require matching source,
snapshot and image hashes, every ordered source page exactly once, and matching
reported and actual PNG dimensions. Consume the returned `result.out` and
`result.report.page_images`; never assume a fixed `render-editable/png` path.

CLI `wait` and `status` exit **0 only on completion**, **1 on failure** and **2
when still pending**. `render-checked` additionally verifies evidence before
exiting 0. Stop dependent copying, OCR and QA when it returns nonzero. A pending
job retains its ID and receipt: wait on that job or do independent work. `submit`
exit 0 means acceptance only. Wake-up metadata remains in the receipt.

Python builders may import `JobClient`, `checked_render`, `JobError` and
`JobPending` from the script. Let failed or pending exceptions stop dependent
stages. Do not fall back to old PNGs. For a diagnosed retry or a renderer change
outside the encoded settings, supply a new attempt label; preserve prior evidence.
Workers keep separate source/output namespaces. This is not a render quota.

Other backends remain valid when they provide equivalent fresh-attempt isolation,
explicit terminal success, source-to-output binding and complete page/dimension
coverage. Do not assume this service exists or substitute a mock result for an
unavailable renderer. These checks establish evidence identity and coverage;
inspect the actual pixels separately to judge text, layout and visual quality.

For tool maintenance, run `python -m unittest discover -s <skill>/scripts/tests
-p test_checked_jobs.py`. Tests use isolated synthetic receipts and do not submit
production jobs.
