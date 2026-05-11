# M11 Soak Tests — Live Browser Scenarios

Status: PROPOSED — wired into `M11-H` (#878) as its acceptance gate.

Companion docs: `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md`,
`workstreams/M11-runtime-unification.md`.

## Purpose

After every M11 PR (#871–#878) merges to `main` and the fleet
binary is deployed, this suite validates that the two architectural
invariants M11 establishes — profile-scope and session-scope —
actually hold under live, real-LLM, real-skill-binary traffic. Unit
tests in M11-G cover the type contract; this suite covers what only
production-shape traffic can reveal.

## Test environment

| Host | Role | Profile | Skills | Notes |
|---|---|---|---|---|
| mini1 (.128) | Primary soak target | `dspfac` | `mofa-fm` + `mofa-podcast` + standard | Has yangmi.wav clone |
| mini2 (.129) | Multi-profile + Telegram | `dspfac`, `alphalab` (synthetic 2nd tenant) | varies per profile | Validates profile isolation |
| mini3 (.203) | Long-session stress | `dspfac` | Same as mini1 | Marathon target |

Driver: Playwright (existing `e2e/` infrastructure). Each spec uses
the `live-browser-helpers.ts` shape that the M9 / M10 specs follow.

A synthetic second profile (`alphalab`) is required on mini2 so the
multi-tenant tests have something to test against. Create it via the
admin dashboard before the suite runs:
- `alphalab` → moonshot/kimi-k2.5 with a *different* `KIMI_API_KEY`
  than `dspfac` uses (a second key from autodl is fine)
- `alphalab` has `mofa-fm` installed but no voice clones registered
- `alphalab` is owned by a different OS user-like identity, but lives
  in the same serve process

## Tests

Each test specifies:
- **Goal**: which M11 invariant it proves
- **Failure caught**: which architectural gap would cause it to fail
- **Pre-state**: required setup
- **Steps**: exact browser/curl actions
- **Pass criteria**: observable signal
- **Telemetry**: serve.log greps to capture in the test report

### M11-SOAK-1 — yangmi voice clone end-to-end (regression)

**Goal**: Workspace policy + per-session workspace dir are
bootstrapped automatically. The 2026-05-10 incident chain cannot recur.

**Failure caught**: workspace policy not found; mp3 generated but not
delivered; mp3 not generated at all.

**Pre-state**: mini1 on M11 binary. No hotfix `/Users/cloud/.octos-workspace.toml`
present (delete if it exists). Browser logged in to
`https://dspfac.crew.ominix.io/chat` as the admin profile.

**Steps**:
1. Open a fresh chat room (no prior history).
2. Send: `用 yangmi 语音说北京今天天气晴朗`
3. Wait up to 60s for the assistant reply.
4. Assert the chat bubble shows an audio attachment.
5. Click the attachment, confirm it plays (≥1.5s duration).

**Pass criteria**:
- HTTP 200 from `/api/chat` with `Background work started for fm_tts`.
- Within 60s, an mp3 file appears in
  `<profile_data_dir>/users/<session_key>/workspace/skill-output/yangmi_*.mp3`.
- The chat bubble renders the audio attachment.
- `serve.log` contains zero `"workspace policy not found"` entries
  in the test window.

**Telemetry**:
- Capture the session_key, the mp3 filename, and the line count of
  the `default.jsonl` for the session.

### M11-SOAK-2 — Coding-agent two-session isolation (canonical)

**Goal**: Two concurrent AppUI coding sessions on the same profile,
pointed at different workspace_roots, see different file trees and
maintain independent state.

**Failure caught**: server-wide base ToolRegistry leak; shared
workspace_root; shared chat history.

**Pre-state**: Two empty repos on mini1: `/tmp/repo-A` (containing
`a.txt` = "hello-A") and `/tmp/repo-B` (containing `b.txt` =
"hello-B"). Browser logged in to dspfac.

**Steps**:
1. Open AppUI coding session α via UI Protocol WS with
   `session.workspace_cwd.v1 = /tmp/repo-A`. Send: `read a.txt`.
2. While α is mid-turn, open a second AppUI coding session β with
   `session.workspace_cwd.v1 = /tmp/repo-B`. Send: `read b.txt`.
3. Wait both turns to complete.
4. In α, send: `list files`. In β, send: `list files`.
5. Assert α's `read_file` returned `hello-A` and never saw `b.txt`.
6. Assert β's `read_file` returned `hello-B` and never saw `a.txt`.
7. Assert both `list files` turns return only their own workspace
   contents.

**Pass criteria**:
- Both sessions complete without 5xx errors.
- α's transcript contains `hello-A`, never `hello-B`.
- β's transcript contains `hello-B`, never `hello-A`.
- α's session jsonl is at `<dspfac_data>/users/<α_key>/sessions/default.jsonl`,
  β's at a distinct user_key directory.
- `<α_workspace>/.octos-workspace.toml` and
  `<β_workspace>/.octos-workspace.toml` both exist.

**Telemetry**:
- α's session_key, β's session_key, both jsonl head + tail lines,
  both workspace policy files' mtime.

### M11-SOAK-3 — Web multi-room same-profile isolation

**Goal**: One logged-in user opens three rooms on the same profile;
each maintains independent workspace + history.

**Failure caught**: chat history bleed; per-room workspace not
bootstrapped; per-room tool filter ignored.

**Pre-state**: Browser logged in to dspfac with no prior session
state. mini1 on M11.

**Steps**:
1. Open Room R1 (chat). Send: `remember the secret BLUE42`.
2. Open Room R2 (chat). Send: `what was the secret in this chat?`
   (expect: no prior mention; the model should say it doesn't know).
3. Open Room R3 (slides). Send: `start a slides project named demo`.
4. Return to R1. Send: `what secret did I tell you?` (expect: BLUE42).
5. Return to R2. Confirm R2 still has no BLUE42 reference.
6. Confirm R3's `mofa_slides` skill spawn used a workspace under
   R3's session_key, not R1's.

**Pass criteria**:
- R1 recalls BLUE42; R2 does not; R3 has its own workspace.
- Each room's jsonl lives under a distinct `user_key`.
- Three distinct `<workspace_root>/.octos-workspace.toml` files exist
  by end of test, one per room.

**Telemetry**:
- Three session_keys, three jsonl excerpts, three workspace_root paths.

### M11-SOAK-4 — Profile A skill not visible to profile B

**Goal**: Tools registered to profile A's tool_specs are not visible
to a session opened on profile B.

**Failure caught**: server-wide ToolRegistry leak; profile
boundary not enforced at session bootstrap.

**Pre-state**: mini2 with two profiles: `dspfac` (mofa-fm installed)
and `alphalab` (no mofa-fm). Both enabled.

**Steps**:
1. Open a chat session on `dspfac` via `dspfac.crew.ominix.io`. Send:
   `list available tools and confirm fm_tts is in the list`.
2. Open a chat session on `alphalab` via its dashboard subdomain.
   Send: `list available tools and confirm fm_tts is NOT in the list`.
3. On `alphalab`, send: `用 yangmi 语音说测试`.

**Pass criteria**:
- `dspfac` session reports `fm_tts` available.
- `alphalab` session reports `fm_tts` not available — or, if the LLM
  attempts it, the runtime rejects with `tool not registered`.
- `alphalab`'s yangmi turn falls back to `voice_synthesize` / preset
  voice (not the dspfac-side yangmi clone).
- `<alphalab_data>/voice_profiles/yangmi.wav` does not exist (and is
  not somehow read from dspfac).

**Telemetry**:
- Both sessions' tool list, both transcripts, fs listing of each
  profile's voice_profiles dir.

### M11-SOAK-5 — Per-session sandbox override

**Goal**: A coding-agent session that requests a no-network sandbox
override actually has network egress blocked from its shell tool.

**Failure caught**: SandboxConfig ignored at SessionRuntime
construction; sandbox override leaks to other sessions.

**Pre-state**: Two AppUI coding sessions on dspfac, both pointed at
`/tmp/repo-A`. Session γ requests sandbox override `network: false`;
session δ takes the profile default (`network: true`).

**Steps**:
1. Open γ with sandbox override `network: false`.
2. Open δ with no sandbox override.
3. In γ, send: `run shell: curl -s https://example.com | head -c 50`.
4. In δ, send: `run shell: curl -s https://example.com | head -c 50`.

**Pass criteria**:
- γ's curl fails with a network-denied error (sandbox blocks egress).
- δ's curl succeeds and returns HTML bytes.
- γ's sandbox failure does not affect δ's subsequent turns.

**Telemetry**:
- γ's transcript showing sandbox denial, δ's transcript showing 200
  response, both sessions' `sandbox` field in their session metadata.

### M11-SOAK-6 — Hot-reload profile LLM swap

**Goal**: Changing `cfg.llm.primary.model_id` in the dashboard for an
active profile updates the ProfileRuntime without a serve restart.
New sessions use the new model; existing sessions continue with their
already-bound LLM until they expire.

**Failure caught**: ProfileRuntime is built once at startup and never
refreshed; profile config drift between disk and runtime.

**Pre-state**: mini1 on M11. dspfac profile currently using
moonshot/kimi-k2.5. Two browser sessions open (R1 active, R2 idle).

**Steps**:
1. In R1, send any short prompt; capture assistant reply + verify
   `Model: kimi-k2.5` log line.
2. Switch dspfac's primary in dashboard to minimax/MiniMax-M2.5-highspeed.
3. Wait 5s for hot-reload watcher.
4. Open a fresh R3. Send a short prompt. Assert assistant reply with
   `Model: MiniMax-M2.5-highspeed` log line.
5. In R1 (still open), send another short prompt. Acceptable either:
   - R1 continues on kimi-k2.5 (existing SessionRuntime cached its LLM).
   - R1 picks up the new model on next turn (acceptable if
     SessionRuntime re-resolves per turn).
   Document which behavior M11 chose.

**Pass criteria**:
- R3 uses the new model immediately.
- R1's behavior matches whichever side M11 commits to (per ADR);
  cross-session contamination (R1 on minimax mid-turn while
  responding to a kimi prompt) does NOT happen.

**Telemetry**:
- Three sessions' Model log lines, the dashboard profile_id change
  timestamp.

### M11-SOAK-7 — Cache eviction + rebuild from disk

**Goal**: SessionRuntimeCache evicts idle sessions; when the user
returns, the session rebuilds from disk-persisted state.

**Failure caught**: SessionRuntime is the source of truth (rather
than a cache over disk); session loss when cache evicts.

**Pre-state**: mini1 on M11 with `SessionRuntimeCache` configured for
`idle_ttl=2min` (override for this test, default is 30min).

**Steps**:
1. Open a chat session. Send: `remember secret YELLOW77`.
2. Note the chat history is persisted to disk
   (`<user_key>/sessions/default.jsonl` line count).
3. Idle for 3 minutes (no traffic to this session).
4. From a fresh browser tab/incognito, reopen the same session_id.
5. Send: `what secret did I tell you?`.

**Pass criteria**:
- After 3 minutes, `serve.log` shows the session was evicted from the
  cache (`session_runtime_cache: evicting ...`).
- Reopening the session_id rebuilds a fresh SessionRuntime from disk.
- The model recalls YELLOW77 from the persisted history.

**Telemetry**:
- Cache eviction log line, jsonl path + line count pre/post, recall
  test transcript.

### M11-SOAK-8 — Workspace boundary enforcement

**Goal**: A session's shell tool cannot read or write outside its
`workspace_root`.

**Failure caught**: workspace_root set but not applied to sandbox
allow-list; SBPL/bwrap rule generation broken.

**Pre-state**: Coding-agent session on dspfac with
`workspace_hint=/tmp/repo-A`. `/etc/hosts` is readable by the OS
user running serve.

**Steps**:
1. Send: `run shell: cat /etc/hosts | head -c 100`.
2. Send: `run shell: cat a.txt | head -c 100` (where a.txt is in the
   workspace).

**Pass criteria**:
- Step 1 fails with a sandbox permission denial; the response does
  NOT contain `/etc/hosts` contents.
- Step 2 succeeds and returns "hello-A".

**Telemetry**:
- Both turns' transcripts, the sandbox config sent at session bootstrap.

### M11-SOAK-9 — Marathon-60 mixed traffic

**Goal**: 60 minutes of mixed chat + spawn_only + coding turns
across mini1/2/3 without:
- workspace policy errors
- "no output files produced" errors
- cross-session content bleed
- LLM/credential errors

**Failure caught**: any flake or regression that doesn't show up in
short tests.

**Pre-state**: M11 binary on all three minis. 4 active session types
running simultaneously per mini:
- A) Chat with fm_tts every 30s (audio attachment)
- B) Chat with deep_search every 60s (long tool call)
- C) Coding agent in `/tmp/repo-A` with `read_file` + `edit_file`
  every 45s
