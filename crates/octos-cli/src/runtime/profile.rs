//! Profile-scope runtime state.
//!
//! See the crate-level [`super`] module docs and
//! `docs/M11-PROFILE-SESSION-RUNTIME-ADR.md` for the two-scope model.
//! This file owns the [`ProfileRuntime`] type and its `bootstrap`
//! signature; M11-B fills in the body.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::Result;
use octos_agent::{SandboxConfig, ToolPolicy, ToolRegistry};
use octos_llm::{AdaptiveRouter, LlmProvider, QosCatalog};
use octos_memory::{EpisodeStore, MemoryStore};

use crate::profiles::{UserProfile, config_from_profile};
use crate::skills_scope::push_runtime_plugin_env;

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

    /// The primary (base) provider's `model_id()` *before* the
    /// adaptive router / retry / swappable wrapping is applied.
    /// Gateway uses this for `resolve_provider_policy(..., model_id)`
    /// and as the `primary_key` of the sub-provider router's
    /// fallback ranking. We capture it here at bootstrap time
    /// because `Arc<dyn LlmProvider>::model_id()` on the wrapped
    /// chain can dispatch through `AdaptiveRouter::model_id()`,
    /// which performs lane selection and may return a fallback
    /// model's id rather than the primary's — that would silently
    /// change the per-provider tool policy and the auto router
    /// keys across the M11-B refactor.
    pub primary_model_id: String,

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

/// Pre-built inputs the caller hands to [`ProfileRuntime::bootstrap`].
///
/// Bundles the LLM chain pieces and the memory stores the caller has
/// already constructed so `bootstrap` does not duplicate the
/// side-effects (no second `create_provider` print, no second
/// `model_catalog.json` exporter spawn, no double `EpisodeStore::open`
/// against the same redb file). The caller is responsible for
/// emitting whatever log lines that gateway emits today around the
/// LLM and memory-store construction (`Model:`, `[gateway] LLM
/// provider created`, `[gateway] opening episode store`, ...); this
/// is what lets gateway preserve byte-identical startup logs across
/// the M11-B refactor.
///
/// M11-D will fold these back into the helper once the gateway /
/// serve split is gone and the caller-agnostic helper can own the
/// log emissions.
pub struct ProfileRuntimeInputs {
    /// The fully-wrapped LLM provider chain (RetryProvider /
    /// ProviderChain / AdaptiveRouter). Gateway and serve build
    /// this via `qos_catalog::build_adaptive_provider_chain` and
    /// hand the result to `bootstrap`.
    pub llm: Arc<dyn LlmProvider>,

    /// Typed handle to the adaptive router when QoS adaptive
    /// routing is wired. `None` for single-provider profiles.
    pub adaptive_router: Option<Arc<AdaptiveRouter>>,

    /// Materialized runtime QoS catalog (cold-start derived from
    /// `model_catalog.json` even when `adaptive_router` is `None`).
    pub runtime_qos_catalog: Option<QosCatalog>,

    /// The base provider's `model_id()` captured *before* the
    /// adaptive / retry / swappable wrapping.
    pub primary_model_id: String,

    /// Profile-scope episode store opened against
    /// `<data_dir>/episodes.redb`.
    pub memory: Arc<EpisodeStore>,

    /// Profile-scope memory store opened against `<data_dir>`.
    pub memory_store: Arc<MemoryStore>,

    /// The effective sandbox config gateway / serve derived from
    /// the profile's config (potentially with
    /// `read_allow_paths` augmented for `project_dir`). Sessions
    /// inherit this by default.
    pub default_sandbox: SandboxConfig,

    /// The base tool registry the caller has already built (e.g.
    /// via `ToolRegistry::with_builtins_and_sandbox`). Bootstrap
    /// takes it by value and stores it as `Arc<ToolRegistry>` so
    /// it does NOT call `create_sandbox` itself — that call
    /// already happened on the caller's side (gateway already runs
    /// it inside its tool registry construction block). Doing it
    /// here too would emit a duplicate
    /// `"sandbox disabled, shell commands run without isolation"`
    /// info-level log line when the profile disables sandboxing.
    /// M11-D will fold this responsibility back in once the
    /// gateway's tool-registry construction is fully unified with
    /// serve's.
    pub tool_specs: ToolRegistry,

    /// The per-profile skills dir candidate path, or `None` when
    /// the caller knows the profile has no installed skills. The
    /// caller decides whether to do the existence check; bootstrap
    /// does NOT stat the filesystem here so the helper does not
    /// add an observable filesystem read outside the pre-PR
    /// sequence. Gateway hands in
    /// `Some(data_dir.join("skills"))` when the caller would
    /// otherwise check existence — the read happens later inside
    /// `build_account_plugin_dirs` exactly as pre-PR.
    pub skills_dir: Option<PathBuf>,

    /// Pre-computed Ominix discovery URL. Caller resolves it once
    /// via `crate::skills_scope::discover_ominix_url` (gateway
    /// already does this for its voice / ASR plumbing) and hands
    /// the result to bootstrap so we do not read
    /// `~/.ominix/api_url` a second time on profile boot.
    pub ominix_url: Option<String>,
}

