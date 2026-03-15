# Security Architecture

crew-rs multi-tenant AI agent gateway security reference. Last updated: 2026-03-15.

---

## 1. Threat Model

crew-rs runs multiple AI agent profiles on a single host, each with access to shell execution, file I/O, web requests, and LLM APIs. The security surface includes:

| Threat | Vector | Impact |
|--------|--------|--------|
| **Tenant cross-read** | Profile A reads Profile B's session files, memory, or episodes | Data leak between tenants |
| **Prompt injection** | Malicious user input causes the LLM to invoke dangerous tools | Arbitrary code execution, data exfiltration |
| **Tool abuse** | LLM-generated shell commands (`rm -rf /`, fork bombs, `dd`) | Host compromise, DoS |
| **Path traversal** | `../../etc/passwd` in tool arguments | Escape from working directory |
| **Symlink escape** | Symlink inside cwd points to `/etc/shadow` | Read/write outside sandbox |
| **SSRF** | `web_fetch` / `browser` to `169.254.169.254` or `localhost` | Cloud metadata theft, internal service access |
| **Secret leakage** | API keys in env vars leak into LLM context or tool output | Credential exposure |
| **Plugin tampering** | Modified plugin binary executed after manifest verification | Arbitrary code execution |
| **MCP tool shadowing** | Remote MCP server registers a tool named `shell` | Core tool replacement |
| **Env injection** | `LD_PRELOAD` or `NODE_OPTIONS` passed to child processes | Library injection, code execution |
| **Webhook spoofing** | Forged requests to `/webhook/feishu/{id}` or `/webhook/twilio/{id}` | Unauthorized message injection |
| **Admin API abuse** | Unauthorized access to profile management endpoints | Full platform control |

---

## 2. Architecture Layers

### 2.1 Session Isolation

Session isolation operates at three levels: **profile** (OS process), **user** (per-actor `SessionHandle` + directory), and **session** (JSONL file).

#### Profile-level isolation (OS process boundary)

Each profile runs as a separate gateway child process with its own `data_dir` (typically `~/.crew/profiles/{id}/`). Profiles share no in-process state — cross-profile access requires filesystem traversal (mitigated by sandboxing, §2.3).

#### User-level isolation (per-actor SessionHandle)

Within a profile, each user (identified by `channel:chat_id`) gets a dedicated `SessionActor` — a long-lived tokio task with its own inbox (`mpsc` channel). Each `SessionActor` owns a `SessionHandle` wrapped in `Arc<Mutex<SessionHandle>>`, scoped to that user's data only.

**Previous design**: All `SessionActor`s shared a single `Arc<Mutex<SessionManager>>`. This created cross-user contention and a shared-state bottleneck. The current design eliminates all cross-user lock contention — each actor's mutex protects only its own session data.

**Per-user directory layout**:
```
{data_dir}/
  users/
    telegram%3A12345/          # percent-encoded base_key (channel:chat_id)
      sessions/
        default.jsonl          # default session
        research.jsonl         # topic-specific session (#research)
    feishu%3A67890/
      sessions/
        default.jsonl
```

**SessionKey construction** (`crew-core/src/types.rs`): Keys are `{channel}:{chat_id}` (e.g., `telegram:12345`), optionally with topic suffix `#research`. The `base_key` (without topic) determines the user directory. Session filenames are percent-encoded with an FNV-1a hash suffix on truncation to prevent collisions.

**Backward compatibility**: `SessionHandle::open()` tries the new per-user path first, then falls back to the legacy flat path (`{data_dir}/sessions/{encoded_key}.jsonl`). On successful legacy load, the file is auto-migrated to the new path and the old file is removed.

#### Session-level isolation (JSONL file)

Each session is an independent JSONL file with the following protections:

- File size limit: 10 MB per session file (`MAX_SESSION_FILE_SIZE`). Prevents OOM on adversarial files.
- Atomic write-then-rename for crash safety.
- No cross-session file access — `SessionHandle` only reads/writes within its `sessions_dir`.

#### Speculative overflow isolation

When queue mode is `speculative`, overflow tasks run as spawned tokio tasks within the same `SessionActor`, sharing the actor's `Arc<Mutex<SessionHandle>>`. The per-actor mutex serializes writes, preventing concurrent corruption. Overflow tasks do NOT create new actors or access other users' data.

**Limitation**: User isolation is directory-based and actor-scoped, not kernel-enforced. A compromised shell tool in one session can read another user's directory within the same profile unless sandboxing is enabled (§2.3). For kernel-enforced user isolation, see §4 recommendation #6 (per-profile UID isolation).

#### Per-user workspace isolation (tool file access)

Each user gets a dedicated workspace directory for tool execution:

```
{data_dir}/
  users/
    telegram%3A12345/
      sessions/           # session history (JSONL)
      workspace/          # tool cwd — read_file, write_file, shell, etc.
        src/
        output.txt
    telegram%3A67890/
      sessions/
      workspace/
```

**Two-layer enforcement**:

1. **Application-level** (file tools): `resolve_path(base_dir, user_path)` checks that all paths stay within the per-user `workspace/` directory. Tools `read_file`, `write_file`, `edit_file`, `diff_edit`, `glob`, `grep`, `list_dir`, `git` all use this `base_dir`.

2. **Kernel-level** (shell tool on macOS): `sandbox-exec` SBPL restricts `file-write*` to `(subpath "{user_workspace}")`. The macOS kernel enforces this — even if the LLM generates a shell command writing to another user's workspace, the kernel denies it.

