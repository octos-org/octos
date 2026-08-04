//! RunPipelineTool — implements `octos_agent::Tool` to expose pipeline execution.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_agent::cost_ledger::CostAccountant;
use octos_agent::{Tool, ToolPolicy, ToolResult};
use octos_llm::{EmbeddingProvider, LlmProvider, ProviderRouter};
use octos_memory::EpisodeStore;
use serde::Deserialize;

use crate::context::PipelineContext;
use crate::discovery::PipelineDiscovery;
use crate::executor::{ExecutorConfig, PipelineExecutor, PipelineResult, PipelineStatusBridge};
use crate::run_dir::{PipelineRunSummary, RunDir};
use octos_core::{SessionScope, TokenUsage};

/// #1020 / M17-B — reason string stamped onto every pipeline run's
/// `summary.json` because pipeline workers do not yet propagate the
/// parent's `ContextManager`. Evidence validators look for this reason
/// to confirm the acceptance bullet is satisfied.
pub const PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON: &str =
    "pipeline workers don't yet propagate ContextManager (M17-B)";

/// Gap 4.1 — the sanctioned generic pipeline name. Bundled into the binary
/// via `octos_agent::bundled_pipelines` and used as the no-discovery fallback
/// for the `run_pipeline` `pipeline` arg enum so the advertised choices are
/// never empty even before bootstrap has written the `.dot`.
const FALLBACK_PIPELINE_NAME: &str = "deep_research";

