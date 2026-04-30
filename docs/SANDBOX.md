# Sandbox

octos can isolate shell commands inside a sandbox, preventing the AI agent from modifying the host system outside the project workspace. Only the `shell` tool is sandboxed; file tools (`read_file`, `write_file`, `edit_file`) use their own path validation (`O_NOFOLLOW`, traversal checks).

## Quick Start

Add to `.octos/config.json` (or `~/.config/octos/config.json`):

```json
{
  "sandbox": {
    "enabled": true
  }
}
```

This auto-detects the best available backend for your platform.

## Backends

### Auto-Detection Order

When `mode` is `"auto"` (default), octos probes in order, picking the first match for the host OS:

1. **bwrap** on Linux (checked via `which bwrap`)
2. **sandbox-exec** on macOS (checked via `which sandbox-exec`)
3. **AppContainer** on Windows (uses the `octos-sandbox` helper binary built on `rappct`)
4. **docker** on any platform as a fallback (checked via `which docker`)
5. **none** — pass-through if nothing is found

Sandbox source lives in `crates/octos-agent/src/sandbox/` (`mod.rs`, `bwrap.rs`, `macos.rs`, `docker.rs`, `windows.rs`). The Windows helper binary is the standalone `crates/octos-sandbox/` crate.

### Bwrap (Linux)

