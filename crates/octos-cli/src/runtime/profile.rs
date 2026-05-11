//! Profile-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`ProfileRuntime`] type and its `bootstrap`
//! signature; M11-B fills in the body.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, WrapErr};
use octos_agent::{SandboxConfig, ToolPolicy, ToolRegistry};
use octos_llm::{AdaptiveRouter, LlmProvider, QosCatalog};
use octos_memory::{EpisodeStore, MemoryStore};

use crate::commands::chat;
use crate::config::detect_provider;
use crate::profiles::{UserProfile, config_from_profile};
use crate::qos_catalog::{ExporterMode, build_adaptive_provider_chain};
use crate::skills_scope::{discover_ominix_url, push_runtime_plugin_env};

/// All long-lived state that belongs to a single profile within the
/// current host process.
///
/// One `ProfileRuntime` per `(host process, profile_id)`. The host
/// process is `octos serve`, `octos gateway` (each subprocess), or
/// `octos tui` — every entry point that today reads a [`UserProfile`]
/// off disk and turns it into a running agent ends up holding an
/// `Arc<ProfileRuntime>`.
///
/// # What lives here
///
/// Anything that is an *account property* of the logged-in user:
///
/// - **`llm`** — the top-level LLM provider chain (already wrapped by
///   `RetryProvider` → `ProviderChain` → optional [`AdaptiveRouter`]).
///   Two sessions opened by the same user hit the same provider chain.
/// - **`adaptive_router`** — `Some` only when QoS-aware adaptive
///   routing was successfully built (more than one provider). Owned
///   here because the per-profile metrics exporter wants a typed
///   handle, not a `dyn` provider.
/// - **`credentials`** — resolved API keys / secrets keyed by env-var
///   name. Populated from `profile.config.env_vars` via the keychain
///   in M11-B; passed to MCP server spawns and plugin invocations on
///   the session side.
/// - **`skills_dir`** — the per-profile plugin directory
///   (`~/.octos/profiles/<id>/data/skills/`), if it exists. Used at
///   bootstrap time to register profile-scoped skills into
///   [`Self::tool_specs`].
/// - **`plugin_env_template`** — the env-var pairs (e.g.
///   `OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`) every plugin spawn for
///   this profile should inherit. Sessions clone this into their own
///   plugin spawns; if a session needs to add session-scoped vars it
///   does so on top of this template.
/// - **`tool_policy`** — the profile's allow/deny tool policy. The
///   policy is *applied per session* (after the session clones
///   [`Self::tool_specs`]) so policy edits don't require rebuilding
///   the base registry.
/// - **`default_sandbox`** — the sandbox config every session
///   inherits unless it explicitly overrides via
///   [`super::SessionRuntime::sandbox`].
/// - **`tool_specs`** — the base [`ToolRegistry`] template. It has
///   builtins registered, plugins loaded, MCP agents wired, the LRU
///   pin set applied — *but no workspace bound*. Sessions clone this
///   and call `with_workspace_root` to get a workspace-bound registry.
///   This is the M11 fix for the multi-tenant base-registry leak
///   codex flagged on PR #868.
/// - **`memory`** / **`memory_store`** — the per-profile
///   [`EpisodeStore`] (redb at `<data_dir>/episodes.redb`) and
///   [`MemoryStore`] (MEMORY.md, daily notes). Memory is profile-
///   scoped because it crosses sessions — a long-running fact a user
///   teaches the agent in one room should be recallable in another
///   room of the same profile.
///
/// # What does NOT live here
///
/// Anything that can legitimately differ between two chats opened by
/// the same logged-in user — `workspace_root`, conversation history,
/// the per-session `Agent`, the session's tool-registry view, the
/// effective sandbox after a session-level override. Those live on
/// [`super::SessionRuntime`].
///
/// # Lifecycle
///
/// Built once per profile on first use via [`Self::bootstrap`]. Held
/// behind an `Arc` so every [`super::SessionRuntime`] for the profile
/// can cheaply share it. Hot-reloaded (rebuilt) when the profile
/// config on disk changes; the [`crate::config_watcher`] decides what
/// constitutes a reload-worthy change.
pub struct ProfileRuntime {
    /// Stable identifier for the profile (matches
    /// `UserProfile::id`). Used as part of the cache key in
    /// [`super::SessionRuntimeCache`] and as the value of
    /// `OCTOS_PROFILE_ID` in plugin spawns.
    pub profile_id: String,

