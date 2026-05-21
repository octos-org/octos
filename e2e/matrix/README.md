# M22-H Onboarding Operational Matrix

Issue: [#1056](https://github.com/octos-org/octos/issues/1056)
Contract: [UPCR-2026-018 Local Solo Onboarding And Policy Inspection](../../docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_2026_018_LOCAL_SOLO_ONBOARDING_AND_POLICY.md)

The matrix is a scenario-driven harness that exercises the AppUI onboarding
surface against `octos serve --stdio`. The first PR (this directory) lands
the manifest schema, a stdlib-only runner, and the **tier-fast** scenario
lane. Validators, **tier-local**, and **tier-release** are deferred to
follow-up PRs.

## Layout

```
e2e/matrix/
  README.md          this file
  onboarding.toml    scenario manifest (M22-H onboarding pack)
  run.mjs            stdlib-only runner (TOML parser + stdio client)
```

Output artifacts land under `e2e/test-results-matrix/<pack>-<tier>/<UTC>/`:

```
<UTC>/
  summary.json                  aggregate run summary
  <scenario-name>/
    rpc-transcript.jsonl        every JSON-RPC frame (redacted)
    server-stderr.log           captured octos stderr
    result.json                 per-scenario status + step results
    data/                       per-scenario --data-dir
    workspace/                  per-scenario --cwd
```

## Running

```bash
# Build the octos binary once; the runner refuses to start without it.
cargo build -p octos-cli --features api

cd e2e
npm run matrix -- --pack onboarding --tier fast
```

Other invocations:

```bash
npm run matrix -- --pack onboarding --tier local     # placeholder scenarios (skipped)
npm run matrix -- --pack onboarding --tier release   # placeholder scenarios (skipped)
```

Environment knobs:

| Variable                       | Meaning                                                                 |
| ------------------------------ | ----------------------------------------------------------------------- |
| `OCTOS_BIN`                    | Path to the `octos` binary. Defaults to `<repo>/target/debug/octos`.    |
| `OCTOS_MATRIX_DIR`             | Override the output root for this run.                                  |
| `OCTOS_MATRIX_RPC_TIMEOUT_MS`  | Per-RPC timeout in ms. Defaults to `10000`.                             |

## Tiering rules

| Tier      | Provider key required | Live tmux | Wall-clock budget       | Status this PR |
| --------- | --------------------- | --------- | ----------------------- | -------------- |
| `fast`    | No                    | No        | < 30 s per scenario     | Active         |
| `local`   | Yes (local provider)  | Optional  | < 5 min per scenario    | Placeholder    |
| `release` | Yes (release lane)    | Yes       | Bounded by gating CI    | Placeholder    |

`tier=fast` scenarios MUST be mock-or-deterministic. The runner does not
expose any provider configuration knobs in this PR — the only entry points
are `profile/local/create`, `onboarding/workspace_probe`,
`permission/profile/*`, `session/open`, and `session/status/read`.

## Manifest schema (summary)

See the comment header at the top of [`onboarding.toml`](./onboarding.toml)
for the full schema. The runner today understands:

- `[pack]` metadata: `name`, `contract`, `issue`.
- `[[scenarios]]` with: `name`, `tier`, `transport`, `description`,
  optional `skip_reason`, `validators`, `artifacts`.
- `[[scenarios.steps]]` with: `id`, `rpc`, `params`, optional `expect`.

`expect` supports three light, shape-only checks that the runner enforces
itself (full validators land with PR #2):

| Key           | Meaning                                                            |
| ------------- | ------------------------------------------------------------------ |
| `ok`          | `false` means the RPC MUST return an error. Defaults to `true`.    |
| `error_kind`  | When `ok=false`, expected `data.kind` typed-error discriminator.   |
| `result_has`  | Array of dotted paths that MUST exist in `result`.                 |

### Placeholders

Inside `params`, the runner substitutes:

- `${workspace}`         — scenario-local cwd (writable, seeded).
- `${missing_path}`      — a deterministic path that does not exist.
- `${root_escape_path}`  — a `/etc/...` path used to assert `root_escape=true`.
- `${session_id}`        — per-scenario session id.
- `${profile_id}`        — per-scenario profile id.
- `${email}`             — per-scenario seeded email metadata.

## Why a stdlib-only runner?

- The repo already pins runtime soak scripts to Node stdlib (`m15-*`,
  `m16-*`). Adding `toml` or `yaml` deps to `e2e/package.json` would
  enlarge the audit surface for a harness that's intentionally small.
- The TOML parser in `run.mjs` is intentionally minimal: it covers the
  manifest's needs and rejects everything else with a parse error so a
  scenario authoring mistake surfaces immediately.

## Follow-ups (not in this PR)

1. **Validators** — promote each scenario's `validators = [...]` entries
   into Python or Node validators that run after the steps emit
   `rpc-transcript.jsonl`. Required validators per issue #1056:
   `no_otp_emitted`, `profile_id_consistency`,
   `workspace_probe_typed_result`, `runtime_policy_stamp_present`,
   `secret_redaction`, `unsupported_appui_method_gated`,
   `resume_partial_setup_safe`.
2. **tier-local** — live tmux capture, live provider key paths.
3. **tier-release** — required-cannot-skip gate, OTP-leak detection,
   secret-leak detection, blank/stuck tmux capture detection.

GitHub follow-up issues are filed alongside this PR.