[Bubblewrap](https://github.com/containers/bubblewrap) uses Linux namespaces for lightweight isolation.

**Install:**

```bash
# Ubuntu / Debian
sudo apt install bubblewrap

# Fedora
sudo dnf install bubblewrap

# Arch
sudo pacman -S bubblewrap
```

**What it does:**

| Resource | Policy |
|----------|--------|
| `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`, `/etc` | Read-only bind mount |
| Working directory (`cwd`) | Read-write bind mount |
| `/tmp` | tmpfs (scratch space) |
| `/dev` | Minimal device nodes |
| `/proc` | Mounted |
| PID namespace | Isolated (`--unshare-pid`) |
| Network | Blocked by default (`--unshare-net`) |
| Parent process | `--die-with-parent` (killed if octos exits) |

**Limitations:**

- `/home` is not mounted — user config files (`.gitconfig`, `.cargo/config.toml`, `.npmrc`) are unavailable inside the sandbox. If your tools depend on these, consider Docker or adding custom bind mounts.
- Requires unprivileged user namespaces (enabled by default on most distros; check `sysctl kernel.unprivileged_userns_clone`).

### macOS (sandbox-exec)

Uses Apple's Seatbelt sandbox framework with a [SBPL](https://reverse.put.as/wp-content/uploads/2011/09/Apple-Sandbox-Guide-v1.0.pdf) profile.

**No installation needed** — `sandbox-exec` ships with macOS.

**SBPL profile:**

```scheme
(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow sysctl-read)
(allow file-read*)
(allow file-write* (subpath "<cwd>"))
(allow file-write* (subpath "/private/tmp"))
(allow file-write* (subpath "/private/var/folders"))
; (allow network*) or (deny network*) based on config
```

| Resource | Policy |
|----------|--------|
| File reads | Allowed globally |
| File writes | Restricted to `cwd`, `/private/tmp`, `/private/var/folders` |
| Network | Blocked by default |
| Process execution | Allowed (fork + exec) |

**Limitations:**

- File reads are unrestricted — the sandboxed process can read any file accessible to the user (SSH keys, dotfiles, etc.). Only writes are confined.
- `sandbox-exec` is deprecated by Apple but still functional as of macOS 15 (Sequoia).
- Paths containing SBPL metacharacters (`(`, `)`, `\`, `"`) or control characters are rejected to prevent profile injection. The command fails closed with an error instead of running unsandboxed.

### Windows AppContainer

Uses Windows AppContainer isolation via the `octos-sandbox` helper binary (built on the [`rappct`](https://crates.io/crates/rappct) crate).

**No installation needed** — the helper ships with the Octos binary on Windows.

**What it does:**

| Resource | Policy |
|----------|--------|
| Process token | Restricted token + per-process AppContainer SID |
| File system | Workdir granted via per-AppContainer ACL; everything else denied by default |
| Integrity level | Low |
| Network | Per-capability — denied by default unless `allow_network: true` |
| Capabilities | None of the standard Windows capabilities granted (no `lpacAppExperience`, etc.) |

**Limitations:**

- AppContainer is per-process, so very short-lived commands pay the per-launch isolation setup cost.
- Some legacy Windows tools (those that try to talk to the parent console directly) may fail under low IL.
- Paths containing control characters, NUL, or drive-escape sequences are rejected.

### Docker

Full container isolation using Docker.

**Requires:** Docker installed and the daemon running.

**Security hardening applied:**

- `--cap-drop ALL` — no Linux capabilities
- `--security-opt no-new-privileges` — prevents privilege escalation
- `--rm` — container auto-removed after execution
- `--network none` — no network access (unless `allow_network: true`)

| Resource | Policy |
|----------|--------|
| Workspace | Mounted per `mount_mode` (rw/ro/none) |
| Network | Blocked by default (`--network none`) |
| CPU | Capped via `--cpus` (optional) |
| Memory | Capped via `--memory` (optional) |
| Processes | Capped via `--pids-limit` (optional) |
| Capabilities | All dropped |

**Limitations:**

- Higher latency per command (~200-500ms overhead for container creation).
- Paths containing `:`, null bytes, or newlines are rejected to prevent Docker volume mount injection.

## Configuration Reference

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "auto",
    "allow_network": false,
    "docker": {
      "image": "alpine:3.21",
      "cpu_limit": "1.0",
      "memory_limit": "512m",
      "pids_limit": 100,
      "mount_mode": "rw"
    }
  }
}
```

### Top-Level Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch. When `false`, commands run without isolation. |
| `mode` | string | `"auto"` | Backend selection. One of: `auto`, `bwrap`, `macos`, `docker`, `appcontainer`, `none`. |
| `allow_network` | bool | `false` | Allow network access inside the sandbox. |
| `read_allow_paths` | array<string> | (none) | macOS only: tighten read access to just these paths. When unset, file-read* is allowed (matches legacy behaviour). |
| `docker` | object | (see below) | Docker-specific settings. Ignored for other backends. |

### Docker Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image` | string | `"alpine:3.21"` | Docker image for the sandbox container. |
| `cpu_limit` | string | (none) | CPU quota (e.g. `"1.0"` = 1 core, `"0.5"` = half core). Maps to `--cpus`. |
| `memory_limit` | string | (none) | Memory limit (e.g. `"512m"`, `"1g"`). Maps to `--memory`. |
| `pids_limit` | int | (none) | Max number of processes. Maps to `--pids-limit`. |
| `mount_mode` | string | `"rw"` | Workspace mount: `"rw"` (read-write), `"ro"` (read-only), `"none"` (no mount). |

## Configuration Examples

### Minimal — Auto-Detect

```json
{
  "sandbox": {
    "enabled": true
  }
}
```

### Allow Network Access

Needed if the agent must run `curl`, `git clone`, `pip install`, etc.:

```json
{
  "sandbox": {
    "enabled": true,
    "allow_network": true
  }
}
```

### Docker with Resource Limits

Strict isolation for untrusted workloads:

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "docker",
    "allow_network": false,
    "docker": {
      "image": "ubuntu:24.04",
      "cpu_limit": "1.0",
      "memory_limit": "512m",
      "pids_limit": 50,
      "mount_mode": "ro"
    }
  }
}
```

### Docker Read-Only Workspace

Allow the agent to read project files but not modify them:

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "docker",
    "docker": {
      "mount_mode": "ro"
    }
  }
}
```

### Docker No Workspace Mount

Fully isolated — the agent cannot access project files at all:

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "docker",
    "docker": {
      "mount_mode": "none"
    }
  }
}
```

The working directory inside the container defaults to `/tmp` in this mode.

### Force a Specific Backend

```json
{
  "sandbox": {
    "enabled": true,
    "mode": "bwrap"
  }
}
```

## Environment Sanitization

All sandbox backends automatically clear 18 environment variables that are code injection vectors:

| Category | Variables |
|----------|-----------|
| Linux shared library injection | `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT` |
| macOS dylib injection | `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`, `DYLD_FRAMEWORK_PATH`, `DYLD_FALLBACK_LIBRARY_PATH`, `DYLD_VERSIONED_LIBRARY_PATH` |
| Runtime code injection | `NODE_OPTIONS`, `PYTHONSTARTUP`, `PYTHONPATH`, `PERL5OPT`, `RUBYOPT`, `RUBYLIB`, `JAVA_TOOL_OPTIONS` |
| Shell startup injection | `BASH_ENV`, `ENV`, `ZDOTDIR` |

This blocklist is shared across sandbox backends, MCP server spawning, and hooks execution.

## Path Injection Prevention

Each backend validates the working directory path before constructing the sandbox command:

| Backend | Rejected Characters | Reason |
|---------|-------------------|--------|
| macOS | Control chars (`< 0x20`), DEL (`0x7F`), `(`, `)`, `\`, `"` | Prevents SBPL profile injection |
| Docker | `:`, `\0`, `\n`, `\r` | Prevents volume mount injection (`-v host:container`) |
| Bwrap | (none currently) | Arguments are passed directly to bwrap, not interpreted by a shell |