**Implementation** (`session_actor.rs`): `ActorFactory::spawn()` creates a per-user workspace path from the session key's `base_key`, then calls `ToolRegistry::rebind_cwd()` which re-registers all cwd-bound tools with the new path while preserving non-cwd tools (web_search, browser, MCP, plugins).

**Remaining limitations of per-user workspace isolation**:

1. **`file-read*` is globally allowed in SBPL** — the macOS sandbox allows reading any file on disk. Restricting reads to only the user workspace would break shell commands that need system binaries (`/usr/bin/*`, `/Library/*`). A tighter SBPL could allowlist system paths: `(allow file-read* (subpath "{user_workspace}")) (allow file-read* (subpath "/usr")) (allow file-read* (subpath "/bin")) ...` but this is fragile across macOS versions.

2. **Network is shared** — all users share the same network stack. Per-user network isolation requires containers (Docker with `--network none` per container) and is not achievable via `sandbox-exec` alone.

3. **Process visibility** — user A's shell can observe user B's processes via `ps`. macOS `sandbox-exec` does not support PID namespace isolation (unlike Linux bubblewrap `--unshare-pid`). Requires containers for process-level isolation.

### 2.2 File I/O Safety

`resolve_path()` in `crew-agent/src/tools/mod.rs`:

1. **Rejects absolute paths** -- user-provided paths must be relative.
2. **Normalizes `..` components** without filesystem access (`normalize_path`).
3. **Verifies containment** -- resolved path must `starts_with(base_dir)`.

Symlink protection uses two layers:
- **`reject_symlink()`**: Async metadata check (used for directory operations like `list_dir`).
- **`read_no_follow()` / `write_no_follow()`**: Atomic rejection via `O_NOFOLLOW` flag on Unix. Opens the file with `libc::O_NOFOLLOW`, which returns `ELOOP` if the path is a symlink. This eliminates the TOCTOU race between check and open.

On non-Unix platforms, a fallback `symlink_metadata()` check is used (TOCTOU window exists but is narrow).

### 2.3 Sandbox Backends

Three sandbox backends in `crew-agent/src/sandbox.rs`, selectable via `SandboxConfig`:

**Bubblewrap (Linux)**:
- Read-only bind mounts for `/usr`, `/lib`, `/bin`, `/sbin`, `/etc`.
- Read-write bind for the working directory only.
- `--tmpfs /tmp`, minimal `/dev`, `/proc`.
- `--unshare-pid`, `--die-with-parent`.
- `--unshare-net` when `allow_network` is false.

**macOS sandbox-exec**:
- SBPL profile: `(deny default)` base policy.
- `(allow file-write*)` scoped to `(subpath "{cwd}")`, `/private/tmp`, `/private/var/folders`. With per-user workspace isolation, `{cwd}` is the user's own workspace directory (`{data_dir}/users/{base_key}/workspace/`), so write access is kernel-restricted to that user's files.
- `(allow file-read*)` globally (read-only access to system). See §2.1 limitation #1 for discussion.
- Path injection prevention: rejects paths containing control chars (`< 0x20`, `0x7F`), parentheses, backslash, and double-quote -- all SBPL metacharacters. Fails closed (error, not unsandboxed execution).

**Docker**:
- Default image: `ubuntu:24.04` (configurable via `docker.image` in sandbox config).
- Per-user bind mount: each user's workspace directory is mounted as `/workspace` in the container. The container **only sees that user's files** — no other user's data or host filesystem is mounted.
- All users share the **same Docker image** (stored once, ~80MB). No per-user image duplication. Storage cost is only the user's workspace files on the host.
- `--security-opt no-new-privileges`, `--cap-drop ALL`.
- Configurable resource limits: `--cpus`, `--memory`, `--pids-limit`.
- `--network none` when `allow_network` is false.
- Mount mode: `none` (no host access), `ro` (read-only), `rw` (read-write).
- Path injection prevention: rejects paths containing `:`, `\0`, `\n`, `\r` (prevents volume mount injection).
- Blocked bind sources: `/var/run/docker.sock`, `/etc`, `/proc`, `/sys`, `/dev` rejected as cwd or extra bind mounts (prevents container escape).

**Sandbox backend comparison**:

| Feature | SBPL (macOS) | Bubblewrap (Linux) | Docker |
|---------|-------------|-------------------|--------|
| Startup overhead | ~6ms | ~10ms | **130-700ms** |
| Per-user isolation | SBPL `subpath` (kernel) | Bind mount (namespace) | Bind mount (container) |
| Write restriction | Yes (`subpath`) | Yes (bind mount) | Yes (only `/workspace` mounted) |
| Read restriction | Configurable (`read_allow_paths`) | Yes (`--ro-bind`) | **Strongest** — host FS not visible |
| Cross-user visibility | Reads allowed (same host FS) | Reads allowed (same host FS) | **No** — other users not mounted |
| Network isolation | Yes (`deny network*`) | Yes (`--unshare-net`) | Yes (`--network none`) |
| PID isolation | No | Yes (`--unshare-pid`) | Yes |
| CPU/memory limits | No | No | Yes (`--cpus`, `--memory`) |
| Process visibility | No | Yes (PID namespace) | Yes |
| Disk quota | No | No | Yes (storage driver) |
| Root required | No | No | No (but Docker daemon) |
| Image storage | N/A | N/A | Shared (one image, all users) |
| Persistent state | N/A (wraps single cmd) | N/A (wraps single cmd) | **No** — `--rm` per command |

**Measured performance** (Apple M-series, Colima/Docker on macOS, 2026-03-11):

