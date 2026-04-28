# M8 Runtime Migration Runbook

**Status**: Draft (W4 deliverable, M8 Runtime Parity epic)
**Owner**: ymote
**Last updated**: 2026-04-26
**Companion**: [`m8-runtime-parity-prd.md`](./m8-runtime-parity-prd.md), [`m8-runtime-contract.md`](./m8-runtime-contract.md)

This is the operational playbook for rolling the M8 Runtime Parity work
out to the production fleet. Read the PRD first if you're new to the
epic. Use this runbook when you are about to merge a track, deploy a
canary, or roll back a regression.

The work is split into four parallel tracks (W1 / W2 / W3 / W4) running
off `main@889e5e05`. The merge order is fixed (§3) because the tracks
have a hard dependency chain: protocol v2 (W3) lands before plugin
adoption (W4); spawn / pipeline host wiring (W1+W2) depends on each
other only loosely; W4 ties them all together with end-to-end specs.

---

## 1. Fleet topology

Mini hosts (per `~/.claude/projects/-Users-yuechen-home-octos/memory/reference_minis_ssh.md`):

| Host  | IP            | Tree        | Role for this rollout |
|-------|---------------|-------------|------------------------|
| mini1 | 69.194.3.128  | yellow      | Canary (deploy first). |
| mini2 | 69.194.3.129  | yellow      | Live-test target (`https://dspfac.bot.ominix.io`). |
| mini3 | 69.194.3.203  | yellow      | Standby (deploy after mini2 green). |
| mini4 | 69.194.3.66   | blue        | Standby (deploy with mini3). |
| mini5 | 69.194.3.19   | yellow      | **DO NOT DEPLOY** — reserved for coding-green tests. |

All hosts run as user `cloud` (key-based SSH). The deploy daemon runs as
**root**: per the user-memory note `reference_minis_deploy_daemon.md`,
`sudo launchctl unload` the root daemon before stopping; user `pkill`
alone is insufficient to fully release the binary.

Host mapping in tests: see `e2e/tests/m8-runtime-invariants-live.spec.ts`
`HOST_MAP` constant — keep that in sync with this table.

---

## 2. Per-track deploy order

Each track's PR follows the same merge-and-deploy template:

1. CI green (`cargo build/clippy/test --workspace`, web typecheck/build).
2. PR review by another agent or operator (one approving review minimum).
3. Self-merge via `gh pr merge --squash` (fast-forward only).
4. Tag the merge commit: `git tag m8-w<N>-<deliverable>-rc<rev> && git push origin <tag>`.
5. Build release artifact: `cargo build --release -p octos-cli --features "octos-cli/api,octos-cli/telegram"`.
6. Deploy to **mini1** first (canary).
7. Smoke test (§5).
8. If green, deploy to mini2 / mini3 / mini4 (parallel ok).
9. Run the cross-track live spec suite (§6).
10. If a regression appears, follow the rollback playbook (§7).

### 2.1 Track-specific notes

**W1 (pipeline host + frontend cards/cost)**

- Merging this changes both backend and `octos-web`. The web bundle is
  served from `crates/octos-cli/static/admin/`; ensure the new bundle is
  in the release build (cargo wraps it via `build.rs`).
- Smoke: trigger `run_pipeline` from the chat UI, verify NodeCards appear
  under the run_pipeline pill, verify the cost panel renders.

**W2 (spawn host + workflows + API + cancel/restart UI)**

- Adds two POST endpoints (`/api/tasks/:id/cancel`, `.../restart-from-node`).
  Audit `Authorization` plumbing on those handlers; both require admin
  token or session-owner token.
- Smoke: trigger a long pipeline, click cancel, verify the task moves to
  `Failed/Cancelled` within 15s. Then trigger a fault-injected pipeline,
  click restart-from-node, verify only the failed node and downstream
  re-run.

**W3 (deep_search/deep_crawl + plugin protocol v2)**

- Lands first. Backward-compat shim in `octos-plugin/src/lifecycle.rs` is
  load-bearing — without it, v1 plugins (mofa_slides, fm_tts,
  podcast_generate before W4) regress.
