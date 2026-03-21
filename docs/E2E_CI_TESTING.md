# E2E & CI Testing Architecture

## Test Inventory Summary

| Layer | Tests | Duration | CI Status |
|-------|-------|----------|-----------|
| Unit tests (all crates) | 1,496 | ~15s | In CI |
| Integration (API-dependent, `#[ignore]`) | 59 | ~5min | Manual (needs API keys) |
| Web UI (Playwright) | 21 | ~5-15min | Manual (needs running server) |
| Telegram E2E (tg-auto-testing) | 497 steps / 24 scenarios | ~30min | Manual |
| **Total** | **1,575+** | | |

## Unit & Integration Tests by Crate

| Crate | Tests | Coverage |
|-------|-------|----------|
| octos-agent | 663 | Agent loop, 60+ tools, sandbox (bwrap/Docker/macOS), prompt injection (73), security (69), MCP, plugins |
| octos-llm | 248 | 5 native + 8 OpenAI-compatible providers, adaptive routing (lane/hedge/off), failover, retry, streaming |
| octos-pipeline | 127 | DOT parser, executor, condition evaluation, JSON extraction, state machines, handlers |
| octos-cli | 127 | Config, session actor (queue modes, adaptive commands, /reset), auth (OAuth PKCE), API handlers |
| octos-bus | 119 | Sessions (flat + per-user layout), coalescing, channels, heartbeat, cron, API channel |
| octos-core | 70 | Types, SessionKey, UTF-8 truncation, serialization |
| octos-memory | 39 | Episode store (redb), BM25 + vector hybrid search, HNSW index |
| octos-plugin | 29 | Schema validation, manifest parsing, skill bootstrapping |

### Running Unit Tests

```bash
cargo test --workspace           # 1,496 tests, ~15s
cargo fmt --all -- --check       # Format check
cargo clippy --workspace         # Lint
```

## Ignored Integration Tests (Require API Keys)

59 tests marked `#[ignore]` that hit real LLM APIs:

| File | Tests | Providers Tested |
|------|-------|-----------------|
| `octos-llm/tests/tool_call_conversation.rs` | 27 | 14+ models (GPT-4o, Claude, Gemini, DeepSeek, Kimi, MiniMax, GLM, etc.) |
| `octos-llm/tests/ux_adaptive.rs` | 11 | Kimi + DeepSeek (hedge mode, lane switching, circuit breaker) |
| `octos-pipeline/tests/ux_pipeline.rs` | 14 passing + 26 ignored | Pipeline E2E with file I/O, media flow |
| `octos-agent/tests/security_sandbox.rs` | ~7 | macOS sandbox-exec kernel enforcement |

### Running Ignored Tests

```bash
# Requires API keys in environment
KIMI_API_KEY="..." DEEPSEEK_API_KEY="..." DASHSCOPE_API_KEY="..." \
  cargo test --workspace -- --ignored
```

## Web UI Tests (Playwright)

20 tests covering UX behavior via the web dashboard:

| Spec | Tests | What's Tested |
|------|-------|--------------|
| adaptive-routing | 4 | `/adaptive` status, hedge/lane/off switching, message after switch |
| command-hints | 4 | `/` shows hints, filtering, clear hides, `/help` feedback |
| queue-mode | 4 | `/queue` collect/steer/interrupt switching, collect merges |
| session-switching | 4 | New session empty, isolated history, sidebar populates, switch-back restores history |
| streaming-fidelity | 2 | SSE events arrive with no gaps, response renders in bubble |
| error-recovery | 2 | Multi-turn bubble count, cancel + new session |
| deep-research | 1 | Pipeline executes with SSE streaming progress |

### Running Web UI Tests

```bash
# Terminal 1: Start server
KIMI_API_KEY="..." DEEPSEEK_API_KEY="..." DASHSCOPE_API_KEY="..." \
  octos serve --host 0.0.0.0 --port 8080 --auth-token crew2026

# Terminal 2: Start Vite dev server
cd octos-web && npx vite --port 5174

# Terminal 3: Run tests
cd octos-web && npx playwright test
```

### Test Infrastructure

- **Selectors**: All use `data-testid` attributes (no CSS class selectors)
- **Test isolation**: `/reset` command in `beforeEach` resets queue mode, adaptive mode, and session history
- **Streaming detection**: Dual stability — streaming indicator stop + text content stable for 9s fallback
- **Auth**: `AUTH_TOKEN` env var or default `crew2026`

## Server-Side Fixes for Testing

These fixes were required to make full UX testing work:

1. **Session proxy** — `octos serve` proxies `/api/sessions` to gateway (sessions live in gateway process, not serve process)
2. **`/reset` command** — Resets queue=collect, adaptive=off, clears session history
3. **`_completion` marker** — `send_reply()` sends SSE `done` event for slash commands (closes stream)
4. **`supports_edit()` on ApiChannel** — Enables stream forwarder for SSE progressive streaming
5. **`list_sessions()` per-user scan** — Scans both legacy `data/sessions/` and per-user `data/users/*/sessions/` directories

## CI Server Setup (Mac Mini)

Recommended setup for a dedicated CI test server:

### Self-Hosted GitHub Actions Runner

```bash
# Install runner on Mac Mini
cd /Users/cloud/actions-runner
./config.sh --url https://github.com/octos-org/crew-rs --token <TOKEN>
./svc.sh install && ./svc.sh start
```

### CI Workflow

```yaml
name: Full Test Suite
on: [push, pull_request]

jobs:
  unit:
    runs-on: self-hosted
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace

  integration:
    runs-on: self-hosted
    needs: unit
    env:
      KIMI_API_KEY: ${{ secrets.KIMI_API_KEY }}
      DEEPSEEK_API_KEY: ${{ secrets.DEEPSEEK_API_KEY }}
      DASHSCOPE_API_KEY: ${{ secrets.DASHSCOPE_API_KEY }}
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --workspace -- --ignored

  web-ui:
    runs-on: self-hosted
    needs: unit
    env:
      KIMI_API_KEY: ${{ secrets.KIMI_API_KEY }}
      DEEPSEEK_API_KEY: ${{ secrets.DEEPSEEK_API_KEY }}
      DASHSCOPE_API_KEY: ${{ secrets.DASHSCOPE_API_KEY }}
      TELEGRAM_BOT_TOKEN: ${{ secrets.TELEGRAM_BOT_TOKEN }}
    steps:
      - uses: actions/checkout@v4
      - run: cargo build --release -p octos-cli --features "api,telegram"
      - run: |
          ./target/release/octos serve --port 8080 --auth-token crew2026 &
          sleep 5
      - run: cd octos-web && npm ci && npx playwright install chromium
      - run: cd octos-web && npx playwright test
      - uses: actions/upload-artifact@v4
        if: failure()
        with:
          name: playwright-report
          path: octos-web/test-results/
```

### Expected CI Run Time

| Job | Duration | Tests |
|-----|----------|-------|
| unit | ~30s | 1,496 |
| integration | ~5min | 59 |
| web-ui | ~6min | 20 |
| **Total** | **~12min** | **1,575** |