| Workload | Bare (no sandbox) | macOS sandbox-exec | Docker (`--rm`) |
|----------|------------------|--------------------|-----------------|
| `echo` + `cat` | 7.0ms | 12.8ms (1.8×) | 134.8ms (19×) |
| Python JSON write | 29.4ms | 63.8ms (2.2×) | 707.0ms (24×) |

Key observations:
- **macOS sandbox-exec adds ~6ms** per command — nearly free. Kernel SBPL enforcement with no process/namespace creation overhead.
- **Docker adds 130-680ms** per command. Each invocation creates a full container (image layer mount → namespace creation → start → run → destroy).
- For an agent doing 5-10 tool calls per turn: sandbox-exec adds **30-60ms** total, Docker adds **0.7-7 seconds**.
- `mode: "auto"` correctly picks sandbox-exec on macOS, avoiding Docker overhead.
- Docker overhead is dominated by container lifecycle, not the actual command — heavier workloads see a lower *relative* overhead ratio.

**Recommendation**: Use `"mode": "auto"` (default) which selects macOS sandbox-exec on macOS and Docker on Linux. Only force `"mode": "docker"` when you need full OS-level isolation (different filesystem namespace, PID isolation, CPU/memory limits).

**Mitigation for Docker overhead** (planned): Adopt persistent container model — one container per session with `docker exec` for subsequent commands, auto-pruned after idle timeout. This amortizes startup cost (~130ms first command, ~5ms subsequent) and preserves filesystem state across commands within a session. See §4.6 for details.

**BLOCKED_ENV_VARS** (18 variables, shared across all backends + MCP + hooks + browser):
```
LD_PRELOAD, LD_LIBRARY_PATH, LD_AUDIT,
DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYLD_FRAMEWORK_PATH,
DYLD_FALLBACK_LIBRARY_PATH, DYLD_VERSIONED_LIBRARY_PATH,
NODE_OPTIONS, PYTHONSTARTUP, PYTHONPATH, PERL5OPT, RUBYOPT, RUBYLIB,
JAVA_TOOL_OPTIONS, BASH_ENV, ENV, ZDOTDIR
```

All backends remove these from the child process environment before execution.

### 2.4 Tool Policy

`ToolPolicy` in `crew-agent/src/tools/policy.rs` provides allow/deny lists with **deny-wins** semantics:

- **Deny list checked first**. If a tool matches any deny entry, it is blocked regardless of allow list.
- **Empty allow list** = allow everything not denied.
- **Named groups**: `group:fs` (read/write/edit/diff_edit), `group:runtime` (shell), `group:web` (web_search/web_fetch/browser), `group:search` (glob/grep/list_dir), `group:sessions` (spawn).
- **Wildcard matching**: `web_*` matches `web_search`, `web_fetch`.
- **Tag-based filtering**: `require_tags` restricts tool visibility by semantic tags (e.g., `code`, `web`, `gateway`). Tools with no tags are universal (pass any filter).
- **Provider-specific policies**: `set_provider_policy()` filters both `specs()` and `execute()` -- an LLM cannot call a tool it cannot see.

**ShellTool SafePolicy**: Denies dangerous commands (`rm -rf /`, `dd`, `mkfs`, fork bombs). Whitespace-normalized before matching. Timeout clamped to `[1, 600]` seconds.

**Tool argument size limit**: 1 MB (`estimate_json_size` -- non-allocating recursive walk of `serde_json::Value`, prevents OOM on deeply nested payloads).

### 2.5 SSRF Protection

`crew-agent/src/tools/ssrf.rs` provides shared SSRF validation for `web_fetch`, `browser`, and MCP HTTP transports.

**Two-phase check** (`check_ssrf`):
1. **Hostname validation** (`is_private_host`): Blocks `localhost`, `localhost.`, and any IP literal that resolves to a private range.
2. **DNS resolution check**: After hostname passes, resolves via `tokio::net::lookup_host` and checks all returned addresses against `is_private_ip`.

**Blocked IP ranges** (`is_private_ip`):
- IPv4: loopback (127/8), private (10/8, 172.16/12, 192.168/16), link-local (169.254/16 -- AWS metadata), unspecified (0.0.0.0).
- IPv6: loopback (::1), unspecified (::), multicast (ff00::/8), ULA (fc00::/7), link-local (fe80::/10), site-local (fec0::/10).
- IPv4-mapped (::ffff:x.x.x.x) and IPv4-compatible (::x.x.x.x) addresses are unwrapped and checked against IPv4 rules.

### 2.6 Plugin Integrity

`crew-agent/src/plugins/loader.rs` handles plugin loading with integrity verification:

1. **Manifest parsing**: `manifest.json` must exist and contain valid JSON with tool definitions.
2. **Executable discovery**: Tries directory name, then `main`. Must be executable (`mode & 0o111 != 0` on Unix).
3. **Size limit**: 100 MB maximum executable size (checked before reading into memory).
4. **SHA-256 verification**: If `sha256` is present in manifest, the executable bytes are hashed and compared. Mismatch = reject.
5. **TOCTOU-safe execution**: After verification, the exact bytes read are written to a `.{name}_verified` sibling file with `0o500` permissions (read+execute, no write). `PluginTool` executes this verified copy, not the original. This prevents swapping the binary after hash verification.
6. **Env sanitization**: `BLOCKED_ENV_VARS` are stripped from the plugin's environment.
7. **Execution timeout**: Configurable per-manifest (`timeout_secs`), default 30 seconds. On timeout, child process is killed (including process group kill on Unix via `kill -9 -{pid}`).

**Warning**: Plugins without `sha256` in manifest are loaded with a warning log. This is intentional for development but should be audited in production.

### 2.7 MCP Security