- Smoke: trigger deep research, verify the synthesized `_report.md`
  contains paragraphs and source citations (not a raw Bing dump). Verify
  no more than 3 concurrent Chromium processes during a deep_crawl run
  (`pgrep Chromium | wc -l`).

**W4 (other plugin v2 + integration tests + docs)**

- Adopts v2 in mofa_slides, podcast_generate, fm_tts (via the external
  mofa-skills repos — separate PR per plugin). When the upstream plugin
  PR merges, octos picks up the new binary on the next `octos skills
  upgrade` run.
- The host-side integration tests in `e2e/tests/live-{pipeline,spawn,cost}-end-to-end.spec.ts`
  require all four tracks merged to fully pass.
- Smoke: run `live-pipeline-end-to-end.spec.ts` against mini2.

---

## 3. Merge order

Fixed order to minimise conflicts and ensure each merge is independently
deployable:

```
W3 → W2 → W1 → W4
```

Rationale:

- **W3 first** (plugin protocol v2): smallest host-side surface, lands
  the v1/v2 backward-compat shim. Other tracks consume it.
- **W2 next** (spawn host + API): adds API endpoints and rewires the
  spawn-subagent path. W1 frontend depends on the API1/API2 endpoints
  for cancel/restart.
- **W1 third** (pipeline host + frontend cards/cost): largest UI surface,
  consumes W2's API and W3's protocol v2 events.
- **W4 last** (other plugins + integration tests): pulls everything
  together; ships the cross-cutting specs.

Each merge gets a fleet redeploy + smoke test before the next merges.
**Do not stack merges** — let each one bake on the canary for at least 4
hours before promoting.

---

## 4. Deploy commands

### 4.1 Build the release artifact

```bash
cd /Users/yuechen/home/octos
cargo build --release -p octos-cli --features "octos-cli/api,octos-cli/telegram"
# Artifact: target/release/octos
```

If the merge changes web assets, rebuild the dashboard bundle first:

```bash
cd /Users/yuechen/home/octos/dashboard
pnpm install --frozen-lockfile
pnpm build
# Bundle output is consumed by octos-cli's static admin assets.
```

### 4.2 Stop the running gateway on a mini

```bash
ssh cloud@69.194.3.128 'sudo launchctl unload /Library/LaunchDaemons/com.octos.gateway.plist'
ssh cloud@69.194.3.128 'pkill -TERM -x octos || true'
ssh cloud@69.194.3.128 'sleep 5; pgrep -x octos || echo stopped'
```

### 4.3 Push the binary

```bash
scp target/release/octos cloud@69.194.3.128:/tmp/octos.new
ssh cloud@69.194.3.128 'sudo install -m 755 /tmp/octos.new /usr/local/bin/octos'
```

### 4.4 Restart

```bash
ssh cloud@69.194.3.128 'sudo launchctl load /Library/LaunchDaemons/com.octos.gateway.plist'
ssh cloud@69.194.3.128 'sleep 8; pgrep -x octos && echo running'
```

### 4.5 Confirm health

```bash
curl -s -H 'Authorization: Bearer octos-admin-2026' \
  https://dspfac.bot.ominix.io/api/health | jq .
```

A 200 with `{"status":"healthy"}` is the green light.

---

## 5. Smoke tests (post-deploy)

Run after each canary deploy. The full live-spec suite runs after the
fleet-wide deploy (§6).

### 5.1 Quick smoke (5 minutes)

```bash
cd /Users/yuechen/home/octos/e2e
OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
OCTOS_AUTH_TOKEN=octos-admin-2026 \
OCTOS_PROFILE=dspfac \
  npx playwright test tests/runtime-regression.spec.ts --workers=1
```

This covers: session persistence, background TTS lifecycle, SSE done
events, slides project init, cross-session isolation. ~3 minutes.

### 5.2 Targeted M8 invariants (8 minutes)

```bash
cd /Users/yuechen/home/octos/e2e
OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
OCTOS_PROFILE=dspfac \
  npx playwright test tests/m8-runtime-invariants-live.spec.ts --workers=1
```

This validates M8.4 / M8.6 / M8.7 / M8.9 individually. Failures here
are blocking — do not promote to mini2/3/4.

