# Configuration Architecture Guide

> Reference for implementing new features that interact with config, profiles, or gateway lifecycle.

## Core Principle

**The profile JSON is the single source of truth.** The dashboard writes `~/.octos/profiles/{id}.json`, and everything else derives from it. There is no intermediate config file.

```
profiles/{id}.json ──→ gateway process (reads directly)
                   ──→ control plane watcher (auto-restart)
                   ──→ gateway's ConfigWatcher (hot-reload)
```

## Type Hierarchy

```
UserProfile                          Config
├── id, name, enabled                ├── provider, model, base_url
├── data_dir                         ├── api_key_env, max_iterations
├── config: ProfileConfig            ├── gateway: Option<GatewayConfig>
│   ├── provider, model, base_url    │   ├── channels: Vec<ChannelEntry>
│   ├── api_key_env                  │   ├── max_history, system_prompt
│   ├── fallback_models              │   ├── queue_mode, max_sessions
│   ├── channels: Vec<ChannelCreds>  │   ├── max_concurrent_sessions
│   ├── gateway: GatewaySettings     │   └── browser_timeout_secs
│   │   ├── max_history              ├── fallback_models: Vec<FallbackModel>
│   │   ├── max_iterations           ├── mcp_servers, sandbox, hooks
│   │   ├── system_prompt            ├── tool_policy, embedding
│   │   ├── max_concurrent_sessions  ├── sub_providers: Vec<SubProviderConfig>
│   │   └── browser_timeout_secs     ├── context_filter: Option<Vec<String>>
│   └── env_vars: HashMap<String,String>  ├── adaptive_routing: Option<AdaptiveRoutingConfig>
├── created_at                       ├── voice: Option<VoiceConfig>
└── updated_at                       ├── email: Option<EmailConfig>
                                     ├── dashboard_auth: Option<DashboardAuthConfig>  #[cfg(feature = "api")]
                                     └── monitor: Option<MonitorConfig>               #[cfg(feature = "api")]
```

`UserProfile` is the dashboard/API format. `Config` is the gateway runtime format. The bridge between them is `config_from_profile()`.

## Data Flow

### Managed gateway (via `octos serve`)

```
1. Dashboard PUT /api/admin/profiles/{id}
       ↓
2. API handler calls ProfileStore::save_with_merge()
       ↓
3. Writes profiles/{id}.json (0600 permissions)
       ↓
4. Control plane watcher (serve.rs, 5s polling)
   - SHA-256 hash comparison per profile
   - diff_profiles() classifies change
       ↓
5a. RestartRequired → ProcessManager::restart()
    - stop (kill signal) → 500ms delay → start
    - Spawns: octos gateway --profile {id}.json --data-dir ... [--bridge-url ...] [--feishu-port ...]
       ↓
5b. HotReloadable → no action (gateway's own watcher handles it)
       ↓
6. Gateway's ConfigWatcher (5s polling on profile file)
   - parse_first() tries Config, falls back to UserProfile
   - diff_and_emit() applies hot-reloadable changes in-process
```

### Standalone gateway (no serve)

```
octos gateway --config config.json     # traditional format
octos gateway --profile profile.json   # profile format
octos gateway                          # auto-detect from cwd/.octos/config.json
```

All three paths produce a `Config` struct. The gateway doesn't care which format the file is — `ConfigWatcher::parse_first()` handles both transparently.

## Change Classification

### Restart-required fields

Changes to these fields require killing and restarting the gateway process. There is a brief service interruption (~1-3s).

| Field | Where checked |
|---|---|
| `provider` | diff_profiles() + diff_and_emit() |
| `model` | diff_profiles() + diff_and_emit() |
| `base_url` | diff_profiles() + diff_and_emit() |
| `api_key_env` | diff_profiles() + diff_and_emit() |
| `channels` | diff_profiles() + diff_and_emit() |
| `fallback_models` | diff_profiles() |
| `env_vars` | diff_profiles() |
| `sandbox` | diff_and_emit() only |
| `mcp_servers` | diff_and_emit() only |
| `hooks` | diff_and_emit() only |
| `gateway.queue_mode` | diff_and_emit() only |

### Hot-reloadable fields

Applied in-process with zero interruption. The gateway's `ConfigWatcher` applies these via a watch channel.

| Field | Applied how |
|---|---|
| `system_prompt` | `agent.set_system_prompt()` |
| `max_history` | `AtomicUsize::store()` |

### Not yet classified

These fields exist in `GatewaySettings` and participate in `diff_profiles()` via the `PartialEq` derive on `GatewaySettings`, but they are not individually handled in `diff_and_emit()`:

