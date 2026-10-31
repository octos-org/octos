# Portable local runtime

The skill package includes its Python dependency lock, a LibreOffice renderer,
and a loopback-only asynchronous tool service. No previous deck or evaluation
directory is required. They measure and render files; the producing model still
reads the images and decides whether the result is correct.

The release embeds this entire skill and extracts it into the current profile's
`bundled-resource-skills/pptx/`. Use the actual skill path from the skill catalog;
no repository checkout or previous deck is needed. Python is not required merely
to discover/read the skill. User-installed `skills/pptx/` takes precedence.

Install Python 3.10+ and LibreOffice, then check/setup an isolated workspace:

```sh
python3 <skill>/scripts/setup_runtime.py --workspace . --check
python3 <skill>/scripts/setup_runtime.py --workspace . --install
.venv/bin/python <skill>/scripts/local_tools.py --workspace .
```

On Windows use `.venv/Scripts/python.exe`. Keep the service running while using
`native_cycle.py`. It writes `tools/service.json`, chooses a free loopback port,
and persists job receipts under `requests/tool-jobs/`. Restarting marks unfinished
jobs interrupted; it never silently repeats a potentially billed image request.
Restart after changing fonts or LibreOffice so render-cache identity changes.

`--check` is read-only. `--install` installs only Python dependencies; it does
not install LibreOffice/fonts, configure keys, or send image/model requests.
Exit code 1 with `python_ready: true` and `render_ready: false` means the Python
tools are ready but LibreOffice is still missing. Choose fonts available on this
host, including CJK fonts when required by the deck, before building.

Rendering uses LibreOffice to export the actual PPTX to PDF, then PyMuPDF to
produce page PNGs and a contact sheet. This does not claim PowerPoint application
validation. Native text inspection remains available on the rendered PDF.

The optional `image` operation uses an Images-compatible API with
`OPENAI_API_KEY`, `PPTX_IMAGE_MODEL` (default `gpt-image-2`) and
`PPTX_IMAGE_BASE_URL` (default `https://api.openai.com/v1`). Configure these on the
host; never put credentials in the skill, deck, job request or release archive.
The service advertises image generation only when a key is configured. API
access and credits are separate from installation and rendering.

Submit an image job with `checked_jobs.py --workspace . submit request.json`:

```json
{"id":"art-first","op":"image","prompt":"Your actual page design prompt",
 "output":"art/page-design.png","size":"3840x2160","quality":"high",
 "reference_paths":[]}
```

For edits, pass existing workspace-relative `reference_paths`; generated pixel
dimensions are recorded from the returned image. Use a new output name for a new
attempt. Never describe an export resize as native generation resolution.

The service does not include a separate model-based visual judge. Use the main
agent's image-reading capability for visual review; OCR is optional assistance.
The existing `native_review.py` remains an optional independently configured
second opinion, not an installation prerequisite or completion authority.
