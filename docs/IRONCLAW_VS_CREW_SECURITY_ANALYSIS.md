# Security Feature Analysis: IronClaw vs crew-rs

> Comparative deep-dive based on source code review of both codebases.
> Date: 2026-03-07

## Executive Summary

IronClaw and crew-rs take fundamentally different security postures:

- **IronClaw**: Defense-in-depth with encrypted secrets, WASM sandboxing, network proxies, and trust-based attenuation. Heavier operational footprint, stronger isolation guarantees.
- **crew-rs**: Lightweight, practical security with O_NOFOLLOW file safety, sophisticated prompt injection detection, and platform-native sandboxing (bwrap/sandbox-exec). Leaner, easier to deploy, fewer moving parts to fail.

Neither is strictly superior. Each has critical gaps the other addresses.

---

## 1. Credential & Secrets Management

### IronClaw: Encrypted Store + Zero-Exposure Model

IronClaw implements a full secrets management system (`src/secrets/`):

| Component | Implementation |
|-----------|---------------|
| Encryption | AES-256-GCM (`src/secrets/crypto.rs`) |
| Master key | OS keychain (macOS Keychain, GNOME Keyring) via `src/secrets/keychain.rs` |
| Storage | Encrypted blobs in database (PostgreSQL or libSQL) |
| Access model | Zero-exposure: secrets never enter container/WASM memory |

**Zero-exposure credential injection** (`src/sandbox/proxy/http.rs`, `src/tools/wasm/credential_injector.rs`):
- Docker containers and WASM tools never see raw secret values
- HTTP requests are intercepted at the host boundary
- Credentials injected into headers/query params at transit time
- If WASM module is compromised, secrets cannot be exfiltrated

**Secret tools** (`src/tools/builtin/secrets_tools.rs`):
- `secret_list` / `secret_delete` exposed to agent
- No `secret_read` tool — values are never exposed, even to the LLM

### crew-rs: Environment Variables + Redaction

crew-rs stores credentials as environment variables or config values:

| Component | Implementation |
|-----------|---------------|
| Storage | Environment variables, config JSON files |
| Protection | `BLOCKED_ENV_VARS` (18 vars) scrubbed before shell/MCP execution |
| Redaction | Pattern-based credential detection in `sanitize.rs` |
| Access model | SDK-level — credentials in process memory |

**Credential redaction patterns** (`crates/crew-agent/src/sanitize.rs`):
- OpenAI: `sk-` prefix (20+ chars)
- Anthropic: `sk-ant-` prefix
- AWS: `AKIA` + 16 uppercase alphanumeric
- GitHub: `ghp_`, `gho_`, `ghs_`, `ghr_`, `github_pat_`
- GitLab: `glpat-` prefix
- Bearer tokens (generic)
- Redaction preserves 4-char prefix for debugging

### Verdict

**IronClaw is significantly stronger.** Encrypted-at-rest secrets with OS keychain master key and zero-exposure injection is a production-grade secrets model. crew-rs relies on env vars (plaintext in memory, visible via `/proc/self/environ` on Linux, leaked by debug dumps). The `BLOCKED_ENV_VARS` scrubbing is defense against accidental exposure, not a security boundary.

**crew-rs gap**: No encrypted storage. A compromised process can read all secrets from memory.
**IronClaw gap**: Gateway auth token stored as plaintext `String` instead of `SecretString` (`src/config/channels.rs:47`), inconsistent with its own encrypted model.

---

## 2. Sandbox Isolation

### IronClaw: Docker + WASM (wasmtime)

**Docker sandbox** (`src/sandbox/`):

| Feature | Detail |
|---------|--------|
| Policies | ReadOnly, WorkspaceWrite, FullAccess |
| Network | Domain-allowlist proxy (`src/sandbox/proxy/`) |
| Credentials | Injected by host proxy, never in container env |
| Resources | Memory limit (512MB default), CPU limit, timeout (1800s) |
| CONNECT tunnel | HTTPS validated against allowlist before tunnel established |