    /// The profile's data directory, conventionally
    /// `~/.octos/profiles/<profile_id>/data`. Resolved by the caller
    /// and passed into [`Self::bootstrap`]; held here so sessions and
    /// session-scope bootstrap code don't have to re-derive it.
    pub data_dir: PathBuf,

    /// The fully-wrapped LLM provider chain for this profile.
    /// Includes retry, provider failover, and (if `adaptive_router`
    /// is `Some`) adaptive routing. Every session for this profile
    /// uses this same provider.
    pub llm: Arc<dyn LlmProvider>,

    /// Typed handle to the adaptive router if QoS-aware adaptive
    /// routing was wired in. `None` when only a single provider was
    /// configured (no failover to optimize). Held separately from
    /// `llm` so the metrics exporter and the runtime QoS catalog
    /// reader don't have to downcast the `dyn LlmProvider`.
    pub adaptive_router: Option<Arc<AdaptiveRouter>>,

    /// Materialized runtime QoS catalog produced alongside the
    /// adaptive chain. Populated even when [`Self::adaptive_router`]
    /// is `None` — `build_adaptive_provider_chain` derives a
    /// cold-start catalog from `model_catalog.json` for single-
    /// provider profiles too, and the downstream sub-provider
    /// router needs that seed for fallback ranking. Held here so
    /// gateway's `provider_router.seed_qos_scores` path stays
    /// byte-identical with the pre-M11-B inline assembly.
    pub runtime_qos_catalog: Option<QosCatalog>,

    /// Resolved credentials for this profile, keyed by env-var name
    /// (e.g. `OPENAI_API_KEY`, `AUTODL_API_KEY`). Populated from
    /// `profile.config.env_vars` via the keychain resolver. Sessions
    /// read this when spawning MCP servers, plugins, and shell tools
    /// that need the profile's API keys.
    pub credentials: HashMap<String, String>,

    /// Path to the per-profile skills directory if one exists
    /// (`<data_dir>/skills/`). `None` when the profile has no
    /// dashboard-installed skills, in which case the base
    /// [`ToolRegistry`] only carries built-in tools and global
    /// skills.
    pub skills_dir: Option<PathBuf>,

    /// Env-var pairs every plugin spawn for this profile should
    /// inherit (`OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`, etc.). Kept
    /// as a vector of `(name, value)` rather than a map so the
    /// session-side spawner can build the child env in stable order.
    /// Sessions are free to add session-scoped vars on top of this
    /// template.
    pub plugin_env_template: Vec<(String, String)>,

    /// The profile's tool policy (allow/deny lists, named groups,
    /// per-provider overrides). Stored on the profile and applied
    /// per session when the session clones [`Self::tool_specs`].
    /// `None` means "no profile-level policy" — the agent's default
    /// permissions apply.
    pub tool_policy: Option<ToolPolicy>,

    /// The default sandbox config sessions inherit. Sessions may
    /// override (e.g. a slides-builder session wants
    /// `no-network`); when they don't, the runtime falls back to
    /// this value.
    pub default_sandbox: SandboxConfig,

    /// The base [`ToolRegistry`] template — builtins + plugins +
    /// MCP agents + the LRU pin set — but **NOT** workspace-bound.
    /// Sessions clone this and call `with_workspace_root` to obtain
    /// a workspace-bound registry. The "no workspace bound" rule is
    /// load-bearing: it's the M11 fix for the codex-flagged
    /// multi-tenant base-registry leak (one global registry shared
    /// across sessions would otherwise let session A's workspace
    /// path leak into session B).
    pub tool_specs: Arc<ToolRegistry>,

    /// Long-lived [`EpisodeStore`] for this profile (redb at
    /// `<data_dir>/episodes.redb`). Shared across all sessions of
    /// the profile so task summaries written in one session are
    /// recallable from another.
    pub memory: Arc<EpisodeStore>,

    /// Long-lived [`MemoryStore`] (MEMORY.md + daily notes + recent
    /// memories window) for this profile. Same sharing rationale as
    /// [`Self::memory`].
    pub memory_store: Arc<MemoryStore>,
}

