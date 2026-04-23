---
name: harness-starter-report
description: Harnessed report-generator starter. Writes a markdown artifact under reports/ and relies on the workspace contract to deliver it.
version: 1.0.0
author: octos
always: false
---

# Harness Starter: Report

A report generator that produces a markdown artifact as its single
deliverable. Copy this crate when you want to build an app that ships a
rendered document (weekly summary, research brief, status report, etc.).

## What this starter demonstrates

- `primary = "reports/*.md"` — glob-based artifact resolution.
- `file_size_min:$primary:256` — validator gates delivery on non-trivial
  content length.
- `on_failure: ["notify_user:..."]` — structured failure notification.

See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md` for the full contract.

## Tools

### generate_report

Generate a markdown report for a topic.

```json
{"topic": "Q1 Sales Review"}
```

**Parameters:**
- `topic` (required): report topic; also used to derive the filename
  (`reports/q1-sales-review.md`).
- `body` (optional): markdown body. When absent, a stub body is written.

**Artifact:**
- writes `reports/<slug>.md`
- the workspace policy's `primary` glob (`reports/*.md`) resolves to this
  path.
- the runtime refuses to mark the task `ready` until the file exists and
  is at least 256 bytes (the starter writes padded stub bodies to keep the
  smoke test deterministic).