`crew-agent/src/mcp.rs` secures MCP server integration:

**Schema validation** (`validate_schema`):
- Maximum nesting depth: 10 levels (`MAX_SCHEMA_DEPTH`).
- Maximum serialized size: 64 KB (`MAX_SCHEMA_SIZE`).
- Tools with invalid schemas are rejected at registration.

**Env filtering** (stdio transport):
- `BLOCKED_ENV_VARS` are filtered case-insensitively from the `env` map in `McpServerConfig`.
- Blocked vars are logged as warnings.

**SSRF on HTTP transport**:
- `check_ssrf(url)` is called before connecting to any MCP HTTP server URL.
- Blocks connections to private/internal endpoints via DNS resolution check.

**Tool name protection**:
- `PROTECTED_NAMES` list (18 names) prevents MCP tools from shadowing built-in tools like `shell`, `read_file`, `write_file`, etc.
- Collisions are logged and the MCP tool is skipped.

**Response limits**:
- Stdio: `MAX_LINE_BYTES` = 1 MB per JSON-RPC response line.
- HTTP: 30-second timeout per request.
- MCP tool execution: 30-second timeout per tool call.

### 2.8 Auth Middleware

`crew-cli/src/api/router.rs` implements two-tier authentication:

**Token extraction**: Bearer header (`Authorization: Bearer {token}`) with query parameter fallback (`?token={token}`) for SSE/EventSource.

**Constant-time comparison** (`constant_time_eq`): XOR-based comparison that processes both strings fully regardless of where they differ. Also checks length equality separately to prevent length oracle.

**Two middleware layers**:
- `user_auth_middleware`: Accepts admin token OR authenticated user session. Required for chat, session, and self-service endpoints.
- `admin_auth_middleware`: Accepts admin token OR user with `Admin` role. Required for profile management, user management, system operations.

**Identity resolution** (`resolve_identity`):
1. Check admin token (constant-time).
2. Check user session via `AuthManager::validate_session()`.

**Unauthenticated routes**: Auth endpoints (`/api/auth/*`), webhook proxy (`/webhook/*`), static files.

### 2.9 Hook Security

`crew-agent/src/hooks.rs` runs lifecycle hooks with multiple safety measures:

**Argv execution**: Commands are specified as an argv array (not shell strings). No shell interpretation -- prevents command injection via hook configuration.

**Env sanitization**: `BLOCKED_ENV_VARS` removed from hook process environment.

**Tilde expansion**: `~/` and `~username/` expanded safely to home directories.

**Circuit breaker**: Per-hook consecutive failure counter. After `failure_threshold` (default 3) consecutive failures, the hook is auto-disabled with a one-time warning log. Successful execution resets the counter.

**Timeout**: Configurable per-hook (`timeout_ms`, default 5000ms). On timeout, the child process is killed to prevent orphans.

**Deny semantics**: Before-hooks (before_tool_call, before_llm_call) can deny operations by exiting with code 1. Exit code 0 = allow, exit code >= 2 = error (logged, does not block).

**Payload**: JSON on stdin with event type, tool name, arguments, session context. Tool arguments are included in before_tool_call payloads (allows audit hooks to inspect commands before execution).

### 2.10 Per-Profile CWD Isolation

When `crew serve` spawns a gateway subprocess for each profile, the child process now receives `--cwd {data_dir}` (e.g., `~/.crew/profiles/{id}/data/`) instead of inheriting the parent's home directory. This narrows the default working directory from the entire user home to the profile's own data directory, strengthening several existing defenses.

#### CWD scoping

The gateway `--cwd` flag sets the process working directory before any tool initialization. Since builtin file tools (`read_file`, `write_file`, `edit_file`, `diff_edit`, `glob`, `grep`, `list_dir`, `git`) resolve user-supplied paths via `resolve_path(cwd, user_path)`, setting `cwd = ~/.crew/profiles/{id}/data/` means these tools can only access files within that profile's data directory. Cross-profile file access is blocked because `resolve_path()` verifies the resolved path `starts_with(base_dir)`, and `base_dir` is now the profile's own directory.

#### Shell sandbox read restriction

On macOS, the shell sandbox SBPL profile supports a `read_allow_paths` list. When `crew serve` populates `read_allow_paths` with `project_dir` (the `--crew-home` path, typically `~/.crew/`), the SBPL policy replaces the blanket `(allow file-read*)` with per-path rules:

```scheme
;; Instead of (allow file-read*), generate:
(allow file-read* (subpath "{data_dir}"))        ;; profile's own data
(allow file-read* (subpath "{crew_home}"))        ;; shared skills, configs
(allow file-read* (subpath "/usr"))               ;; system paths
(allow file-read* (subpath "/bin"))
;; ... other system paths
```

This restricts shell command reads at the kernel level to the profile's data, shared crew resources, and system paths. A shell command in profile A cannot `cat` files from profile B's data directory.

#### SendFileTool base_dir validation

`SendFileTool` validates file paths against `data_dir` using `canonicalize()` before sending files via chat channels. The canonical (symlink-resolved, absolute) path must start with `data_dir`. This prevents data exfiltration where an LLM is tricked into sending files from outside the profile's directory via a chat response.

#### Plugin symlink rejection

`is_executable()` in plugin loading now uses `symlink_metadata()` instead of `metadata()` to check the executable bit. `symlink_metadata()` inspects the symlink itself rather than following it to the target. If the plugin executable is a symlink, it is rejected. This is defense-in-depth against an attacker who places a symlink in the plugin directory pointing to an arbitrary binary.

#### project_dir decoupled from cwd