When a path is rejected, the sandbox returns an error command (`exit 1`) instead of falling back to unsandboxed execution. This is a **fail-closed** design.

## How It Works Internally

```
config.json
    │
    ▼
create_sandbox(&config.sandbox)  ──→  Box<dyn Sandbox>
    │                                      │
    │  (at startup)                        │  (per shell command)
    ▼                                      ▼
ToolRegistry::with_builtins_and_sandbox    ShellTool.execute()
                                               │
                                               ▼
                                     sandbox.wrap_command(cmd, cwd)
                                               │
                                               ▼
                                     tokio::process::Command
                                     (bwrap | sandbox-exec | docker | sh)
```

1. **Config loading** — `SandboxConfig` is deserialized from `config.json`
2. **Backend creation** — `create_sandbox()` returns a `Box<dyn Sandbox>` (trait object)
3. **Tool injection** — The sandbox is passed to `ToolRegistry::with_builtins_and_sandbox()`, which gives it to `ShellTool`
4. **Command wrapping** — On each `shell` tool call, `ShellTool` calls `sandbox.wrap_command(cmd, cwd)`, which returns a platform-specific `tokio::process::Command`
5. **Execution** — The wrapped command is spawned as a child process with the appropriate isolation

## What Is and Is Not Sandboxed

| Component | Sandboxed? | Protection Mechanism |
|-----------|-----------|---------------------|
| `shell` tool | Yes | Sandbox backend (bwrap/macOS/Docker) |
| `read_file` | No | `O_NOFOLLOW` (symlink-safe), path traversal rejection |
| `write_file` | No | `O_NOFOLLOW`, path traversal rejection, base directory check |
| `edit_file` | No | Same as `write_file` |
| `glob` / `grep` | No | Path traversal rejection |
| `web_fetch` / `web_search` | No | SSRF protection (private IP/host blocking) |
| `browser` | No | `BLOCKED_ENV_VARS` cleared from Chrome process |
| MCP servers | No | `BLOCKED_ENV_VARS` cleared, schema validation (max depth 10, max size 64KB) |

File tools rely on Rust-level protections rather than OS-level sandboxing:

- **`O_NOFOLLOW`** on Unix prevents symlink-following, eliminating TOCTOU races
- **`resolve_path`** normalizes paths and rejects anything outside the working directory
- **`reject_symlink`** checks for symlinks in directory operations

## Verifying Sandbox Setup

```bash
# Check if sandbox binaries are available
which bwrap           # Linux
which sandbox-exec    # macOS (should always exist)
which docker          # Any platform

# Check octos status — currently shows provider/config info
octos status

# Test with verbose logging
RUST_LOG=octos_agent=debug octos chat --message "Run: echo hello"
```

In debug logs, you'll see either:
- `sandbox disabled, shell commands run without isolation`
- The wrapped command being executed (bwrap/sandbox-exec/docker)

## Security Considerations

1. **Sandbox scope is shell-only.** File tools bypass the sandbox entirely. If you need full isolation, also use tool policies to restrict file tools:

   ```json
   {
     "sandbox": { "enabled": true },
     "tools": { "deny": ["write_file", "edit_file"] }
   }
   ```

2. **macOS sandbox allows all file reads.** The sandboxed process can read SSH keys, credentials, and other sensitive files. For stronger read isolation, use Docker.

3. **Network access is off by default.** Enable only if the agent needs to run commands that access the network (package managers, git, curl).

4. **Docker image trust.** The default image is `alpine:3.21`. If you override it, ensure the image is from a trusted source.

5. **`allow_network: true` opens all network access.** There is no fine-grained network filtering (e.g., allow only specific hosts). Use Docker's `--network` options outside of octos if you need more control.