### 5.3 Manual chat probe (2 minutes)

Open `https://dspfac.bot.ominix.io/chat` in a browser. Ask:

> "Use deep_search to summarize the last 24 hours of news on agentic AI."

Expected: NodeCards under the `run_pipeline` bubble, a synthesized prose
report with citations, cost panel populated, no orphan tool_progress
events. If you see only a flat "running run_pipeline" pill or a raw Bing
dump, the deploy regressed — pause and investigate.

---

## 6. Cross-track live spec suite

Run after the fleet-wide deploy (every mini except mini5 has the new
binary). This is the final gate before declaring the epic done.

```bash
cd /Users/yuechen/home/octos/e2e
OCTOS_TEST_URL=https://dspfac.bot.ominix.io \
OCTOS_AUTH_TOKEN=octos-admin-2026 \
OCTOS_PROFILE=dspfac \
OCTOS_TEST_EMAIL=dspfac@gmail.com \
  npx playwright test --workers=2 \
    tests/live-pipeline-end-to-end.spec.ts \
    tests/live-spawn-end-to-end.spec.ts \
    tests/live-cost-tracking.spec.ts \
    tests/live-progress-gate.spec.ts \
    tests/m8-runtime-invariants-live.spec.ts
```

Expected runtime: ~25 minutes. All specs must be green.

If a spec is flaky (one fail in two runs), file an issue and re-run; do
not promote with a flaky live spec.

---

## 7. Rollback

### 7.1 Identify the offending merge

```bash
git log --oneline main..HEAD | head -10
```

Pick the merge commit that introduced the regression. Note its hash.

### 7.2 Revert (preferred)

```bash
cd /Users/yuechen/home/octos
git checkout main
git pull --ff-only
git revert <commit-hash>
# Editor opens; write the rollback rationale in the commit body.
git push origin main
```

Then re-build the release artifact and re-deploy to the fleet (§4).

### 7.3 Force-push (last resort)

If multiple commits are intertwined and a clean revert is not possible,
force-push to the previous green commit:

```bash
git checkout main
git reset --hard <last-green-hash>
git push origin main --force
```

⚠️ Force-push to main is restricted: requires admin bypass and one
operator on the call. Document the rationale in the umbrella issue.

The migration force-push of `c8787472` (yellow-tree → main) is preserved
in `release/coding-yellow` and `release/coding-purple` branches — full
rollback to pre-M8 epic state is still available by force-push, but
should not be needed for incremental M8 issues.

### 7.4 Hot-patch (when revert is too slow)

If only the binary needs to roll back and the revert is in flight, push
the previous binary directly:

```bash
ssh cloud@69.194.3.128 'sudo cp /usr/local/bin/octos /usr/local/bin/octos.failed'
scp target/release/octos.previous cloud@69.194.3.128:/tmp/octos.rollback
ssh cloud@69.194.3.128 'sudo install -m 755 /tmp/octos.rollback /usr/local/bin/octos'
ssh cloud@69.194.3.128 'sudo launchctl unload /Library/LaunchDaemons/com.octos.gateway.plist && sudo launchctl load /Library/LaunchDaemons/com.octos.gateway.plist'
```

Then file a follow-up issue, revert at the source, and re-build cleanly.

---

## 8. Plugin v2 adoption (per external skill)

W4's plugin work touches three external skills that live outside this
repo:

- `mofa-slides` (`https://github.com/mofa-org/mofa-skills/mofa-slides`) — `mofa_slides`
- `mofa-podcast` (`https://github.com/mofa-org/mofa-skills/mofa-podcast`) — `podcast_generate`
- `mofa-fm` (`https://github.com/mofa-org/mofa-skills/mofa-fm`) — `fm_tts`

For each plugin:

### 8.1 Add SIGTERM handling