- D) Coding agent in `/tmp/repo-B` with shell + read_file every 45s

**Steps**:
- Launch all 4 session types on each of 3 minis (12 concurrent
  sessions).
- Drive turns at the cadence above for 60 minutes.
- Capture serve.log and session jsonls every 5 minutes.

**Pass criteria**:
- ≥95% of turns complete successfully (LLM provider timeouts OK if <5%).
- Zero `workspace policy not found` entries.
- Zero `no output files produced` entries from informational tools.
- Each session's chat history contains only its own context (sample 10
  random session pairs for cross-bleed inspection).
- Per-mini steady-state memory < 1.5GB.

**Telemetry**:
- Per-5-minute snapshot of serve.log error counts, session count,
  process RSS, plus a final cross-bleed grep across all sessions.

### M11-SOAK-10 — Gateway parity (no regression)

**Goal**: M11's refactor of `gateway_runtime.rs` (the M11-B move into
`ProfileRuntime::bootstrap`) did NOT change observable gateway
behavior on the Telegram / Discord / Email channels.

**Failure caught**: M11-B regressed a side effect of gateway's
inline bootstrap.

**Pre-state**: mini1 + mini2 on M11 binary. Telegram bot + Discord
bot active.

**Steps**:
1. Send a Telegram message: `hi`. Expect normal echo response.
2. Send a Telegram message: `用 yangmi 语音说测试`. Expect audio
   delivery in Telegram chat.