/// S1-5 opt-in: whether the typed-IR ([`crate::ir`]) authoring path is exposed
/// to the LLM by default. OFF unless the operator sets `OCTOS_PIPELINE_IR=1`
/// (or `true`) in the daemon environment (e.g. the launchd plist) — the same
/// env-based opt-in pattern used for other gated capabilities. Explicit
/// [`RunPipelineTool::with_ir_enabled`] overrides this (used by tests).
fn ir_authoring_default() -> bool {
    std::env::var("OCTOS_PIPELINE_IR")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Phase 2-A of the [`SessionScope`] migration (load-bearing follow-up
/// to PR #1199 / Phase 1).
///
/// Resolves the effective working directory pipeline workers should
/// spawn into, given the tool-level fallback (`tool_working_dir`) and
/// the parent session's optional [`SessionScope`].
///
/// * When the parent session attached a scope via [`ToolContext::session_scope`]
///   (snapshotted into [`PipelineHostContext::session_scope`]),
///   workers spawn into `scope.workspace()` so per-session reads stay
///   isolated to that session's ephemeral workspace dir. This is the
///   root cause fix for the mini5 NEW-06 cross-session contamination
///   bug: today's `RunPipelineTool` pins `working_dir` at construction
///   time (profile-level `data/`), so pipeline workers `read_file` and
///   `list_dir` against 200+ stale `.md` files from prior sessions.
/// * When no scope is present (legacy callers — CLI, unit tests, hosts
///   that haven't migrated yet), fall back to the tool's
///   `working_dir`. Behaviour is byte-for-byte identical to pre-Phase-2-A.
///
/// Also creates the workspace dir on disk when it doesn't exist, so a
/// freshly minted session can spawn workers without the caller having
/// to pre-create directories. Per the Phase 1 spec doc, this is the
/// caller's responsibility — the scope itself never does I/O. We do it
/// here (at the `RunPipelineTool` boundary) rather than inside the
/// executor so any callers of `PipelineExecutor::run(...)` direct stay
/// on the pre-Phase-2-A path.
///
/// `create_dir_all` failures fall back to the tool-level working dir
/// and emit a WARN — the production pipeline should not regress its
/// user-visible outcome on a transient filesystem error (e.g. quota
/// hit on first session creation). Per-session isolation is the
/// happy-path invariant; the fallback preserves legacy behaviour as a
/// safety net.
///
/// [`ToolContext::session_scope`]: octos_agent::tools::ToolContext::session_scope
/// [`PipelineHostContext::session_scope`]: crate::host_context::PipelineHostContext::session_scope
pub(crate) fn resolve_pipeline_working_dir(
    tool_working_dir: &std::path::Path,
    session_scope: Option<&SessionScope>,
) -> PathBuf {
    let Some(scope) = session_scope else {
        return tool_working_dir.to_path_buf();
    };
    let workspace = scope.workspace().to_path_buf();
    if let Err(error) = std::fs::create_dir_all(&workspace) {
        tracing::warn!(
            workspace = %workspace.display(),
            error = %error,
            tool_working_dir = %tool_working_dir.display(),
            "phase2a: failed to create session workspace; falling back to tool working_dir \
             (pipeline workers will NOT be session-isolated for this run)"
        );
        return tool_working_dir.to_path_buf();
    }
    tracing::debug!(
        workspace = %workspace.display(),
        tool_working_dir = %tool_working_dir.display(),
        "phase2a: pipeline workers will spawn in session-scoped workspace"
    );
    workspace
}

/// Tool that runs DOT-based pipelines.
pub struct RunPipelineTool {
    default_provider: Arc<dyn LlmProvider>,
    provider_router: Option<Arc<ProviderRouter>>,
    memory: Arc<EpisodeStore>,
    working_dir: PathBuf,
    provider_policy: Option<ToolPolicy>,
    plugin_dirs: Vec<PathBuf>,
    /// Section B (codex review P1.1): pipeline-level strict-signing
    /// policy. Defaults to `false` (legacy permissive path). When the
    /// host has opted into `plugins.require_signed`, this is set via
    /// [`Self::with_plugin_require_signed`] so per-node plugin loads
    /// enforce the same gate.
    plugin_require_signed: bool,
    discovery: PipelineDiscovery,
    /// Per-message status bridge (set via `set_status_bridge` before each call).
    status_bridge: std::sync::Mutex<Option<PipelineStatusBridge>>,
    /// Optional cost accountant (coding-blue FA-7). When set, every
    /// pipeline run reserves a pipeline-level budget at dispatch start
    /// and per-node sub-budgets for LLM-call nodes.
    cost_accountant: Option<Arc<CostAccountant>>,
    /// Logical contract id used when the pipeline context
    /// auto-populates from the workspace policy. Defaults to the
    /// graph id + `"pipeline"` fallback when empty.
    contract_id: Option<String>,
    /// NEW-06 fix: optional embedder for hybrid memory search.
    ///
    /// Without this set, worker `Agent` instances spawned per pipeline node
    /// SKIP episodic memory recall entirely (the no-embedder branch in
    /// `octos_agent::agent::memory`): BM25-only keyword recall within a single
    /// shared workspace can't discriminate on-task from cross-task episodes, so
    /// it would leak stale unrelated memory. The gateway / serve runtimes own
    /// the embedder; this lets the orchestrator propagate it down to pipeline
    /// workers so episodic memory recall is available AND contamination-safe
    /// end-to-end.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// S1-5: opt-in gate for the typed-IR ([`crate::ir`]) authoring path.
    /// When false (default) the tool only runs sanctioned named pipelines /
    /// inline DOT — byte-identical to pre-S1-5 behaviour. When true, the LLM
    /// may pass an `ir` program which is compiled (capability-locked via the
    /// closed palette + [`crate::profile::ValidationProfile`]) and executed.
    ir_enabled: bool,
    /// #1607 (codex-review follow-up): the session sandbox forwarded onto the
    /// [`ExecutorConfig`] so the pipeline's terminal / per-node command
    /// validators run confined instead of on the host. Defaults to
    /// `SandboxConfig::default()` (no-op on a host without a backend).
    sandbox: octos_agent::SandboxConfig,
}

impl RunPipelineTool {
    pub fn new(
        default_provider: Arc<dyn LlmProvider>,
        memory: Arc<EpisodeStore>,
        working_dir: PathBuf,
        data_dir: PathBuf,
    ) -> Self {
        // Agent-facing resolution searches ONLY operator-trusted dirs — never
        // the agent-writable `<working_dir>/.octos/pipelines` — so a model
        // cannot run a `.dot` it wrote by bare name (codex security review).
        let discovery = PipelineDiscovery::new_operator_trusted(&data_dir);
        Self {
            default_provider,
            provider_router: None,
            memory,
            working_dir,
            provider_policy: None,
            plugin_dirs: Vec::new(),
            plugin_require_signed: false,
            discovery,
            status_bridge: std::sync::Mutex::new(None),
            cost_accountant: None,
            contract_id: None,
            embedder: None,
            ir_enabled: ir_authoring_default(),
            sandbox: octos_agent::SandboxConfig::default(),
        }
    }

    /// #1607: thread the session sandbox onto the pipeline executor so
    /// terminal / per-node `Command` validators run confined. Mirrors
    /// `SpawnTool::with_sandbox` / `DelegateTool::with_sandbox`.
    pub fn with_sandbox(mut self, sandbox: octos_agent::SandboxConfig) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// S1-5: enable the typed-IR authoring path (opt-in; default off). The
    /// operator/runtime sets this when an autonomy level permits LLM-composed
    /// workflows. With it off, `ir` inputs are ignored and the tool advertises
    /// only the named-pipeline contract.
    pub fn with_ir_enabled(mut self, enabled: bool) -> Self {
        self.ir_enabled = enabled;
        self
    }

    /// NEW-06 fix: attach an embedder that the pipeline executor will
    /// propagate onto every per-node worker [`octos_agent::Agent`].
    ///
    /// When set, the worker's "Relevant Past Experiences" memory recall
    /// runs the modality-aware hybrid path that applies
    /// [`octos_agent::agent::memory::MIN_EPISODE_SIMILARITY`] BEFORE
    /// injecting episodes into the worker's prompt. Without it, workers
    /// fell back to the unfiltered cwd-only path in
    /// `EpisodeStore::find_relevant` and pulled in cross-domain
    /// episodes (e.g. a JWST research prompt rendered with an Apple
    /// CEO / GPT-5.5 podcast episode on mini5).
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Attach a [`CostAccountant`] (coding-blue FA-7). When set, pipeline
    /// executions reserve budget against the configured contract id and
    /// commit the cumulative token attribution at pipeline terminal.
    pub fn with_cost_accountant(mut self, accountant: Arc<CostAccountant>) -> Self {
        self.cost_accountant = Some(accountant);
        self
    }

    /// Set the logical contract id for the cost ledger rollups
    /// associated with this tool. Defaults to the pipeline graph id.
    pub fn with_contract_id(mut self, contract_id: impl Into<String>) -> Self {
        self.contract_id = Some(contract_id.into());
        self
    }

    /// Build the [`PipelineContext`] for a single invocation.
    ///
    /// Reads the workspace policy from `self.working_dir` when present
    /// and attaches the tool's LLM provider for LLM-iterative
    /// compaction. When no policy is found the context is empty —
    /// legacy behaviour intact. This is the adoption path for the
    /// slides + site delivery workflows: a workspace with a
    /// `workspace_policy.toml` automatically opts into terminal
    /// validators + per-node compaction on every `run_pipeline` call
    /// without threading new constructor args.
    /// Build the pipeline workspace context, preferring the parent
    /// session's `CostAccountant` from [`PipelineHostContext`] over the
    /// tool's locally configured one. Keeps the pipeline ledger
    /// attribution consistent with the parent session's accountant when
    /// the tool runs inside a session actor (M8 parity W1.A4).
    fn build_workspace_context_with_host(
        &self,
        host: &crate::host_context::PipelineHostContext,
        effective_working_dir: &std::path::Path,
    ) -> PipelineContext {
        // Phase 2-A (codex review of #1203, P2) — when a scoped run
        // overrides `working_dir` onto a per-session workspace, the
        // workspace policy (validators + compaction) may live under
        // that scope dir, NOT the profile-level tool root. AppUI /
        // runtime sessions provision the policy file inside the
        // session workspace; reading from `self.working_dir` (the
        // profile root) would miss it and the session would run
        // without its declared validators or compaction policy.
        //
        // Resolution order: (1) policy under the effective working
        // dir (scope when present), (2) policy under the tool's
        // profile root (legacy / shared policy). Falling back to the
        // profile root preserves pre-Phase-2-A behaviour for
        // non-scoped callers (where `effective_working_dir == self.working_dir`).
        let policy = self
            .read_workspace_policy_for_session(effective_working_dir)
            .or_else(|| self.read_workspace_policy_for_session(&self.working_dir));
        let mut ctx = PipelineContext::new();
        if let Some(policy) = policy {
            ctx = ctx.with_policy(policy);
            ctx = ctx.with_agent_llm_provider(self.default_provider.clone());
        }
        // Prefer the host-context (parent session's) accountant. Falls
        // back to the tool-configured one for non-session callers.
        if let Some(accountant) = host
            .cost_accountant
            .clone()
            .or_else(|| self.cost_accountant.clone())
        {
            ctx = ctx.with_cost_accountant(accountant);
        }
        if let Some(contract_id) = self.contract_id.as_deref() {
            ctx = ctx.with_contract_id(contract_id);
        }
        ctx
    }

    /// Read a workspace policy from a candidate root, downgrading
    /// errors to a WARN + `None` (mirrors the legacy
    /// `build_workspace_context_with_host` behaviour). Lifted out so
    /// the scope-aware lookup can try the session workspace first and
    /// fall back to the profile root without duplicating the
    /// error-handling shape.
    fn read_workspace_policy_for_session(
        &self,
        candidate: &std::path::Path,
    ) -> Option<octos_agent::workspace_policy::WorkspacePolicy> {
        match octos_agent::workspace_policy::read_workspace_policy(candidate) {
            Ok(policy) => policy,
            Err(error) => {
                tracing::warn!(
                    candidate = %candidate.display(),
                    error = %error,
                    "run_pipeline: failed to read workspace policy from candidate root; \
                     trying fallback or running legacy path"
                );
                None
            }
        }
    }

    /// Add the global octos-home skills + pipelines directories as search
    /// paths. This ensures pipelines installed globally (e.g.
    /// `~/.octos/skills/`, `~/.octos/pipelines/`) are discoverable even when
    /// `data_dir` is per-profile (the per-profile discovery default only
    /// searches `<profile_data_dir>/pipelines`, not the shared home).
    ///
    /// Gap 4.1 BLOCKER 3: the bundled generic pipelines written to
    /// `~/.octos/bundled-pipelines/` by `bootstrap_bundled_pipelines` are
    /// registered as a LOWEST-precedence search path (via
    /// `add_bundled_pipelines_dir`), so an installed `deep_research.dot` in
    /// any skills/pipelines location always wins over the bundled fallback.
    pub fn with_octos_home(mut self, octos_home: PathBuf) -> Self {
        self.discovery.add_search_path(octos_home.join("skills"));
        self.discovery.add_search_path(octos_home.join("pipelines"));
        self.discovery.add_bundled_pipelines_dir(&octos_home);
        self
    }

    /// Register `<root>/bundled-pipelines` as the LOWEST-precedence
    /// discovery path. Used by the non-octos-home hosts (`octos chat`,
    /// `octos serve`) that bootstrap the bundle into `<data_dir>/bundled-pipelines`
    /// but do not otherwise call `with_octos_home`. Keeps bootstrap-dir ==
    /// search-dir while preserving installed-wins (BLOCKER 2 + BLOCKER 3).
    pub fn with_bundled_pipelines_root(mut self, root: PathBuf) -> Self {
        self.discovery.add_bundled_pipelines_dir(&root);
        self
    }

    pub fn with_provider_router(mut self, router: Arc<ProviderRouter>) -> Self {
        self.provider_router = Some(router);
        self
    }

    pub fn with_provider_policy(mut self, policy: Option<ToolPolicy>) -> Self {
        self.provider_policy = policy;
        self
    }

    pub fn with_plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.plugin_dirs = dirs;
        self
    }

    /// Section B (codex review P1.1): opt into strict signature
    /// enforcement for pipeline-spawned plugin loads. Inherited from
    /// `plugins.require_signed` on the host config.
    pub fn with_plugin_require_signed(mut self, require_signed: bool) -> Self {
        self.plugin_require_signed = require_signed;
        self
    }

    /// Build a model catalog string for the LLM, showing each model's key,
    /// output capacity, context window, and cost.
    /// Resolve a `pipeline` argument to runnable DOT.
    ///
    /// Both unsafe authoring surfaces are **rejected** here: free-form inline
    /// DOT (a model could request arbitrary tools/handlers incl. `shell`, or an
    /// empty tool-list that silently expanded to all builtins) AND caller-
    /// supplied file PATHS (a model could write `/tmp/pwn.dot` with
    /// `handler=shell` and feed the path — the same arbitrary surface via
    /// `PipelineDiscovery`'s direct-path read). This method accepts ONLY a bare
    /// sanctioned pipeline NAME, resolved through discovery + the embedded
    /// bundled bytes. Agents author ad-hoc work via the capability-locked `ir`.
    async fn resolve_with_fallback(&self, pipeline_str: &str) -> Result<String> {
        if looks_like_inline_dot(pipeline_str) {
            eyre::bail!(INLINE_DOT_REJECTION);
        }
        let name = pipeline_str.trim();
        if !is_bare_pipeline_name(name) {
            eyre::bail!(PIPELINE_PATH_REJECTION);
        }
        // Resolve the TRIMMED name so a whitespace-padded input can't miss an
        // installed copy and let the embedded fallback out-rank installed-wins.
        self.resolve_named_with_bundled_fallback(name).await
    }

    /// Resolve a bare sanctioned pipeline NAME to a runnable program, preferring
    /// the capability-locked typed-IR rebuild of a sanctioned pipeline over the
    /// embedded DOT. Precedence:
    /// 1. an operator-INSTALLED copy in a skill/user dir (installed-wins);
    /// 2. the bundled IR (canonical sanctioned rebuild, e.g. `deep_research`);
    /// 3. the embedded bundled DOT (legacy fallback).
    ///
    /// Inline DOT and file paths are rejected (same as `resolve_with_fallback`).
    async fn resolve_named(&self, pipeline_str: &str) -> Result<ResolvedPipeline> {
        if looks_like_inline_dot(pipeline_str) {
            eyre::bail!(INLINE_DOT_REJECTION);
        }
        let name = pipeline_str.trim();
        if !is_bare_pipeline_name(name) {
            eyre::bail!(PIPELINE_PATH_REJECTION);
        }
        // 1. Operator-installed copy wins (skill dirs, NOT the bundled dir).
        if let Some(dot) = self.discovery.resolve_installed(name).await? {
            return Ok(ResolvedPipeline::Dot(dot));
        }
        // 2. Bundled IR — the canonical, audited rebuild.
        if let Some(ir) = octos_agent::bundled_pipelines::bundled_ir(name) {
            return Ok(ResolvedPipeline::Ir(ir.to_string()));
        }
        // 3. Embedded bundled DOT (discovery full search + embedded bytes).
        Ok(ResolvedPipeline::Dot(
            self.resolve_named_with_bundled_fallback(name).await?,
        ))
    }

    /// Resolve a pipeline by name/path via on-disk discovery first, falling
    /// back to the EMBEDDED bundled `.dot` bytes (compiled into the binary
    /// via `octos_agent::bundled_pipelines`) when discovery cannot find it.
    ///
    /// Gap 4.1 NIT 2 — the `run_pipeline` enum advertises the sanctioned
    /// `deep_research` name unconditionally (it is bundled into the binary).
    /// Blockers 2+3 make bootstrap write that `.dot` to a discoverable dir on
    /// every host path, but a degraded filesystem (read-only, quota) could
    /// still leave discovery empty. Without this in-memory fallback the enum
    /// would advertise a name the tool cannot resolve — a masking lie that
    /// `pre_flight_validate` turns into a runtime failure. With it, every
    /// advertised name resolves: advertise == resolvable on all paths.
    ///
    /// Discovery still WINS when it finds a match, so an installed/operator
    /// `deep_research.dot` overrides the embedded copy (installed-wins is
    /// preserved — the embedded bytes are strictly a last resort).
    async fn resolve_named_with_bundled_fallback(&self, name_or_path: &str) -> Result<String> {
        match self.discovery.resolve(name_or_path).await {
            Ok(dot) => Ok(dot),
            Err(discovery_err) => {
                // Gap 4.1 (codex review): the embedded fallback may fire ONLY
                // on a TRUE discovery miss (`PipelineResolveError::NotFound`).
                // If discovery LOCATED an installed candidate but failed to
                // read/parse it (`PipelineResolveError::Read`, or any other
                // error kind), propagate that error — falling back would MASK
                // the broken install and let the bundled copy out-rank a
                // present installed pipeline ("fallback only on a true miss /
                // can never out-rank an installed pipeline").
                let is_true_miss = matches!(
                    discovery_err.downcast_ref::<crate::discovery::PipelineResolveError>(),
                    Some(crate::discovery::PipelineResolveError::NotFound { .. })
                );
                if !is_true_miss {
                    return Err(discovery_err);
                }

                // Match against the embedded bundled pipelines by bare name.
                // Gap 4.1 BLOCKER 2: canonicalize the input to the SAME bare
                // file stem discovery uses (strip any directory + trailing
                // `.dot`) so `deep_research` and `deep_research.dot` match the
                // embedded bytes identically — and, critically, so this
                // fallback only runs on a TRUE discovery miss for either form
                // (when an installed copy exists, discovery now resolves both
                // forms and this branch is never reached → installed-wins).
                let want = crate::discovery::pipeline_name_stem(name_or_path.trim());
                for &(file_name, dot) in octos_agent::bundled_pipelines::BUNDLED_PIPELINES {
                    let stem = file_name.strip_suffix(".dot").unwrap_or(file_name);
                    if want == stem {
                        tracing::info!(
                            pipeline = want,
                            "resolved pipeline from embedded bundled bytes (discovery miss; \
                             likely a degraded/read-only bootstrap dir)"
                        );
                        return Ok(dot.to_string());
                    }
                }
                Err(discovery_err)
            }
        }
    }

    /// Set the status bridge for the current message.
    /// Called per-message to connect pipeline progress to the messaging channel's
    /// StatusIndicator (status words + token tracker).
    pub fn set_status_bridge(&self, bridge: PipelineStatusBridge) {
        *self.status_bridge.lock().unwrap_or_else(|e| e.into_inner()) = Some(bridge);
    }

    /// Doc-hidden test accessor — confirms the embedder propagation
    /// path is wired (NEW-06 regression guard). The pipeline worker
    /// memory-recall threshold gate only runs when an embedder is
    /// present; this lets tests assert the constructor + builder paths
    /// keep it threaded through.
    #[doc(hidden)]
    pub fn embedder_for_test(&self) -> Option<&Arc<dyn EmbeddingProvider>> {
        self.embedder.as_ref()
    }

    /// Doc-hidden test accessor — resolves a pipeline name to its DOT body
    /// through the same discovery + embedded-bundled-fallback path
    /// `pre_flight_validate` / `execute` use. Lets the Gap 4.1 tests assert
    /// installed-wins and the bundled fallback at the resolution boundary
    /// (which exact `.dot` content wins) without standing up a full run.
    #[doc(hidden)]
    pub async fn resolve_named_for_test(&self, name_or_path: &str) -> Result<String> {
        self.resolve_with_fallback(name_or_path).await
    }

    /// Test-only: which form a named pipeline resolves to — `"ir"` (bundled IR,
    /// composed via the safe palette) or `"dot"` (installed/embedded DOT).
    #[doc(hidden)]
    pub async fn resolve_named_kind_for_test(&self, name: &str) -> Result<&'static str> {
        Ok(match self.resolve_named(name).await? {
            ResolvedPipeline::Ir(_) => "ir",
            ResolvedPipeline::Dot(_) => "dot",
        })
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    pipeline: String,
    input: String,
    /// S1-5: an optional typed-IR workflow program (JSON). When present and the
    /// tool has IR enabled, it is compiled to a capability-locked
    /// [`crate::graph::PipelineGraph`] and run instead of a named pipeline.
    #[serde(default)]
    ir: Option<String>,
    #[serde(default)]
    variables: serde_json::Map<String, serde_json::Value>,
    /// Pipeline-level timeout in seconds. Default: 1800 (30 min),
    /// optionally overridden per-pipeline via the DOT graph attribute
    /// `default_timeout_secs`. Clamped to [60, 3600].
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Hard wall-clock floor on `run_pipeline` (seconds).
///
/// Below this value the pipeline cannot complete even the smallest
/// 2-node graph reliably; we clamp up so a careless caller never disarms
/// the timeout entirely.
const PIPELINE_TIMEOUT_MIN_SECS: u64 = 60;

/// Hard wall-clock ceiling on `run_pipeline` (seconds, NEW-15).
///
/// Raised from 1800 → 3600 to give honest deep research with crawl +
/// synthesize on slow production LLM lanes (e.g. wisemodel
/// kimi/MiniMax) room to finish without synthesize-node starvation.
/// Anything above this is treated as a runaway and clamped — operators
/// can still observe the original requested value in the bridge logs,
/// they just don't get more than an hour per spawn_only invocation.
const PIPELINE_TIMEOUT_MAX_SECS: u64 = 3600;

/// Hard-coded fallback when neither the LLM nor the DOT graph specify
/// a timeout. Kept at 1800s for byte-identical backward-compat with
/// pre-NEW-15 callers.
const PIPELINE_TIMEOUT_DEFAULT_SECS: u64 = 1800;

/// Resolve the effective wall-clock timeout for a `run_pipeline` run.
///
/// Precedence: LLM-supplied > DOT-graph default > hard-coded 1800s.
/// Always clamped to [`PIPELINE_TIMEOUT_MIN_SECS`,
/// `PIPELINE_TIMEOUT_MAX_SECS`].
///
/// Extracted so the resolution policy can be unit-tested without
/// constructing a full `RunPipelineTool` + `TOOL_CTX`.
fn resolve_pipeline_timeout(llm_value: Option<u64>, dot_default: Option<u64>) -> u64 {
    llm_value
        .or(dot_default)
        .unwrap_or(PIPELINE_TIMEOUT_DEFAULT_SECS)
        .clamp(PIPELINE_TIMEOUT_MIN_SECS, PIPELINE_TIMEOUT_MAX_SECS)
}

/// Returned when an agent passes a free-form inline DOT graph to
/// `run_pipeline`. Free-form DOT was the unsafe legacy authoring surface — the
/// model could name arbitrary tools/handlers (incl. `shell`) or an empty
/// tool-list that silently expanded to all builtins. Agents now author via the
/// capability-locked `ir` palette, or name a sanctioned pipeline.
const INLINE_DOT_REJECTION: &str = "inline DOT graphs are not accepted: free-form DOT was the unsafe legacy \
     authoring surface and has been removed. To run a multi-step workflow, \
     either name a sanctioned pipeline (e.g. `deep_research`) in `pipeline`, or \
     compose a typed-IR workflow program in `ir`.";

/// Returned when an agent supplies a file PATH (rather than a bare sanctioned
/// name) to `run_pipeline`. A caller-supplied `.dot` path would let a model
/// smuggle the same arbitrary handler/tool surface that inline DOT did (e.g. a
/// written `/tmp/pwn.dot` with `handler=shell`) through direct-path resolution.
const PIPELINE_PATH_REJECTION: &str = "pipeline file paths are not accepted: name a sanctioned pipeline (e.g. \
     `deep_research`) — a bare name, not a path — or compose a typed-IR workflow \
     program in `ir`.";

/// A sanctioned pipeline NAME is a bare identifier (ASCII alphanumerics plus
/// `_`/`-`). Anything containing a path separator, `.` (so `.dot`/`./`/`..`),
/// whitespace, or other characters is a path/expression and is rejected, so a
/// model cannot point `run_pipeline` at an arbitrary on-disk `.dot` file.
fn is_bare_pipeline_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Outcome of resolving a sanctioned pipeline NAME: a typed-IR program (composed
/// via the safe palette) or a DOT graph string (parsed).
enum ResolvedPipeline {
    Ir(String),
    Dot(String),
}

/// True when `s` looks like an inline DOT digraph rather than a pipeline
/// name/path. Conservative on the leading token so a real name is never
/// mistaken for DOT; fenced/garbled DOT that slips past simply fails name
/// resolution downstream (still rejected, just with a less specific message).
fn looks_like_inline_dot(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("digraph ")
        || t.starts_with("digraph{")
        || t.starts_with("digraph\t")
        || t.starts_with("digraph\n")
        || t.starts_with("digraph\r")
}

#[async_trait]
impl Tool for RunPipelineTool {
    fn name(&self) -> &str {
        "run_pipeline"
    }

    fn description(&self) -> &str {
        if self.ir_enabled {
            "Run a multi-step pipeline, either by NAME or by composing one. \
             (a) Name a sanctioned pipeline (`deep_research`) in `pipeline`. \
             ALWAYS use `deep_research` for an in-depth / comprehensive / \
             multi-source research request — e.g. \"deep research X\", \"research \
             and write a report on Y\", \"thoroughly investigate Z\". Do NOT \
             answer such a request with a single inline `web_search`/`web_fetch`: \
             that is a shallow one-angle pass; `deep_research` fans out PARALLEL \
             searches across multiple distinct angles and synthesizes a cited \
             report. Reserve inline `web_search` for a quick single-fact lookup. \
             `deep_research` is WEB-ONLY: it has no access to your repository, so \
             NEVER use it for code review, local-codebase analysis, or debugging \
             (\"investigate this test failure\", \"audit this code\") — answer \
             those directly with the local file/shell tools (`read_file`, \
             `grep`, `glob`, `list_dir`, `shell`). \
             (b) For an ad-hoc multi-step task, compose your own workflow as a \
             typed-IR program in `ir`: a closed, capability-safe palette of node \
             kinds (research, transform, synthesize, report, gate, fanout). You \
             choose the kinds, their prompts, and how they connect — capability \
             (tools/model) is fixed per kind, so you never request shell or tools \
             directly. Use `ir` to offload research→synthesize or parallel \
             fan-out→converge work to the harness. If composition is invalid the \
             tool returns the exact errors — fix the `ir` and call again."
        } else {
            "Run a sanctioned multi-step pipeline by NAME. The only currently \
             sanctioned pipeline is `deep_research`, which performs MULTI-SOURCE \
             WEB-RESEARCH SYNTHESIS: it fans out PARALLEL web-search workers \
             across distinct angles and synthesizes a source-citing report. \
             ALWAYS use `deep_research` for an in-depth / comprehensive / \
             multi-source research request — e.g. \"deep research X\", \"research \
             and write a report on Y\", \"investigate Z thoroughly\". Do NOT \
             answer such a request with a single inline `web_search`/`web_fetch`: \
             that is a shallow one-angle pass that misses the parallel-angle \
             coverage + synthesis the pipeline provides. Reserve inline \
             `web_search` for a quick single-fact lookup. \
             deep_research MUST NOT be used for code review, local-codebase \
             analysis, debugging, or anything answerable from the files already \
             in the working directory — it has no access to your repository and \
             will fabricate or recall unrelated material. For those tasks do NOT \
             call run_pipeline at all; answer directly with the local tools \
             (`read_file`, `grep`, `glob`, `list_dir`, `shell`). Likewise do NOT \
             compose your own inline DOT graph for ad-hoc tasks (slides, media, \
             code edits, partial regenerations, etc.) — those have purpose-built \
             tools (`mofa_slides`, `podcast_generate`, etc.). If no purpose-built \
             tool exists for what the user asked, surface that as a limitation \
             rather than improvising a custom pipeline or force-fitting \
             deep_research."
        }
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        let pipeline_desc = "Name of the sanctioned pipeline to run. The only currently \
             sanctioned name is `deep_research`, which is for MULTI-SOURCE \
             WEB-RESEARCH SYNTHESIS ONLY (parallel web-search workers + a \
             cited synthesis). PREFER `deep_research` over a single inline \
             `web_search`/`web_fetch` for any in-depth, comprehensive, or \
             multi-source research request (\"deep research X\", \"research and \
             write a report on Y\", \"investigate Z thoroughly\") — one inline \
             search is a shallow one-angle pass, whereas the pipeline fans out \
             parallel angles and synthesizes a cited report; reserve inline \
             search for a quick single-fact lookup. `deep_research` MUST NOT be \
             selected for code \
             review, local-codebase analysis, debugging, or any task \
             answerable from the working directory — those are NOT web \
             research; answer them directly with the local file/shell tools \
             (`read_file`, `grep`, `glob`, `list_dir`, `shell`) instead of \
             calling run_pipeline. Do NOT pass an inline DOT graph here — \
             free-form DOT was the unsafe legacy contract and is now REJECTED; \
             this field accepts only a sanctioned pipeline name. For an ad-hoc \
             multi-step workflow, compose a typed-IR program in `ir` instead. \
             If you find yourself wanting to compose \
             your own DOT, the correct response is to use the purpose-built \
             tool for that domain (`mofa_slides` for slides, \
             `podcast_generate` for podcasts, `voice_synthesize` for TTS, \
             etc.), or tell the user no such tool exists for their request."
            .to_string();

        // Gap 4.1: advertise the LIVE discovery list, not a hard-coded
        // `["deep_research"]`. The old static enum lied when a profile had
        // extra installed/bundled pipelines (the model couldn't name them)
        // and lied the other way when `deep_research` had drifted off the
        // profile (the model emitted a name that resolved to
        // `Available: (none)`). Populating from `list_available()` keeps the
        // advertised choices in lock-step with what `resolve()` can actually
        // find. No-discovery fallback keeps the sanctioned generic
        // `deep_research` baseline (it is bundled into the binary), so the
        // enum is never empty and the model always has the generic pipeline.
        // Only advertise names that `resolve_with_fallback` will accept (bare
        // sanctioned names), so advertise == resolvable: a discovered stem with
        // a dot/space/etc. would otherwise be advertised yet rejected as a path.
        let mut pipeline_names: Vec<String> = self
            .discovery
            .list_available()
            .into_iter()
            .map(|p| p.name)
            .filter(|n| is_bare_pipeline_name(n))
            .collect();
        if !pipeline_names.iter().any(|n| n == FALLBACK_PIPELINE_NAME) {
            pipeline_names.push(FALLBACK_PIPELINE_NAME.to_string());
        }

        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "pipeline": {
                    "type": "string",
                    "description": pipeline_desc,
                    "enum": pipeline_names
                },
                "input": {
                    "type": "string",
                    "description": "The input query or task description for the pipeline"
                },
                "variables": {
                    "type": "object",
                    "description": "Optional key-value pairs for template substitution in node prompts",
                    "additionalProperties": { "type": "string" }
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds. Estimate from real production execution times (kimi/MiniMax on wisemodel — frontier models may be 2-3x faster):\n- simple 2-node pipeline ~3min → 300s\n- standard 3-node research pipeline ~10min → 800s\n- 5-7 topic deep research with crawl+synthesize ~25-30min → 1800s\n- complex multi-source analysis with many nodes ~40-50min → 3000s\n- exhaustive deep research with broad fan-out ~60min → 3600s\nMax: 3600. Default: 1800 (per-pipeline DOT may override via the `default_timeout_secs` graph attribute). Prefer higher estimates on production LLM lanes — under-estimating causes synthesize-node starvation when plan_and_search consumes >60% of the wall-clock budget."
                }
            },
            "required": ["pipeline", "input"]
        });
        if self.ir_enabled {
            schema["properties"]["ir"] = serde_json::json!({
                "type": "string",
                "description": "A typed-IR workflow as a JSON string. Shape: {\"id\":\"<name>\",\"nodes\":[{\"id\":\"<nid>\",\"kind\":<KIND>}],\"edges\":[{\"source\":\"a\",\"target\":\"b\",\"condition\":\"<opt>\"}]}. <KIND> is EXACTLY one of (tagged by \"type\", no other fields): {\"type\":\"research\",\"prompt\":\"...\"} (web+file read), {\"type\":\"transform\",\"prompt\":\"...\"}, {\"type\":\"synthesize\",\"prompt\":\"...\"} (read-only writeup), {\"type\":\"report\",\"prompt\":\"...\"} (final writeup that SAVES a file via write_file), {\"type\":\"gate\"} (pure routing; conditions on edges), {\"type\":\"fanout\",\"worker_prompt\":\"... {task} ...\",\"converge\":\"<nid>\"} (optional \"plan_prompt\" customizes the task planner; workers get web_search/web_fetch/read_file). There are no tools/handler/model fields — capability is fixed per kind. Execution walks a SINGLE path: each non-fanout node hands off to exactly ONE next node. Routing from a node with several outgoing edges: the executor takes a matching `condition` edge, and if none match (or several tie) it falls through to the lowest target id — it never stops. So put `condition`s on the branch edges AND include exactly one unconditional default edge as the catch-all. The ONLY parallelism is `fanout` — it runs workers then continues at `converge` (which also needs an edge into it). Do NOT author diamond fan-ins (e.g. a→c and b→c expecting both a and b to finish first) — only one path runs. Examples: (1) linear research→report — {\"id\":\"demo\",\"nodes\":[{\"id\":\"research\",\"kind\":{\"type\":\"research\",\"prompt\":\"Research the topic; list 5 key facts each with a source URL\"}},{\"id\":\"report\",\"kind\":{\"type\":\"report\",\"prompt\":\"Write a cited report from the findings and save it with write_file\"}}],\"edges\":[{\"source\":\"research\",\"target\":\"report\"}]}. (2) parallel fan-out — {\"id\":\"demo2\",\"nodes\":[{\"id\":\"plan\",\"kind\":{\"type\":\"research\",\"prompt\":\"Identify the sub-topics to cover\"}},{\"id\":\"work\",\"kind\":{\"type\":\"fanout\",\"worker_prompt\":\"Investigate {task}\",\"converge\":\"final\"}},{\"id\":\"final\",\"kind\":{\"type\":\"report\",\"prompt\":\"Synthesize the findings into a report and save it with write_file\"}}],\"edges\":[{\"source\":\"plan\",\"target\":\"work\"},{\"source\":\"work\",\"target\":\"final\"}]}."
            });
            schema["required"] = serde_json::json!(["input"]);
        }
        schema
    }

    /// Synchronously parse and structurally validate the DOT graph before
    /// the spawn_only intercept dispatches the actual run to the background.
    ///
    /// Without this pre-flight, an LLM-generated invalid DOT (e.g. multiple
    /// dangling roots → `rule 1: ambiguous start`) failed inside the
    /// background task and surfaced only as a user-visible error bubble —
    /// the agent's foreground turn already returned "started in background"
    /// to the LLM, so the model thought it succeeded and never retried.
    /// Catching the bad shape here turns the failure into a tool_result the
    /// LLM can react to in its next iteration.
    ///
    /// Scope is deliberately limited to `parse_dot` + the same `validate::`
    /// lint pass the executor runs — model assignment is skipped because
    /// the topology checks (`ambiguous start`, dangling refs, etc.) are
    /// what the LLM gets wrong; model fields are auto-filled by the
    /// executor and never the failure source.
    async fn pre_flight_validate(&self, args: &serde_json::Value) -> Result<(), String> {
        let input: Input = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid run_pipeline input: {e}"))?;
        // S1-5: when an IR program is supplied (and enabled), the pre-flight is
        // a compose() — the same parse/compile/cycle/profile gates the run uses,
        // surfaced synchronously so a malformed IR fails the foreground turn
        // (the LLM can repair) instead of dead-ending in the spawn_only task.
        if self.ir_enabled {
            if let Some(ir) = input.ir.as_deref().filter(|s| !s.trim().is_empty()) {
                return crate::compose::compose(
                    ir,
                    &crate::profile::ValidationProfile::l2_default(),
                    &input.variables,
                )
                .map(|_| ())
                .map_err(|e| format!("IR validation failed:\n{}", e.feedback_lines().join("\n")));
            }
        }
        // Free-form inline DOT is rejected (unsafe legacy surface) — surface the
        // same actionable message the run path returns, synchronously, so the
        // LLM sees it in the foreground turn rather than as a spawn_only failure.
        if looks_like_inline_dot(&input.pipeline) {
            return Err(INLINE_DOT_REJECTION.to_string());
        }
        let dot_content = match self.resolve_named(&input.pipeline).await {
            Ok(ResolvedPipeline::Ir(ir)) => {
                // A bundled IR is validated by compose() itself — return its
                // structured feedback synchronously.
                return crate::compose::compose(
                    &ir,
                    &crate::profile::ValidationProfile::l2_default(),
                    &input.variables,
                )
                .map(|_| ())
                .map_err(|e| {
                    format!(
                        "bundled pipeline IR failed to compose:\n{}",
                        e.feedback_lines().join("\n")
                    )
                });
            }
            Ok(ResolvedPipeline::Dot(dot)) => dot,
            Err(e) => return Err(format!("failed to resolve pipeline: {e}")),
        };
        let graph = crate::parser::parse_dot(&dot_content)
            .map_err(|e| format!("failed to parse pipeline DOT: {e}"))?;
        let validation_context = crate::validate::ValidationContext::default()
            .with_runtime_variables(input.variables.keys().cloned())
            .with_known_models(crate::model_assignment::known_model_keys_from_catalog_dir(
                &self.working_dir,
            ))
            // codex pre-merge P2: include plugin tool names so a graph that
            // allow-lists a legitimate plugin tool isn't rejected by Rule 19 in
            // preflight (runs BEFORE the plugin-aware executor). Shares logic
            // with `PipelineExecutor::validation_context`; loads plugins only
            // when the graph actually references a non-built-in tool.
            .with_known_tools(crate::validate::known_tool_names_with_plugins(
                &self.working_dir,
                &self.plugin_dirs,
                self.plugin_require_signed,
                &crate::validate::referenced_tool_entries(&graph),
            ));
        let diags = crate::validate::diagnostics_with_context(&graph, &validation_context);
        if crate::validate::has_errors(&diags) {
            // codex pre-merge DO-NOT-SHIP: this spawn_only early-return is
            // returned verbatim as a tool Message (agent execution.rs) BEFORE
            // both the per-tool truncation AND the run_pipeline body/footer
            // ceiling. An unbounded join of validation errors (many errors, or
            // huge node IDs in a malformed DOT) would therefore emit an
            // oversized tool result. Bound the message at the source.
            return Err(format_bounded_preflight_errors(&diags));
        }
        Ok(())
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid run_pipeline input")?;

        let using_ir = self.ir_enabled && input.ir.as_deref().is_some_and(|s| !s.trim().is_empty());

        // Free-form inline DOT is no longer an agent-authorable surface. Reject
        // it up front with an actionable message (mirrors the IR compose-error
        // path) rather than letting it reach the parser. When IR is in use the
        // `pipeline` field is ignored, so only guard the DOT path.
        if !using_ir && looks_like_inline_dot(&input.pipeline) {
            return Ok(ToolResult {
                success: false,
                output: INLINE_DOT_REJECTION.to_string(),
                ..Default::default()
            });
        }

        tracing::info!(
            using_ir,
            pipeline_arg = if using_ir {
                "(ir)"
            } else {
                input.pipeline.as_str()
            },
            "run_pipeline invoked"
        );

        // #1020 / M17-B: capture run start so we can stamp the summary's
        // `start_time` field with the same instant the pipeline launched.
        // RFC3339 keeps the audit-trail JSON human-readable.
        let run_started_at = std::time::SystemTime::now();
        let run_start_rfc3339 = systemtime_to_rfc3339(run_started_at);
        let pipeline_started = std::time::Instant::now();

        // S1-5: obtain the executable graph from either the typed-IR program
        // (compiled + capability-locked) or a named/inline DOT pipeline. The
        // entire downstream (config, timeout, summary, files_to_send, spawn_only
        // delivery) is graph-agnostic, so only acquisition differs.
        let (graph, graph_id): (crate::graph::PipelineGraph, String) = if using_ir {
            let ir = input.ir.as_deref().unwrap_or_default();
            match crate::compose::compose(
                ir,
                &crate::profile::ValidationProfile::l2_default(),
                &input.variables,
            ) {
                Ok(g) => {
                    let id = g.id.clone();
                    (g, id)
                }
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: format!(
                            "IR compose failed — fix the workflow and call run_pipeline again:\n{}",
                            e.feedback_lines().join("\n")
                        ),
                        ..Default::default()
                    });
                }
            }
        } else {
            // A named pipeline resolves to a bundled IR (composed via the safe
            // palette) or a DOT graph; the canonical sanctioned pipelines (e.g.
            // `deep_research`) now ship as IR and run the audited palette.
            match self.resolve_named(&input.pipeline).await {
                Ok(ResolvedPipeline::Ir(ir)) => match crate::compose::compose(
                    &ir,
                    &crate::profile::ValidationProfile::l2_default(),
                    &input.variables,
                ) {
                    Ok(g) => {
                        let id = g.id.clone();
                        (g, id)
                    }
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: format!(
                                "bundled pipeline IR failed to compose:\n{}",
                                e.feedback_lines().join("\n")
                            ),
                            ..Default::default()
                        });
                    }
                },
                Ok(ResolvedPipeline::Dot(dot)) => {
                    let graph =
                        crate::parser::parse_dot(&dot).wrap_err("failed to parse pipeline DOT")?;
                    let id = graph_id_from_dot(&dot);
                    (graph, id)
                }
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: e.to_string(),
                        ..Default::default()
                    });
                }
            }
        };

        let status_bridge = self
            .status_bridge
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();

        // Shutdown signal for cancelling all pipeline workers on timeout/drop.
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // M8 parity (W1.A1/A3/A4): pull the parent session's shared
        // FileStateCache, SubAgentOutputRouter, AgentSummaryGenerator,
        // TaskSupervisor, and CostAccountant from TOOL_CTX so pipeline
        // workers inherit them via the M8 contract instead of
        // constructing fresh per-run handles. Falls back to whatever
        // self holds when the tool is invoked outside of a session
        // (e.g. unit tests).
        let host_context = octos_agent::tools::TOOL_CTX
            .try_with(crate::host_context::PipelineHostContext::from_tool_context)
            .unwrap_or_default();

        // Phase 2-A: resolve the effective working dir for the
        // executor. When the host context carries a [`SessionScope`]
        // (Phase 1 wiring), every per-node worker spawns into the
        // session's ephemeral workspace dir instead of the tool-level
        // (profile-level) `data/` dir. The helper also ensures the
        // workspace exists on disk before any worker tries to CWD into
        // it. When no scope is present we fall back to the tool's
        // working dir — byte-for-byte identical to pre-Phase-2-A
        // behaviour.
        let shared_working_dir =
            resolve_pipeline_working_dir(&self.working_dir, host_context.session_scope.as_deref());

        // Per-run working-directory isolation: node workers (search fan-out,
        // analyze, synthesize) follow a "read every findings-*.md" deliverable
        // contract. When all runs share one flat working dir, findings files
        // from UNRELATED runs accumulate (research A/B, judge probes, ad-hoc
        // tests) and the convergence nodes try to read all of them — the
        // observed 1800s synthesize timeouts (direct evidence: the shared dir
        // held 27 findings files spanning multiple runs, and the analyze node's
        // prompt instructs reading each one). Scope each run to its own
        // subdirectory so a run's workers only ever see their own findings.
        //
        // `graph_id` is already computed above (IR id or `graph_id_from_dot`)
        // and `run_started_at` was captured at entry, so the run-scoped dir is
        // unique per invocation without re-deriving a timestamp. This is
        // additive-only: nothing is deleted from the shared dir, and final
        // report files written via write_file land in the run dir (callers
        // that surface the report read it back from the same executor run, so
        // delivery is unaffected).
        let run_id = generate_run_id(&graph_id, run_started_at);
        let runs_root = shared_working_dir.join("pipeline-runs");
        let run_working_dir = runs_root.join(&run_id);
        let effective_working_dir = if let Err(e) = std::fs::create_dir_all(&run_working_dir) {
            // Fall back to the shared dir on mkdir failure rather than fail
            // the run — isolation is a hygiene improvement, not a hard
            // requirement for correctness.
            tracing::warn!(
                error = %e,
                dir = %run_working_dir.display(),
                "pipeline: failed to create per-run working dir; falling back to shared dir"
            );
            shared_working_dir.clone()
        } else {
            // Keep a stable `pipeline-runs/latest` entry pointing at the most
            // recent run dir so follow-up `read_file findings-3.md` (relative
            // to the session workspace) and human browsing still find the
            // files at a predictable path. On Unix use a symlink; on platforms
            // where symlinks need privilege (Windows), a plain text pointer
            // file is the portable fallback.
            update_latest_run_link(&runs_root, &run_id, &run_working_dir);
            // Bound unbounded growth: retain only the most recent run dirs.
            prune_old_run_dirs(&runs_root, MAX_RETAINED_RUN_DIRS);
            run_working_dir
        };

        // Build the workspace_context BEFORE moving `effective_working_dir`
        // into the struct literal — the policy lookup reads from the
        // effective root (scope-aware) so scoped sessions pick up
        // validators + compaction declared inside their workspace.
        let workspace_context =
            self.build_workspace_context_with_host(&host_context, &effective_working_dir);

        let config = ExecutorConfig {
            default_provider: self.default_provider.clone(),
            provider_router: self.provider_router.clone(),
            memory: self.memory.clone(),
            working_dir: effective_working_dir.clone(),
            provider_policy: self.provider_policy.clone(),
            plugin_dirs: self.plugin_dirs.clone(),
            plugin_require_signed: self.plugin_require_signed,
            status_bridge,
            shutdown: shutdown.clone(),
            max_parallel_workers: 8,
            max_pipeline_fanout_total: None,
            guards: Vec::new(),
            max_concurrent_llm_calls: None,
            checkpoint_store: None,
            hook_executor: None,
            // coding-blue FA-7: adopt workspace-contract enforcement.
            // Reads the policy from the working dir on every call so
            // the slides + site delivery workflows (and any other
            // opted-in workflow) get validator + compaction + cost
            // reservation for free. When no policy is present the
            // context is empty and the executor stays on the legacy
            // path.
            workspace_context,
            host_context,
            // NEW-06 fix: thread the parent embedder onto every pipeline
            // worker Agent so episodic memory recall stays on the
            // contamination-safe hybrid scored + filtered path. When
            // unset (legacy callers or hosts without an embedder
            // configured), workers stay on the cwd-only fallback path —
            // identical to pre-fix behaviour.
            embedder: self.embedder.clone(),
            // Phase 2-A (codex review of #1203, P2) — keep model
            // catalog / `pipeline_models.json` reads anchored to the
            // PROFILE data dir even when `working_dir` was swapped to
            // the per-session workspace. Without this split, scoped
            // runs silently lose strong/fast model defaults and cost
            // projections fall back to the minimum estimate.
            catalog_dir: Some(self.working_dir.clone()),
            // #1607 (codex-review follow-up): forward the session sandbox so
            // the terminal / per-node command validators run confined.
            sandbox: self.sandbox.clone(),
        };

        // Pipeline-level timeout resolution (NEW-15):
        // 1. LLM-supplied `timeout_secs` always wins (so an operator can
        //    override a pipeline's baked-in default per-call).
        // 2. Otherwise, fall back to the DOT graph's `default_timeout_secs`
        //    attribute (set by skill authors per-pipeline — e.g.
        //    `deep_research` ships with 2400s because its fan-out shape
        //    consistently exceeds the historical 1800s default on
        //    production LLM lanes).
        // 3. Otherwise, fall back to the hard-coded 1800s.
        // Final value is clamped to [60, 3600] — the upper bound was
        // raised from 1800 → 3600 so honest deep research with crawl +
        // synthesize on slow LLM lanes has room to finish without
        // synthesize-node starvation.
        let dot_default_timeout = graph.default_timeout_secs;
        let timeout_secs = resolve_pipeline_timeout(input.timeout_secs, dot_default_timeout);
        // Gap 3.4: per-pipeline result-size fidelity annotation (if any).
        // `Some(mode)` WINS over the default ceiling; `None` lets the
        // default ceiling degrade an oversized result. Cloned out of the
        // parsed graph here so it survives the executor.run() borrow below.
        let declared_result_fidelity = graph.result_fidelity.clone();

        let executor = PipelineExecutor::new(config);
        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            executor.run_graph(graph, &input.input, &input.variables),
        )
        .await;

        // Signal shutdown to all workers regardless of how we finished
        shutdown.store(true, std::sync::atomic::Ordering::Release);

        // #1126 codex P2: compute the run_id + graph_id BEFORE we
        // branch on success vs timeout. The marker write must happen
        // on the timeout path too (the prior shape only emitted on
        // success), otherwise timed-out runs were the one scenario
        // missing audit-trail evidence — exactly the runs validators
        // most need to inspect.
        // graph_id computed at acquisition (works for both IR and DOT paths).
        // run_id already computed above for per-run working-dir isolation —
        // reuse it so the audit marker and the run dir share one identity.

        let result = match result {
            Ok(inner) => inner?,
            Err(_) => {
                let duration_ms =
                    u64::try_from(pipeline_started.elapsed().as_millis()).unwrap_or(u64::MAX);
                emit_external_context_unmanaged_timeout_summary(
                    &self.working_dir,
                    &run_id,
                    &graph_id,
                    duration_ms,
                    &run_start_rfc3339,
                    timeout_secs,
                );
                // Cascade-fail every still-active `pipeline:<node>` child
                // task registered under this `run_pipeline` invocation.
                // The pipeline's executor calls `supervisor.mark_running`
                // for each node task but only `mark_completed`/`mark_failed`
                // *after* the dispatch returns; on timeout the awaiting
                // future is dropped before either fires and the children
                // stay as `state: "running"` forever.
                let host_context = octos_agent::tools::TOOL_CTX
                    .try_with(crate::host_context::PipelineHostContext::from_tool_context)
                    .unwrap_or_default();
                cascade_fail_orphan_node_tasks(&host_context, timeout_secs);
                // NEW-09: surface the timeout as a `ToolResult { success: false }`
                // rather than `Err(eyre)`. The Err arm of the spawn_only
                // background executor in `octos-agent/src/agent/execution.rs`
                // calls `bg_sender(...)` (so the `message/persisted` /
                // `turn/spawn_complete` bubble IS emitted), but soak round-8
                // observed that the harness's `isFinalArrived` heuristic
                // missed the completion. Returning the failure-with-result
                // shape routes the timeout through the SAME `Ok(r) if
                // !r.success` arm that has been live-tested for every other
                // failing spawn_only tool. The bubble shape becomes
                // `✗ run_pipeline failed: pipeline timed out after Ns`
                // (matching the success-path "failed" wording instead of the
                // Err-path "error" wording), the JSONL row is persisted via
                // the existing failure path, and downstream `read_task_output`
                // reads see the same error text.
                return Ok(build_pipeline_timeout_result(timeout_secs));
            }
        };

        // #1020 / M17-B: stamp the run's `summary.json` with the
        // `external_context_unmanaged` marker so evidence validators can
        // confirm pipeline workers ran without the parent's ContextManager
        // propagated. Failures are logged at WARN and never bubble up:
        // missing audit trail must not regress the user-visible outcome.
        let duration_ms = u64::try_from(pipeline_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        emit_external_context_unmanaged_summary(
            &self.working_dir,
            &run_id,
            &graph_id,
            &result,
            duration_ms,
            &run_start_rfc3339,
        );

        // Per-node summary LINES. The footer that embeds these is appended
        // AFTER the (capped) body and was the last unbounded tail: it iterated
        // ALL node_summaries (each line embeds an arbitrary-length
        // node_id/model), so a many-node pipeline could push the serialized
        // frame past the 1 MiB cap even though the body was bounded. We hand
        // the lines to `bound_footer`, which caps the whole footer's serialized
        // length to its reserved FOOTER_BUDGET_BYTES (with an
        // `[+N more nodes omitted]` marker), closing the last frame_too_large
        // hole.
        let node_lines = result
            .node_summaries
            .iter()
            .map(|n| {
                format!(
                    "- {} ({}): {}ms, {}+{} tokens",
                    n.node_id,
                    n.model.as_deref().unwrap_or("default"),
                    n.duration_ms,
                    n.token_usage.input_tokens,
                    n.token_usage.output_tokens,
                )
            })
            .collect::<Vec<_>>();

        // Gap 3.4 / Blocker 1 — DEGRADE, don't wedge. Bound the result body
        // that becomes the tool-result text (and downstream AppUI frame) by
        // its JSON-SERIALIZED size. An explicit per-pipeline `result_fidelity`
        // annotation WINS; otherwise the default ceiling truncates an
        // oversized result at a UTF-8 boundary so the serialized body+footer
        // stays provably under the 1 MiB `MAX_TEXT_FRAME_BYTES` frame cap,
        // regardless of content (incl. all-control-byte bodies that escape up
        // to 6x). The marker is appended below once the full-output report
        // file name is known. Computed BEFORE report delivery so the
        // `truncated` flag can drive the always-write-full-report decision.
        let ceiling = crate::fidelity::compute_result_ceiling(
            &result.output,
            declared_result_fidelity.as_ref(),
        );

        // Blocker 2 — when the body is truncated, ALWAYS write the synthetic
        // FULL-output report (the untruncated `result.output`) and deliver it,
        // independent of whatever other `.md` the pipeline touched. The
        // truncation marker then points at this report so nothing is lost and
        // the LLM/user knows where the full output is. When not truncated, the
        // prior spawn_only delivery path is unchanged (real `.md` wins; else
        // synthesize a payload so `files_to_send` is non-empty). Keep the
        // synthetic report under `working_dir` so the spawn_only send_file path
        // can deliver it; system temp is outside that allowlist.
        let synthetic_dir = effective_working_dir
            .join("skill-output")
            .join("run_pipeline");
        let delivery = resolve_report_delivery(
            &result.output,
            &result.files_modified,
            ceiling.truncated,
            &synthetic_dir,
        );

        // Append the truncation marker, pointing at the full-output report.
        let bounded_output = ceiling.with_marker(delivery.full_report_name.as_deref());

        // Surface per-node cost attribution in the structured side-channel so
        // the session actor can pull it back into the SSE `done` event for the
        // W1.G4 cost panel. The data was being silently dropped at the tool
        // boundary before we extended `ToolResult` with `structured_metadata`.
        let structured_metadata = node_costs_metadata(&result.node_costs);

        // Bound the per-node footer to its reserved 32 KiB serialized budget so
        // body + marker + footer is provably under the 1 MiB frame cap for ANY
        // number/size of node summaries. `bound_footer` owns the scaffold and
        // `Total:` line so the bound covers the COMPLETE footer.
        let total_line = format!(
            "Total: {} input + {} output tokens",
            result.token_usage.input_tokens, result.token_usage.output_tokens,
        );
        let footer = crate::fidelity::bound_footer(&node_lines, &total_line);

        Ok(ToolResult {
            output: format!("{bounded_output}{footer}"),
            success: result.success,
            tokens_used: Some(result.token_usage),
            file_modified: delivery.report_file,
            files_to_send: delivery.files_to_send,
            structured_metadata,
            named_outputs: None,
        })
    }
}

