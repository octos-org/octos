# Plugin Sandbox & Tool Policy Design

> Two-layer security for crew-rs plugins: tool-level permission policies
> (which tools can be called) and runtime sandboxing (what a tool can do).

## Status

- **Current**: Subprocess isolation (Phase 1 of Plugin SDK)
- **Priority**: Tool permission policies (orchestration layer)
- **Future**: WebAssembly sandbox for untrusted/third-party plugins
- **Evaluated & rejected**: Theseus OS cell model

---

## 1. Why Sandbox Plugins?

crew-rs plugins are currently arbitrary binaries invoked via subprocess. This works
for trusted first-party plugins but provides no protection against:

- Memory exhaustion (plugin allocates unbounded memory)
- CPU exhaustion (infinite loop or runaway computation)
- Filesystem access (plugin reads/writes arbitrary files)
- Network access (plugin phones home, exfiltrates data)
- Process escape (plugin spawns child processes, signals, etc.)

A sandbox restricts what plugin code can do at runtime, independent of what
language it's written in.

**But sandboxing is only half the problem.** The more immediate need is
controlling which tools an agent can call in the first place.

---

## 2. Tool Permission Policies (Priority)

This is the **orchestration-layer** concern: before a tool call reaches any
runtime (subprocess or Wasm), the system decides whether this agent/profile is
allowed to call it.

### The Two-Layer Model