impl ProfileRuntime {
    /// Construct a [`ProfileRuntime`] for the given profile.
    ///
    /// Lifts the per-profile bootstrap sequence currently inlined in
    /// `gateway_runtime.rs::init` so that the gateway, serve, and TUI
    /// entry points can share one helper.
    ///
    /// # Steps (per workstreams/M11-runtime-unification.md § M11-B)
    ///
    /// 1. Derive a per-profile `Config` via
    ///    `crate::profiles::config_from_profile` with `None, None`
    ///    (preserves the per-profile LLM contract PR #866
    ///    introduced).
    /// 2. Wrap the primary LLM via
    ///    `crate::qos_catalog::build_adaptive_provider_chain` with
    ///    `ExporterMode::Spawn` (PR #867's shared helper). Stores
    ///    both `llm` and `adaptive_router` on the returned struct.
    /// 3. Surface `profile.config.env_vars` as a pass-through copy
    ///    under `credentials`. Keychain resolution stays at the
    ///    downstream call sites (`profile_plugin_env`,
    ///    `profile_search_provider_keys`) for M11-B so warnings are
    ///    not duplicated; M11-D will unify both onto a single
    ///    resolution.
    /// 4. Resolve `skills_dir = data_dir.join("skills")` when the
    ///    directory exists (PR #868's logic).
    /// 5. Build `plugin_env_template` via
    ///    `crate::skills_scope::push_runtime_plugin_env` (PR #868's
    ///    helper).
    /// 6. Construct the base [`ToolRegistry`] via
    ///    `ToolRegistry::with_builtins_and_sandbox` — gateway's
    ///    full registration sequence (browser, web_search, MCP,
    ///    plugins, ...) is layered on top of this base by the caller
    ///    so cmd-flag-dependent tools (`SwitchModelTool`,
    ///    `SwappableProvider`, admin tools, etc.) can still be
    ///    wired without leaking into the profile-scope abstraction.
    /// 7. Plugin loading + LRU pinning happen on the caller's copy of
    ///    the registry (see `ToolRegistry::snapshot_excluding`) so
    ///    `ProfileRuntime::bootstrap` stays signature-stable across
    ///    callers — gateway today and `octos serve` / TUI tomorrow.
    /// 8. Open [`EpisodeStore`] and [`MemoryStore`] against
    ///    `data_dir`.
    /// 9. Return `Arc<Self>`.
    ///
    /// # Parameters
    ///
    /// - `profile` — the parsed [`UserProfile`] from the profile
    ///   store; carries the on-disk config that drives the
    ///   bootstrap.
    /// - `data_dir` — the resolved per-profile data dir, typically
    ///   `~/.octos/profiles/<id>/data`. The bootstrap creates it if
    ///   missing.
    /// - `no_retry` — when `true`, skip the `RetryProvider` wrapping.
    ///   Plumbed through to `build_adaptive_provider_chain` so the
    ///   gateway `--no-retry` flag still inhibits retries when the
    ///   gateway delegates here.
    /// - `octos_home` — the host's `~/.octos` (or `--octos-home`
    ///   override). Used to seed `OCTOS_HOME` in
    ///   `plugin_env_template`; defaults to `data_dir` when `None`
    ///   so call sites without the flag stay in lockstep with
    ///   gateway's current `effective_octos_home` fallback.
    ///
    /// **Note on the missing `store` parameter:** the M11-A skeleton
    /// listed `store: &ProfileStore` for "admin/sub-account
    /// resolution" but M11-B's body has no such lookup — gateway's
    /// admin/sub-account logic lives downstream of bootstrap and
    /// stays there. Dropping the parameter lets the gateway keep
    /// its `ProfileStore::open` at its original position in the
    /// init sequence (preserving filesystem-side-effect timing).
    /// Re-introducing the parameter when M11-C/D actually needs it
    /// is a one-line API change.
    ///
    /// # Errors
    ///
    /// Returns an error if any of the steps above fail: provider
    /// construction, redb open, or tool-registry build. The
    /// bootstrap is fail-fast — a partially constructed
    /// [`ProfileRuntime`] is never returned.
    pub async fn bootstrap(
        profile: &UserProfile,
        data_dir: &Path,
        no_retry: bool,
        octos_home: Option<&Path>,
    ) -> Result<Arc<Self>> {
        // Step 1: derive the per-profile Config. We deliberately pass
        // `None, None` for bridge_url / feishu_port — those overrides
        // are channel-level concerns and `ProfileRuntime` only owns
        // LLM/tools/memory state. Gateway derives its own `Config`
        // (with the overrides) for channel wiring; the LLM/tools
        // pieces derived here are byte-identical regardless.
        let config = config_from_profile(profile, None, None);

        // Step 2a: resolve provider_name + model + base_url the same
        // way gateway does today (see `gateway_runtime.rs::init`).
        let model = config.model.clone();
        let base_url = config.base_url.clone();
        let provider_name = config
            .provider
            .clone()
            .or_else(|| model.as_deref().and_then(detect_provider).map(String::from))
            .ok_or_else(|| eyre::eyre!("no LLM provider configured for this profile"))?;

        // Step 2b: build the base provider, then the QoS-aware
        // adaptive chain via the shared helper PR #867 introduced.
        let base_provider = chat::create_provider(&provider_name, &config, model, base_url)?;
        let bundle = build_adaptive_provider_chain(
            base_provider,
            &config,
            data_dir,
            no_retry,
            ExporterMode::Spawn,
        );
        let llm = bundle.llm;
        let adaptive_router = bundle.adaptive_router;
        let runtime_qos_catalog = bundle.runtime_qos_catalog;

        // Step 3: surface the profile's declared env vars under
        // `credentials` as a pass-through copy. Keychain resolution
        // is deferred to the gateway / serve helpers that already
        // call `keychain::resolve_env_vars` downstream (today via
        // `profile_plugin_env` and `profile_search_provider_keys`).
        // Doing the resolution here too would duplicate any
        // failure-path keychain warnings the downstream helpers
        // already emit, which violates the byte-identical gateway
        // boot invariant. M11-D will move both call sites onto a
        // single shared resolution and lift the resolved map into
        // this field.
        let credentials: HashMap<String, String> = profile.config.env_vars.clone();

        // Step 4: derive skills_dir as Some(...) only when the
        // directory exists (mirrors `build_account_plugin_dirs`).
        let candidate_skills_dir = data_dir.join("skills");
        let skills_dir = if candidate_skills_dir.exists() {
            Some(candidate_skills_dir)
        } else {
            None
        };

        // Step 5: build the per-profile plugin env template. Only the
        // runtime envs (`OCTOS_DATA_DIR`, `OCTOS_HOME`,
        // `OCTOS_PROFILE_ID`, `OCTOS_VOICE_DIR`, `OMINIX_API_URL`)
        // belong here — `ProfileRuntime` is provider-agnostic, so
        // provider-API-key envs (`build_plugin_env`,
        // `profile_plugin_env`) stay on the caller until gateway's
        // legacy non-profile path is retired and we can lift them
        // safely.
        let mut plugin_env_template: Vec<(String, String)> = Vec::new();
        let ominix_url = discover_ominix_url();
        let effective_octos_home = octos_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.to_path_buf());
        push_runtime_plugin_env(
            &mut plugin_env_template,
            data_dir,
            &effective_octos_home,
            Some(profile.id.as_str()),
            ominix_url.as_deref(),
        );