/// Blocker 2 — the outcome of deciding which file(s) carry a pipeline run's
/// report payload to the spawn_only delivery path, and (when the result body
/// was truncated by the ceiling) where the FULL untruncated output landed.
#[derive(Debug, Default)]
pub(crate) struct ReportDelivery {
    /// The primary report file surfaced as `ToolResult.file_modified` — the
    /// real `.md` if one exists, else the synthesized payload.
    pub report_file: Option<PathBuf>,
    /// The synthetic FULL-output report, written ONLY when the body was
    /// truncated. Holds the untruncated `result.output`. May coexist with a
    /// real `.md` (it is then delivered alongside it).
    pub full_report: Option<PathBuf>,
    /// The set of files auto-delivered to the user (existing files only).
    pub files_to_send: Vec<PathBuf>,
    /// The file NAME of `full_report` (if any) for the truncation marker so
    /// the LLM/user knows where the full output is.
    pub full_report_name: Option<String>,
}

/// Blocker 2 — decide report-file delivery for a pipeline run.
///
/// Invariants:
/// * When `truncated` is true, the synthetic FULL-output report is ALWAYS
///   written (containing `full_output` verbatim) and ALWAYS included in
///   `files_to_send` — independent of whatever other `.md` the pipeline
///   touched. Its name is returned in `full_report_name` so the truncation
///   marker can point at it.
/// * When `truncated` is false, behaviour is the prior spawn_only delivery
///   path: deliver a real `.md` if present, else synthesize a payload from
///   the (untruncated, in-budget) output so `files_to_send` is non-empty. No
///   `full_report`/marker name is produced (the body wasn't truncated, so the
///   `ToolResult.output` already carries the whole result).
///
/// `synthetic_dir` is injected so tests can use a tempdir; production passes
/// `<working_dir>/skill-output/run_pipeline` so spawn_only delivery stays
/// inside the `send_file` allowlist.
pub(crate) fn resolve_report_delivery(
    full_output: &str,
    files_modified: &[PathBuf],
    truncated: bool,
    synthetic_dir: &std::path::Path,
) -> ReportDelivery {
    // Find a real markdown report from this run's files_modified, normalized
    // to an absolute path so the execution loop can find and deliver it.
    let real_report_file = files_modified
        .iter()
        .find(|f| {
            let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.ends_with(".md") && !name.starts_with("_search")
        })
        .map(|f| {
            if f.is_absolute() {
                f.clone()
            } else {
                std::fs::canonicalize(f).unwrap_or_else(|_| f.clone())
            }
        });

    // Decide whether we must synthesize a report, and whether it is the
    // FULL-output (truncation) report.
    //  * truncated  -> ALWAYS write the full-output report (Blocker 2), even
    //    if a real .md exists; the marker will point at it.
    //  * !truncated && no real .md && non-empty -> synthesize a delivery
    //    payload so the spawn_only path always has a file to attach.
    let needs_full_report = truncated && !full_output.is_empty();
    let needs_delivery_payload =
        !truncated && real_report_file.is_none() && !full_output.is_empty();

    let synthesized = if needs_full_report || needs_delivery_payload {
        write_synthetic_report(full_output, synthetic_dir)
    } else {
        None
    };

    let mut delivery = ReportDelivery::default();
    if needs_full_report {
        delivery.full_report = synthesized.clone();
        delivery.full_report_name = synthesized
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string);
    }

    // Primary report file: real .md wins for `file_modified`; otherwise the
    // synthesized payload (which, on truncation, is the full-output report).
    delivery.report_file = real_report_file.clone().or_else(|| synthesized.clone());
    if let Some(ref path) = delivery.report_file {
        tracing::info!(file = %path.display(), "pipeline produced report file");
    }

    // files_to_send: the real .md (if any) AND, on truncation, the full-output
    // report — so the FULL output is always delivered even alongside an
    // unrelated .md. De-duplicate (they may be the same path when no real .md
    // exists). Only include files that actually exist on disk.
    let mut send: Vec<PathBuf> = Vec::new();
    for candidate in [real_report_file.as_ref(), synthesized.as_ref()]
        .into_iter()
        .flatten()
    {
        if candidate.exists() && !send.contains(candidate) {
            send.push(candidate.clone());
        }
    }
    delivery.files_to_send = send;
    delivery
}