**WASM tool sandbox** (`src/tools/wasm/`):

| Feature | Detail |
|---------|--------|
| Runtime | wasmtime with component model |
| Fuel metering | 10M fuel units per invocation |
| Memory | 50MB limit |
| Network | Host-side HTTP allowlist + credential injection |
| Storage | Linear memory persistence (optional) |
| Isolation | Fresh store per invocation (no state leakage) |

**WASM channel sandbox** (`src/channels/wasm/`):
- Same wasmtime isolation for Telegram/Slack channel code
- Capabilities declared via JSON (allowed HTTP hosts, polling intervals, rate limits)
- Workspace writes scoped to `channels/<name>/` prefix

### crew-rs: bwrap + sandbox-exec + Docker

**Three sandbox backends** (`crates/crew-agent/src/sandbox.rs`), auto-detected:

| Backend | Platform | Isolation |
|---------|----------|-----------|
| bubblewrap (bwrap) | Linux | Namespace-based: PID isolation, read-only `/usr /lib /bin /sbin /etc`, tmpfs `/tmp`, network blocked, `--die-with-parent` |
| sandbox-exec (SBPL) | macOS | Apple Seatbelt: write confined to cwd + `/private/tmp` + `/private/var/folders`, network blocked |
| Docker | Any | Configurable image, CPU/memory/PID limits, mount mode (none/ro/rw) |
| None | Fallback | No sandbox (development only) |