```rust
// crates/.../src/main.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn install_signal_handler() -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_handler = cancel.clone();
    #[cfg(unix)]
    {
        use signal_hook::consts::SIGTERM;
        use signal_hook::iterator::Signals;
        std::thread::spawn(move || {
            let mut signals = Signals::new([SIGTERM]).expect("signal handler");
            for _ in signals.forever() {
                cancel_for_handler.store(true, Ordering::SeqCst);
            }
        });
    }
    cancel
}

fn main() {
    let cancel = install_signal_handler();
    // ... existing main body ...
    // Periodically check `cancel.load(Ordering::Acquire)` in long loops.
    // On true: clean up child processes (browsers, ffmpeg, python helpers),
    // flush partial state, exit 130 (SIGTERM convention).
}
```

### 8.2 Emit structured progress events

Replace ad-hoc `eprintln!("status: ...")` with v2 events:

```rust
fn emit_progress(phase: &str, message: &str, progress: Option<f64>) {
    let session_id = std::env::var("OCTOS_HARNESS_SESSION_ID").unwrap_or_default();
    let task_id    = std::env::var("OCTOS_HARNESS_TASK_ID").unwrap_or_default();
    let event = serde_json::json!({
        "schema":   "octos.harness.event.v1",
        "kind":     "progress",
        "session_id": session_id,
        "task_id":    task_id,
        "phase":      phase,
        "message":    message,
        "progress":   progress,
    });
    eprintln!("{}", serde_json::to_string(&event).unwrap());
}
```

The host parser falls back to legacy text-line handling when the line
doesn't start with `{`, so existing free-form `eprintln!` calls keep
working unchanged. You can adopt incrementally.

### 8.3 Emit cost attribution on each LLM/API call

```rust
fn emit_cost(model: &str, tokens_in: u32, tokens_out: u32, usd: f64) {
    let session_id = std::env::var("OCTOS_HARNESS_SESSION_ID").unwrap_or_default();
    let task_id    = std::env::var("OCTOS_HARNESS_TASK_ID").unwrap_or_default();
    let event = serde_json::json!({
        "schema":         "octos.harness.event.v1",
        "kind":           "cost_attribution",
        "session_id":     session_id,
        "task_id":        task_id,
        "attribution_id": uuid::Uuid::now_v7().to_string(),
        "contract_id":    "<workflow id from manifest>",
        "model":          model,
        "tokens_in":      tokens_in,
        "tokens_out":     tokens_out,
        "cost_usd":       usd,
        "outcome":        "success",
    });
    eprintln!("{}", serde_json::to_string(&event).unwrap());
}
```

### 8.4 Add a `summary` field to the result JSON

```rust
let result = serde_json::json!({
    "output":  human_readable,
    "success": true,
    "files_to_send": output_paths,
    "summary": {
        "kind": "podcast.episode",
        "duration_seconds": runtime_secs,
        "n_speakers": n_speakers,
        "n_lines": n_lines,
    },
    "cost": {
        "tokens_in":  total_tokens_in,
        "tokens_out": total_tokens_out,
        "usd":        total_usd,
    },
});
println!("{}", serde_json::to_string(&result).unwrap());
```

The `summary` field feeds the chat UI's per-task summary card.

### 8.5 Verify

```bash
# Build the plugin locally
cd ~/home/mofa-skills/mofa-fm && cargo build --release

# Run with the v2 contract env vars set
OCTOS_HARNESS_SESSION_ID=test-sess \
OCTOS_HARNESS_TASK_ID=test-task \
echo '{"voice":"vivian","text":"hello"}' | \
  ./target/release/mofa-fm fm_tts

# Verify stderr contains v2 events (one JSON per line, parseable)
# Verify stdout result contains optional summary + cost fields
# Send SIGTERM mid-flight and confirm exit within 10s
```

The W4 PR adds matching unit tests in `crates/octos-plugin/tests/` that
parse a representative event from each plugin to ensure the schema is
honoured.

### 8.6 Publish a new release

For each external skill:

```bash
cd ~/home/mofa-skills/<skill>
# Bump version in manifest.json and Cargo.toml
git tag v<new>
git push --tags
gh release create v<new> --notes "M8 protocol v2 adoption"
```

The `manifest.json` `binaries.<arch>.url` points at the GitHub release
asset; once the asset is published, the next `octos skills upgrade` run
on each mini picks up the new binary automatically.

---

## 9. Fleet redeploy commands