Shared resources -- installed skills (`~/.crew/skills/`), global config (`~/.crew/config.json`), bundled app-skills -- are loaded from `--crew-home` (the `project_dir`), not from `cwd`. This decoupling means narrowing `cwd` to the profile's data directory does not break access to shared pipelines and configurations.

#### Remaining gaps

- **SpawnTool and PipelineTool sub-agents**: These use `with_builtins()` without sandbox configuration. `resolve_path()` still enforces path containment, but the shell tool in sub-agents runs unsandboxed.
- **bwrap `read_allow_paths`**: The Bubblewrap (Linux) sandbox backend does not yet implement `read_allow_paths`. Only macOS SBPL applies read restrictions when `read_allow_paths` is populated.
- **SBPL read restriction is conditional**: The blanket `(allow file-read*)` is only replaced with per-path rules when `read_allow_paths` is non-empty. If the list is not populated (e.g., standalone `crew chat` without `crew serve`), the old permissive behavior remains.

---

## 3. Known Limitations & Mitigations

### 3.1 Global Environment Variables

**Issue**: All gateway child processes inherit the parent process environment. API keys set as env vars (e.g., `KIMI_API_KEY`) are visible to all profiles.

**Mitigation**: Use per-profile `env_vars` map in profile configuration. The gateway process sets only the profile's configured env vars. Avoid exporting shared API keys in the shell environment.

### 3.2 MCP Inherits Parent Env

**Issue**: MCP stdio servers inherit the full parent process environment minus `BLOCKED_ENV_VARS`. This may expose API keys or other secrets to MCP server processes.

**Mitigation**: MCP `env` config allows explicit env var injection. For sensitive deployments, run MCP servers with minimal env using the `env` field rather than relying on inheritance.

### 3.3 No Pre-LLM Secret Redaction

**Issue**: Tool output (e.g., `shell` reading `.env` files, `read_file` on config files) is passed directly to the LLM. There is no filter to redact secrets before they enter the LLM context window.

**Mitigation**: Use tool policies to restrict `read_file` access. Consider adding a `before_llm_call` hook that inspects message content for secret patterns (regex-based).

### 3.4 Unauthenticated Webhooks

**Issue**: `/webhook/feishu/{profile_id}` and `/webhook/twilio/{profile_id}` are unauthenticated by design -- external platforms (Feishu, Twilio) cannot authenticate with Bearer tokens.

**Mitigation**: Rely on platform-specific signature validation within each handler. The profile_id in the URL path provides routing but not access control.

### 3.5 Admin API Without Auth in Dev Mode

**Issue**: When no `auth_token` is configured and no `AuthManager` is present, all API routes (including admin) are accessible without authentication.

**Mitigation**: Always set an auth token in production. The router explicitly checks `has_auth` and only applies middleware when configured.

### 3.6 read_file Loads Full Content

**Issue**: `read_no_follow` reads the entire file into memory before any slicing or offset is applied. A large file (e.g., multi-GB log) can cause OOM.

**Mitigation**: Session files have a 10 MB limit. For general file reads, the tool should implement streaming or size-check-before-read. Currently relies on the LLM not targeting excessively large files.

### 3.7 Sandbox Disabled by Default

**Issue**: `SandboxConfig::default()` has `enabled: false`. Shell commands run without isolation unless explicitly configured.

**Mitigation**: Enable sandboxing in production profile configs. Auto-detection (`SandboxMode::Auto`) selects the best available backend.

---

## 4. Hardening Recommendations

Inspired by LAMP shared hosting's 25-year-old multi-tenant isolation model, which crew-rs is essentially re-solving for AI agents. LAMP's key insight: **the kernel should enforce isolation, not application code**. Application-level checks (`resolve_path()`, tool policy) are defense-in-depth, not the primary boundary.

### 4.1 Enable sandbox by default (short-term)

Change `SandboxConfig::default()` to `enabled: true`. Auto-detection (`SandboxMode::Auto`) selects the best available backend. Every production profile should have kernel-enforced tool isolation with zero configuration.

### 4.2 Per-profile UID isolation (medium-term, highest impact)

**LAMP equivalent**: Each PHP-FPM pool runs as a separate Unix user (`webA`, `webB`). The kernel enforces everything — file permissions, process visibility, signal delivery.

**crew-rs target**: `crew serve` spawns each profile's gateway child process as a dedicated Unix user.

```rust
// In process_manager.rs, when spawning a profile gateway:
pub struct ProfileProcess {
    pub run_as_user: Option<String>,  // e.g., "crew_profile_abc"
}

// Spawn with UID switch:
// Option A: sudo -u crew_profile_abc crew gateway --profile abc
// Option B: setuid() after fork (requires root parent)
// Option C: macOS launchd per-user plist
```

**What this gives us for free (kernel-enforced)**:
- File isolation: `chmod 700 /home/crew_abc/` — other profiles can't read or write
- Process isolation: `kill()` fails across UIDs without root
- Signal isolation: can't `SIGKILL` another profile's processes
- Socket isolation: Unix sockets owned by UID
- `/proc` hiding: `hidepid=2` on Linux hides other UIDs' processes

**Profile user provisioning** (in `crew serve` or admin API):
```bash
# Create profile user (one-time, requires admin)
sudo useradd -r -m -d /home/crew_abc -s /usr/sbin/nologin crew_profile_abc
sudo chown -R crew_profile_abc:crew_profile_abc /home/crew_abc/
sudo chmod 700 /home/crew_abc/

# crew serve spawns:
sudo -u crew_profile_abc crew gateway \
  --data-dir /home/crew_abc/.crew \
  --cwd /home/crew_abc/workspace
```

