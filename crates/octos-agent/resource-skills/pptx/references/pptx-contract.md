# Configuring project-independent PPTX acceptance

This is the optional independent acceptance deployment described in
[on-demand quality tools](host-checks.md), not the default PPTX workflow.
Ordinary model-led review can use the inspection scripts without this service
or an enforced delivery gate.

Use this when deploying `scripts/pptx_host_check.py`. The producer receives the
brief, acceptance requirements, client command and reports. The host protects
the policy, checker process, reference inputs, credentials and receipts. It is
not enough to put files in another directory if the producer can write there.

## Project contract v1

The same checker accepts native text/shape/table decks, image-derived decks,
different languages, page counts and styles. It never assumes that a deck has
paired artwork. This illustrative contract has a centered cover and left-aligned
body; it is not a default template:

```json
{
  "version": 1,
  "artifact": "exports/review.pptx",
  "pages": [
    {"id": "cover", "title": "Quarterly review", "brief": "Introduce the review."},
    {"id": "results", "title": "Results", "brief": "Revenue is $12M.",
     "required_text": [{"text": "$12M", "count": 1}]}
  ],
  "checks": {"text_in_canvas": true, "min_native_text_objects": 1},
  "rules": [
    {"id": "cover-title", "page_ids": ["cover"],
     "select": {"role": "title"}, "count": 1,
     "style": {"font_names": ["Arial"], "font_size_pt": 28, "alignment": "center"}},
    {"id": "body-title", "page_ids": ["results"],
     "select": {"role": "title"}, "count": 1,
     "style": {"font_names": ["Arial"], "font_size_pt": 24, "alignment": "left"},
     "consistent": ["left_origin", "top_origin"], "tolerance": 0.001}
  ],
  "visual": {"required": true, "instructions": "Preserve the requested editorial style."}
}
```

- `version` and `artifact` are required. Artifact paths are confined to the
  producer workspace. Configured `pages` is an ordered, complete manifest; its
  length sets expected count. Omit it when count/content have not been specified;
  do not infer expected count from producer claims when the brief fixes it.
- A page may supply `title`, `brief`, `required_text`, `reference` and `blank`.
  The last two are optional host-protected paths relative to the contract file.
  Titles and required text are matched against whole native objects (including
  table cells), with whitespace normalized while preserving word boundaries.
  Splitting a required sentence across objects needs a different contract/check;
  this adapter does not silently treat the fragments as an exact match.
- `checks.text_in_canvas` is opt-in: some decks deliberately bleed text past the
  edge. `canvas_tolerance` is a normalized fraction; default zero.
  `min_native_text_objects` applies per slide only when supplied; do not require
  text on intentional image-only pages without a user requirement.
- Each `rules` entry has a unique ID. `page_ids` optionally scopes the rule to
  a consistency group; omit it to apply on every slide. Select one of
  `{"role":"title"}`, `{"name":"shape name"}`, `{"text":"exact text"}` or
  `{"all_text":true}`. A title uses the configured authoritative text, falling
  back to a native title placeholder only when no title text is specified.
  `count` asserts matching objects per selected page. A non-count rule matching
  nothing fails rather than vacuously passing.
- Style fields are optional: accepted `font_names`, `font_size_pt`, `bold`,
  six-digit `color_rgb`, and `alignment` (`left`, `center`, `right`, `justify`).
  Direct and paragraph formatting are read; unresolved master/theme formatting
  produces an `unknown` finding. No fixed font family/size is built into the code.
- `consistent` compares title/text origins within that rule's selected group,
  not against coordinates learned from previous slides. Cross-page consistency
  is checked again on the assembled deck; a one-page check cannot prove it.
- `visual.required` defaults false. When false, `visual: not_requested` in the
  result is not visual acceptance. When true, every page and inventoried object
  must receive an independent pass; uncertainty is not a pass. Briefs are factual
  authority, while supplied artwork is a visual reference that may contain errors.

## Host configuration and execution