```
┌─────────────────────────────────────────────────────┐
│  LLM requests tool call: "github_cli"               │
│                                                     │
│  Layer 1: Tool Permission Policy                    │
│  ┌───────────────────────────────────────────────┐  │
│  │  Profile "customer-bot" → policy check        │  │
│  │  github_cli matches deny["admin_*","github_*"]│  │
│  │  → BLOCKED (never reaches runtime)            │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  Layer 2: Runtime Sandbox (only if Layer 1 allows)  │
│  ┌───────────────────────────────────────────────┐  │
│  │  Tool runs in subprocess or Wasm              │  │
│  │  Memory cap, filesystem restrictions, timeout │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

Layer 1 is cheaper, simpler, and solves the real problem: preventing an agent
from calling tools it shouldn't have access to. Layer 2 (Wasm) protects against
what happens inside an allowed tool — important for untrusted code, but a
separate concern.

### Prior Art

| Framework | Layer 1 Approach |
|-----------|-----------------|
| **Claude Code** | `allow/ask/deny` rules per tool pattern. Deny wins. Hierarchical: enterprise > user > project. |
| **OpenAI Agents SDK** | `allowed_tools` list per request + input/output guardrails that can skip/replace/tripwire. |
| **Wassette** (Microsoft) | Deny-by-default Wasm capabilities + per-domain network approval prompts. |
| **SupraWall** | Security shim between agent and tools. Per-agent permissions, cost budgets, audit trail. |
| **Cerbos + MCP** | Policy-based access control for MCP tool calls. Identity-aware (who is the agent acting for). |

Major agent frameworks (LangChain, CrewAI, AutoGen) have **no built-in per-tool
authorization**. This is widely recognized as a gap.

### Proposed Design for crew-rs

Tool policies are defined per-profile in the profile config:

```json
{
  "id": "customer-bot",
  "model": "gpt-4o",
  "tool_policy": {
    "default": "allow",
    "rules": [
      { "pattern": "admin_*",    "action": "deny" },
      { "pattern": "github_cli", "action": "deny" },
      { "pattern": "send_file",  "action": "ask"  },
      { "pattern": "*",          "action": "allow" }
    ]
  }
}
```

**Semantics:**

- `"allow"` — tool call proceeds normally
- `"deny"` — tool call is blocked, LLM receives an error message
- `"ask"` — (for interactive channels) prompt user for approval before executing
- Patterns support glob: `admin_*`, `mofa_*`, exact names, or `*` (wildcard)
- Rules evaluated top-to-bottom, first match wins
- `"default"` applies when no rule matches

**Evaluation order (deny wins across layers):**

1. Enterprise/managed policy (highest priority, cannot be overridden)
2. Profile-level `tool_policy`
3. Skill-level declarations (a skill can restrict its own tools)
4. Default: allow (backward compatible)

**Implementation location:** `crew-agent` tool dispatch, before calling
`ToolRunner::run()`. This is a ~50-line check with no new dependencies.

### What This Enables

- **Customer-facing bots**: Only expose safe, read-only tools
- **Admin bots**: Full tool access, restricted to admin channel
- **Delegated agents**: Spawned sub-agents inherit parent's policy (or stricter)
- **Audit trail**: Log every blocked/allowed tool call for compliance

---

## 3. Theseus OS — Evaluated & Rejected

[Theseus](https://github.com/theseus-os/Theseus) is a research OS exploring
**intralingual design**: using Rust's type system instead of hardware MMU for
isolation. Its "cell" model loads individual Rust crates at runtime with fault
recovery via crate reloading.

### Why it looked promising

- Cell-granularity module loading with live hot-swap
- Fault recovery: reload corrupted crate without reboot
- No MMU overhead — type-system isolation is lightweight
- Written entirely in Rust

### Why it doesn't work as a plugin sandbox

| Problem | Detail |
|---------|--------|
| **Compile-time isolation only** | Safety depends on Rust's borrow checker. A plugin written in Python, Go, or C can't be constrained by Rust's type system. Untrusted code in a single address space can read/write any memory. |
| **Bare-metal only** | Theseus is a full OS kernel. Running it in a VM (QEMU) for sandboxing is heavier than a subprocess. Porting its module loader to userspace would be a massive effort and lose the fault recovery (which requires controlling the entire execution environment). |
| **Rust-only plugins** | The cell system loads Rust crates compiled as ELF objects with a specific ABI. It cannot sandbox arbitrary binaries. |
| **Toolchain coupling** | Requires exact nightly match (currently 2023-10-27). Every plugin would need the same nightly and target triple (`x86_64-unknown-theseus`). |
| **No resource limits** | No cgroups, memory quotas, CPU time limits, or syscall filtering. A plugin could spin forever or allocate all memory. |

### What to learn from Theseus

- **MappedPages guard type**: Tying memory access to ownership of a guard object
  is a pattern worth adopting for plugin-allocated shared memory.
- **Fault recovery via reload**: The concept of "if a module crashes, reload a
  fresh copy" maps well to Wasm module re-instantiation.
- **Cell-granularity hot-swap**: Wasm component hot-swap achieves the same goal
  with language-agnostic safety.

---

## 4. Sandbox Approaches Compared

| Approach | Isolation | Startup | Language-agnostic | Resource limits | Effort |
|----------|-----------|---------|-------------------|-----------------|--------|
| **Subprocess** (current) | OS process boundary | ~2ms | Yes | OS-level (ulimit) | Done |
| **Wasm (Wasmtime)** | Memory-safe sandbox | ~0.5ms | Yes (compile to wasm) | Fuel + memory caps | Medium |
| **Wasm (WasmEdge)** | Memory-safe sandbox | ~0.5ms | Yes | Gas + memory caps | Medium-High |
| **Landlock/seccomp** | Syscall filtering on subprocess | ~0 overhead | Yes | Partial | Low |
| **gVisor/Firecracker** | microVM | ~125ms | Yes | Full | High |
| **eBPF** | Verified bytecode | ~0 overhead | No (C/Rust→BPF) | Verifier-enforced | High |
| **Theseus cells** | Rust type system | ~1ms | No (Rust only) | None | Very High |

---

## 5. Agent Projects Using Wasm Sandboxing

Several production projects already combine AI agents with Wasm sandboxing:

| Project | Runtime | What it does |
|---------|---------|-------------|
| **[Wassette](https://github.com/microsoft/wassette)** (Microsoft) | Wasmtime | MCP server where each tool is a Wasm component. Deny-by-default capabilities. Works with Claude Code, Copilot, Cursor, Gemini CLI. |
| **[wasmcp](https://github.com/wasmcp/wasmcp)** | Wasmtime/Spin | Polyglot MCP framework. Compose tools (Python, TS, Rust) as isolated Wasm components in one process. Inter-component isolation (no shared memory). |
| **[Amla Sandbox](https://github.com/amlalabs/amla-sandbox)** | Wasm | Capability-based agent sandbox. QuickJS inside Wasm. Agents only get tools you explicitly provide. Deterministic replay. |
| **[Hyper-MCP](https://github.com/tuananh/hyper-mcp)** | Wasm | Fast MCP server with Wasm plugin extensions. |
| **[Extism](https://extism.org)** | Wasmtime | General Wasm plugin framework with explicit AI/LLM integration guides. Host grants capabilities to each plugin. |

Wassette is the closest to what crew-rs needs: a Rust + Wasmtime host that runs
tools as sandboxed Wasm components with deny-by-default capabilities and
per-domain network approval.

---

## 6. Wasmtime vs WasmEdge

Both are production-grade Wasm runtimes. Here's why **Wasmtime** is the better
fit for crew-rs.

### Head-to-head

| Dimension | Wasmtime | WasmEdge |
|-----------|----------|----------|
| **Maintainer** | Bytecode Alliance (Mozilla, Fastly, Intel, Red Hat) | CNCF Sandbox (Second State) |
| **Core language** | Rust | C++ |
| **Rust embedding** | Pure Rust crate (`cargo add wasmtime`) | FFI bindings over C++ library (requires system install) |
| **Async support** | Native tokio integration (`call_async`, `ResourceLimiterAsync`) | Own async mechanism, not tokio-native |
| **Component Model** | Production-ready, reference implementation | Partial, catching up |
| **WASI Preview 2** | Full, production-ready | Supported, following Wasmtime's lead |
| **Execution metering** | Fuel system (per-instruction) | Gas metering (CLI flag) |
| **Memory limits** | `ResourceLimiter` trait (per-store) | `SetMaxMemoryPage()` API |
| **LTS releases** | Yes, every 12th version, 2-year security support | No LTS policy |
| **Production users** | Fastly, Fermyon (75M RPS), American Express, Shopify | Docker Desktop, CNCF edge ecosystem |
| **Cold start** | Fastest among JIT/AOT runtimes | Comparable |
| **WASI-NN (ML)** | ONNX runtime | GGML, Whisper, TF-Lite, ONNX, OpenVINO |
| **Docker/OCI integration** | No | Yes (Docker Desktop ships WasmEdge) |

### Why Wasmtime wins for crew-rs

1. **Pure Rust crate** — `cargo add wasmtime`, zero system dependencies. WasmEdge
   requires installing a C++ library and cmake.

2. **Tokio-native async** — crew-rs is a tokio application. Wasmtime's
   `call_async` and `ResourceLimiterAsync` integrate directly with our event loop.
   WasmEdge would require bridging between async runtimes.

3. **Component Model** — Define plugin interfaces in WIT (Wasm Interface Types),
   generate typed Rust host/guest bindings with `bindgen!`. This is the correct
   abstraction for a plugin system. WasmEdge lacks this.

4. **Fuel metering** — Fine-grained per-instruction execution limits. Essential
   for preventing runaway plugins.

5. **Production maturity** — Multi-company governance, LTS releases, formal
   security practices (cargo vet, OSS-Fuzz 24/7).

### When WasmEdge would be better

- Running Wasm plugins as **OCI containers** deployed to Kubernetes/edge nodes
- Plugins that need **WASI-NN** for ML inference (Whisper, GGML, etc.)
- Environments where Docker integration matters

These are real advantages but not relevant for crew-rs's use case of sandboxing
plugin code within a Rust host process.

---

## 7. Recommended Phased Approach

### Phase 1 — Subprocess Isolation (Done)

Current model. Plugins are binaries invoked via stdin/stdout JSON protocol.

- **Isolation**: OS process boundary
- **Limits**: `timeout_secs` in manifest, OS-level ulimit
- **Works for**: Trusted first-party plugins, any language

### Phase 2 — Tool Permission Policies (Next)

Add per-profile tool allow/deny rules to the orchestration layer. See §2 above.

- **Effort**: ~50 lines in `crew-agent` tool dispatch
- **No new dependencies**
- **Solves**: "customer bot can't call admin tools" use case
- **Implementation**: Check `tool_policy` rules before `ToolRunner::run()`

### Phase 3 — Wasm Sandbox (Future)

Add Wasmtime as an optional plugin runtime for untrusted plugins.

```
manifest.json:
{
  "id": "my-plugin",
  "version": "1.0.0",
  "type": "tool",
  "runtime": "wasm",          // ← new field, default: "native"
  "binary": "plugin.wasm",    // ← Wasm module instead of native binary
  "sandbox": {
    "max_memory_mb": 256,     // Memory cap
    "max_fuel": 10000000,     // Instruction budget
    "allow_network": false,   // WASI capability grants
    "allow_fs_read": ["/tmp/crew"],
    "allow_fs_write": ["/tmp/crew"],
    "allow_env": ["API_KEY"]
  },
  "tools": [...]
}
```

**Architecture**:

```
┌─────────────────────────────────────────────┐
│  crew-rs host process                       │
│                                             │
│  ┌───────────────┐  ┌───────────────────┐   │
│  │ Native Plugin │  │ Wasm Sandbox      │   │
│  │ (subprocess)  │  │ ┌───────────────┐ │   │
│  │               │  │ │ Wasmtime      │ │   │
│  │ stdin/stdout  │  │ │ Engine        │ │   │
│  │ JSON-RPC      │  │ │               │ │   │
│  │               │  │ │ plugin.wasm   │ │   │
│  │               │  │ │ (sandboxed)   │ │   │
│  └───────────────┘  │ └───────────────┘ │   │
│                     │ Fuel + Memory cap  │   │
│                     │ WASI capabilities  │   │
│                     └───────────────────┘   │
│                                             │
│  Plugin Manager (dispatch by runtime field) │
└─────────────────────────────────────────────┘
```

**Implementation steps**:

1. Add `wasmtime` dependency to `crew-plugin` (feature-gated: `sandbox`)
2. Define plugin WIT interface:
   ```wit
   package crew:plugin@0.1.0;

   interface tool {
     record tool-call {
       name: string,
       input: string,  // JSON
     }
     record tool-result {
       output: string,  // JSON
       is-error: bool,
     }
     call: func(req: tool-call) -> tool-result;
   }

   world plugin {
     export tool;
   }
   ```
3. `WasmPluginRunner` struct: instantiates Wasm module with fuel/memory limits
   and WASI capability configuration
4. Plugin dispatch: check `manifest.runtime` → route to subprocess or Wasm runner
5. Guest SDK crate (`crew-plugin-guest`) for Rust plugins compiling to
   `wasm32-wasip2`

### Phase 4 — Syscall Filtering on Native Plugins (Optional)

For native subprocess plugins on Linux, add Landlock LSM filtering:

- Restrict filesystem access to declared paths only
- Restrict network access based on manifest declarations
- Zero overhead (kernel-enforced)

On macOS, use `sandbox-exec` (App Sandbox) for equivalent restrictions.

### Phase 5 — Plugin Marketplace (Future)

With Wasm sandboxing in place, untrusted third-party plugins become safe:

- Plugins published as `.wasm` modules to a registry
- `crew plugin install <name>` downloads and verifies
- Sandbox limits enforced per manifest
- Capability review before install (like mobile app permissions)

---

## 8. Guest Language Support

Plugins compiled to Wasm can be written in:

| Language | Toolchain | Status |
|----------|-----------|--------|
| Rust | `cargo build --target wasm32-wasip2` | Excellent |
| Go | TinyGo | Good |
| C/C++ | wasi-sdk | Good |
| Python | componentize-py | Experimental |
| JavaScript | ComponentizeJS (SpiderMonkey) | Experimental |
| C# | .NET 9 WASI | Experimental |
| Kotlin | Kotlin/Wasm | Experimental |

The Component Model + WIT interface means all guests implement the same typed
API regardless of source language.

---

## 9. References

- [Wasmtime Documentation](https://docs.wasmtime.dev/)
- [Wasmtime Component Model](https://component-model.bytecodealliance.org/)
- [WasmEdge Documentation](https://wasmedge.org/docs/)
- [WASI Specification](https://wasi.dev/)
- [Microsoft Wassette](https://github.com/microsoft/wassette) — Wasm + MCP agent tooling
- [wasmcp](https://github.com/wasmcp/wasmcp) — Polyglot Wasm MCP framework
- [Amla Sandbox](https://github.com/amlalabs/amla-sandbox) — Capability-based agent sandbox
- [Extism](https://extism.org/) — Wasm plugin framework with AI integration
- [NVIDIA: Sandboxing Agentic AI with Wasm](https://developer.nvidia.com/blog/sandboxing-agentic-ai-workflows-with-webassembly/)
- [Theseus OS](https://github.com/theseus-os/Theseus)
- [Landlock LSM](https://landlock.io/)
- [Claude Code Permissions](https://code.claude.com/docs/en/permissions) — allow/ask/deny tool policy model
