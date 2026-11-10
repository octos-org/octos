# DOT pipeline retirement — provenance and retained contracts

This is a provenance record for decisions already made and shipped, not a new
architecture proposal or a request to restore the DOT runtime.

## Decision record

- **PR [#2691](https://github.com/octos-org/octos/pull/2691)**, merged as
  commit `92047764630e16882e45dd66708a30c418ad0809` by maintainer **ymote**
  on **2026-10-07**, is the runtime decision: “use native research with shared
  credentials and retire DOT runtime.” It removed DOT tool registration and
  bundled graph installation from serve, gateway, child-agent, and plugin
  reload paths. `ProfileRuntime::bootstrap` now deliberately leaves
  `pipeline_factory = None`, with the comment that DOT graph execution is
  retired from Octos. The PR validation included profile/session/reload checks
  that `run_pipeline` was absent and native-research credential checks.
- **PR [#3044](https://github.com/octos-org/octos/pull/3044)**, merged as
  commit `96124ea2faaa2cb8ba3c29274efcbdb0acfec96a` on **2026-11-06**, removed
  the then-unused standalone `octos-pipeline` crate under shu's 2026-11-05
  T-0396 decision. Its **UPCR-2026-RA5312057** records that this crate/comment
  cleanup made no OUP wire-contract change. That PR deliberately left generic
  task-supervisor fixtures, the reserved host extension hook, and historical
  design documents for separate, semantic review; those strings are not proof
  that the model-facing tool still exists.
- Pipeline design issues **#1408**, **#1409**, and **#1760** concerned the old
  IR/DOT engine and are closed. They are historical requirements context, not
  current implementation commitments.

## What is retired

The following Octos CLI/server product commitments are no longer available:

1. The model-facing `run_pipeline` tool, including installed DOT graphs and
   typed graph IR execution. Files left in a profile's old `pipelines/`
   directory are not loaded or executed.
2. An IR pipeline producing a later `turn/spawn_complete` background result.
   The generic background-completion event remains, but there is no IR producer
   for the old retained scenario.
3. DOT-specific M8 per-node task trees under a `run_pipeline` tool call, and
   restart from a DOT node using cached upstream node outputs.

The general OUP task, progress, cancellation, relaunch, and event types were
not removed by this product decision. A remaining API name or a generic test
fixture using `run_pipeline` as an example string must not be read as a promise
that Octos registers that tool.

## Replacement and coverage that remains

Research uses the native Rust **`search`** tool and **`deep_crawl`** for
rendered sites. The host supplies synthesis credentials from the saved
`strong` lane when present, otherwise from the chat route. Matching provider
and endpoint rules prevent a lane from borrowing an unrelated provider's key;
explicit lane overrides still win. Native research preserves failed reads and
coverage gaps and refuses to present a report synthesized from no usable
sources as successful research.

Long research can still use `spawn` in background mode. The retained
`live-browser.spec.ts` research/reload scenario, generic spawn failure
recovery, native plugin cost attribution, session recovery, and overflow/thread
binding suites continue to cover cross-system delivery and recovery. The
`octos-services` research-lane credential tests cover inheritance and the two
credential-isolation negative cases. Focused `octos-cli` profile tests pin both
DOT absence across bootstrap/reload/session discovery and native synthesis
configuration across plugin reload.

Historical M8/M17 documents remain useful for the generic supervised-spawn and
plugin contracts. Their DOT-specific tracks and acceptance rows are superseded
by this record; they are not instructions to recreate the deleted engine.
