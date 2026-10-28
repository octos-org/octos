# Application memory access implementation

Scope: expose explicit remember/correct/forget requests and existing document
promotion through OUP, without a turn or a foreground model call. Memory
processing remains in the existing background consolidator. This serves the
application-access follow-up to #2355; it adds no product-specific concepts.

Baseline: origin/main f28aeebc. The earlier seven pipeline tests are retained
as a separate prerequisite commit (rebased from 29471385).

Review slices:

1. Durable host-request admission and focused recovery/replay tests.
2. Source-filtered promotion and serialization of concurrent nomination, with
   existing callers retaining their defaults.
3. OUP types, codecs, capability and permission wiring, authenticated handlers,
   protocol/authorization tests, specification and wire-inventory updates.

Acceptance: the same request id/body is admitted once even after consumption
or restart; conflicting bodies fail; target validation uses the authenticated
profile; restricted credentials cannot invoke either method; promotion filters
before limiting and preserves real-load eligibility. Acknowledgements mean
accepted/staged, never model completion. Tests must exercise the actual handlers
and existing consolidation, not just parameter helpers.

No permanent app record ledger, consumer revision changes, teacher policy,
background model scheduler, deployment or publication is included. Full product
integration and model-quality/latency acceptance remain separate work. Handling
arbitrary in-flight regeneration of deleted information is not a requirement.

## Validation of reviewed candidate b0acfe04

Executed on macOS against the candidate source:

| Command (prefix `cargo`) | Result |
| --- | --- |
| `test -p octos-memory --lib` | 169 passed, including five durable-admission tests |
| `test -p octos-core --lib ui_protocol` | 156 passed |
| `test -p octos-agent --lib memory_index` | 3 passed |
| `test -p octos-cli --features api --lib memory_` | 241 passed, including the five new handler tests and seven earlier pipeline tests |
| same CLI invocation, filter `aux_rest_to_ws` | 8 passed |
| same CLI invocation, filter `session_ingress` | 11 passed |
| same CLI invocation, filter `host_managed_tests` | 23 passed |
| same CLI invocation, filter `advertised_stdio_methods_have_dispatch_path` | 1 passed |
| `clippy -p octos-core -p octos-memory -p octos-agent -p octos-cli --features api --lib --tests -- -D warnings` | passed, zero warnings |
| `fmt --all -- --check` and `git diff --check` | passed |

Counts overlap; they are not a sum of distinct tests. Test models are scripted
or panic if called on the admission path. Handler tests use isolated temporary
profile fixtures and the real storage/consolidator. No paid model, real account
memory write, browser/course integration or power-loss test is implied.

## Review boundaries

- Both new OUP methods require authenticated profile resolution and opt-in
  `auxiliary.rest_to_ws.v1`. Stdio without identity does not advertise them.
- A receipt is acceptance only. It contains a payload fingerprint and generated
  note id, not the body or a completion result. Receipt retention has no expiry
  in this slice. A crash before receipt publication can leave a private prepared
  file; retry overwrites it before admission. A crash after the receipt is
  recovered on retry. Recovery is not a new background scheduler.
- This first OUP request contract accepts current MEMORY.md ids for correction
  and forget; free-text deletion, direct archive-only targeting and per-request
  completion queries are not exposed. The existing CLI remains unchanged.
- Promotion preserves existing title/abstract-only and once-per-record behavior.
  It serializes concurrent nomination, but does not make the existing separate
  staging/marking writes transactional across process failure. Guard-rejected
  promotion notes are skipped by the existing function and not counted.

## Independent review follow-up

The independent review approved f28aeebc..b0acfe04 for an upstream PR and
reproduced all validation counts above. It identified one non-blocking issue:
a host correction can survive deletion of its target and keep inviting invalid
updates on later consolidation attempts. A new regression failed on the
reviewed code before the fix.

Before a merge, the consolidator now retires only host corrections with an
explicit replaces_id absent from live memory, archive files and pending-confirm
candidates. This also runs when model merging is disabled. Retirement records a
reason in the outcome, preserves unrelated requests, and leaves acceptance
receipts intact so retries do not recreate the obsolete correction. Other host
asks and model-origin corrections keep their existing lifecycle. This does not
implement a general barrier against in-flight regeneration of forgotten data.

Four added regressions cover deletion followed by cancellation (with unrelated
work still succeeding), recovery of a previously orphaned correction, archive
preservation, and retention of other note classes. The focused pipeline suite
now has 11 tests; the CLI memory filter has 245 passing tests. The follow-up
also clarifies that has_staging_request can publish a prepared note during
recovery; no public helper is renamed.

Follow-up validation passed: `cargo test -p octos-cli --features api --lib
app_pipeline_tests` (11), the same command with `memory_` (245),
`cargo clippy -p octos-cli --features api --lib --tests -- -D warnings`
(zero warnings), formatting and diff checks. The independent approval applies
to b0acfe04; the follow-up remains a separate, reviewable commit.

## PR review: protocol change record

Round-1 review on PR #2848 identified missing UPCR coverage for the two new
wire methods. The local coverage gate reproduced that failure. Added
[UPCR-2026-052](OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_052_APP_MEMORY_ACCESS.md),
its registry row, and a specification cross-reference. The record makes the
authorization matrix, admission replay/recovery, acknowledgement meaning,
and promotion-lock/crash boundaries explicit. This follow-up changes only
documentation; it does not change the reviewed Rust behaviour.

Follow-up checks: `bash scripts/check-ui-protocol-upcr.sh` passed (it failed
before this addition); `bash scripts/tests/test-check-ui-protocol-upcr.sh`
passed all 19 scenarios. With Python 3.13, `scripts/lint-upcr-registry.py`
and `scripts/lint-ui-protocol-inventory.py` passed, including both self-tests.
New documentation links and `git diff --check` passed. Rust code is unchanged
by this documentation-only follow-up, so its previously recorded tests and
Clippy checks were not rerun.