impl ProfileRuntime {
    /// Assemble a [`ProfileRuntime`] from already-constructed LLM /
    /// memory inputs plus the profile + paths.
    ///
    /// In M11-B this is a pure assembler: the caller hands in
    /// pre-built LLM chain, memory stores, and sandbox config (so
    /// gateway can keep the pre-PR ordering of its `create_provider`
    /// prints, `[gateway] …` markers, and `ProfileStore::open` /
    /// `EpisodeStore::open` filesystem side effects). The helper
    /// then derives:
    ///
    /// 1. `credentials` — pass-through clone of
    ///    `profile.config.env_vars`. Keychain resolution stays at
    ///    the downstream call sites (`profile_plugin_env`,
    ///    `profile_search_provider_keys`) so warnings are not
    ///    duplicated; M11-D will unify the call sites and lift the
    ///    resolved map into this field.
    /// 2. `skills_dir` — `Some(<data_dir>/skills)` when the
    ///    directory exists, else `None`. Mirrors
    ///    `build_account_plugin_dirs`.
    /// 3. `plugin_env_template` — built via
    ///    `crate::skills_scope::push_runtime_plugin_env`. Carries
    ///    `OCTOS_DATA_DIR`, `OCTOS_HOME`, `OCTOS_PROFILE_ID`,
    ///    `OCTOS_VOICE_DIR`, `OMINIX_API_URL` (when discoverable).
    /// 4. `tool_specs` — the builtin floor via
    ///    `ToolRegistry::with_builtins_and_sandbox`. Gateway
    ///    snapshots this and layers its full registration sequence
    ///    on top so cmd-flag-dependent tools
    ///    (`SwitchModelTool`, admin tools, ...) stay on the
    ///    caller side. The "no workspace bound" rule is preserved.
    ///
    /// M11-D will fold steps 1-3 of the M11-A docstring (Config
    /// derivation, LLM chain, memory store opens) back into this
    /// helper once `octos serve` / TUI adopt it and the gateway
    /// path is the last caller still doing them inline. Until then
    /// the assembler shape is what lets gateway preserve
    /// byte-identical boot behavior.
    ///
    /// # Parameters
    ///
    /// - `profile` — the parsed [`UserProfile`] from the profile
    ///   store; drives the per-profile derivations.
    /// - `data_dir` — the resolved per-profile data dir, typically
    ///   `~/.octos/profiles/<id>/data`.
    /// - `octos_home` — the host's `~/.octos` (or `--octos-home`
    ///   override). Used to seed `OCTOS_HOME` in
    ///   `plugin_env_template`; defaults to `data_dir` when `None`
    ///   so call sites without the flag stay in lockstep with
    ///   gateway's current `effective_octos_home` fallback.
    /// - `inputs` — pre-built LLM / memory / sandbox pieces the
    ///   caller has already constructed. See
    ///   [`ProfileRuntimeInputs`] for the contract.
    ///
    /// # Errors
    ///
    /// Returns an error only if the synchronous derivations fail.
    /// In M11-B's assembler shape that means *no* error path
    /// remains — all I/O has already been performed by the caller
    /// before this function is called. The `Result` return is kept
    /// so the M11-D version (which folds the I/O back in) doesn't
    /// need a signature change.
    pub async fn bootstrap(
        profile: &UserProfile,
        data_dir: &Path,
        octos_home: Option<&Path>,
        inputs: ProfileRuntimeInputs,
    ) -> Result<Arc<Self>> {
        // Step 1: surface the profile's declared env vars under
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

        // Step 2: take the pre-computed `skills_dir` from the
        // caller. We deliberately do not stat
        // `data_dir.join("skills")` here — gateway already performs
        // the same check via `build_account_plugin_dirs` further
        // down, and doing it twice would add an observable
        // filesystem read outside the pre-PR sequence.
        let skills_dir = inputs.skills_dir;

        // Step 3: build the per-profile plugin env template using
        // the caller's pre-resolved Ominix URL. Gateway already
        // calls `discover_ominix_url()` once for its voice / ASR
        // plumbing and hands the result to bootstrap so we do not
        // read `~/.ominix/api_url` a second time on profile boot.
        let mut plugin_env_template: Vec<(String, String)> = Vec::new();
        let effective_octos_home = octos_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.to_path_buf());
        push_runtime_plugin_env(
            &mut plugin_env_template,
            data_dir,
            &effective_octos_home,
            Some(profile.id.as_str()),
            inputs.ominix_url.as_deref(),
        );

        // Step 4: wrap the caller-built tool registry in `Arc` so
        // sessions can share it cheaply. We deliberately do NOT
        // call `octos_agent::create_sandbox` here — the caller
        // (gateway today, serve / TUI tomorrow) has already built a
        // registry against its own `Sandbox` instance, and calling
        // `create_sandbox` here too would emit a duplicate
        // `"sandbox disabled, ..."` info-level log line when the
        // profile disables sandboxing.
        let tool_specs = Arc::new(inputs.tool_specs);

