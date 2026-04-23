---
name: harness-starter-generic
description: Minimal harnessed single-artifact starter. Use as a template for a custom app that produces one deliverable.
version: 1.0.0
author: octos
always: false
---

# Harness Starter: Generic

The minimum legal shape of a harnessed custom app. Copy this crate when you
want to produce a single deliverable file under a declared `primary`
artifact and rely on the runtime to verify and deliver it.

## What this starter proves

- A `[spawn_tasks.produce_artifact]` block bound to `[artifacts].primary`.
- A single `on_verify` file-exists check.
- A smoke test asserting manifest parses, workspace policy parses, the
  artifact is produced, and the stable `lifecycle_state` values are
  `Queued -> Running -> Verifying -> Ready`.

See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md` for the full contract.

## Tools

### produce_artifact

Produce a single text artifact under `output/`.

```json
{"label": "quarterly review"}
```

**Parameters:**
- `label` (required): free-text label recorded inside the artifact and
  used to derive its filename.

**Artifact:**
- writes `output/artifact-<slug>.txt`
- the workspace policy's `primary` glob (`output/artifact-*.txt`)
  resolves to this path.