        // Step 8: open the per-profile memory stores. Gateway logs
        // these with `eprintln!("[gateway] …")`; that logging stays
        // on the caller so this helper is caller-agnostic (serve and
        // TUI will use the same helper without inheriting gateway's
        // log prefixes).
        let memory = Arc::new(
            EpisodeStore::open(data_dir)
                .await
                .wrap_err("failed to open episode store")?,
        );
        let memory_store = Arc::new(
            MemoryStore::open(data_dir)
                .await
                .wrap_err("failed to open memory store")?,
        );

        // Step 6: build the base ToolRegistry. We register only the
        // sandbox-bound builtins here; gateway layers its full
        // registration sequence (WebSearchTool with provider keys,
        // BrowserTool with timeout override, MCP, plugin loading,
        // ProviderRouter-aware tools, SwitchModelTool, admin tools)
        // on top by snapshotting `tool_specs` into a mutable copy
        // and extending it. Keeping the layering on the caller is
        // what lets gateway preserve byte-identical startup logs +
        // ordering across the M11-B refactor — the spec's
        // "construct base registry" is taken to mean "the builtin
        // floor every caller shares" and the caller-specific
        // additions stay where they are.
        let sandbox_config = config.sandbox.clone();
        let sandbox = octos_agent::create_sandbox(&sandbox_config);
        let cwd = data_dir; // workspace_root is bound per-session; the base registry
        // anchors to the data dir as a stable default before sessions rebind.
        let tools = ToolRegistry::with_builtins_and_sandbox(cwd, sandbox);
        let tool_specs = Arc::new(tools);