**Config** (`~/.crew/profiles/{id}.json`):
```json
{
  "isolation": {
    "run_as_user": "crew_profile_abc",
    "data_dir": "/home/crew_abc/.crew",
    "cwd": "/home/crew_abc/workspace"
  }
}
```

**Files to modify**:
- `crates/crew-cli/src/process_manager.rs` — Add `run_as_user` to `Command::new()` via `sudo -u`
- `crates/crew-cli/src/profiles.rs` — Add `isolation` config section
- `crates/crew-cli/src/commands/gateway/mod.rs` — Read isolation config

### 4.3 Read isolation (medium-term)

**LAMP equivalent**: `open_basedir = /home/webA/:/tmp/` — PHP interpreter refuses to open files outside these paths.

**Current gap**: macOS SBPL allows `(file-read*)` globally. `resolve_path()` restricts file tools at the application level, but shell commands can `cat` anything.

**Linux — Landlock** (kernel 5.13+, no root):
```rust
// In ShellTool or sandbox, before exec:
use landlock::{Access, AccessFs, PathBeneath, Ruleset, RulesetAttr};

let ruleset = Ruleset::default()
    .handle_access(AccessFs::from_read(ABI))?
    .create()?;

// Allow reads only to: user workspace + system paths
for path in [&user_workspace, "/usr", "/bin", "/lib", "/etc", "/tmp"] {
    ruleset.add_rule(PathBeneath::new(
        File::open(path)?,
        AccessFs::from_read(ABI),
    ))?;
}

ruleset.restrict_self()?;
// Kernel now denies reads outside these paths
```

**macOS — tighter SBPL**:
```scheme
;; Replace (allow file-read*) with:
(allow file-read* (subpath "{user_workspace}"))
(allow file-read* (subpath "/usr"))
(allow file-read* (subpath "/bin"))
(allow file-read* (subpath "/sbin"))
(allow file-read* (subpath "/Library"))
(allow file-read* (subpath "/System"))
(allow file-read* (subpath "/private/tmp"))
(allow file-read* (subpath "/private/var/folders"))
(allow file-read* (subpath "/Applications"))  ;; for tool binaries
(allow file-read* (subpath "/opt/homebrew"))   ;; for Homebrew
```

**Risk**: Overly restrictive read lists break commands that need unexpected system paths. Needs a configurable allowlist with sensible defaults and an escape hatch (`sandbox.read_allow_paths` in config).

**Files to modify**:
- `crates/crew-agent/src/sandbox.rs` — Add `read_paths: Vec<PathBuf>` to `SandboxConfig`, update SBPL generation and bwrap bind mounts
- New: Landlock backend in `sandbox.rs` (Linux 5.13+ detection via `prctl(PR_GET_NO_NEW_PRIVS)`)

### 4.4 Disk quotas (medium-term)

**LAMP equivalent**: `setquota -u webA 5G 5.5G 0 0 /home` — kernel-enforced storage limit per tenant.

**Current gap**: No disk quota. A malicious shell command (`dd if=/dev/zero of=big bs=1G count=100`) can fill the disk, affecting all profiles.

**With per-profile UID** (§4.2): Use standard Unix quotas.
```bash
# Enable quotas on the filesystem (one-time)
sudo quotaon -u /home

# Set per-profile quota
sudo setquota -u crew_profile_abc \
  5242880 5767168 \  # 5GB soft / 5.5GB hard (in KB)
  0 0 \              # no inode limit
  /home
```

**Without per-profile UID**: Application-level check before file writes.
```rust
// In WriteFileTool and ShellTool, check workspace size before execution
fn check_workspace_quota(workspace: &Path, max_bytes: u64) -> Result<()> {
    let usage = fs_extra::dir::get_size(workspace)?;
    if usage > max_bytes {
        eyre::bail!("workspace quota exceeded: {usage} > {max_bytes} bytes");
    }
    Ok(())
}
```

Application-level quota is bypassable (shell commands write directly), so per-profile UID + kernel quota is the real solution.

**Config** (`config.json`):
```json
{
  "isolation": {
    "workspace_quota_mb": 5120
  }
}
```

### 4.5 Dangerous bind mount blocking (short-term)

**Learned from OpenClaw**: Block dangerous Docker bind mount sources that could lead to container escape or host compromise.

**Blocked sources** (in `DockerSandbox`):
- `/var/run/docker.sock` — container escape via Docker API
- `/etc` — host configuration access
- `/proc` — kernel interface, process info leakage
- `/sys` — kernel parameters, device access
- `/dev` — raw device access

```rust
const BLOCKED_DOCKER_BIND_SOURCES: &[&str] = &[
    "/var/run/docker.sock",
    "docker.sock",
    "/etc",
    "/proc",
    "/sys",
    "/dev",
];

fn validate_bind_mount(source: &str) -> Result<()> {
    for blocked in BLOCKED_DOCKER_BIND_SOURCES {
        if source == *blocked || source.starts_with(&format!("{blocked}/")) {
            eyre::bail!("dangerous bind mount source blocked: {source}");
        }
    }
    Ok(())
}
```

**Files to modify**:
- `crates/crew-agent/src/sandbox.rs` — Add validation in `DockerSandbox::wrap_command()`

### 4.6 Persistent Docker containers (medium-term)

**Current**: One container per shell command (`docker run --rm`). 200-500ms startup overhead per command, filesystem state lost between commands.

**Target**: One container per session, `docker exec` for subsequent commands.