3. Repeat both for Discord on mini2 if active.
4. Run M9/M10's existing soak harness (e.g. wave-6 patterns) for
   15 minutes against gateway-only channels.

**Pass criteria**:
- Telegram + Discord remain functionally identical to pre-M11 state.
- Audio attachments delivered for yangmi via Telegram (parity with web).
- Soak harness reports no new failure modes.

**Telemetry**:
- Telegram + Discord round-trip latencies, soak harness's existing
  summary output.

## Runner

A new Playwright spec `e2e/tests/m11-soak.spec.ts` orchestrates the
browser scenarios (SOAK-1 through SOAK-8). The marathon (SOAK-9) and
gateway-parity (SOAK-10) run via shell-driven harnesses
(`scripts/m11-marathon-60.sh` and the existing M9/M10 patterns
respectively).

Suite invocation: `scripts/m11-soak.sh --target mini1,mini2,mini3`
runs everything end-to-end, posts results to the tracker issue #870,
and fails CI if any SOAK-N gate fails.

## Acceptance gate for M11-H

All 10 tests must pass on mini1, mini2, mini3 before M11-H can close.
Per-test results posted to #878. Final summary linked from #870.

Any test failure: revert the offending M11 PR, file a follow-up, do
NOT close M11. We are not patching over M11 the way we patched over
the 2026-05-10 incident chain — that's the whole point of the
milestone.