        Ok(Arc::new(Self {
            profile_id: profile.id.clone(),
            data_dir: data_dir.to_path_buf(),
            llm,
            adaptive_router,
            runtime_qos_catalog,
            credentials,
            skills_dir,
            plugin_env_template,
            tool_policy: config.tool_policy.clone(),
            default_sandbox: sandbox_config,
            tool_specs,
            memory,
            memory_store,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{GatewaySettings, ProfileConfig};
    use chrono::Utc;

    /// Smoke-test the bootstrap path on a synthetic profile with a
    /// stub keychain-free env_vars map. The assertion targets the
    /// two contractual outputs M11-B promises:
    ///
    /// - `tool_specs` carries the builtin floor (probe: `read_file`).
    /// - `credentials` is populated from `profile.config.env_vars`.
    /// Build a per-process-unique env-var name so this test doesn't
    /// collide with other tests in the same process when run under
    /// `cargo test`'s default parallel scheduler.
    fn unique_env_key(suffix: &str) -> String {
        format!("M11B_BOOTSTRAP_TEST_{}_{}", std::process::id(), suffix)
    }

    /// Process-wide mutex that serializes env-var mutation across
    /// every test in this module. Mirrors the pattern in
    /// `commands/gateway/profile_factory.rs::tests::synthesis_env_lock`
    /// — `std::env::set_var` / `remove_var` are not thread-safe even
    /// with unique key names because they mutate the process-wide
    /// environment, so all env-mutating tests in the binary need to
    /// share a single lock to be sound.
    fn bootstrap_env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn bootstrap_populates_tool_specs_and_credentials() {
        let _env_guard = bootstrap_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        // The provider chain needs a resolvable API key to construct
        // the base LLM. We point `api_key_env` at a process-local env
        // var we set ourselves so the test doesn't touch the OS
        // keychain or require a real provider key.
        let env_key = unique_env_key("API_KEY");
        // SAFETY: serialized by `bootstrap_env_lock`; the env-var
        // mutation happens under the module-wide mutex so no other
        // env-mutating test in this binary reads or writes the
        // process environment concurrently with this block.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var(&env_key, "synthetic-key");
        }

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("profiles").join("test").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mut env_vars = HashMap::new();
        env_vars.insert("CREDENTIAL_PROBE".to_string(), "probe-value".to_string());

        let profile = UserProfile {
            id: "m11b-test".to_string(),
            name: "M11-B Test".to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            public_subdomain: None,
            config: ProfileConfig {
                gateway: GatewaySettings::default(),
                env_vars,
                llm: Some(crate::profiles::LlmProfileConfig {
                    primary: Some(crate::profiles::LlmModelSelectionConfig {
                        family_id: Some("gemini".to_string()),
                        model_id: Some("gemini-2.0-flash".to_string()),
                        route: Some(crate::profiles::LlmRouteConfig {
                            api_key_env: Some(env_key.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let runtime = ProfileRuntime::bootstrap(&profile, &data_dir, false, None)
            .await
            .expect("bootstrap should succeed with a synthetic profile");

        // Acceptance #6 — `tool_specs` carries the builtin floor.
        let specs = runtime.tool_specs.specs();
        let names: std::collections::HashSet<&str> =
            specs.iter().map(|spec| spec.name.as_str()).collect();
        assert!(
            names.contains("read_file"),
            "tool_specs must include read_file (got: {names:?})",
        );

        // Acceptance #6 — `credentials` populated from env_vars.
        assert_eq!(
            runtime
                .credentials
                .get("CREDENTIAL_PROBE")
                .map(String::as_str),
            Some("probe-value"),
            "credentials must carry the profile's env_vars entries",
        );

        // Profile id + data_dir are stamped onto the runtime so
        // session bootstrap (M11-C) can derive workspace paths
        // without re-resolving from the store.
        assert_eq!(runtime.profile_id, "m11b-test");
        assert_eq!(runtime.data_dir, data_dir);

        // plugin_env_template carries the M11 contract env vars that
        // dashboard-installed skills depend on.
        let env_map: HashMap<&str, &str> = runtime
            .plugin_env_template
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(env_map.get("OCTOS_PROFILE_ID"), Some(&"m11b-test"));
        assert!(env_map.contains_key("OCTOS_DATA_DIR"));
        assert!(env_map.contains_key("OCTOS_VOICE_DIR"));

        // M11-B codex review fix: the QoS runtime catalog is
        // materialized for single-provider profiles too (cold-start
        // derivation from the seed catalog), so it must survive
        // bootstrap and remain available for the gateway's
        // sub-provider router seeding. We don't assert the exact
        // shape (it depends on the seed catalog on disk) but the
        // field is observably exposed as `Option<QosCatalog>` — the
        // sub-provider-router seeding path skips when `None`, which
        // is the correct behavior when no catalog is reachable.
        // The struct-shape check below is enough to wedge a
        // regression test against any future refactor that drops
        // the field again.
        let _: &Option<octos_llm::QosCatalog> = &runtime.runtime_qos_catalog;

        // Cleanup — leave the process env clean for other tests.
        // SAFETY: still under the `_env_guard` lock acquired at the
        // top of the test.
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var(&env_key);
        }
    }
}