**Shared protections across all backends:**
- `BLOCKED_ENV_VARS` (18 vars): `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `NODE_OPTIONS`, `PYTHONSTARTUP`, `PERL5OPT`, `RUBYOPT`, `GEM_HOME`, `BASH_ENV`, `ENV`, `CDPATH`, `GLOBIGNORE`, `PROMPT_COMMAND`, `PYTHONPATH`, `NODE_PATH`, `JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`, `MAVEN_OPTS`, `GRADLE_OPTS`
- Applied to shell commands AND MCP server process spawning

**SBPL path sanitization:**
- Rejects paths containing SBPL metacharacters and control chars
- Prevents sandbox-exec profile injection

### Verdict

**Both are strong, different trade-offs.** IronClaw's WASM sandbox provides finer-grained isolation (fuel metering, per-invocation fresh state) but only for WASM tools. Docker adds network proxy with credential injection. crew-rs provides **platform-native sandboxing** (bwrap on Linux, sandbox-exec on macOS) that's lighter weight and doesn't require Docker. crew-rs's `BLOCKED_ENV_VARS` list is more comprehensive (18 vars vs IronClaw's shell scrubbing).

**crew-rs gap**: No network-level proxy. Containers/sandboxes either have full network or none. No credential injection at transit time.
**IronClaw gap**: No macOS sandbox-exec fallback. No bwrap support. Requires Docker daemon for non-WASM isolation.

---

## 3. Prompt Injection Defense

### IronClaw: Multi-Layer Safety System

**SafetyLayer** (`src/safety/`):

| Layer | File | Function |
|-------|------|----------|
| Sanitizer | `sanitizer.rs` | Aho-Corasick pattern matching, content escaping, role marker detection |
| Validator | `validator.rs` | Length limits, encoding checks, forbidden patterns |
| Policy | `policy.rs` | Rules with severity (Critical/High/Medium/Low) and actions (Block/Warn/Review/Sanitize) |
| Leak Detector | `leak_detector.rs` | 15+ secret patterns, scans tool output AND LLM responses |

**Sanitizer detection categories:**
- System prompt extraction attempts
- Role/instruction override markers (`<|system|>`, `[INST]`, etc.)
- Command injection (chained commands, subshells, path traversal)
- XML/HTML injection
- Encoding bypass attempts (hex, base64, unicode)

**Tool output wrapping:**
```xml
<tool_output name="search" sanitized="true">
[escaped content]
</tool_output>
```

### crew-rs: Sophisticated Prompt Guard

**PromptGuard** (`crates/crew-agent/src/prompt_guard.rs`):

| Feature | Detail |
|---------|--------|
| Threat categories | 5: RoleHijack, InstructionOverride, DataExfiltration, SystemPromptLeak, SocialEngineering |
| Severity levels | 3: Critical, High, Medium |
| Detection | Regex-based scanning with defanging |
| Defanging | Wraps detected injection in `[injection-blocked:...]` markers |

**Edge cases tested and handled:**
- Unicode homoglyph attacks (Cyrillic 'а' vs Latin 'a')
- Zero-width character injection (U+200B, U+FEFF)
- RTL override characters (U+202E)
- CJK encoding bypass attempts
- Mixed-case evasion (`SyStEm PrOmPt`)
- Nested injection (`ignore above ignore above...`)
- Base64/hex encoded payloads
- Markdown/HTML comment hiding

**Output sanitization** (`crates/crew-agent/src/sanitize.rs`):
- Strips base64 data URIs (images, binary)
- Removes long hex strings (64+ chars)
- Redacts known credential patterns
- Generic high-entropy secret detection

### Verdict

**crew-rs has more sophisticated prompt injection defense.** The `prompt_guard.rs` handles Unicode homoglyphs, zero-width characters, and RTL overrides that IronClaw's `sanitizer.rs` does not detect. crew-rs's test suite for injection patterns is notably more thorough.

**IronClaw has a stronger overall safety architecture** — the policy engine with severity levels and configurable actions (Block/Warn/Review/Sanitize) is more flexible than crew-rs's binary block/allow. IronClaw's leak detector scanning LLM responses before they reach users is a layer crew-rs lacks.

**crew-rs gap**: No leak detection on LLM output. No configurable policy engine. No tool output wrapping/escaping.
**IronClaw gap**: No Unicode homoglyph detection. No zero-width character stripping. No RTL override detection. Multiple UTF-8 byte-slice bugs in the safety layer itself (see Section 8).

---

## 4. File I/O Safety

### IronClaw: Check-Then-Open Pattern

File tools (`src/tools/builtin/file.rs`) use:
```rust
fn validate_path(path: &str) -> Result<PathBuf, ToolError> {
    let canonical = std::fs::canonicalize(path)?;
    // Check against allowed directories
}
```

**Vulnerability**: This is a classic TOCTOU (Time-of-Check-Time-of-Use) race. Between `canonicalize()` and `open()`, a symlink can be swapped in. The path is validated as safe, then a different file is actually opened.

### crew-rs: O_NOFOLLOW Pattern

File tools (`crates/crew-agent/src/tools/read_file.rs`, `write_file.rs`) use:
```rust
// Unix: O_NOFOLLOW eliminates TOCTOU
std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW)
    .open(&path)?;