/// Write `output` to a uniquely-named `.md` file under `dir`, creating the
/// directory if needed. Returns the path on success; logs and returns `None`
/// on I/O error so a missing audit file never regresses the run outcome.
fn write_synthetic_report(output: &str, dir: &std::path::Path) -> Option<PathBuf> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id();
    // A process-unique counter avoids collisions when two runs land in the
    // same second within the same process.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let filename = format!("run_pipeline_{timestamp}_{pid}_{seq}.md");
    match std::fs::create_dir_all(dir).and_then(|_| {
        let path = dir.join(&filename);
        std::fs::write(&path, output).map(|_| path)
    }) {
        Ok(path) => {
            tracing::info!(file = %path.display(), "wrote synthetic pipeline report");
            Some(path)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to write synthetic pipeline report");
            None
        }
    }
}

/// #1020 / M17-B — Build a [`PipelineRunSummary`] stamped with the
/// `external_context_unmanaged` marker for a completed pipeline run.
///
/// `RunPipelineTool` constructs this for every run because pipeline
/// workers don't yet propagate the parent's `ContextManager` — workers
/// run with per-node prompt context instead. Evidence validators look
/// for `context_mode = "external_context_unmanaged"` plus the reason
/// string to confirm M17-B's acceptance bullet is satisfied.
///
/// `start_time_rfc3339` should be a caller-supplied RFC3339 timestamp
/// (the pipeline-run start) so the summary on disk is comparable across
/// runs and matches the `RunDir` audit trail. We accept it as a string
/// to keep this helper dependency-free of `chrono`.
pub(crate) fn build_pipeline_run_summary(
    graph_id: impl Into<String>,
    result: &PipelineResult,
    duration_ms: u64,
    start_time_rfc3339: impl Into<String>,
) -> PipelineRunSummary {
    PipelineRunSummary {
        graph_id: graph_id.into(),
        success: result.success,
        duration_ms,
        total_tokens: result.token_usage.clone(),
        nodes_executed: result.node_summaries.len(),
        start_time: start_time_rfc3339.into(),
        context_mode: None,
        context_reason: None,
    }
    .with_external_context_unmanaged(PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON)
}

