# Action Plan: Lessons from IronClaw Comparison

> Prioritized action items for crew-rs derived from deep code review of IronClaw.
> Date: 2026-03-07

---

## Priority 1: Critical Bugs (Fix Now)

### 1.1 Fix `&body[..200]` UTF-8 byte-slice panic
**File**: `crates/crew-llm/src/provider.rs` (truncate_error_body)
**Risk**: Panics on multi-byte UTF-8 error bodies from LLM providers (CJK, emoji, accented chars).
**Fix**: Use `crew_core::truncate_utf8()` which already exists and handles char boundaries correctly.
**Effort**: 1 hour

### 1.2 Audit all byte-index slicing across codebase
**Action**: `grep -rn '\[\.\.' crates/` and verify every slice is on a char boundary or ASCII-only.
**Why**: IronClaw had 3 critical UTF-8 panics in safety-critical code. Same bug class likely exists in crew-rs beyond the known provider.rs instance.
**Effort**: 2-3 hours

---

## Priority 2: Security Gaps (This Sprint)

### 2.1 Add LLM output leak detection
**What**: Scan LLM responses for secrets before delivering to users.
**Why**: IronClaw scans at two points (tool output + LLM response). crew-rs only scans tool output. An LLM can hallucinate or regurgitate API keys from its context window.
**How**:
- Extend `sanitize.rs` credential patterns into a `LeakDetector` that runs on assistant messages
- Hook into the agent loop after LLM response, before channel delivery
- Actions: redact (default), warn, block
**Effort**: 1-2 days

### 2.2 Add trust-based skill attenuation
**What**: Downloaded/third-party skills get restricted tool access.
**Why**: Currently all skills get full tool access. A malicious skill from a registry can execute shell commands, write files, make HTTP requests.
**How**:
- Add `SkillTrust` enum: `Trusted` (user-placed), `Installed` (downloaded)
- `Installed` skills lose access to: `shell`, `write_file`, `edit_file`, `web_fetch`, `spawn`
- Apply as a filter layer in `ToolRegistry` when skills are active
**Effort**: 2-3 days