```

**Advantage**: The kernel rejects symlinks atomically at open time. No race window exists. If the path is a symlink, the open fails — period.

**Additional protections:**
- `resolve_path()` with parent directory traversal checks
- Rejects paths outside workspace boundaries
- Handles edge cases (`.` components, excessive `..`)

### Verdict

**crew-rs is materially safer.** `O_NOFOLLOW` is the correct solution to symlink-based path traversal. IronClaw's `canonicalize → open` pattern has a real race condition window that could be exploited by a malicious tool or concurrent process.

**IronClaw gap**: No `O_NOFOLLOW` usage anywhere in file tools.
**crew-rs gap**: `O_NOFOLLOW` is Unix-only; Windows file tools fall back to check-then-open.

---

## 5. Shell Command Safety

### IronClaw

**Shell tool** (`src/tools/builtin/shell.rs`):

| Protection | Detail |
|------------|--------|
| Dangerous patterns | Blocklist: `sudo`, `rm -rf`, `chmod 777`, `eval`, `curl \| sh`, `dd`, `mkfs`, `:(){ :\|:& }` |
| Env scrubbing | Removes sensitive env vars before execution |
| Working dir | Accepts `workdir` parameter — **NOT validated for path traversal** |
| Timeout | Configurable per-execution |
| Output truncation | Limits output size |

**Vulnerability**: The `workdir` parameter from the LLM is used directly as `PathBuf` without validation (`shell.rs:606-611`). A prompt injection could set `workdir` to `/etc` or `~/.ssh` and run commands relative to that directory.

**Pattern matching weakness**: `"sudo "` requires a trailing space. `sudo\t` (tab), `sudo\n` (newline), `eval\t` all bypass the check.

### crew-rs

**Shell tool** (`crates/crew-agent/src/tools/shell.rs`) + **CommandPolicy** (`policy.rs`):

| Protection | Detail |
|------------|--------|
| SafePolicy | Denies: `rm -rf /`, `dd if=/dev/zero`, `mkfs.*`, fork bombs |
| Whitespace normalization | Patterns checked after normalizing whitespace |
| Env scrubbing | 18 `BLOCKED_ENV_VARS` applied consistently |
| Sandbox integration | Shell runs inside bwrap/sandbox-exec/Docker when enabled |
| Argument validation | 1MB limit on tool arguments (non-allocating size check) |
| Timeout | Clamped to [1, 600] seconds |

**Advantage**: Whitespace normalization means `sudo\t` and `sudo\n` variants are caught. Sandbox integration means even if a pattern is missed, the process is confined.

### Verdict

**crew-rs is stronger.** Whitespace-normalized pattern matching + mandatory sandbox execution is a better defense model than IronClaw's raw pattern matching without sandbox fallback. IronClaw's unvalidated `workdir` parameter is a real vulnerability.

**IronClaw gap**: `workdir` path traversal, whitespace bypass in patterns, no lightweight sandbox for shell (Docker only).
**crew-rs gap**: Simpler pattern set (fewer specific patterns than IronClaw).

---

## 6. Network Security

### IronClaw: Domain-Level Proxy

**Network proxy** (`src/sandbox/proxy/`):
- All container HTTP/HTTPS traffic routed through host proxy
- Domain allowlist validation before forwarding
- CONNECT tunnel: validates target domain before establishing
- Credential injection at proxy level
- `NetworkPolicyDecider` trait for custom allow/deny logic
- Hop-by-hop header stripping on proxied requests

**Default allowlist includes**: Package registries (npm, PyPI, crates.io), docs sites, GitHub, common APIs.

### crew-rs: SSRF Protection + Binary Network Control

**SSRF protection** (`crates/crew-agent/src/tools/ssrf.rs`):

Private IP ranges blocked:
- IPv4: `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16` (AWS metadata), `0.0.0.0`
- IPv6: loopback, unspecified, multicast, ULA (`fc00::/7`), link-local (`fe80::/10`), site-local (`fec0::/10`), IPv4-mapped/compatible
- DNS resolution fallback check (resolves hostname, checks if it maps to private IP)

**Sandbox network**: Binary on/off. bwrap and sandbox-exec block all network by default. Docker can be configured for network isolation.

### Verdict

**IronClaw has finer-grained network control.** The domain-allowlist proxy allows specific APIs while blocking everything else — crew-rs can only do all-or-nothing network access per sandbox. However, crew-rs's SSRF protection (private IP blocking with DNS resolution check) catches a class of attacks that IronClaw's domain allowlist doesn't directly address (DNS rebinding to private IPs).

**IronClaw gap**: No SSRF private IP checking. Domain allowlist doesn't prevent DNS rebinding.
**crew-rs gap**: No domain-level proxy. No credential injection at network level. Binary network on/off.

---

## 7. Trust & Access Control

### IronClaw: Trust-Based Skill Attenuation

**Skills trust model** (`src/skills/attenuation.rs`):

| Trust Level | Source | Tool Access |
|-------------|--------|-------------|
| Trusted | User-placed in `~/.ironclaw/skills/` or workspace `skills/` | All tools |
| Installed | Downloaded from ClawHub registry | Read-only tools only (no shell, file write, HTTP) |

Downloaded skills automatically lose access to dangerous tools. This prevents a malicious skill from the registry from executing shell commands or writing files.

**Tool approval model** (`src/tools/tool.rs`):
- `ApprovalRequirement::Never` — always auto-approved
- `ApprovalRequirement::UnlessAutoApproved` — needs approval first time, then session-remembered
- `ApprovalRequirement::Always` — requires approval every invocation
- `ToolDomain::Orchestrator` vs `Container` — prevents container-domain tools from running on host

### crew-rs: Policy-Based Tool Filtering

**ToolPolicy** (`crates/crew-agent/src/tools/policy.rs`):

```json
{
  "allow": ["group:fs", "read_file"],
  "deny": ["shell", "spawn"],
  "byProvider": {
    "ollama": { "deny": ["web_search", "browser"] }
  }
}
```

- Allow/deny lists with wildcard matching (`web_*`)
- Named groups: `group:fs`, `group:runtime`, `group:web`, `group:search`, `group:sessions`
- **Deny always wins** over allow
- Per-LLM-provider restrictions (weaker models get fewer tools)

### Verdict

**Complementary strengths.** IronClaw's trust-based attenuation is specifically designed for untrusted code from registries — a threat model crew-rs doesn't address. crew-rs's per-provider tool policies (restricting local Ollama models from web tools) address a different threat: weaker models being less reliable with dangerous tools.

**IronClaw gap**: No per-provider tool restrictions. A local Ollama model gets the same tools as Claude.
**crew-rs gap**: No trust model for skills. Downloaded skills get full tool access.

---

## 8. Known Vulnerabilities Found in Code Review

### IronClaw

| # | Severity | Issue | Location |
|---|----------|-------|----------|
| 1 | **Critical** | UTF-8 byte-slice panic in WASM host log truncation | `src/tools/wasm/host.rs:162-165` |
| 2 | **Critical** | `mask_secret` mixes byte length and char count | `src/safety/leak_detector.rs:356-366` |
| 3 | **Critical** | `apply_redactions` panics on overlapping regex matches | `src/safety/leak_detector.rs:369-390` |
| 4 | **High** | Auth token exposed in URL query parameter | `src/main.rs:526-531` |
| 5 | **High** | Shell `workdir` accepts arbitrary paths without validation | `src/tools/builtin/shell.rs:606-611` |
| 6 | **High** | `.expect()` in MCP client constructors (3 instances) | `src/tools/mcp/client.rs:70,90,115` |
| 7 | **High** | Shell dangerous-pattern bypass via tab/newline | `src/tools/builtin/shell.rs:86-102` |
| 8 | **High** | FTS5 query injection in libSQL backend | `src/db/libsql/workspace.rs:523-527` |
| 9 | **Medium** | Internal errors exposed to users | `src/agent/agent_loop.rs:627-628` |
| 10 | **Medium** | Debug logging of full LLM request/response bodies | `src/llm/nearai_chat.rs:214-218` |
| 11 | **Medium** | Gateway auth token stored as plaintext String | `src/config/channels.rs:47` |
| 12 | **Medium** | WebSocket origin check ignores port | `src/channels/web/server.rs:848-860` |

### crew-rs

| # | Severity | Issue | Location |
|---|----------|-------|----------|
| 1 | **High** | `&body[..200]` byte-slice in error truncation | `crates/crew-llm/src/provider.rs` (truncate_error_body) |
| 2 | **Medium** | macOS sandbox-exec is deprecated (macOS 15+) | `crates/crew-agent/src/sandbox.rs` |
| 3 | **Low** | No leak detection on LLM output before sending to user | Architectural gap |
| 4 | **Low** | Downloaded skills get full tool access | `crates/crew-agent/src/skills.rs` |

### Verdict

IronClaw has more discovered vulnerabilities, partly because it has more attack surface (WASM runtime, network proxy, dual DB backends). The critical UTF-8 bugs in IronClaw's safety layer are particularly concerning because they're in the code meant to provide security guarantees.

crew-rs has fewer issues but the `&body[..200]` byte-slice bug is the same class of vulnerability, and the lack of LLM output scanning is an architectural gap.

---

## 9. Leak Detection & Data Exfiltration Prevention

### IronClaw: Dual-Point Scanning

**LeakDetector** (`src/safety/leak_detector.rs`):
- Scans at **two points**: tool output before LLM, and LLM response before user
- 15+ pattern types: API keys (OpenAI, Anthropic, AWS, GitHub, GitLab, Slack, Twilio, Stripe, SendGrid), tokens (JWT, Bearer), private keys (RSA/EC PEM), connection strings, high-entropy hex
- Per-pattern actions: Block (reject), Redact (mask), Warn (flag but allow)
- `mask_secret()`: preserves 4-char prefix + 4-char suffix for debugging

**Shell environment scrubbing** (`src/tools/builtin/shell.rs`):
- Removes sensitive env vars before command execution
- Detects command injection patterns (chained commands, subshells)

### crew-rs: Output Sanitization

**Sanitizer** (`crates/crew-agent/src/sanitize.rs`):
- Scans tool output only (not LLM responses)
- Redacts: OpenAI keys, Anthropic keys, AWS keys, GitHub tokens, GitLab tokens, Bearer tokens, generic secrets
- Strips: base64 data URIs, long hex strings (64+ chars)
- Pattern: preserves 4-char prefix, replaces rest with `[credential-redacted]`

### Verdict

**IronClaw is stronger.** Dual-point scanning (tool output + LLM response) catches secrets that the LLM might generate or hallucinate. crew-rs only scans tool output, so a leaked secret in an LLM response goes straight to the user.

---

## 10. Feature Comparison Matrix

| Security Feature | IronClaw | crew-rs |
|-----------------|----------|---------|
| **Encrypted secrets at rest** | AES-256-GCM + OS keychain | None (env vars) |
| **Zero-exposure credential injection** | Docker proxy + WASM host | None |
| **WASM tool sandbox** | wasmtime (fuel, memory, fresh store) | None |
| **Docker sandbox** | Yes (with network proxy) | Yes (configurable) |
| **Platform-native sandbox (bwrap)** | None | Linux (bwrap) |
| **Platform-native sandbox (macOS)** | None | sandbox-exec (SBPL) |
| **Prompt injection detection** | Pattern matching (sanitizer) | Regex + Unicode homoglyphs + ZWC + RTL |
| **Prompt injection severity/actions** | Policy engine (Block/Warn/Review/Sanitize) | Binary (block or allow) |
| **Leak detection (tool output)** | 15+ patterns, configurable actions | 7+ patterns, redaction |
| **Leak detection (LLM output)** | Yes (before user delivery) | None |
| **File I/O safety** | canonicalize (TOCTOU vulnerable) | O_NOFOLLOW (atomic) |
| **Shell command policy** | Pattern blocklist | SafePolicy + whitespace normalization |
| **Shell workdir validation** | None (vulnerability) | Not applicable (sandbox confines) |
| **Env var scrubbing** | Shell tool only | 18 vars, shell + MCP + sandbox |
| **SSRF protection** | None (domain proxy only) | Private IP blocking (IPv4+IPv6, DNS check) |
| **Network proxy (domain allowlist)** | Yes (HTTP + CONNECT) | None |
| **Trust-based skill attenuation** | Trusted vs Installed (tool ceiling) | None |
| **Per-provider tool policies** | None | Allow/deny + wildcards + groups |
| **Tool approval model** | Never/UnlessAutoApproved/Always | None |
| **Tool domain separation** | Orchestrator vs Container | None |
| **Credential detection in HTTP** | `credential_detect.rs` (headers, URL params) | None |
| **Content escaping** | XML/HTML in tool output wrapping | Markdown markers in `[injection-blocked:]` |
| **UTF-8 safety** | Multiple byte-slice bugs | `truncate_utf8()` utility (1 bug in provider.rs) |
| **unsafe_code deny** | Documented in CLAUDE.md | Enforced in Cargo.toml lint |

---

## 11. Recommendations

### For IronClaw (adopt from crew-rs)

1. **Adopt `O_NOFOLLOW`** on all file I/O operations — eliminates TOCTOU race conditions
2. **Add Unicode homoglyph detection** to sanitizer — Cyrillic lookalikes bypass current patterns
3. **Add zero-width character stripping** — invisible characters can hide injection payloads
4. **Add RTL override detection** — text direction manipulation can obscure malicious content
5. **Add `truncate_utf8()` utility** — fix 3+ byte-slice panic vectors in safety-critical code
6. **Validate shell `workdir`** parameter against allowed directories
7. **Normalize whitespace** in shell dangerous-pattern matching
8. **Add SSRF private IP blocking** — DNS rebinding can bypass domain allowlists
9. **Expand `BLOCKED_ENV_VARS`** to match crew-rs's 18-var list (add `PERL5OPT`, `RUBYOPT`, `GEM_HOME`, etc.)
10. **Enforce `unsafe_code = "deny"`** in `Cargo.toml`, not just documentation

### For crew-rs (adopt from IronClaw)

1. **Add encrypted secrets store** — env vars are insufficient for production credential management
2. **Add LLM output leak detection** — scan responses before delivery to users
3. **Add trust-based skill attenuation** — downloaded skills should not get shell/write access
4. **Add tool approval model** — dangerous tools should require user confirmation
5. **Add network domain-level proxy** — binary network on/off is too coarse
6. **Add policy engine** for prompt injection — configurable severity and actions vs binary block
7. **Add credential injection for containers** — zero-exposure model for Docker sandbox
8. **Fix `&body[..200]`** byte-slice in `provider.rs` — same vulnerability class as IronClaw's bugs
9. **Add tool domain separation** — prevent container-scoped tools from executing on host
10. **Add tool output wrapping** — structured escaping before LLM context injection

---

## 12. Threat Model Comparison

| Threat | IronClaw Defense | crew-rs Defense | Winner |
|--------|-----------------|-----------------|--------|
| **Compromised WASM tool** | Sandbox + zero-exposure credentials | N/A (no WASM tools) | IronClaw |
| **Compromised Docker container** | Network proxy + credential injection | Env scrubbing + resource limits | IronClaw |
| **Prompt injection via tool output** | Sanitizer + policy engine + content escaping | PromptGuard + defanging | crew-rs (better detection) |
| **Prompt injection via Unicode tricks** | Not detected | Homoglyph + ZWC + RTL detection | crew-rs |
| **Secret exfiltration via LLM response** | LeakDetector scans LLM output | Not scanned | IronClaw |
| **Secret exfiltration via tool output** | LeakDetector (15+ patterns) | Sanitizer (7+ patterns) | IronClaw |
| **Symlink-based file traversal** | canonicalize (TOCTOU race) | O_NOFOLLOW (atomic) | crew-rs |
| **Shell command injection** | Pattern blocklist (whitespace bypass) | SafePolicy + normalization + sandbox | crew-rs |
| **DNS rebinding / SSRF** | Domain proxy (no IP check) | Private IP blocking + DNS resolution | crew-rs |
| **Malicious registry skill** | Trust attenuation (read-only tools) | Full tool access | IronClaw |
| **Weak model misusing dangerous tools** | Same tools for all models | Per-provider tool policies | crew-rs |
| **Env var leakage** | Shell scrubbing only | 18 vars across shell + MCP + sandbox | crew-rs |
| **Memory dump / core dump secrets** | Encrypted at rest, keychain master key | Plaintext in memory | IronClaw |