/// #1126 codex P2 follow-up to #1020 / M17-B — write a `summary.json`
/// for the timeout failure path. Without this, runs that hit the
/// pipeline-level timeout had no audit-trail marker at all, even
/// though pipeline workers had been launched and consumed budget.
/// Records `success: false`, a `duration_ms` equal to the elapsed
/// wall-clock at the timeout boundary, zero node summaries, and the
/// same `external_context_unmanaged` marker so validators see a
/// consistent shape for both success and failure paths.
fn emit_external_context_unmanaged_timeout_summary(
    working_dir: &std::path::Path,
    run_id: &str,
    graph_id: &str,
    duration_ms: u64,
    start_time_rfc3339: &str,
    timeout_secs: u64,
) {
    let run_dir = match RunDir::new(working_dir, run_id) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                run_id,
                error = %error,
                "failed to open run dir for M17-B timeout summary; skipping"
            );
            return;
        }
    };
    let reason = format!(
        "{PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON}; pipeline timed out after {timeout_secs}s"
    );
    let summary = PipelineRunSummary {
        graph_id: graph_id.to_string(),
        success: false,
        duration_ms,
        total_tokens: TokenUsage::default(),
        nodes_executed: 0,
        start_time: start_time_rfc3339.to_string(),
        context_mode: None,
        context_reason: None,
    }
    .with_external_context_unmanaged(reason);
    if let Err(error) = run_dir.write_summary(&summary) {
        tracing::warn!(
            run_id,
            error = %error,
            "failed to write M17-B timeout summary; downstream evidence validators may flag this run"
        );
    }
}

/// Cascade-fail every still-active `pipeline:<node>` child task
/// registered in the supervisor under the `run_pipeline` parent's
/// `tool_call_id`. Invoked from the `RunPipelineTool::execute` timeout
/// arm so dropped child futures don't leave orphan `state: "running"`
/// entries in the supervisor (the bug that surfaced as
/// `pipeline:analyze running` indefinitely on the dashboard).
///
/// No-op when the host context didn't snapshot a supervisor (legacy
/// callers / unit tests) or didn't carry a parent_tool_call_id. The
/// underlying [`TaskSupervisor::mark_descendants_failed`] filters to
/// the `pipeline:` `tool_name` prefix so the parent `run_pipeline`
/// task (which shares the same `tool_call_id` as its node children)
/// is never touched by the cascade — its own `mark_failed` path in
/// the timeout arm handles parent-level transition. The supervisor
/// method is also idempotent on already-terminal tasks, so
/// re-invocation is safe.
fn cascade_fail_orphan_node_tasks(
    host_context: &crate::host_context::PipelineHostContext,
    timeout_secs: u64,
) -> usize {
    let Some(supervisor) = host_context.task_supervisor.as_ref() else {
        return 0;
    };
    let Some(parent_tcid) = host_context.parent_tool_call_id.as_deref() else {
        return 0;
    };
    let reason = format!("pipeline timed out after {timeout_secs}s");
    let cascaded = supervisor.mark_descendants_failed(parent_tcid, &reason);
    if cascaded > 0 {
        tracing::warn!(
            parent_tool_call_id = %parent_tcid,
            cascaded,
            timeout_secs,
            "run_pipeline timeout cascade-failed orphan child node tasks",
        );
    }
    cascaded
}

/// NEW-09 — build the canonical [`ToolResult`] returned from
/// `RunPipelineTool::execute` when the pipeline-level timeout fires.
///
/// Returning `Ok(ToolResult { success: false, .. })` (rather than
/// `Err(eyre)`) routes the timeout through the same `Ok(r) if !r.success`
/// arm of the spawn_only background executor in
/// `octos-agent/src/agent/execution.rs` that handles every other failing
/// background tool. That arm:
///
/// 1. Marks the supervisor task `Failed` with the timeout reason.
/// 2. Calls the registered `BackgroundResultSender` which persists the
///    completion row to the session JSONL via
///    `persist_assistant_with_media` and emits both `message/persisted`
///    (legacy clients) and `turn/spawn_complete` (M10 clients).
/// 3. The bubble surface text becomes
///    `✗ run_pipeline failed: pipeline timed out after Ns` — matching
///    the wording every other failing spawn_only tool produces.
///
/// Pre-fix, the timeout returned `Err(eyre)` which DID also call the
/// sender via the `Err(e)` arm, but the harness's `isFinalArrived`
/// heuristic in soak round-8 observed the timeout completion never
/// reached the WS client; consolidating onto a single failure path
/// eliminates the Err/Ok divergence and pins the contract test surface
/// to one shape.
///
/// `files_to_send` is intentionally empty: a timed-out run produced no
/// deliverable artifact. `structured_metadata` and `named_outputs` are
/// `None` since no nodes executed to completion. `tokens_used` is `None`
/// because per-node token accounting was not collected when the parent
/// future was dropped; downstream cost-ledger callers must handle the
/// `None` case (they already do for legacy `Err` returns).
pub(crate) fn build_pipeline_timeout_result(timeout_secs: u64) -> ToolResult {
    ToolResult {
        output: format!("pipeline timed out after {timeout_secs}s"),
        success: false,
        tokens_used: None,
        file_modified: None,
        files_to_send: Vec::new(),
        structured_metadata: None,
        named_outputs: None,
    }
}

/// #1020 / M17-B — write a `summary.json` carrying the
/// `external_context_unmanaged` marker to the run's `.octos/runs/<run_id>/`
/// directory. Failures are logged at WARN and never propagated so the
/// pipeline's user-visible outcome is unchanged when the audit-trail
/// write fails (e.g. read-only filesystem during tests).
fn emit_external_context_unmanaged_summary(
    working_dir: &std::path::Path,
    run_id: &str,
    graph_id: &str,
    result: &PipelineResult,
    duration_ms: u64,
    start_time_rfc3339: &str,
) {
    let run_dir = match RunDir::new(working_dir, run_id) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(
                run_id,
                error = %error,
                "failed to open run dir for M17-B context-mode summary; skipping"
            );
            return;
        }
    };
    let summary = build_pipeline_run_summary(graph_id, result, duration_ms, start_time_rfc3339);
    if let Err(error) = run_dir.write_summary(&summary) {
        tracing::warn!(
            run_id,
            error = %error,
            "failed to write M17-B context-mode summary; downstream evidence validators may flag this run"
        );
    }
}

/// Extract the graph identifier from the resolved DOT body. Falls back
/// to `"pipeline"` when the header lacks an explicit name (matches the
/// sanitiser's `digraph { ... }` -> `digraph pipeline { ... }` rewrite).
fn graph_id_from_dot(dot_content: &str) -> String {
    let header = dot_content
        .trim_start()
        .strip_prefix("digraph")
        .map(|rest| rest.trim_start())
        .unwrap_or("");
    let candidate: String = header
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if candidate.is_empty() {
        "pipeline".to_string()
    } else {
        candidate
    }
}

/// Build a filesystem-safe run id of the form
/// `<graph_id>-<unix_secs>-<nanos>-<pid>-<counter>`.
/// Matches the `validate_pipeline_id` constraint (no `/`, `\`, `..`, control
/// chars, <= 128 bytes) and stays unique across simultaneous runs of the
/// same pipeline so two writers do not race on `summary.json`.
///
/// #1126 codex P2: the prior shape `{graph}-{secs}-{pid}` collided when
/// two `run_pipeline` calls for the same graph started in the same
/// second within the same process. Nanosecond resolution + a
/// per-process monotonic counter make collision practically impossible
/// even for back-to-back synchronous fan-out.
fn generate_run_id(graph_id: &str, started_at: std::time::SystemTime) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

    let (secs, nanos) = started_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0));
    let pid = std::process::id();
    let counter = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    // Sanitize graph_id defensively — `graph_id_from_dot` already strips
    // unsafe chars but a caller-provided value could be anything.
    let safe_graph: String = graph_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let candidate = format!("{safe_graph}-{secs}-{nanos:09}-{pid}-{counter}");
    if candidate.is_empty() || candidate.len() > 128 {
        format!("pipeline-{secs}-{nanos:09}-{pid}-{counter}")
    } else {
        candidate
    }
}

/// How many per-run working directories to retain under `pipeline-runs/`.
/// Each pipeline run creates its own subdirectory (see the isolation fix);
/// without a bound, a frequently-run pipeline accumulates run dirs forever.
/// Keep only the most recent this many and prune the rest.
const MAX_RETAINED_RUN_DIRS: usize = 20;

/// Point a stable `latest` entry inside `runs_root` at the most recent run
/// dir. This preserves BOTH the isolation property (each run gets its own
/// dir) AND a predictable path for follow-up `read_file findings-N.md` calls
/// or human browsing, which would otherwise silently stop finding files once
/// they moved under `pipeline-runs/<run_id>/`.
///
/// Best-effort: any failure (e.g. existing non-symlink path, unsupported
/// platform) is logged and ignored — it must never fail the run.
fn update_latest_run_link(runs_root: &std::path::Path, _run_id: &str, run_dir: &std::path::Path) {
    let latest = runs_root.join("latest");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // Remove a stale symlink/dir entry first (ignore "not found").
        let _ = std::fs::remove_file(&latest);
        if let Err(e) = symlink(run_dir, &latest) {
            tracing::warn!(error = %e, "pipeline: failed to update pipeline-runs/latest symlink");
        }
    }

    #[cfg(not(unix))]
    {
        // Portable fallback: a text pointer file naming the current run dir.
        if let Err(e) = std::fs::write(&latest, _run_id) {
            tracing::warn!(error = %e, "pipeline: failed to write pipeline-runs/latest pointer");
        }
    }
}

/// Delete all but the most recent `keep` run directories under `runs_root`.
/// "Most recent" is determined by directory **mtime**, not by name. Sorting
/// by path would order by pipeline NAME first (run ids are
/// `{graph}-{secs}-{nanos}-{pid}-{counter}`, graph name leading) and only by
/// timestamp within a name — which would prune the NEWEST runs of an
/// alphabetically-earlier pipeline while keeping older ones of a later one
/// (review HIGH). mtime reflects actual creation order regardless of name,
/// and does not re-couple pruning to the id format.
///
/// Best-effort: individual deletion failures are logged and skipped, and a
/// directory that is not a run dir (e.g. the `latest` symlink, or a file) is
/// never touched. Never fails the run.
fn prune_old_run_dirs(runs_root: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(runs_root) else {
        return;
    };
    let mut run_dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "latest"))
        .collect();
    if run_dirs.len() <= keep {
        return;
    }
    // Sort oldest-first by mtime; fall back to the epoch on metadata error so
    // unreadable dirs sort to the front (pruned first, the safe direction).
    run_dirs.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    let to_remove = run_dirs.len() - keep;
    for old in run_dirs.into_iter().take(to_remove) {
        if let Err(e) = std::fs::remove_dir_all(&old) {
            tracing::warn!(error = %e, dir = %old.display(), "pipeline: failed to prune old run dir");
        }
    }
}

/// Format a `SystemTime` as a coarse RFC3339 timestamp without pulling
/// in `chrono`. Falls back to the unix epoch on clock skew.
fn systemtime_to_rfc3339(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Inline a minimal date renderer: we only need year/month/day/hour/min/sec
    // for the audit trail. `chrono` is intentionally not pulled in here —
    // keeping octos-pipeline's deps unchanged is a hard rule for #1020.
    let (year, month, day, hour, min, sec) = unix_secs_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Convert unix seconds (UTC) into (year, month, day, hour, minute, second).
/// Handles dates from 1970-01-01 through 9999-12-31. Returns the epoch on
/// negative values (clock skew).
fn unix_secs_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    if secs < 0 {
        return (1970, 1, 1, 0, 0, 0);
    }
    let total_secs = secs as u64;
    let sec = (total_secs % 60) as u32;
    let total_mins = total_secs / 60;
    let min = (total_mins % 60) as u32;
    let total_hours = total_mins / 60;
    let hour = (total_hours % 24) as u32;
    let mut days = (total_hours / 24) as i64;

    // Compute year/month/day from days-since-epoch (1970-01-01).
    let mut year: i32 = 1970;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let year_days = if leap { 366 } else { 365 };
        if days < year_days as i64 {
            break;
        }
        days -= year_days as i64;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_lens: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month: u32 = 1;
    for &m_len in &month_lens {
        if days < m_len as i64 {
            break;
        }
        days -= m_len as i64;
        month += 1;
    }
    let day = (days as u32) + 1;
    (year, month, day, hour, min, sec)
}

/// Project a non-empty slice of [`NodeCost`] rows into the
/// `ToolResult.structured_metadata` shape the session actor consumes.
///
/// Returns `None` when there are no cost rows so the side-channel stays
/// absent for legacy callers (no accountant / no LLM-call nodes); returns
/// `Some({"node_costs": [...]})` otherwise. Lifted out so tests can assert
/// the projection without standing up a full pipeline run.
fn node_costs_metadata(rows: &[crate::executor::NodeCost]) -> Option<serde_json::Value> {
    if rows.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "node_costs": rows,
        }))
    }
}

