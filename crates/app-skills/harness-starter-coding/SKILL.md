---
name: harness-starter-coding
description: Harnessed coding-assistant starter. Produces a unified-diff artifact and a file-list preview under patches/.
version: 1.0.0
author: octos
always: false
---

# Harness Starter: Coding

A coding-assistant starter. Produces a unified-diff file as its primary
deliverable plus a preview artifact listing the changed files. Copy this
crate when you want to build an app that ships a patch/diff/changeset.

## What this starter demonstrates

- `primary = "patches/*.diff"` — the canonical deliverable.
- `preview = "patches/*.files.txt"` — a secondary artifact the operator
  surface may use to summarize the change.
- `file_size_min:$primary:64` — validator rejects empty patches.
- Multi-artifact spawn task binding (`artifacts = ["primary", "preview"]`)
  so the runtime delivers both files.

See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md` for the full contract.

## Tools

### propose_patch

Render a patch as a unified-diff file plus a preview file list.

```json
{
  "title": "Fix typo in hello",
  "hunks": [
    {
      "file": "src/lib.rs",
      "new_content": "pub fn hello() -> &'static str {\n    \"hi\"\n}\n"
    }
  ]
}
```

**Parameters:**
- `title` (required): short title; derives the filename
  (`patches/fix-typo-in-hello.diff`).
- `hunks` (required, non-empty): list of `{file, new_content}` hunks. Each
  hunk becomes a full-file replacement hunk in the diff.

**Artifacts:**
- `patches/<slug>.diff` — primary unified-diff file.
- `patches/<slug>.files.txt` — preview list of changed files.

## Replace this stub

The starter generates full-file replacement hunks. A real coding assistant
should use `similar` or `diff` to compute minimized unified diffs, and
consider running the patch through `git apply --check` in a validator
before delivery.
