# Handoff: Build + E2E Soaking — 2026-04-30

Audience: another agent (or human) running on a different machine, cold,
to compile `octos` and run the e2e soaking suite against either a local
build or the deployed fleet.

Repo: `https://github.com/octos-org/octos`
Reference commit: `e256e6b5` (current `main` at handoff time)

---

## 0. What this doc covers

1. Compile the daemon binary correctly
2. Build the dashboard bundle (must run BEFORE `cargo build --release`)
3. Start a local server suitable for e2e tests
4. Set up the e2e harness
5. Run the soaking spec families against either a local server or the
   deployed mini fleet
6. Read results + triage common failure modes

What this doc does **not** cover:

- Deploying to the production mini fleet (separate procedure, see
  `docs/reference_fleet_deploy_procedure.md` if available, or the
  `scripts/deploy.sh` script — but that script is gitignored and only
  exists on the maintainer's machine)
- SSH-level operations on the mini fleet (different agent's job)
- Editing the protocol/UPCR contract (governed by spec § 4.1)

---

## 1. Prerequisites

| Tool | Version | Purpose |
|---|---|---|
| Rust toolchain | edition 2024, `rust-version >= 1.85.0` | server build |
| Cargo | bundled with Rust | server build |
| Node.js | 20+ recommended | e2e harness |
| `npm` | bundled with Node | e2e deps |
| `gh` (GitHub CLI) | optional | only needed for PR / issue work |
| Disk space | **at least 30 GB free** for cargo target | release build is heavy; debug build + multiple worktrees can hit 200 GB+ |

Set `CARGO_TARGET_DIR=/path/with/space/target` if `~` is on a small disk.

System deps (if missing): `cmake`, `pkg-config`, OpenSSL is **not** required (the codebase uses pure-Rust `rustls`).

---

## 2. Clone and orient

```bash
git clone https://github.com/octos-org/octos.git
cd octos
git checkout e256e6b5   # or current main
```

Project layout (top of `CLAUDE.md`):

```
octos-cli      # CLI: clap commands, config loading, daemon entry
octos-agent    # Agent loop, tool system, sandbox, MCP, plugins
octos-bus      # Message bus (Telegram/Discord/Slack/...), sessions, cron
octos-pipeline # DOT-graph pipeline engine
octos-plugin   # Plugin SDK (manifests + discovery)
octos-memory   # hybrid search + memory store
octos-llm      # LLM provider trait + native impls
octos-core     # types: Task, Message, UI Protocol v1
crates/app-skills/         # bundled skills (deep-search, deep-crawl, voice, …)
crates/platform-skills/    # bundled skills (voice)
e2e/                       # Playwright tests
api/                       # Protocol spec docs (UI Protocol v1, feature requirements)
docs/                      # Engineering docs, ADRs, runbooks, audits
```

---

## 3. Build the daemon — the **only** correct invocation

**Step 1**: build the dashboard bundle and embed it. **This must come first.**

```bash
./scripts/build-dashboard.sh
```

If you skip this step, the embedded admin/chat dashboard is empty and
the deployed `octos serve` will return blank pages for every dashboard
route. We learned this the hard way during fleet deploys.

**Step 2**: build the binary with the canonical feature set:

```bash
cargo build --release -p octos-cli \
    --features telegram,whatsapp,feishu,twilio,wecom,api
```

**Why every flag matters**:

- `api` — enables the `serve` subcommand. Without it the binary builds
  but `octos serve` fails with `error: unrecognized subcommand 'serve'`.
- `telegram,whatsapp,feishu,twilio,wecom` — enables the bus channels.
  The mini fleet binary ships with all of these.

The bare `cargo build --release -p octos-cli` (without `--features`)
**will silently strip the `serve` subcommand** and looks like a
successful build until the daemon refuses to start. **Don't shortcut
this.**

Output: `target/release/octos`. Verify with:

```bash
target/release/octos --version
# → octos 0.1.1+e256e6b5 ...
```

The version string contains the git short-SHA. If it doesn't match the
checkout, something is stale — run `cargo clean -p octos-cli` and
rebuild.

**Build time**: ~5–15 min on a dedicated machine, longer on shared
hardware. The first compile is the heaviest; incremental rebuilds are
seconds-to-minute.

---

## 4. Smoke-test the binary

```bash
# Should print version info and exit 0
target/release/octos --version

# Should list `serve`, `chat`, `init`, `gateway`, etc. in subcommands
target/release/octos --help
```

If `serve` is missing from `--help`, the `api` feature didn't engage.
Re-run step 3 with the full `--features` list.

---

## 5. Start a local server for e2e (skip if testing against deployed minis)

```bash
# Pick a free port, e.g. 56831
target/release/octos serve --host 127.0.0.1 --port 56831 \
    --auth-token "test-token-please-change" \
    > /tmp/octos-serve.log 2>&1 &
echo $!  # remember PID for cleanup
```

The auth token can be anything; e2e specs read it from
`OCTOS_AUTH_TOKEN`. Use `127.0.0.1` (not `0.0.0.0`) unless you
specifically want LAN exposure.

Health check:

```bash
curl -s http://127.0.0.1:56831/api/version
# → {"version":"0.1.1+e256e6b5","build_date":"...","service":"octos",...}
```

Stop the server when done:

```bash
kill <PID>
# or pkill -f 'target/release/octos serve'
```

---

## 6. E2E harness setup

```bash
cd e2e
npm ci    # not `npm install` — `ci` enforces the lockfile
npx playwright install chromium    # or all browsers if you need them
```

`npm ci` is a one-time cost; subsequent runs reuse `node_modules`.

If `node_modules` is missing AND the worktree is freshly cloned, **the
playwright run will fail with `Cannot find module '@playwright/test'`**.
We hit this on a deploy worktree this morning — symptoms are an instant
failure without any spec running.

Verify:

```bash
ls node_modules/@playwright/test    # should exist
npx playwright --version
```

---

## 7. Soaking spec families

The e2e harness has many specs; for soaking the "hardness engineering"
work, focus on this set (covers the M6-M9 contract surface and the
production-bug repros):

| Spec | What it exercises | Approx duration |
|---|---|---|
| `live-overflow-thread-binding.spec.ts` | 3-user fast follow-up — the literal #649 production-bug repro | ~8 min |
| `live-thread-interleave.spec.ts` | slow Q + fast Q pair with thread-store-v2 | ~6 min |
| `live-realtime-status.spec.ts` | timeline populates with pipeline progress | ~1.5 min |
| `live-tool-retry-collapse.spec.ts` | retries collapse into one bubble with retry counter | ~5 min |
| `live-overflow-stress.spec.ts` | 7-scenario stress (rapid-fire, mofa-deliverables-soak, etc.) | ~25–40 min |
| `m9-protocol-*.spec.ts` (8 specs) | UI Protocol v1 wire-level harness (session/open, turn/start, approval/respond, diff/preview, task/output/read, fault injection, …) | ~5–10 min total |

For a *soak* run (durability under realistic load), use the first two
plus `live-overflow-stress`:

```bash
# Single canonical run
OCTOS_TEST_URL=http://127.0.0.1:56831 \
OCTOS_AUTH_TOKEN=test-token-please-change \
  npx playwright test \
    live-overflow-thread-binding.spec.ts \
    live-thread-interleave.spec.ts \
    live-realtime-status.spec.ts \
    live-tool-retry-collapse.spec.ts \
    --workers=1 \
    --reporter=list \
    > /tmp/octos-soak-$(date +%Y%m%dT%H%M%S).log 2>&1
```

**Why `--workers=1`**: these specs share daemon state; running in
parallel introduces cross-test races that aren't real bugs.

**Why `--reporter=list`**: prints per-test pass/fail without truncation.

---

## 8. Running against the deployed fleet (alternative target)

The deployed minis run an `octos serve` daemon built exactly per step 3
(and deployed via SSH-level operations covered in
`docs/reference_fleet_deploy_procedure.md` — out of scope for *this*
doc). This section just covers reaching them as an e2e target.

### 8.1 Fleet inventory

All minis are macOS ARM, user `cloud@<ip>`, key-based SSH after one-time
`ssh-copy-id`. **Sudo passwords live in the gitignored
`scripts/deploy.sh` on the maintainer's machine** — ask the maintainer
out-of-band; they are not committed to the repo.

| Mini | IP | Domain | Daemon | Color | E2E target? |
|---|---|---|---|---|---|
| mini1 | `69.194.3.128` | `dspfac.crew.ominix.io` | root LaunchDaemon `/Library/LaunchDaemons/io.octos.serve.plist` | yellow | ✅ safe |
| mini2 | `69.194.3.129` | `dspfac.bot.ominix.io` | root LaunchDaemon | yellow | ⚠️ check with maintainer first (separate auth setup) |
| mini3 | `69.194.3.203` | `dspfac.octos.ominix.io` | **USER agent** at `~/Library/LaunchAgents/io.ominix.octos-serve.plist` on port 50080 — root daemon `io.octos.serve` is in pre-existing crash-loop on port 8080, leave it alone | yellow | ✅ safe |
| mini4 | `69.194.3.66` | `dspfac.river.ominix.io` | root LaunchDaemon | blue (intentional baseline / rollback target) | ✅ safe |
| mini5 | `69.194.3.19` | `dspfac.ocean.ominix.io` | root LaunchDaemon | yellow | ❌ **DO NOT SOAK** — reserved for active sprint work; active deploys will break your run |
| mini6 | `69.194.3.249` | (varies — check `~/octos-web` symlink target) | check both root + user daemon | (newer host, profile TBD) | check with maintainer |

**Excluded — do NOT touch**: `cloud@66.201.40.31` (`macmini-31.octos.bot`).
Earlier deploy scripts had it as "mini4"; the river.ominix.io box
replaced it. It's a separate dev/test box.

### 8.2 Per-mini daemon binary path

All minis use **`~/.octos/bin/octos`** (not `~/.local/bin/octos`).
Verify on the target:

```bash
ssh cloud@<ip> '~/.octos/bin/octos --version'
# → octos 0.1.1+<short-sha> ...
```

If the version is older than your local `git rev-parse origin/main`,
the soak result is not authoritative for current main — flag it in the
report and ask the maintainer about a redeploy.

### 8.3 E2E command against a deployed mini

```bash
# Pick mini1, mini3, or mini4 — these are the safe-to-soak targets

OCTOS_TEST_URL=https://dspfac.octos.ominix.io \
OCTOS_AUTH_TOKEN='<production-token-from-maintainer>' \
  npx playwright test \
    live-overflow-thread-binding.spec.ts \
    live-thread-interleave.spec.ts \
    --workers=1 --reporter=list \
    > /tmp/octos-soak-mini3-$(date +%Y%m%dT%H%M%S).log 2>&1
```

The auth token is shared across the fleet for e2e purposes; ask the
maintainer for the current value. It's NOT committed.

### 8.4 OminiX-API companion service (per-mini)

Each mini runs a local TTS/ASR companion service. Voice TTS won't work
on a mini if its `OMINIX_API_URL` env var (in the launchd plist)
points to an unreachable endpoint OR if the local ominix-api isn't
running. If voice-related specs (e.g. fm_tts, mofa-podcast) fail with
network errors, suspect this companion service rather than the octos
daemon itself.

### 8.5 Web bundle

octos-web has only ONE release branch (`release/coding-blue`). Same web
bundle deploys to all minis at `~/octos-web/`. **mini4 has no
`~/octos-web/`** — its admin/chat assets are embedded in the octos
binary directly (this is by design — `./scripts/build-dashboard.sh`
embeds them; same step you ran in §3).

---

## 9. Reading results

**Pass case** (output tail):

```
  ✓  1 tests/live-overflow-thread-binding.spec.ts:141:7 › ... (8.1m)
  ✓  2 tests/live-thread-interleave.spec.ts:207:7 › ... (6.1m)
  2 passed (14.2m)
```

**Fail case** (output tail):

```
  ✘  1 tests/live-thread-interleave.spec.ts:207:7 › slow Q + fast Q pair correctly with thread-store-v2 flag on
    Error: Slow-Q's paired bubble has no research-content marker.
    [test source excerpt]
  1 failed
    tests/live-thread-interleave.spec.ts:207:7 ...
  1 passed (14.2m)
```

For each failure, also look at:
- `e2e/test-results/<spec-slug>/` — Playwright's structured output dir
- `e2e/test-results/<spec-slug>/error-context.md` — assertion details
- `e2e/test-results/<spec-slug>/trace.zip` — load via
  `npx playwright show-trace <path>` for full step-by-step

---

## 10. Common failure modes + triage

| Symptom | Likely cause | First check |
|---|---|---|
| `Cannot find module '@playwright/test'` | `npm ci` was skipped | `ls e2e/node_modules/@playwright/test` |
| Daemon refuses to start: `unrecognized subcommand 'serve'` | binary built without `--features api` | `target/release/octos --help` |
| Empty dashboard / blank `/admin` page | `./scripts/build-dashboard.sh` was skipped | redo step 3 in order |
| `live-realtime-status` poll times out at 60s | `run_pipeline` not surfacing in `/api/sessions/:id/tasks` | check that the daemon under test is at commit `e256e6b5` or later (PR #688 made `run_pipeline` spawn_only — earlier daemons fail this) |
| `live-thread-interleave` slow Q has no research-content marker | gemini schema sanitizer or deep_research backend not completing | check daemon log for `Unknown name "x-octos-host-config-keys"` (closed in PR #691) |
| Disk fills mid-build, `ENOSPC` errors | cargo target dir / multiple worktrees | `du -sh */target` and `rm -rf` old/merged worktrees' target dirs |
| Test passes locally, fails against fleet | fleet at older commit than local | check `<URL>/api/version` matches `git rev-parse HEAD` you tested |

---

## 11. What "soak" actually proves

This soak set covers the M9 milestone delivery contracts:

1. **Late-arrival delivery integrity** — does a slow background result
   land on the bubble that requested it? (overflow-thread-binding,
   thread-interleave)
2. **Terminal state durability** — does a cancelled/completed/failed
   task survive WS backpressure to the client? (cancel-related lines
   in live-realtime-status)
3. **Pipeline observability** — does `task/output/read` /
   `/api/sessions/:id/tasks` reflect the truth that the supervisor
   knows? (live-realtime-status, m9-protocol-task-output-read)
4. **Approval lifecycle** — typed approvals + scope + cancellation +
   audit (m9-protocol-approval-respond, m9-protocol-fault-injection)
5. **UI Protocol v1 wire contract** — golden round-trip + replay
   semantics (m9-protocol-* family + the `octos-core ui_protocol` lib
   tests run via `cargo test -p octos-core ui_protocol`)

A green soak doesn't prove the system is bug-free; it proves the
specific regressions the spec authors knew about are still closed.

---

## 12. Hard constraints for the soaking agent

- **Don't push to main.** The repo's main branch is admin-merge-gated;
  agents that run soak should report failures, not auto-fix.
- **Don't run destructive git operations** (no `git reset --hard
  origin/main`, no `git push --force`) on shared branches.
- **Don't deploy.** Fleet deploys involve SSH operations not covered
  in this doc; that's a separate agent's job.
- **Don't skip pre-commit hooks** if you're committing locally for a
  fix-PR (no `--no-verify`).
- **Always include the daemon's version in the failure report.** Soak
  failures against an old daemon look identical to failures against a
  current one; the version string is the disambiguator.

---

## 13. Failure report format

When reporting back to whoever dispatched the soak, include:

```
- Daemon: <version+sha>     (from /api/version)
- Spec set: <list>
- Workers: 1
- Total duration: <m>
- Pass / Fail / Skip: <numbers>
- For each fail: spec:line, error class, evidence path (test-results/...)
- Daemon log excerpt (last 200 lines) if known to be local
- Anything that looks NOT like a regression but suspicious (timeouts,
  network blips, flaky tests)
```

Don't try to root-cause failures from inside the soak agent — that's a
separate investigation. Capture and report.

---

## 14. Reference: today's main `e256e6b5` shipped

For context (so you know what regressions the soak should pass):

| Closed today | Effect |
|---|---|
| run_pipeline → spawn_only | task supervisor surfaces pipeline tasks; `live-realtime-status` poll succeeds |
| gemini `x-*` schema strip | deep_research workers don't crash on Gemini lane |
| Sandbox config inheritance | AppUi sessions can run `npm install` etc. |
| Cancelled task state mapping | UI shows cancelled tasks correctly |
| Cancel-race guard in `mark_completed`/`mark_failed`/etc | late workers can't overwrite cancelled state |
| MCP swarm policy gate engaged in production | MCP-routed dispatch goes through tool_policy + sandbox + env-allowlist |
| MCP/plugin Exclusive concurrency declarations | file-mutating tools serialize correctly |
| Skill manifest `env` + `risk` enforced at runtime | risky plugin tools require approval; env allowlist active |
| Plugin risk emission lifted out of shell-only guard | UI sees risk classification for plugin approvals |
| `task/cancel` + `task/list` + `task/restart_from_node` RPCs | UI can drive cancel/list/relaunch |
| `is_snapshot_projection: bool` on `task/output/read` | clients can detect snapshot vs live-tail |
| Durable PendingDiffPreviewStore | diff/preview/get survives restart |
| `appui.default_session_cwd` config knob | operator can anchor sessions to a folder |
| Capabilities in `SessionOpened` | clients can discover server features in-band |
| Typed `reason` / `terminal_state` / `ack_timeout` on `TurnInterruptResult` | wire shape matches typed struct |

If your soak fails one of these, it's likely a real regression and
worth a deeper investigation.

---

## 15. Where to ask for help

- Slack/equivalent: ask the maintainer for a current daemon version
  string + a recent passing soak baseline
- Repo: `docs/OCTOS_HARNESS_AUDIT_M6_M9_2026-04-30.md` lists which
  guarantees are tested and which are deferred
- The 8 UPCR docs at `docs/OCTOS_UI_PROTOCOL_CHANGE_REQUEST_UPCR_*.md`
  describe each accepted protocol change; if a soak is asserting on a
  field, the UPCR explains what it should mean