/// Max ERROR diagnostics included in a `pre_flight_validate` failure message,
/// and the max bytes per formatted line. The preflight early-return bypasses
/// both the per-tool truncation and the run_pipeline body/footer ceiling, so
/// the message MUST be bounded here (codex pre-merge DO-NOT-SHIP). 50 lines x
/// ~512 bytes (+ markers) keeps it well under any frame/result ceiling.
const MAX_PREFLIGHT_ERRORS: usize = 50;
const MAX_PREFLIGHT_ERR_LINE_BYTES: usize = 512;

/// Format the ERROR-severity diagnostics into a BOUNDED `pipeline validation
/// failed` message: at most [`MAX_PREFLIGHT_ERRORS`] lines, each truncated to
/// [`MAX_PREFLIGHT_ERR_LINE_BYTES`] on a UTF-8 char boundary, with truncation
/// markers so nothing is silently dropped. Provably small regardless of how
/// many errors a malformed DOT produces or how large its node IDs are.
fn format_bounded_preflight_errors(diags: &[crate::validate::LintDiagnostic]) -> String {
    let errors: Vec<&crate::validate::LintDiagnostic> = diags
        .iter()
        .filter(|d| d.severity == crate::validate::Severity::Error)
        .collect();
    let total = errors.len();
    let shown: Vec<String> = errors
        .iter()
        .take(MAX_PREFLIGHT_ERRORS)
        .map(|d| {
            let line = format!(
                "{} (rule {}, {:?}): {}",
                d.rule_id.code(),
                d.rule,
                d.location,
                d.message
            );
            if line.len() <= MAX_PREFLIGHT_ERR_LINE_BYTES {
                line
            } else {
                let mut end = MAX_PREFLIGHT_ERR_LINE_BYTES;
                while end > 0 && !line.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...(+{} bytes)", &line[..end], line.len() - end)
            }
        })
        .collect();
    let mut msg = format!("pipeline validation failed:\n{}", shown.join("\n"));
    if total > MAX_PREFLIGHT_ERRORS {
        msg.push_str(&format!(
            "\n...and {} more error(s) (truncated)",
            total - MAX_PREFLIGHT_ERRORS
        ));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_error_message_is_bounded_for_many_errors_and_huge_ids() {
        use crate::validate::{GraphLocation, LintDiagnostic, RuleId, Severity};
        // 1000 ERROR diagnostics, each carrying a 200 KB node id + 200 KB
        // message. The OLD unbounded `errors.join("\n")` would be hundreds of
        // MB and (via the spawn_only preflight early-return) bypass every
        // downstream ceiling. The bounded formatter must cap BOTH the line
        // count and each line's length.
        let huge = "x".repeat(200_000);
        let diags: Vec<LintDiagnostic> = (0..1000)
            .map(|_| LintDiagnostic {
                rule: RuleId::Connectivity.number(),
                rule_id: RuleId::Connectivity,
                severity: Severity::Error,
                location: GraphLocation::Node(huge.clone()),
                message: huge.clone(),
                fix_hint: None,
            })
            .collect();
        let msg = format_bounded_preflight_errors(&diags);
        assert!(
            msg.len() < 64 * 1024,
            "preflight failure message must be bounded, got {} bytes",
            msg.len()
        );
        assert!(msg.starts_with("pipeline validation failed"));
        assert!(
            msg.contains("more error(s) (truncated)"),
            "must mark the omitted-error count"
        );
        assert!(msg.contains("...(+"), "must mark per-line byte truncation");
    }
    use crate::executor::NodeCost;

    /// Gap 3.1 — when a pipeline run reports per-node cost rows, the tool
    /// surfaces them in `ToolResult.structured_metadata` under the
    /// `"node_costs"` key so the session actor can project them onto the
    /// SSE `done` event for the W1.G4 CostBreakdown panel.
    #[test]
    fn node_costs_metadata_emits_node_costs_array_for_multi_node_pipeline() {
        let rows = vec![
            NodeCost {
                node_id: "draft".into(),
                model: Some("anthropic/claude-haiku".into()),
                reserved_usd: 0.0010,
                actual_usd: 0.0008,
                tokens_in: 320,
                tokens_out: 110,
                committed: true,
            },
            NodeCost {
                node_id: "refine".into(),
                model: Some("anthropic/claude-sonnet".into()),
                reserved_usd: 0.0040,
                actual_usd: 0.0032,
                tokens_in: 540,
                tokens_out: 220,
                committed: true,
            },
        ];

        let meta = node_costs_metadata(&rows).expect("multi-node pipeline must surface metadata");
        let arr = meta
            .get("node_costs")
            .and_then(|v| v.as_array())
            .expect("structured_metadata must carry a `node_costs` array");
        assert_eq!(arr.len(), 2, "one row per pipeline node");
        assert_eq!(
            arr[0].get("node_id").and_then(|v| v.as_str()),
            Some("draft")
        );
        assert_eq!(
            arr[1].get("node_id").and_then(|v| v.as_str()),
            Some("refine")
        );
        assert!(
            arr[0]
                .get("tokens_in")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0,
            "tokens_in must be threaded through the projection"
        );
        assert!(
            arr[0]
                .get("actual_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                > 0.0,
            "actual_usd must be threaded through the projection"
        );
    }

    /// When a pipeline runs without an accountant attached, no per-node cost
    /// rows are produced; the side-channel stays absent so legacy callers
    /// observe byte-identical behaviour.
    #[test]
    fn node_costs_metadata_returns_none_for_empty_rows() {
        assert!(node_costs_metadata(&[]).is_none());
    }

    /// NEW-15 (1): the 30-min historical default is preserved when
    /// neither the LLM nor the DOT supply a timeout — backward-compat
    /// guard.
    #[test]
    fn resolve_pipeline_timeout_defaults_to_1800_when_unspecified() {
        assert_eq!(resolve_pipeline_timeout(None, None), 1800);
    }

    /// NEW-15 (2): an LLM-supplied value within [60, 3600] passes
    /// through unmodified — the new ceiling no longer truncates honest
    /// deep-research estimates at 1800s.
    #[test]
    fn resolve_pipeline_timeout_passes_llm_value_in_range() {
        assert_eq!(resolve_pipeline_timeout(Some(3000), None), 3000);
    }

    /// NEW-15 (3): an LLM value above the new 3600s ceiling clamps
    /// down — a runaway pipeline still cannot lock the session past
    /// an hour per spawn_only invocation.
    #[test]
    fn resolve_pipeline_timeout_clamps_llm_value_above_ceiling() {
        assert_eq!(resolve_pipeline_timeout(Some(5000), None), 3600);
    }

    /// NEW-15 (4): an LLM value below the 60s floor clamps up — a
    /// careless caller cannot disarm the timeout entirely.
    #[test]
    fn resolve_pipeline_timeout_clamps_llm_value_below_floor() {
        assert_eq!(resolve_pipeline_timeout(Some(10), None), 60);
    }

    /// NEW-15 (5): when the LLM does NOT supply `timeout_secs`, the
    /// DOT graph's `default_timeout_secs` attribute wins over the
    /// hard-coded 1800s default. This is the path that lets skill
    /// authors ship per-pipeline realistic caps.
    #[test]
    fn resolve_pipeline_timeout_uses_dot_default_when_llm_omits() {
        assert_eq!(resolve_pipeline_timeout(None, Some(2400)), 2400);
    }

    /// NEW-15 (6): the LLM's explicit `timeout_secs` always wins over
    /// the DOT default — operators can override per-call without
    /// editing the shipped pipeline.
    #[test]
    fn resolve_pipeline_timeout_llm_overrides_dot_default() {
        assert_eq!(resolve_pipeline_timeout(Some(1500), Some(2400)), 1500);
    }

    /// NEW-15 (7): clamping applies to the DOT default too — a skill
    /// author cannot ship a pipeline whose baked-in fallback exceeds
    /// the new 3600s ceiling.
    #[test]
    fn resolve_pipeline_timeout_clamps_dot_default_above_ceiling() {
        assert_eq!(resolve_pipeline_timeout(None, Some(7200)), 3600);
    }

    /// #1020 / M17-B — `build_pipeline_run_summary` MUST stamp the
    /// summary with `context_mode = "external_context_unmanaged"` plus
    /// the canonical M17-B reason string. Evidence validators grep
    /// these fields off `summary.json`, so any drift here silently
    /// breaks the M17-B acceptance bullet for `run_pipeline`.
    #[test]
    fn build_pipeline_run_summary_stamps_external_context_unmanaged_marker() {
        use octos_core::TokenUsage;
        let result = PipelineResult {
            output: "ok".into(),
            success: true,
            token_usage: TokenUsage::default(),
            node_summaries: Vec::new(),
            files_modified: Vec::new(),
            node_costs: Vec::new(),
        };
        let summary =
            build_pipeline_run_summary("test_pipeline", &result, 1234, "2026-05-20T17:00:00Z");
        assert_eq!(summary.graph_id, "test_pipeline");
        assert_eq!(
            summary.context_mode.as_deref(),
            Some("external_context_unmanaged"),
            "every run_pipeline summary must carry the M17-B marker"
        );
        assert_eq!(
            summary.context_reason.as_deref(),
            Some(PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON),
            "the marker reason must match the canonical M17-B constant"
        );
        // The serialized JSON form is what evidence validators actually
        // see on disk — assert the wire shape directly.
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["context_mode"], "external_context_unmanaged");
        assert!(
            json["context_reason"]
                .as_str()
                .unwrap_or("")
                .contains("M17-B"),
            "context_reason should reference M17-B for grep-ability"
        );
    }

    /// #1020 / M17-B — `emit_external_context_unmanaged_summary` writes
    /// the marker-stamped summary to disk under
    /// `<working_dir>/.octos/runs/<run_id>/summary.json` so the audit
    /// trail satisfies the M17-B evidence requirement at runtime.
    #[test]
    fn emit_external_context_unmanaged_summary_writes_marker_to_disk() {
        use octos_core::TokenUsage;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let result = PipelineResult {
            output: "ok".into(),
            success: true,
            token_usage: TokenUsage::default(),
            node_summaries: Vec::new(),
            files_modified: Vec::new(),
            node_costs: Vec::new(),
        };
        emit_external_context_unmanaged_summary(
            dir.path(),
            "deep_research-1747800000-12345",
            "deep_research",
            &result,
            5000,
            "2026-05-20T17:00:00Z",
        );
        let summary_path = dir
            .path()
            .join(".octos/runs/deep_research-1747800000-12345/summary.json");
        assert!(
            summary_path.exists(),
            "RunPipelineTool must persist summary.json with the M17-B marker"
        );
        let contents = std::fs::read_to_string(&summary_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(json["context_mode"], "external_context_unmanaged");
        assert_eq!(json["graph_id"], "deep_research");
        assert_eq!(
            json["context_reason"], PIPELINE_EXTERNAL_CONTEXT_UNMANAGED_REASON,
            "summary.json must carry the canonical M17-B reason"
        );
    }

    /// Run-id generator must produce a `validate_pipeline_id`-safe id
    /// even when the graph_id contains unsafe characters (slash, dot,
    /// control bytes). Without this defensive sanitization a maliciously
    /// named pipeline would fail to write `summary.json` and the M17-B
    /// marker would be silently dropped.
    #[test]
    fn generate_run_id_is_pipeline_id_safe() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let id = generate_run_id("ev/il..\\name", t);
        assert!(crate::graph::validate_pipeline_id(&id).is_ok());
        // The unix secs anchor + the original name's safe chars should
        // be preserved so operators can correlate the run with its logs.
        assert!(id.contains("1700000000"));
    }

    /// `graph_id_from_dot` extracts the digraph name when present and
    /// falls back to `"pipeline"` for anonymous graphs. The fallback
    /// path is what inline-DOT LLM calls use, so missing it would mean
    /// inline runs write `summary.json` under an empty-string run id —
    /// which `validate_pipeline_id` rejects.
    #[test]
    fn graph_id_from_dot_uses_pipeline_fallback_for_anonymous_graphs() {
        assert_eq!(
            graph_id_from_dot("digraph deep_research { a -> b }"),
            "deep_research"
        );
        assert_eq!(graph_id_from_dot("digraph { a -> b }"), "pipeline");
        assert_eq!(graph_id_from_dot("  digraph  research_42 {"), "research_42");
    }

    /// #1126 codex P2 acceptance: two run_pipeline calls for the same
    /// graph that start within the same second in the same process
    /// must produce DISTINCT run ids so their `summary.json` files do
    /// NOT race / overwrite. Before this fix the id was
    /// `{graph}-{secs}-{pid}`, which collided. After: nanos + counter
    /// make collision practically impossible.
    #[test]
    fn generate_run_id_distinguishes_concurrent_runs_in_same_second() {
        let t = std::time::SystemTime::now();
        let id1 = generate_run_id("deep_research", t);
        let id2 = generate_run_id("deep_research", t);
        assert_ne!(
            id1, id2,
            "two run ids minted in the same second for the same graph must differ"
        );
    }

    /// Per-run working-dir isolation: two runs of the SAME graph must land in
    /// DIFFERENT run directories, so findings files never mix across runs.
    /// This pins the actual fix — a regression where `run_id` reuse made the
    /// dirs collide is exactly the failure mode the isolation depends on
    /// avoiding (review finding #4).
    #[test]
    fn per_run_working_dirs_differ_for_same_graph() {
        let t = std::time::SystemTime::now();
        let id1 = generate_run_id("deep_research", t);
        let id2 = generate_run_id("deep_research", t);
        let root = std::path::Path::new("/tmp/working");
        let dir1 = root.join("pipeline-runs").join(&id1);
        let dir2 = root.join("pipeline-runs").join(&id2);
        assert_ne!(
            dir1, dir2,
            "two runs of the same graph must get distinct working dirs so findings never mix"
        );
    }

    /// `prune_old_run_dirs` retains only the N most recent run dirs and never
    /// touches the `latest` entry or non-directory files (review finding #2).
    #[test]
    fn prune_old_run_dirs_retains_most_recent_and_skips_latest() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let runs_root = root.path().join("pipeline-runs");
        // Create 25 run dirs (monotonic names so lexicographic == recency).
        for i in 0..25 {
            std::fs::create_dir_all(runs_root.join(format!("run-{i:03}"))).unwrap();
        }
        // A `latest` entry (as a plain file here) and an unrelated file must survive.
        std::fs::write(runs_root.join("latest"), "run-024").unwrap();
        std::fs::write(runs_root.join("notes.txt"), "keep me").unwrap();

        prune_old_run_dirs(&runs_root, 20);

        let remaining: Vec<_> = std::fs::read_dir(&runs_root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        let dir_count = remaining
            .iter()
            .filter(|n| runs_root.join(n).is_dir())
            .count();
        assert_eq!(dir_count, 20, "must retain exactly the 20 most recent dirs");
        assert!(runs_root.join("run-024").is_dir(), "newest dir retained");
        assert!(!runs_root.join("run-000").exists(), "oldest dir pruned");
        assert!(
            runs_root.join("latest").exists(),
            "latest entry never pruned"
        );
        assert!(
            runs_root.join("notes.txt").exists(),
            "non-run file never pruned"
        );
    }

    /// Review HIGH: retention must prune by AGE, not by pipeline NAME. Run ids
    /// are `{graph}-{secs}-{nanos}-{pid}-{counter}` — graph name leads, so a
    /// path sort orders by name first and would delete the NEWEST runs of an
    /// alphabetically-earlier pipeline while keeping older ones of a later
    /// one. This test uses two graph names with INTERLEAVED creation times so
    /// name order and age order disagree — the case that fails under name sort.
    #[test]
    fn prune_old_run_dirs_prunes_by_age_not_pipeline_name() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let runs_root = root.path().join("pipeline-runs");

        // Interleave two pipelines whose names sort differently than their
        // creation order: `aaa_old` (created FIRST, oldest) and `zzz_new`
        // (created LAST, newest). A name sort would keep `aaa_*` (sorts first)
        // and prune `zzz_*` (sorts last) even though `zzz_new` is the newest.
        // We create many so the quota forces pruning.
        //
        // mtimes are bumped explicitly because same-second creation would make
        // mtime ordering unreliable on coarse filesystems.
        for i in 0..15 {
            let d = runs_root.join(format!("aaa_old-{i:03}"));
            std::fs::create_dir_all(&d).unwrap();
            set_mtime(&d, 1_000 + i as u64); // oldest
        }
        for i in 0..15 {
            let d = runs_root.join(format!("zzz_new-{i:03}"));
            std::fs::create_dir_all(&d).unwrap();
            set_mtime(&d, 2_000 + i as u64); // newest
        }

        prune_old_run_dirs(&runs_root, 15);

        // The 15 NEWEST (zzz_new-*) must be retained; the 15 oldest (aaa_old-*)
        // pruned — even though `aaa_*` sorts BEFORE `zzz_*` by name.
        assert!(
            runs_root.join("zzz_new-014").is_dir(),
            "newest run (zzz_new) must be retained despite sorting last by name"
        );
        assert!(
            runs_root.join("zzz_new-000").is_dir(),
            "all zzz_new runs are within the 15 most recent"
        );
        assert!(
            !runs_root.join("aaa_old-000").exists(),
            "oldest run (aaa_old) must be pruned despite sorting first by name"
        );
        assert!(
            !runs_root.join("aaa_old-014").exists(),
            "all aaa_old runs are older than the quota cutoff"
        );
    }

    /// Bump a directory's mtime to a fixed unix timestamp so mtime-ordered
    /// pruning is deterministic in tests (creation within the same second is
    /// otherwise too coarse to order). Uses `std::fs::File::set_modified`
    /// (stable, cross-platform) so no external command or extra dep is needed.
    fn set_mtime(path: &std::path::Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        // Open the dir as a File to set its times. On Unix a dir can be opened
        // read-only; set_modified updates its mtime.
        let f = std::fs::File::open(path).expect("open dir for set_modified");
        f.set_modified(t).expect("set_modified");
    }

    /// #1126 codex P2 acceptance: when a pipeline run times out, a
    /// `summary.json` with the `external_context_unmanaged` marker
    /// must still be written so evidence validators can confirm the
    /// run launched workers. The reason string must include the
    /// timeout duration.
    #[test]
    fn emit_external_context_unmanaged_timeout_summary_writes_marker_to_disk() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        emit_external_context_unmanaged_timeout_summary(
            dir.path(),
            "deep_research-1747800000-000000001-12345-0",
            "deep_research",
            1_800_000,
            "2026-05-20T17:00:00Z",
            1800,
        );
        let summary_path = dir
            .path()
            .join(".octos/runs/deep_research-1747800000-000000001-12345-0/summary.json");
        assert!(
            summary_path.exists(),
            "timeout path must persist summary.json with M17-B marker"
        );
        let contents = std::fs::read_to_string(&summary_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(json["success"], false, "timeout summary records failure");
        assert_eq!(json["context_mode"], "external_context_unmanaged");
        assert!(
            json["context_reason"]
                .as_str()
                .unwrap_or("")
                .contains("timed out"),
            "context_reason must explicitly mention the timeout for audit",
        );
        assert!(
            json["context_reason"]
                .as_str()
                .unwrap_or("")
                .contains("1800"),
            "context_reason must include the timeout in seconds",
        );
    }

    /// Regression pin for the `run_pipeline` timeout orphan bug:
    /// when a pipeline times out, every still-active
    /// `pipeline:<node>` child task registered under the parent's
    /// `tool_call_id` must be cascade-failed in the supervisor with
    /// the timeout reason. Previously the future was dropped before
    /// `mark_completed`/`mark_failed` could fire, so children stayed
    /// in `state: "running"` forever. The ledger evidence from mini3
    /// showed parent task hitting `task_updated:failed` while
    /// `pipeline:analyze` had exactly ONE `state:running` event and
    /// never a terminal mark.
    ///
    /// Codex MAJOR follow-up on #1180: the cascade MUST NOT touch the
    /// parent `run_pipeline` task even though it shares the same
    /// `tool_call_id` as its node children — the parent's own
    /// `mark_failed` path handles parent-level transition.
    #[test]
    fn cascade_fail_orphan_node_tasks_marks_all_active_children_failed() {
        use octos_agent::task_supervisor::TaskSupervisor;
        use std::sync::Arc;

        let supervisor = Arc::new(TaskSupervisor::new());
        let parent_tcid = "tool-call-run_pipeline-timeout";

        // The parent run_pipeline task is registered with the SAME
        // tool_call_id its node children reuse (see
        // execution.rs::register_task_with_input_and_cmid +
        // executor.rs::register_node_task). The cascade must filter
        // by `pipeline:` prefix and skip this parent.
        let parent_task =
            supervisor.register("run_pipeline", parent_tcid, Some("session-timeout-test"));
        supervisor.mark_running(&parent_task);

        // Two pipeline-node child tasks registered under the parent
        // tool_call_id, both moved to running (matching what the
        // executor does at node dispatch time).
        let child_analyze = supervisor.register(
            "pipeline:analyze",
            parent_tcid,
            Some("session-timeout-test"),
        );
        let child_plan_and_search = supervisor.register(
            "pipeline:plan_and_search",
            parent_tcid,
            Some("session-timeout-test"),
        );
        supervisor.mark_running(&child_analyze);
        supervisor.mark_running(&child_plan_and_search);

        // Build the host context the way TOOL_CTX would expose it
        // when run_pipeline dispatches inside a real session.
        let host = crate::host_context::PipelineHostContext {
            task_supervisor: Some(supervisor.clone()),
            parent_tool_call_id: Some(parent_tcid.to_string()),
            parent_session_key: Some("session-timeout-test".to_string()),
            ..Default::default()
        };

        // Drive the timeout cascade with timeout_secs = 1200 to match
        // the ledger evidence.
        let cascaded = cascade_fail_orphan_node_tasks(&host, 1200);
        assert_eq!(
            cascaded, 2,
            "both active pipeline:<node> children must cascade-fail"
        );

        for cid in [&child_analyze, &child_plan_and_search] {
            let task = supervisor.get_task(cid).expect("child task survives");
            assert_eq!(
                task.status.as_str(),
                "failed",
                "child task {} must be Failed after pipeline timeout cascade",
                task.tool_name
            );
            let err = task.error.clone().unwrap_or_default();
            assert!(
                err.contains("pipeline timed out after 1200s"),
                "error must carry the canonical timeout reason, got: {err}",
            );
        }

        // Parent task must remain Running — the cascade filters by
        // `pipeline:` prefix so the parent run_pipeline task is
        // never touched even though it shares the same tool_call_id.
        let parent_after = supervisor
            .get_task(&parent_task)
            .expect("parent run_pipeline task survives");
        assert_eq!(
            parent_after.status.as_str(),
            "running",
            "parent run_pipeline task must NOT be touched by the cascade — \
             its own mark_failed path in the timeout arm handles parent-level transition"
        );
        assert!(
            parent_after.error.is_none(),
            "cascade must not write an error to the parent task"
        );
    }

    /// `cascade_fail_orphan_node_tasks` is a no-op when the host
    /// context didn't snapshot a supervisor — preserves the legacy
    /// pre-M8 path (pipelines invoked from CLI / unit tests where
    /// TOOL_CTX is empty).
    #[test]
    fn cascade_fail_orphan_node_tasks_noop_without_supervisor() {
        let host = crate::host_context::PipelineHostContext::default();
        let cascaded = cascade_fail_orphan_node_tasks(&host, 1200);
        assert_eq!(cascaded, 0);
    }

    /// `cascade_fail_orphan_node_tasks` is a no-op when the host
    /// context didn't capture a parent_tool_call_id. Defensive guard
    /// so we never mass-fail unrelated tasks.
    #[test]
    fn cascade_fail_orphan_node_tasks_noop_without_parent_tcid() {
        use octos_agent::task_supervisor::TaskSupervisor;
        use std::sync::Arc;

        let supervisor = Arc::new(TaskSupervisor::new());
        let host = crate::host_context::PipelineHostContext {
            task_supervisor: Some(supervisor),
            parent_tool_call_id: None,
            ..Default::default()
        };
        let cascaded = cascade_fail_orphan_node_tasks(&host, 1200);
        assert_eq!(cascaded, 0);
    }

    /// NEW-09 regression pin: the pipeline-level timeout MUST return
    /// `Ok(ToolResult { success: false, output: "pipeline timed out
    /// after Ns" })` rather than `Err(eyre)`. The spawn_only background
    /// executor in `octos-agent/src/agent/execution.rs` then routes the
    /// timeout through the `Ok(r) if !r.success` arm, which has been
    /// live-tested to call `bg_sender(BackgroundResultPayload { ... })`
    /// — persisting a `message/persisted` (legacy) and
    /// `turn/spawn_complete` (M10) event so the WS client renders the
    /// completion bubble and the harness's `isFinalArrived` heuristic
    /// fires.
    ///
    /// Pre-fix, the timeout returned `Err(eyre)` which routed through
    /// the `Err(e)` arm. Both arms emit a `BackgroundResultPayload`,
    /// but soak round-8 observed the WS client never saw the
    /// completion event for the Err path. Consolidating onto the
    /// `Ok(r) if !r.success` arm eliminates the divergence.
    #[test]
    fn pipeline_timeout_returns_ok_failure_result_not_err() {
        let result = build_pipeline_timeout_result(1200);
        assert!(
            !result.success,
            "timeout result must carry success=false so the spawn_only \
             execution branch routes through the `Ok(r) if !r.success` arm"
        );
        assert_eq!(
            result.output, "pipeline timed out after 1200s",
            "output text must carry the canonical timeout reason — the \
             spawn_only failure arm composes the chat bubble as \
             `✗ run_pipeline failed: <output>`"
        );
        assert!(
            result.files_to_send.is_empty(),
            "timed-out runs produce no deliverable artifact"
        );
        assert!(
            result.file_modified.is_none(),
            "no report file when no nodes completed"
        );
        assert!(
            result.tokens_used.is_none(),
            "per-node token accounting was not collected when the parent \
             future was dropped"
        );
        assert!(
            result.structured_metadata.is_none(),
            "no per-node cost rows on the timeout path"
        );
        assert!(
            result.named_outputs.is_none(),
            "no named outputs without completed nodes"
        );
    }

    /// NEW-09: the timeout-result output text must match what
    /// `cascade_fail_orphan_node_tasks` writes onto the child task
    /// `error` field. Without this invariant, a downstream
    /// `read_task_output` against the parent task surfaces different
    /// timeout-reason wording than the cascade-failed child rows, and
    /// dashboards / debugging tooling lose correlation.
    #[test]
    fn pipeline_timeout_output_matches_cascade_failed_child_error_text() {
        use octos_agent::task_supervisor::TaskSupervisor;
        use std::sync::Arc;

        let supervisor = Arc::new(TaskSupervisor::new());
        let parent_tcid = "tool-call-run_pipeline-timeout-correlation";
        let parent_task =
            supervisor.register("run_pipeline", parent_tcid, Some("session-correlation"));
        supervisor.mark_running(&parent_task);
        let child =
            supervisor.register("pipeline:analyze", parent_tcid, Some("session-correlation"));
        supervisor.mark_running(&child);

        let host = crate::host_context::PipelineHostContext {
            task_supervisor: Some(supervisor.clone()),
            parent_tool_call_id: Some(parent_tcid.to_string()),
            parent_session_key: Some("session-correlation".to_string()),
            ..Default::default()
        };
        let cascaded = cascade_fail_orphan_node_tasks(&host, 1200);
        assert_eq!(cascaded, 1);

        let child_error = supervisor
            .get_task(&child)
            .expect("child task survives")
            .error
            .unwrap_or_default();

        let timeout_result = build_pipeline_timeout_result(1200);
        assert!(
            child_error.contains(&timeout_result.output),
            "child cascade-failure error ({child_error}) must contain the \
             parent timeout result output ({}) so dashboards correlate \
             parent + child rows on a shared reason string",
            timeout_result.output,
        );
    }

    // ───── Phase 2-A: SessionScope-aware working dir resolution ─────
    //
    // The mini5 NEW-06 contamination bug had the same root cause every
    // round: `RunPipelineTool` pins `working_dir` at construction time
    // to the profile-level `data/` dir, so per-node workers spawn with
    // CWD == profile data and `read_file`-loop on 200+ stale `.md`
    // files from prior sessions. Phase 1 (#1199) plumbed `SessionScope`
    // through host contexts but did not consume it; Phase 2-A flips
    // `RunPipelineTool::execute` over to the scope's `workspace()` when
    // present. These tests pin the resolver semantics so the wiring
    // doesn't silently regress as more callers (Phase 2-B, 2-C, 2-D)
    // come online.

    /// Phase 2-A acceptance — when the parent host context attaches a
    /// `SessionScope`, the pipeline executor's `working_dir` MUST be
    /// the scope's `workspace()`, not the tool's pinned `working_dir`.
    /// This is the load-bearing fix for mini5 NEW-06: workers no
    /// longer see other sessions' `read_file` surface.
    #[test]
    fn pipeline_worker_uses_session_scope_workspace_when_present() {
        let tool_wd = std::path::PathBuf::from(if cfg!(windows) {
            "C:/profile/data"
        } else {
            "/profile/data"
        });
        let session_root = tempfile::tempdir().expect("temp dir");
        let scope =
            SessionScope::solo(session_root.path().to_path_buf(), vec![]).expect("solo scope");

        let resolved = resolve_pipeline_working_dir(&tool_wd, Some(&scope));
        assert_eq!(
            resolved,
            scope.workspace(),
            "scope present must override tool-level working_dir"
        );
        assert_ne!(
            resolved, tool_wd,
            "session-scoped path must NOT equal tool's profile-level working_dir"
        );
    }

    /// Backward-compat — legacy callers (CLI, unit tests, hosts not
    /// yet migrated to attach a `SessionScope`) keep their pre-Phase-2-A
    /// behaviour byte-for-byte. The tool's `working_dir` is returned
    /// verbatim and NO directory is created.
    #[test]
    fn pipeline_worker_falls_back_to_self_working_dir_when_no_scope() {
        let tool_wd = std::path::PathBuf::from(if cfg!(windows) {
            "C:/profile/data"
        } else {
            "/profile/data"
        });
        let resolved = resolve_pipeline_working_dir(&tool_wd, None);
        assert_eq!(
            resolved, tool_wd,
            "no scope must fall back to tool-level working_dir (pre-Phase-2-A behaviour)"
        );
    }

    /// Phase 2-A acceptance — the scope's `workspace()` may not exist
    /// on disk yet at run-pipeline time (the Phase 1 scope wiring is
    /// types-only; it never does I/O). The resolver MUST create the
    /// dir so workers can spawn without `current_dir(...)` ENOENT
    /// errors. Per the Phase 1 spec doc, this is the caller's
    /// responsibility — `SessionScope` itself stays I/O-free.
    #[test]
    fn pipeline_creates_session_workspace_dir_on_demand() {
        let tenant_root = tempfile::tempdir().expect("temp dir");
        // Manually craft a scope whose workspace() points into a
        // not-yet-existing subdirectory so we can assert the helper
        // creates it.
        let scope = SessionScope::multi_tenant(
            tenant_root.path().to_path_buf(),
            "tenant-phase2a".into(),
            "session-fresh".into(),
            vec![],
        )
        .expect("multi-tenant scope");
        let workspace = scope.workspace().to_path_buf();
        assert!(
            !workspace.exists(),
            "precondition: workspace should not exist before resolver runs"
        );

        let tool_wd = std::path::PathBuf::from(if cfg!(windows) {
            "C:/profile/data"
        } else {
            "/profile/data"
        });
        let resolved = resolve_pipeline_working_dir(&tool_wd, Some(&scope));
        assert_eq!(resolved, workspace, "resolver returns the scoped workspace");
        assert!(
            workspace.exists(),
            "resolver must create the workspace dir on disk so worker spawn does not ENOENT"
        );
        assert!(
            workspace.is_dir(),
            "resolver must create a directory, not a file"
        );
    }

    /// Multiple workers spawned in the same session (same scope) MUST
    /// share the same workspace CWD. This is what gives in-session
    /// continuity: writes from one node are visible to the next.
    #[test]
    fn pipeline_workers_in_same_session_share_workspace() {
        let tenant_root = tempfile::tempdir().expect("temp dir");
        let scope = SessionScope::multi_tenant(
            tenant_root.path().to_path_buf(),
            "tenant-share".into(),
            "session-share".into(),
            vec![],
        )
        .expect("multi-tenant scope");

        let tool_wd = std::path::PathBuf::from(if cfg!(windows) {
            "C:/profile/data"
        } else {
            "/profile/data"
        });
        let first = resolve_pipeline_working_dir(&tool_wd, Some(&scope));
        let second = resolve_pipeline_working_dir(&tool_wd, Some(&scope));
        assert_eq!(
            first, second,
            "two resolver calls with the same scope must return the same workspace CWD"
        );
        assert_eq!(first, scope.workspace());
    }

    /// The contamination-fixing assertion — two distinct sessions
    /// (same tenant root, same tool-level working_dir) MUST resolve to
    /// DIFFERENT workspaces. This is exactly the mini5 NEW-06 path:
    /// without this, the second session's pipeline workers would
    /// `read_file` 200+ `.md` files from the first session's runs and
    /// hallucinate cross-domain content (the JWST query producing an
    /// Intel/Tim Cook report).
    #[test]
    fn pipeline_workers_in_different_sessions_have_isolated_workspaces() {
        let tenant_root = tempfile::tempdir().expect("temp dir");
        let tool_wd = std::path::PathBuf::from(if cfg!(windows) {
            "C:/profile/data"
        } else {
            "/profile/data"
        });

        let scope_a = SessionScope::multi_tenant(
            tenant_root.path().to_path_buf(),
            "tenant-iso".into(),
            "session-a".into(),
            vec![],
        )
        .expect("scope a");
        let scope_b = SessionScope::multi_tenant(
            tenant_root.path().to_path_buf(),
            "tenant-iso".into(),
            "session-b".into(),
            vec![],
        )
        .expect("scope b");

        let cwd_a = resolve_pipeline_working_dir(&tool_wd, Some(&scope_a));
        let cwd_b = resolve_pipeline_working_dir(&tool_wd, Some(&scope_b));
        assert_ne!(
            cwd_a, cwd_b,
            "two sessions on the same tenant MUST have isolated workspace CWDs — \
             this is the load-bearing assertion for the mini5 NEW-06 fix"
        );
        // Both must be under the tenant root so per-tenant audit trails
        // still work; only the per-session segment differs.
        assert!(cwd_a.starts_with(tenant_root.path()));
        assert!(cwd_b.starts_with(tenant_root.path()));
    }

    // ── S1-5: typed-IR pre-flight (compose gate; no provider call) ─────────

    struct StubProvider;
    #[async_trait]
    impl octos_llm::LlmProvider for StubProvider {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> Result<octos_llm::ChatResponse> {
            unimplemented!("pre-flight never calls the provider")
        }
        fn model_id(&self) -> &str {
            "stub"
        }
        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    async fn make_ir_tool(ir_enabled: bool) -> RunPipelineTool {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(EpisodeStore::open(dir.path()).await.unwrap());
        RunPipelineTool::new(
            Arc::new(StubProvider),
            memory,
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
        )
        .with_ir_enabled(ir_enabled)
    }

    #[tokio::test]
    async fn preflight_accepts_valid_ir_when_enabled() {
        let tool = make_ir_tool(true).await;
        let args = serde_json::json!({
            "input": "x",
            "ir": r#"{"id":"p","nodes":[{"id":"r","kind":{"type":"research","prompt":"find"}},{"id":"s","kind":{"type":"synthesize","prompt":"write"}}],"edges":[{"source":"r","target":"s"}]}"#
        });
        assert!(tool.pre_flight_validate(&args).await.is_ok());
    }

    #[tokio::test]
    async fn preflight_rejects_unsafe_ir_kind() {
        let tool = make_ir_tool(true).await;
        let args = serde_json::json!({
            "input": "x",
            "ir": r#"{"id":"p","nodes":[{"id":"n","kind":{"type":"shell"}}]}"#
        });
        let err = tool.pre_flight_validate(&args).await.unwrap_err();
        assert!(err.contains("IR validation failed"), "got: {err}");
    }

    #[tokio::test]
    async fn input_schema_exposes_ir_only_when_enabled() {
        let base = make_ir_tool(false).await;
        assert!(base.input_schema()["properties"]["ir"].is_null());
        let enabled = make_ir_tool(true).await;
        assert!(enabled.input_schema()["properties"]["ir"].is_object());
    }

    #[test]
    fn advertised_ir_examples_compose() {
        // The worked examples embedded in the `ir` input description MUST be
        // valid — a broken example teaches the LLM the wrong shape.
        let ex1 = r#"{"id":"demo","nodes":[{"id":"research","kind":{"type":"research","prompt":"Research the topic; list 5 key facts each with a source URL"}},{"id":"report","kind":{"type":"synthesize","prompt":"Write a cited report from the findings"}}],"edges":[{"source":"research","target":"report"}]}"#;
        let ex2 = r#"{"id":"demo2","nodes":[{"id":"plan","kind":{"type":"research","prompt":"Identify the sub-topics to cover"}},{"id":"work","kind":{"type":"fanout","worker_prompt":"Investigate {task}","converge":"final"}},{"id":"final","kind":{"type":"synthesize","prompt":"Synthesize the findings into a report"}}],"edges":[{"source":"plan","target":"work"},{"source":"work","target":"final"}]}"#;
        assert!(
            crate::compose::compose_l2(ex1).is_ok(),
            "advertised example 1 must compose: {:?}",
            crate::compose::compose_l2(ex1).err()
        );
        assert!(
            crate::compose::compose_l2(ex2).is_ok(),
            "advertised example 2 must compose: {:?}",
            crate::compose::compose_l2(ex2).err()
        );
    }

    // ───── Blocker 2: full output always preserved + marker points to it ─────

    /// THE Blocker-2 bug. A pipeline modifies an UNRELATED `notes.md` AND
    /// returns an over-ceiling `result.output`. The OLD code only synthesized
    /// the full-output report when NO `.md` existed, so the full untruncated
    /// output landed in no delivered file. After the fix, when the result is
    /// truncated the synthetic FULL-output report is ALWAYS written, contains
    /// the untruncated output, and is in the delivered files — independent of
    /// the unrelated `notes.md`.
    #[test]
    fn truncated_result_always_delivers_full_output_report_even_with_unrelated_md() {
        let dir = tempfile::tempdir().expect("temp dir");
        let synthetic_dir = dir.path().join("synthetic");

        // An unrelated markdown file the pipeline happened to touch.
        let notes = dir.path().join("notes.md");
        std::fs::write(&notes, "# unrelated notes\n").unwrap();
        let files_modified = vec![notes.clone()];

        let full_output = "FULL-OUTPUT-MARKER ".repeat(50_000); // big, untruncated
        let delivery = resolve_report_delivery(
            &full_output,
            &files_modified,
            /* truncated = */ true,
            &synthetic_dir,
        );

        // The synthetic full-output report must exist and hold the FULL output.
        let full = delivery
            .full_report
            .as_ref()
            .expect("truncation must always produce a full-output report");
        let on_disk = std::fs::read_to_string(full).expect("full report readable");
        assert_eq!(
            on_disk, full_output,
            "the synthetic report must contain the UNTRUNCATED final output"
        );

        // It must be in the delivered files even though an unrelated .md exists.
        assert!(
            delivery.files_to_send.iter().any(|p| p == full),
            "the full-output report MUST be among the delivered files"
        );

        // The marker name must be the synthetic report's file name.
        let expected_name = full.file_name().and_then(|n| n.to_str()).unwrap();
        assert_eq!(
            delivery.full_report_name.as_deref(),
            Some(expected_name),
            "marker name must reference the synthetic full-output report"
        );
    }

    /// When the result is truncated AND there is no unrelated .md, the
    /// synthetic full-output report is still written and delivered.
    #[test]
    fn truncated_result_with_no_real_md_still_delivers_full_output_report() {
        let dir = tempfile::tempdir().expect("temp dir");
        let synthetic_dir = dir.path().join("synthetic");
        let full_output = "x".repeat(40_000);

        let delivery = resolve_report_delivery(&full_output, &[], true, &synthetic_dir);
        let full = delivery.full_report.as_ref().expect("full report written");
        assert_eq!(std::fs::read_to_string(full).unwrap(), full_output);
        assert!(delivery.files_to_send.iter().any(|p| p == full));
    }

    /// No truncation + an unrelated .md present: the existing spawn_only
    /// delivery path is unchanged. NO synthetic full-output report is written
    /// (no double-write), and the real .md is delivered.
    #[test]
    fn untruncated_result_with_real_md_does_not_double_write_synthetic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let synthetic_dir = dir.path().join("synthetic");
        let report = dir.path().join("report.md");
        std::fs::write(&report, "# real report\n").unwrap();

        let delivery = resolve_report_delivery(
            "small output",
            std::slice::from_ref(&report),
            false,
            &synthetic_dir,
        );
        assert!(
            delivery.full_report.is_none(),
            "no truncation must NOT write a synthetic full-output report"
        );
        assert!(
            !synthetic_dir.exists()
                || std::fs::read_dir(&synthetic_dir)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "synthetic dir must stay empty when not truncated and a real .md exists"
        );
        assert_eq!(delivery.report_file.as_ref(), Some(&report));
        assert!(delivery.files_to_send.iter().any(|p| p == &report));
        assert!(
            delivery.full_report_name.is_none(),
            "no marker name when not truncated"
        );
    }

    /// No truncation + no real .md + non-empty output: the existing
    /// synthesize-for-spawn_only-delivery behaviour is preserved (a report is
    /// written so files_to_send is non-empty), but it is NOT flagged as a
    /// "full-output" report (the body wasn't truncated) so no marker name is
    /// emitted.
    #[test]
    fn untruncated_result_with_no_md_synthesizes_delivery_payload_but_no_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let synthetic_dir = dir.path().join("synthetic");
        let delivery = resolve_report_delivery("some output", &[], false, &synthetic_dir);
        let path = delivery
            .report_file
            .as_ref()
            .expect("spawn_only delivery still needs a payload file");
        assert!(path.exists());
        assert!(delivery.files_to_send.iter().any(|p| p == path));
        assert!(
            delivery.full_report_name.is_none(),
            "untruncated payload is not a full-output report; no marker name"
        );
    }

    // ───── Footer-bound blocker: the WIRED ToolResult.output (body + marker +
    //       footer) stays under the frame cap for any number/size of nodes ─────

    use crate::fidelity::{
        FOOTER_BUDGET_BYTES, MAX_FRAME_BUDGET_BYTES, bound_footer, compute_result_ceiling,
    };
    use crate::graph::NodeSummary;
    use octos_core::TokenUsage;

    /// Build the per-node footer lines exactly as `execute()` does, so the
    /// test exercises the SAME assembly path the producer ships.
    fn footer_lines(summaries: &[NodeSummary]) -> Vec<String> {
        summaries
            .iter()
            .map(|n| {
                format!(
                    "- {} ({}): {}ms, {}+{} tokens",
                    n.node_id,
                    n.model.as_deref().unwrap_or("default"),
                    n.duration_ms,
                    n.token_usage.input_tokens,
                    n.token_usage.output_tokens,
                )
            })
            .collect()
    }

    /// THE wired end-to-end frame invariant. An over-ceiling all-NUL body
    /// (escapes 6x) PLUS thousands of long-id node summaries → the FINAL
    /// assembled `ToolResult.output` (`{bounded_output}{footer}`, matching
    /// `execute()`) serializes to strictly under the 1 MiB frame cap, and the
    /// footer carries the `[+N more nodes omitted]` marker. This is the case
    /// the codex re-review flagged: the unbounded footer reopened the
    /// `frame_too_large` cliff even with a bounded body.
    #[test]
    fn wired_output_stays_under_frame_cap_with_huge_body_and_huge_footer() {
        // Over-ceiling body.
        let body_input = "\0".repeat(500 * 1024);
        let ceiling = compute_result_ceiling(&body_input, None);
        assert!(ceiling.truncated, "precondition: body must be truncated");
        let bounded_output = ceiling.with_marker(Some("run_pipeline_1717400000_12345_0.md"));

        // Thousands of long-id/model node summaries → unbounded footer is MiBs.
        let long_id = "n".repeat(300);
        let summaries: Vec<NodeSummary> = (0..4_000)
            .map(|i| NodeSummary {
                node_id: format!("{long_id}{i}"),
                label: String::new(),
                model: Some("some-very-long-model-name-xxxxxxxxxxxx".to_string()),
                token_usage: TokenUsage {
                    input_tokens: 100,
                    output_tokens: 200,
                    ..Default::default()
                },
                duration_ms: 9999,
                success: true,
            })
            .collect();
        let lines = footer_lines(&summaries);
        let total_line = "Total: 400000 input + 800000 output tokens";
        let footer = bound_footer(&lines, total_line);

        // Assemble EXACTLY as execute() does.
        let output = format!("{bounded_output}{footer}");

        let serialized = serde_json::to_string(&output).unwrap().len();
        assert!(
            serialized < 1024 * 1024,
            "wired ToolResult.output must serialize under the 1 MiB frame cap, got {serialized}"
        );
        // Components individually honour their reservations.
        assert!(
            footer.contains("more nodes omitted"),
            "footer must carry the omitted-nodes marker"
        );
        assert!(footer.contains(total_line), "footer keeps the Total line");
        assert!(
            footer.starts_with("\n\n---\nPipeline execution summary:\n"),
            "footer keeps the scaffold"
        );
        // Earliest (most load-bearing) node survives in the head.
        assert!(footer.contains(&format!("{long_id}0 ")));
    }

    /// A few-node run with short ids leaves the footer UNCHANGED — no false
    /// truncation, every node line present, no omitted marker, and the wired
    /// output is well under budget.
    #[test]
    fn wired_output_small_pipeline_keeps_full_footer() {
        let bounded_output = "the pipeline produced this small result";
        let summaries = vec![
            NodeSummary {
                node_id: "plan".into(),
                label: String::new(),
                model: Some("gpt-4".into()),
                token_usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 20,
                    ..Default::default()
                },
                duration_ms: 100,
                success: true,
            },
            NodeSummary {
                node_id: "write".into(),
                label: String::new(),
                model: None, // -> "default"
                token_usage: TokenUsage {
                    input_tokens: 30,
                    output_tokens: 40,
                    ..Default::default()
                },
                duration_ms: 200,
                success: true,
            },
        ];
        let lines = footer_lines(&summaries);
        let footer = bound_footer(&lines, "Total: 40 input + 60 output tokens");
        let output = format!("{bounded_output}{footer}");

        assert!(output.contains("- plan (gpt-4): 100ms, 10+20 tokens"));
        assert!(output.contains("- write (default): 200ms, 30+40 tokens"));
        assert!(
            !output.contains("more nodes omitted"),
            "small pipeline must NOT carry an omitted marker"
        );
        assert!(
            serde_json::to_string(&output).unwrap().len()
                <= MAX_FRAME_BUDGET_BYTES + FOOTER_BUDGET_BYTES
        );
    }
}
