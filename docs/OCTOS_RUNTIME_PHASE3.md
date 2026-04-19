# Octos Runtime Phase 3

Phase 3 starts after the Phase 2 runtime hardening work is green on live canary.
The goal is to exploit the new foundation instead of continuing to churn the same
runtime seams.

## Lanes

1. Canary soak and regression triage
   - Watch `#407` counters on real traffic.
   - File bugs only for observed failures.
   - Avoid speculative redesign during the soak.

2. Open-ended coding and debugging loops
   - Improve long code-task retries, repair turns, and bounded delegation.
   - Keep child-session fanout policy explicit and observable.

3. Hard-case live acceptance
   - Move from workflow demos to adversarial coding tasks.
   - Cover repo edits, failing-test repair, fanout/join, idle resume, and
     concurrent load.

4. Operator surface
   - Turn raw counters into a human-usable runtime summary.
   - Keep the first surface simple and scriptable.

5. Structured configuration contracts
   - Replace free-form dashboard/profile config seams with typed durable
     contracts.
   - Reserve `env_vars` for low-level secrets and explicit overrides only.
   - Move first-party app settings under first-class config sections instead of
     hiding product behavior behind generic env-var editing.

## First Operator Surface

The first operator-facing summary is intentionally small:

- API endpoint: `/api/admin/operator/summary`
- CLI command: `octos admin operator-summary`

It summarizes the existing Prometheus counters into a compact JSON or terminal
view with these categories:

- retries
- timeouts
- duplicate suppressions
- orphaned child sessions
- workflow phase transitions
- result delivery paths/outcomes
- session replay/persist/rewrite counts
- child-session lifecycle counts

Example:

```bash
octos admin operator-summary \
  --base-url https://dspfac.crew.ominix.io \
  --auth-token "$OCTOS_AUTH_TOKEN"
```

For automation:

```bash
octos admin operator-summary --json
```

## Hard-Case E2E Scaffold

Phase 3 adds a dedicated repo-level scaffold:

- `e2e/tests/coding-hardcases.spec.ts`
- script: `npm run test:live:coding`

The scaffold defines the target live proofs without pretending they are already
green:

- bounded repo edit with reviewable diff
- failing test then repair in one session
- bounded child-session fanout/join for coding work
- long idle resume without duplicate turns
- concurrent coding sessions under load

These remain `fixme` until the coding-runtime lanes provide deterministic
fixtures and orchestration hooks.

## Acceptance for Phase 3 Kickoff

The kickoff is complete when:

- the issue set exists on GitHub
- the operator summary endpoint and CLI command are merged
- the coding hard-case suite is scaffolded in repo `e2e`

The broader Phase 3 program is complete only when the new coding hard cases run
green against a live canary.

## Structured Config Hardening Contract

This is a required Phase 3 lane, not optional cleanup.

The current dashboard still has several product settings that are effectively
free-form because they are persisted as generic `config` JSON patches or as
plain `env_vars` entries. That weakens the Octos OS position because customer
skills/apps cannot rely on a clear contractual API.

The next structured-config slice must do all of the following:

1. Replace raw config merge in `admin.rs` with typed request parsing per
   section.
2. Move `SearchApiTab` from `env_vars` to a structured `search` contract.
3. Move `DeepCrawlTab` from `env_vars` to a structured `deep_crawl` contract.
4. Move `PptConfigTab` into a first-party `slides` app contract under the
   harness framework.
5. Reserve `env_vars` for true low-level secrets/overrides only, not normal
   product settings.

### Required Invariants

- Product settings must be persisted under typed profile config sections.
- UI sections must map to durable backend structs, not loose JSON patches.
- Runtime consumers must read the typed config first, not scrape product
  behavior from `env_vars`.
- First-party app settings must look like app contracts, not generic shell env.
- Secret material may still be referenced by env-var name, but the product
  behavior using those secrets must live in structured config.

### Immediate Target Sections

- `config.llm`
- `config.search`
- `config.deep_crawl`
- `config.apps.slides`

### Explicit Non-Goal

Do not solve this by introducing one giant opaque JSON blob. The contract must
be sectioned, typed, and durable.