```rust
pub struct DockerSessionContainer {
    container_id: String,
    session_key: String,
    created_at: Instant,
    last_used: Instant,
}

impl DockerSessionContainer {
    /// First command: docker run -d (detached, not --rm)
    async fn create(workspace: &Path, config: &DockerConfig) -> Result<Self> { ... }

    /// Subsequent commands: docker exec (no container startup)
    async fn exec(&self, cmd: &str) -> Result<Output> { ... }

    /// Cleanup: docker rm -f
    async fn destroy(&self) -> Result<()> { ... }
}

/// Prune idle containers (background task)
async fn prune_idle_containers(max_idle: Duration, max_age: Duration) { ... }
```

**Performance**: First command ~300ms (container creation), subsequent ~5ms (`docker exec`). Filesystem state (installed packages, build artifacts) persists across commands within a session.

**Files to modify**:
- `crates/crew-agent/src/sandbox.rs` — Add `DockerSessionSandbox` alongside existing `DockerSandbox`
- `crates/crew-cli/src/session_actor.rs` — Tie container lifecycle to `SessionActor` lifetime

### 4.7 Profile containers with network isolation (long-term)

Currently each profile runs as a native OS process on the host. The next evolution is running each profile inside its own Docker container, giving full kernel isolation (filesystem, PID, network, resource limits) between profiles.

**Current model** (shell sandbox only):
```
Profile "sales" (host process, PID 1001, uid=yuechen)
└── shell("curl api.moonshot.ai") → docker run --rm alpine sh -c "curl ..."
    ↑ only shell commands are containerized
    ↑ profile process itself runs on host with full access
```

**Target model** (profile container):
```
crew serve (host)
├── docker run -d --name crew-sales \
│     --network crew-internal \
│     -v /data/sales:/data \
│     --cpus 2 --memory 1g \
│     crew-rs gateway --profile sales
│
├── docker run -d --name crew-support \
│     --network crew-internal \
│     -v /data/support:/data \
│     --cpus 1 --memory 512m \
│     crew-rs gateway --profile support
```

#### The network problem

Profile containers need selective network access:
- **Must reach**: LLM APIs (api.moonshot.ai), channel APIs (api.telegram.org), control plane (crew serve)
- **Must NOT reach**: cloud metadata (169.254.169.254), local network (192.168.0.0/16), other profile containers

`--network none` blocks everything (broken). `--network bridge` allows everything (no isolation). Neither works.

#### Solution: host proxy with domain allowlist

Run a domain-allowlist HTTP proxy on the host. Profile containers route all traffic through it.

```
crew serve (host, port 3000)
├── allowlist proxy (host, port 8888)
│   ├── sales profile: allow api.moonshot.ai, api.telegram.org
│   ├── support profile: allow api.deepseek.com, api.telegram.org
│   └── deny all: 169.254.0.0/16, 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
│
├── container "crew-sales" (--network=crew-internal)
│   └── HTTPS_PROXY=http://host.docker.internal:8888
│       → proxy allows api.moonshot.ai ✅
│       → proxy blocks 169.254.169.254 ❌
│       → proxy blocks crew-support container ❌
│
├── container "crew-support" (--network=crew-internal)
│   └── HTTPS_PROXY=http://host.docker.internal:8888
│       → proxy allows api.deepseek.com ✅
│       → proxy blocks crew-sales container ❌
```

**Why proxy over iptables**: DNS names resolve to multiple IPs that change. iptables requires static IPs. A domain-allowlist proxy checks the hostname in the HTTP CONNECT request — works regardless of IP changes.

**Proxy options**:
- **Squid** — mature, widely deployed, ACL-based domain filtering
- **Custom Rust proxy** — minimal, embedded in `crew serve`, per-profile config
- **Envoy sidecar** — strongest isolation (one proxy per container), but most complex

**Recommended**: Custom Rust proxy embedded in `crew serve`. Reads `allowed_domains` from each profile config. Single process, no external dependencies.

```rust
// In crew serve, spawn a lightweight HTTPS CONNECT proxy
pub struct AllowlistProxy {
    /// Per-profile domain allowlists, keyed by source IP or auth token.
    rules: HashMap<String, Vec<String>>,
    /// Always-blocked: private IPs (reuse ssrf.rs logic)
    ssrf_checker: SsrfChecker,
}

impl AllowlistProxy {
    async fn handle_connect(&self, profile_id: &str, target_host: &str) -> Result<()> {
        // 1. Check SSRF (private IPs) — always blocked
        self.ssrf_checker.check(target_host)?;
        // 2. Check profile allowlist
        let allowed = self.rules.get(profile_id)
            .map(|domains| domains.iter().any(|d| target_host.ends_with(d)))
            .unwrap_or(false);
        if !allowed {
            eyre::bail!("domain {target_host} not in allowlist for profile {profile_id}");
        }
        Ok(())
    }
}
```

**Config** (`~/.crew/profiles/{id}.json`):
```json
{
  "isolation": {
    "run_in_container": true,
    "image": "crew-rs:latest",
    "cpus": "2",
    "memory": "1g",
    "allowed_domains": [
      "api.moonshot.ai",
      "api.telegram.org",
      "api.openai.com"
    ]
  }
}
```

**What this gives us**:

| Aspect | Current (host process) | Profile container + proxy |
|--------|----------------------|--------------------------|
| Filesystem | Per-user workspace (SBPL writes) | Full container isolation |
| Network | Shared, SSRF check in app | Per-profile domain allowlist, kernel-enforced |
| CPU/memory | Unlimited | Docker `--cpus`, `--memory` |
| Process visibility | Shared on macOS | Full PID namespace isolation |
| Cross-profile access | Filesystem traversal possible | **Impossible** (separate containers) |

