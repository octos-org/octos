# Octos Release Contract — 2026-04-17

This document is the durable record for the release slice completed on
2026-04-17. It exists so the exact scope, proof, and ship state survive chat
compaction, crashes, and local machine drift.

## Scope

This release deliberately did not try to finish all of Phase 3.

It only targeted the user-visible reliability slice that had to be green before
more harness work could continue:

- final background deliverables must persist durably
- slides reload must keep the final deck card
- site reload must keep the final preview card
- long-running background work must survive reload without ghost turns
- normal chat must remain usable while background work is running

## Shipped Fixes

The release branch contains these behavior changes:

- plugin stdout fallback now detects generated files and records
  `file_modified` plus `files_to_send`
- assistant/background file delivery is persisted with topic-aware metadata
- `send_file` delivery stays attached to the correct topic-scoped session
- `activate_tools(["mofa_slides"])` is idempotent for already-active tools
  instead of falsely reporting failure
- slides prompts explicitly call `mofa_slides` directly in slides sessions

## Public Release Truth

The only release truth for this slice was:

- `https://dspfac.crew.ominix.io`

Verification-only lanes:

- `mini1`
- `mini3`

## Deploy State

The built `octos` binary from the release branch was deployed to:

- `mini1`
- `mini3`

Both hosts were verified healthy on:

- `http://127.0.0.1:3000/api/auth/status`

## Required Proof

The release was not considered complete until all of these passed after deploy:

1. `tests/live-slides-site.spec.ts`
2. `tests/live-browser.spec.ts`
   - `deep research survives reload without ghost turns`
   - `research podcast delivers exactly one audio card after reload`
3. `tests/runtime-regression.spec.ts`
   - `TTS spawn_only returns immediately with bg_tasks=true`
   - `regular messages work while TTS runs in background`

## Result

All required post-deploy gates passed on the public canary.

Specifically:

- `live-slides-site.spec.ts`: `2 passed`
- `live-browser.spec.ts` targeted reload/background checks: `2 passed`
- `runtime-regression.spec.ts` targeted nonblocking/background checks: `2 passed`

## Shipped Git State

Release branch:

- `release/2026-04-17-canary-slides-background`

Release commit:

- `9cc0ce1` `Fix canary background artifact delivery and slides reload`

## Explicitly Not Claimed

This release does **not** claim that all of Phase 3 is complete.

Still open after this release:

- broader Phase 3 hardening and operator work
- full harness runtime formalization
- full coding-harness expansion
- the remaining issue set under `#412-#416` and `#433-#439`

This document only certifies the completed release slice above.