### 2.3 Add tool approval model
**What**: Dangerous tools require user confirmation before execution.
**Why**: IronClaw has Never/UnlessAutoApproved/Always approval levels. crew-rs auto-executes everything including shell commands.
**How**:
- Add `approval_required: bool` to tool metadata (or per-tool in config)
- Default: shell, write_file, edit_file, spawn require approval
- Gateway/CLI prompt user, auto-approve in non-interactive mode with config flag
- Session-level "always approve" memory (like IronClaw's `auto_approved_tools`)
**Effort**: 3-5 days

### 2.4 Validate shell working directory
**What**: If shell tool accepts a `cwd`/`workdir` parameter, validate it against allowed paths.
**Why**: IronClaw has this exact vulnerability — LLM-supplied workdir used as-is. Verify crew-rs isn't vulnerable to the same pattern.
**Action**: Audit `crates/crew-agent/src/tools/shell.rs` for workdir handling. If present, add path validation.
**Effort**: 2-4 hours

---

## Priority 3: Architecture Improvements (Next Sprint)

### 3.1 Encrypted secrets store
**What**: Replace env var credential storage with encrypted-at-rest secrets.
**Why**: Env vars are plaintext in memory, visible via `/proc/self/environ`, leaked in core dumps, visible in `docker inspect`. IronClaw uses AES-256-GCM + OS keychain master key.
**How**:
- New crate: `crew-secrets`
- AES-256-GCM encryption (use `aes-gcm` crate)
- Master key from OS keychain (`keyring` crate — macOS Keychain, GNOME Keyring, Windows Credential Manager)
- Store: encrypted JSON file at `~/.crew/secrets.enc`
- Migration: import existing env vars on first run
- CLI: `crew secret set/get/list/delete`
- Zero-exposure: secrets never in tool/WASM process memory
**Effort**: 1-2 weeks

### 3.2 Network domain-level proxy for Docker sandbox
**What**: Route container HTTP through a host proxy with domain allowlist.
**Why**: Current sandbox is binary network on/off. IronClaw's proxy allows specific APIs (npm, PyPI, GitHub) while blocking everything else, plus injects credentials at transit time.
**How**:
- Lightweight HTTP/CONNECT proxy (use `hyper` or `tokio` directly)
- Domain allowlist config (default: package registries, docs sites)
- Credential injection via `CredentialResolver` trait
- Set `http_proxy`/`https_proxy` env in container
**Effort**: 1-2 weeks

### 3.3 Policy engine for prompt injection
**What**: Replace binary block/allow with configurable severity and actions.
**Why**: IronClaw's policy engine lets users tune sensitivity: Critical→Block, High→Warn, Medium→Sanitize, Low→Allow. crew-rs currently blocks or doesn't.
**How**:
- `PromptPolicy` with severity levels and actions
- Config-driven rules (users can adjust thresholds)
- Default: Critical/High→block, Medium→sanitize, Low→warn
**Effort**: 3-5 days

### 3.4 SSRF defense: add DNS resolution check
**What**: After resolving hostname, verify IP isn't private before connecting.
**Why**: crew-rs blocks direct private IPs in `ssrf.rs`, but DNS rebinding can resolve `evil.com` → `169.254.169.254`. Need to check resolved IP after DNS.
**How**:
- In `web_fetch` and `browser` tools, resolve DNS first
- Check all resolved IPs against private ranges
- Reject if any resolve to private
**Effort**: 1-2 days

---

## Priority 4: Feature Parity (Backlog)

### 4.1 Response caching layer
**What**: LRU + TTL cache for LLM responses.
**Why**: IronClaw caches non-tool completions with SHA-256 keyed LRU (1000 entries, 1hr TTL). Saves cost on repeated/similar queries.
**How**:
- Add `CachedProvider` wrapper in `crew-llm`
- Key: hash of (model, messages, temperature, max_tokens)
- Never cache tool-calling responses
- Config: `cache_enabled`, `cache_max_entries`, `cache_ttl_secs`
**Effort**: 2-3 days

### 4.2 Circuit breaker for LLM providers
**What**: 3-state circuit breaker (Closed→Open→HalfOpen) wrapping LLM calls.
**Why**: IronClaw has a full circuit breaker that stops hammering failing providers. crew-rs's RetryProvider retries blindly.
**How**:
- `CircuitBreakerProvider` decorator in `crew-llm`
- Config: failure_threshold (5), recovery_timeout (30s), half_open_probes (2)
- Transient errors trip breaker; auth/config errors don't
**Effort**: 2-3 days

### 4.3 Tool domain separation
**What**: Mark tools as `Host` or `Container` domain. Container-domain tools can't run on host.
**Why**: IronClaw separates `Orchestrator` vs `Container` tools. If a worker container requests a host-only tool, it's rejected. Prevents privilege escalation.
**How**:
- Add `domain: ToolDomain` to tool metadata
- Validate domain before execution in sandbox context
**Effort**: 1-2 days

### 4.4 Tool output structured wrapping
**What**: Wrap tool output in structured markers before injecting into LLM context.
**Why**: IronClaw wraps as `<tool_output name="x" sanitized="true">`. This gives the LLM clear boundaries, reducing confusion between tool output and instructions.
**How**:
- Wrap in the agent loop after tool execution, before message assembly
- Format: `<tool_result tool="name" status="success|error">\n{output}\n</tool_result>`
**Effort**: Half day

### 4.5 Credential detection in HTTP requests
**What**: Detect and warn when credentials appear in HTTP request URLs/headers in logs.
**Why**: IronClaw's `credential_detect.rs` scans HTTP traffic for leaked credentials. Useful for catching accidental token exposure in URLs.
**Effort**: 1-2 days

---

## Priority 5: Code Quality (Ongoing)

### 5.1 Enforce `unsafe_code = "deny"` workspace-wide
**Status**: Already done in crew-rs Cargo.toml. Verify no `#[allow(unsafe_code)]` bypasses exist.
**Action**: `grep -rn 'allow.*unsafe_code' crates/`
**Effort**: 30 minutes

### 5.2 Reduce `.unwrap()` in production code
**Current**: ~1,465 unwraps across 131 files (~16.5/KLOC).
**Target**: Zero in non-test code paths.
**Priority files** (highest unwrap density):
- `crates/crew-bus/src/session.rs` (~101 unwraps)
- `crates/crew-agent/src/skills.rs` (~62 unwraps)
- `crates/crew-memory/src/memory_store.rs` (~50 unwraps)
**Effort**: Ongoing, file-by-file

### 5.3 Add transaction safety to multi-step DB operations
**Why**: IronClaw's code review found multiple non-atomic multi-step operations. Same pattern likely exists in crew-rs's redb usage.
**Action**: Audit all redb write operations for atomicity. Multi-step writes should use redb's `WriteTransaction`.
**Effort**: 1-2 days

### 5.4 Expand env var scrubbing list
**Current**: 18 `BLOCKED_ENV_VARS` (already strong).
**Add**: Verify coverage against IronClaw's shell scrubbing list. Consider adding: `HISTFILE`, `MYSQL_PWD`, `PGPASSWORD`, `AWS_SESSION_TOKEN`, `GOOGLE_APPLICATION_CREDENTIALS`.
**Effort**: 1 hour

---

## Priority 6: Architectural Patterns to Consider (Future)

### 6.1 In-session async task spawning with context merge
**What**: Allow users to continue chatting while a long-running task processes in background, then merge results back.
**Status**: Neither IronClaw nor crew-rs supports this. IronClaw's background jobs are isolated islands with no merge-back.
**Challenge**: LLM conversation history has no clean merge semantic for divergent branches.
**Approach**:
- Fork conversation context on long-running task detection
- User continues on main branch
- On completion: LLM-generated summary of background result injected as system message
- Not a true merge — append-only with summarization
**Effort**: 2-4 weeks (research + implementation)

### 6.2 Smart routing with session stickiness
**What**: IronClaw's 13-dimension complexity scorer routes messages to different model tiers. Interesting idea but flawed: cross-model routing thrashes KV cache and breaks prompt caching.
**Recommendation**: If implementing model routing, pin model selection per conversation (not per message). Re-evaluate only on `/new` thread or after long idle.
**Effort**: 1-2 weeks

### 6.3 WASM tool sandbox
**What**: IronClaw runs tools as WASM components with fuel metering, memory limits, and fresh-instance-per-call isolation.
**Trade-off**: Stronger isolation than process sandbox, but significant dev friction (WIT bindings, wasm32-wasip2 toolchain). IronClaw's WASM channel ecosystem has only 2 working implementations despite the infrastructure investment.
**Recommendation**: Not worth adopting unless crew-rs plans a third-party tool marketplace. bwrap/sandbox-exec is sufficient for first-party tools.
**Effort**: 3-4 weeks

---

## Summary: Effort vs Impact Matrix

```
                        HIGH IMPACT
                            |
    [2.1] LLM leak detect  |  [3.1] Encrypted secrets
    [2.3] Tool approval     |  [3.2] Network proxy
    [2.2] Skill attenuation |
                            |
  LOW EFFORT ---------------+--------------- HIGH EFFORT
                            |
    [1.1] Fix byte-slice    |  [6.1] Async task spawn
    [1.2] Audit slicing     |  [6.3] WASM sandbox
    [5.4] Expand env vars   |  [6.2] Smart routing
    [4.4] Tool wrapping     |
                            |
                        LOW IMPACT
```

## Recommended Execution Order

| Week | Items | Theme |
|------|-------|-------|
| **Now** | 1.1, 1.2 | Fix critical bugs |
| **Week 1** | 2.1, 2.4, 5.4 | Close security gaps (quick wins) |
| **Week 2** | 2.2, 2.3 | Trust model + approval |
| **Week 3** | 3.3, 3.4, 4.4 | Policy engine + SSRF + tool wrapping |
| **Week 4** | 4.1, 4.2, 4.3 | LLM resilience (cache, circuit breaker, domains) |
| **Month 2** | 3.1, 3.2 | Encrypted secrets + network proxy |
| **Month 3** | 5.2, 5.3 | Code quality sweep |
| **Backlog** | 6.1, 6.2, 6.3 | Architectural exploration |