Keep host configuration outside the producer workspace:

```json
{
  "workspace": "/work/producer",
  "contract": "review-contract.json",
  "evidence_dir": "/host/evidence/review",
  "renderer": {
    "identity": "renderer-version+font-manifest-hash+settings-version",
    "command": ["/host/venv/bin/python", "/host/render_deck.py",
                "{pptx}", "--out", "{out}", "--size", "3840x2160"],
    "timeout_seconds": 400
  },
  "reviewer": {
    "base_url": "https://YOUR-PROVIDER/v1",
    "model": "YOUR-VISION-MODEL",
    "api_key_env": "HOST_REVIEW_API_KEY",
    "parameters": {"max_tokens": 12000},
    "timeout_seconds": 600
  }
}
```

Renderer dimensions must match the actual deck aspect ratio; the example's
16:9 size is not imposed by the checker. The command is an argument list, never
a shell template. It receives an immutable snapshot and a protected output
directory. Its `render-report.json` must include:

```json
{"input_sha256":"actual PPTX SHA256", "pages":2, "page_images":[
  {"page":1,"path":"png/slide-01.png","sha256":"actual image SHA256"},
  {"page":2,"path":"png/slide-02.png","sha256":"actual image SHA256"}
]}
```

The host verifies hashes, ordered full coverage, confined paths and readable
images. Supply a renderer identity that changes with renderer/font/environment
updates. Local renderer command files are also hashed. A PDF or PNG supplied by
the producer is not acceptable proof of the current PPTX. This adapter reports
native PowerPoint opening as unverified; configure a separate validator if that
is a delivery requirement. LibreOffice rendering cannot satisfy it.

The reviewer uses an OpenAI-compatible chat-completions endpoint. Choose a model
that accepts images and provider-supported parameters. No model or reasoning
mode is hardcoded. Credentials stay in the host service environment. Other API
protocols need another adapter, not a false assertion of compatibility.

`review_transport.py` collects streamed responses and requires terminal markers
before parsing a judgment. It retries only transient network failures, throttling
and selected server errors, at most three attempts. A valid negative/uncertain
judgment is not retried to obtain a pass. Failed or partial streams never satisfy
acceptance. This avoids non-streaming reviews silently exceeding a proxy's idle
read timeout while the model is still producing its answer.

```sh
python /host/pptx_host_check.py --host-config /host/host.json
python /host/pptx_host_check.py --host-config /host/host.json --serve 65453
```

The service binds localhost and accepts `POST /check` with `{"mode":"delivery"}`,
or `{"mode":"pages","pptx":"jobs/intro/candidate.pptx","pages":["cover"]}`.
Page requests can select only host-declared IDs and inherit their rules. Use
`scripts/pptx_check_client.py --url http://127.0.0.1:65453` as the host-declared
root `phase="delivery"` command. For worker preflight append `--pptx` and `--pages`.
The client needs only the Python standard library; it never receives model keys.
Exit codes: 0 for
the configured checks passing, 1 for artifact findings, 2 for checker errors.
The root checks the full artifact regardless of optional worker preflight calls.

Acceptance receipts bind artifact bytes, project policy, source images, checker
code, renderer identity and reviewer configuration. Same page pixels/native
objects can reuse protected visual evidence after another page changes. Public
reports are copies for repair; they never authorize acceptance. Reference/policy
changes require rechecking and restarting a service to pin the new contract.

## Scope and limitations

Package preflight checks ZIP integrity, XML parsing and known autofit conflicts;
it is not complete OOXML validation. The native inventory covers text frames,
nested groups and table cells; chart/SmartArt labels and theme-resolved typography
are not fully measured. Rotated/flipped group placement is unresolved, so a
required geometric check reports unknown. Rendered review can inspect appearance
but does not prove all internal editability or all factual claims.

Do not translate a model's visual opinion into certainty. Findings need specific
visible evidence. Host checks enforce execution, coverage and current-artifact
binding; a visual model can still miss a defect or flag a correct layout.