After all four tracks merge, redeploy the entire fleet (skip mini5):

```bash
for host in 69.194.3.128 69.194.3.129 69.194.3.203 69.194.3.66; do
  echo "=== deploying to $host ==="
  scp target/release/octos cloud@$host:/tmp/octos.new
  ssh cloud@$host 'sudo install -m 755 /tmp/octos.new /usr/local/bin/octos'
  ssh cloud@$host 'sudo launchctl unload /Library/LaunchDaemons/com.octos.gateway.plist'
  ssh cloud@$host 'pkill -TERM -x octos || true; sleep 4; pkill -KILL -x octos || true'
  ssh cloud@$host 'sudo launchctl load /Library/LaunchDaemons/com.octos.gateway.plist'
  ssh cloud@$host 'sleep 8; pgrep -x octos && echo $host running'
done
# DO NOT touch mini5 (69.194.3.19) — reserved for coding-green tests.
```

Verify all four hosts respond healthy before declaring the deploy done:

```bash
for host in dspfac.crew.ominix.io dspfac.bot.ominix.io \
            dspfac.octos.ominix.io dspfac.river.ominix.io; do
  echo -n "$host: "
  curl -s -H 'Authorization: Bearer octos-admin-2026' \
    "https://$host/api/health" | jq -r .status
done
```

All four should print `healthy`.

---

## 10. Post-rollout checklist

Once the fleet-wide deploy is green and the cross-track live spec suite
(§6) is green:

- [ ] Update `~/home/octos/CLAUDE.md` to reflect any architecture changes.
- [ ] Update `~/.claude/projects/-Users-yuechen-home-octos/memory/reference_minis_ssh.md`
      with anything operationally new.
- [ ] Close the umbrella issue (`#591`) with the final test matrix.
- [ ] Tag the final fleet build: `git tag m8-parity-final && git push origin m8-parity-final`.
- [ ] Open the next epic's umbrella issue if applicable (M9 family is
      separate scope per the PRD §9).

---

## 11. Known risks

| Risk                                                       | Mitigation |
|------------------------------------------------------------|------------|
| Plugin SIGTERM handler not installed → 10s timeout → SIGKILL → orphan helpers | Validate per-plugin via `live-spawn-end-to-end.spec.ts` SIGTERM probe. |
| v2 event with malformed JSON → host falls back to legacy text path → silently lost progress | Backward-compat shim treats non-`{`-prefixed lines as text; v2 parser emits a `tracing::warn` on JSON parse failure of `{`-prefixed lines. |
| Cost reservation handle leak (created, never committed/released) | F-003 reservation guard impls `Drop` to release on panic. Add a unit test per actor. |
| Restart-from-node wipes downstream cached outputs → user surprise | UI confirmation modal explains scope; API1 docs the behaviour. |
| Fleet deploy hits one host but not others → split-brain canary | The §9 loop is sequential per host with a health-check after each; fail fast on the first host that doesn't come back healthy. |
| Plugin `summary` field schema drift across plugin versions | `summary.kind` is a string discriminator; the host treats unknown kinds as opaque (round-trips through JSONL). |

---

## 12. Quick reference

```text
# build a release
cargo build --release -p octos-cli --features "octos-cli/api,octos-cli/telegram"

# deploy to one mini
scp target/release/octos cloud@<ip>:/tmp/octos.new
ssh cloud@<ip> 'sudo install -m 755 /tmp/octos.new /usr/local/bin/octos && \
  sudo launchctl unload /Library/LaunchDaemons/com.octos.gateway.plist && \
  pkill -TERM -x octos || true; sleep 4; pkill -KILL -x octos || true; \
  sudo launchctl load /Library/LaunchDaemons/com.octos.gateway.plist'

# verify health
curl -s -H 'Authorization: Bearer octos-admin-2026' \
  https://dspfac.bot.ominix.io/api/health | jq .

# run the M8 invariants suite
cd e2e && OCTOS_TEST_URL=https://dspfac.bot.ominix.io OCTOS_PROFILE=dspfac \
  npx playwright test tests/m8-runtime-invariants-live.spec.ts --workers=1
```