**SSRF protection upgrade**: With the proxy model, SSRF protection moves from application-level (`ssrf.rs` checking in every tool) to infrastructure-level (proxy rejects private IPs for all traffic). Defense in depth — `ssrf.rs` stays as a second check, but the proxy is the primary gate.

**Files to modify**:
- `crates/crew-cli/src/process_manager.rs` — Spawn profiles as Docker containers
- `crates/crew-cli/src/profiles.rs` — Add container isolation config
- New: `crates/crew-cli/src/proxy.rs` — Domain-allowlist HTTPS CONNECT proxy (~200 LOC)
- `crates/crew-cli/src/commands/serve.rs` — Start proxy alongside control plane

### 4.8 Additional hardening

**Webhook signature validation** (short-term, all profiles):
- Implement Twilio request signature verification (`X-Twilio-Signature`)
- Feishu event verification token checking in webhook proxy handlers

**Secret redaction filter** (short-term):
- Configurable regex-based filter scanning tool output for API keys, tokens, passwords before appending to LLM context
- Token prefixes: `sk-`, `ghp_`, `gho_`, `xox[bpas]-`, `AIza`, `AKIA`, `SG.`, `glpat-`
- PEM blocks: `-----BEGIN.*PRIVATE KEY-----`

**MCP env isolation** (short-term):
- Change MCP stdio spawning to `env_clear()` then explicitly set only the configured `env` map (plus `PATH`, `HOME`, `LANG`)

**Rate limiting** (medium-term):
- Per-session and per-profile rate limits on tool invocations (especially `shell`, `web_fetch`, `spawn`)
- Sliding window counter with configurable burst and sustained rates

**Audit logging** (medium-term):
- Structured audit log: session ID, profile ID, tool name, arguments hash, result status, timestamp
- Ship to external log aggregator for compliance and forensics

**Plugin signature requirement** (medium-term):
- Reject plugins without `sha256` in production mode
- Require `--allow-unsigned-plugins` flag to override

**Landlock/seccomp** (long-term):
- On Linux 5.13+, apply Landlock LSM for file access restriction (per-user read/write paths)
- Seccomp-bpf for syscall filtering (block ptrace, mount, setuid)
- See §4.3 for Landlock implementation details

**Encrypted session storage** (long-term):
- Encrypt session JSONL files at rest with per-profile keys
- Protects against disk-level data leakage (stolen disk, backup exposure)

**mTLS for MCP HTTP** (long-term):
- Require mutual TLS for MCP HTTP transports to prevent MITM attacks on tool execution

---

## 5. LAMP Stack Comparison

crew-rs is solving the same multi-tenant isolation problem that PHP shared hosting solved 25+ years ago. This comparison identifies gaps and guides the hardening roadmap.

### Isolation model comparison

```
LAMP shared hosting (1990s):          crew-rs (2026):

Apache/nginx (root)                    crew serve (control plane)
├── PHP-FPM pool for tenant A          ├── Profile A (child process)
│   ├── runs as Unix user "webA"       │   ├── runs as SAME user (gap)
│   ├── chroot /home/webA/             │   ├── per-user workspace dir
│   ├── open_basedir restricted        │   ├── resolve_path() check
│   ├── MySQL user "dbA"               │   ├── episodes.redb (file-level)
│   └── disk quota: 5GB               │   └── no disk quota (gap)
├── PHP-FPM pool for tenant B          ├── Profile B (child process)
│   ├── runs as Unix user "webB"       │   ├── same user (gap)
│   ...                                │   ...
```

### Gap analysis

| LAMP Feature | Enforcement | crew-rs Equivalent | Enforcement | Gap | Planned Fix |
|-------------|-------------|-------------------|-------------|-----|-------------|
| Per-tenant Unix UID | Kernel (DAC) | Per-profile OS process | Process boundary only | **No UID isolation** | §4.2 per-profile UID |
| `chroot` / bind mount | Kernel | Per-user workspace | Application + SBPL writes | **Reads not restricted** | §4.3 read isolation (Landlock/SBPL) |
| `open_basedir` | PHP runtime | `resolve_path()` | Application code | Bug = bypass | §4.3 + Landlock |
| `disable_functions` | PHP runtime | `ToolPolicy` deny list | Application code | Bug = bypass | Defense in depth |
| Disk quota (`setquota`) | Kernel | None | — | **No quota** | §4.4 with per-profile UID |
| `/proc` hiding (`hidepid=2`) | Kernel | None (macOS N/A) | — | **Processes visible** | §4.7 profile containers |
| MySQL per-user grants | MySQL ACL | Per-profile redb file | Filesystem | Adequate | — |
| Network ACL (iptables) | Kernel | SSRF check in app | Application code | **No per-profile network** | §4.7 proxy allowlist |
| Bandwidth metering | Kernel/iptables | None | — | **No metering** | §4.7 proxy can meter |

### What crew-rs already does better than LAMP

| Area | crew-rs Advantage |
|------|-------------------|
| **SSRF protection** | DNS-resolved private IP blocking — PHP has no built-in SSRF guard |
| **Symlink safety** | `O_NOFOLLOW` atomic rejection — PHP historically vulnerable to symlink races |
| **Env sanitization** | 18-var blocklist — PHP-FPM relies on pool config |
| **Command filtering** | SafePolicy blocks `rm -rf /`, fork bombs — PHP's `disable_functions` is all-or-nothing |
| **Tool argument limits** | 1MB non-allocating size check — no PHP equivalent |
| **Per-user write isolation** | SBPL kernel-enforced per workspace — PHP relies on UID (stronger) but has no per-request sandboxing |