- `max_iterations` — change detected as HotReloadable by diff_profiles(), but the gateway doesn't apply it live (requires restart in practice)
- `max_concurrent_sessions` — same
- `browser_timeout_secs` — same

## Adding a New Config Field

### Step 1: Decide where the field lives

- **Profile-level** (user-facing, editable via dashboard): add to `ProfileConfig` or `GatewaySettings` in `profiles.rs`
- **Runtime-level** (not in profiles, only in config.json): add to `Config` in `config.rs`
- **Both**: add to both, and map in `config_from_profile()`

### Step 2: Add to the profile type

```rust
// profiles.rs — GatewaySettings
pub struct GatewaySettings {
    #[serde(default)]
    pub my_new_field: Option<u32>,
    // ...
}
```

`#[serde(default)]` is required for backward compatibility with existing profile files that don't have the field.

### Step 3: Map in config_from_profile()

```rust
// profiles.rs — config_from_profile()
Config {
    // ... existing fields ...
    my_new_field: profile.config.gateway.my_new_field,
    // ...
}
```

If the field belongs on `Config` directly, add it there with `#[serde(default)]`.

### Step 4: Wire into gateway.rs

```rust
// gateway.rs — run_async()
let my_value = config.my_new_field.unwrap_or(DEFAULT);
```

Follow the existing precedence pattern: CLI arg > config value > default.

### Step 5: Classify for change detection

**If restart-required**, add to `diff_profiles()`:

```rust
// profiles.rs — diff_profiles()
if oc.my_new_field != nc.my_new_field {
    restart_fields.push("my_new_field".into());
}
```

And to `diff_and_emit()` in `config_watcher.rs` if applicable.

**If hot-reloadable**, add handling in gateway.rs where `ConfigChange::HotReload` is processed:

```rust
// gateway.rs — main loop
ConfigChange::HotReload { system_prompt, max_history, my_new_field } => {
    if let Some(val) = my_new_field {
        my_atomic.store(val, Ordering::Relaxed);
    }
}
```

And extend `ConfigChange::HotReload` in `config_watcher.rs`.

### Step 6: Add to dashboard (if user-facing)

Add the field to the React form in `dashboard/src/`. The API handler is a thin passthrough — no backend changes needed beyond the type definition.

## Key Functions Reference

| Function | File | Purpose |
|---|---|---|
| `config_from_profile()` | profiles.rs | UserProfile → Config (in-memory, no file I/O) |
| `diff_profiles()` | profiles.rs | Classify changes between two UserProfiles |
| `save_with_merge()` | profiles.rs | Save profile, preserving masked secret values |
| `parse_first()` | config_watcher.rs | Parse file as Config or UserProfile transparently |
| `diff_and_emit()` | config_watcher.rs | Classify Config changes, emit via watch channel |
| `ProcessManager::start()` | process_manager.rs | Spawn gateway with --profile arg + env vars |
| `ProcessManager::restart()` | process_manager.rs | Stop → 500ms delay → start (not graceful) |

## Secret Handling

Secrets live in `ProfileConfig.env_vars` (key=env var name, value=actual secret).

- **API responses**: `mask_secrets()` replaces values with `sk-1***def` (first 4 + last 3 for long values, `***` for short)
- **Save round-trip**: `save_with_merge()` detects masked/empty values and preserves the original from disk
- **Gateway process**: secrets passed via `cmd.env(key, value)`, filtered through `BLOCKED_ENV_VARS`
- **File permissions**: profile JSON written with mode 0600

### Gotcha

A value containing the literal string `***` will be treated as masked and replaced with the old value. This is unlikely in practice but worth knowing.

## Backward Compatibility

The traditional `--config config.json` path is fully preserved. The `--profile` path is additive. Both formats are supported in:

- Gateway CLI args (`--config` vs `--profile`)
- ConfigWatcher's `parse_first()` (tries both formats)
- All existing tests continue to pass

## Architecture Constraints

1. **No generated config files.** The profile JSON is read directly. Don't introduce intermediate files.
2. **ProfileStore is always available.** The `profiles` module is not feature-gated (moved out of `#[cfg(feature = "api")]` so gateway can use it).
3. **Polling, not inotify.** Both watchers use 5-second SHA-256 polling. This is intentional — works across all platforms, no dependency on filesystem notification APIs.
4. **Restart is not graceful.** `ProcessManager::restart()` kills the process. In-flight messages are lost. This is acceptable for config changes (rare, user-initiated) but should not be used for routine operations.
5. **One format, two schemas.** `Config` has fields that profiles don't expose (mcp_servers, sandbox, hooks, etc.). These default to empty/disabled in profile mode. If you need them, either add to `ProfileConfig` or use the traditional `--config` path.