        // Step 5: derive the tool_policy from the profile's Config.
        // Same derivation gateway used pre-PR — read the policy off
        // the per-profile `Config`.
        let config = config_from_profile(profile, None, None);

        Ok(Arc::new(Self {
            profile_id: profile.id.clone(),
            data_dir: data_dir.to_path_buf(),
            llm: inputs.llm,
            adaptive_router: inputs.adaptive_router,
            runtime_qos_catalog: inputs.runtime_qos_catalog,
            primary_model_id: inputs.primary_model_id,
            credentials,
            skills_dir,
            plugin_env_template,
            tool_policy: config.tool_policy.clone(),
            default_sandbox: inputs.default_sandbox,
            tool_specs,
            memory: inputs.memory,
            memory_store: inputs.memory_store,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{GatewaySettings, ProfileConfig};
    use async_trait::async_trait;
    use chrono::Utc;
    use octos_core::Message;
    use octos_llm::{ChatConfig, ChatResponse, ChatStream, ToolSpec};

    /// Minimal stub LLM that satisfies the [`LlmProvider`] trait so
    /// `ProfileRuntime::bootstrap` can be exercised end-to-end
    /// without hitting the OS keychain, the network, or
    /// `chat::create_provider` (which would require a registered
    /// provider entry with a resolvable API key). The stub returns
    /// an error on `chat` / `chat_stream` — those paths are not
    /// exercised by bootstrap, which only reads `model_id()` /
    /// `provider_name()`.
    struct StubLlm {
        model_id: String,
    }

    #[async_trait]
    impl LlmProvider for StubLlm {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            Err(eyre::eyre!(
                "StubLlm::chat not used by ProfileRuntime::bootstrap"
            ))
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            Err(eyre::eyre!(
                "StubLlm::chat_stream not used by ProfileRuntime::bootstrap"
            ))
        }

        fn context_window(&self) -> u32 {
            0
        }

        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    /// Smoke-test the bootstrap assembler on a synthetic profile +
    /// temp data_dir + stub LLM. The assertion targets every
    /// contractual output M11-B promises: `tool_specs` carries the
    /// builtin floor, `credentials` is populated from `env_vars`,
    /// `plugin_env_template` carries the M11 contract env vars,
    /// `primary_model_id` round-trips, and `runtime_qos_catalog` is
    /// exposed as `Option<QosCatalog>` so a future refactor that
    /// drops the field again fails CI immediately.
    #[tokio::test]
    async fn bootstrap_populates_tool_specs_and_credentials() {
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
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let memory = Arc::new(
            EpisodeStore::open(&data_dir)
                .await
                .expect("EpisodeStore::open"),
        );
        let memory_store = Arc::new(
            MemoryStore::open(&data_dir)
                .await
                .expect("MemoryStore::open"),
        );

        let stub_llm: Arc<dyn LlmProvider> = Arc::new(StubLlm {
            model_id: "stub-model-id".to_string(),
        });

        // The test builds the same builtin-floor registry M11-D's
        // bootstrap will eventually own — pre-built here so
        // bootstrap stays sandbox-call-free for byte-identical
        // gateway boot.
        let sandbox_config = SandboxConfig {
            // Disable so `create_sandbox` returns NoSandbox without
            // touching the host's bwrap / sandbox-exec binaries
            // (the test runs cross-platform).
            enabled: false,
            ..SandboxConfig::default()
        };
        let sandbox = octos_agent::create_sandbox(&sandbox_config);
        let tools = ToolRegistry::with_builtins_and_sandbox(&data_dir, sandbox);

        let inputs = ProfileRuntimeInputs {
            llm: stub_llm.clone(),
            adaptive_router: None,
            runtime_qos_catalog: None,
            primary_model_id: "stub-model-id".to_string(),
            memory,
            memory_store,
            default_sandbox: sandbox_config,
            tool_specs: tools,
            // The test does not care whether the synthetic
            // `<data_dir>/skills` path exists on disk — bootstrap
            // does not stat it, by contract.
            skills_dir: Some(data_dir.join("skills")),
            ominix_url: None,
        };

        let runtime = ProfileRuntime::bootstrap(&profile, &data_dir, None, inputs)
            .await
            .expect("bootstrap should succeed with a synthetic profile + stub LLM");

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

        // M11-B codex review fix #1: `primary_model_id` round-trips
        // through the assembler so gateway can use it for
        // `resolve_provider_policy(..., model_id)` + sub-provider
        // router primary key without dispatching through
        // `AdaptiveRouter::model_id()` (which performs lane
        // selection and would return a fallback model's id).
        assert_eq!(runtime.primary_model_id, "stub-model-id");

        // M11-B codex review fix #2: `runtime_qos_catalog` is
        // exposed as `Option<QosCatalog>` — gateway needs this
        // (populated by `build_adaptive_provider_chain` for single-
        // provider profiles too) for the sub-provider router's QoS
        // seeding. The struct-shape check below is enough to wedge
        // a regression test against any future refactor that drops
        // the field again.
        let _: &Option<QosCatalog> = &runtime.runtime_qos_catalog;
    }
}
