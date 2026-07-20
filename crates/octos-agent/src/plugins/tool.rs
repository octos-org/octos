//! Plugin tool: wraps a plugin executable as a Tool.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use octos_core::{PathClassification, SessionScope};

use crate::harness_errors::HarnessError;
use crate::harness_events::{
    OCTOS_EVENT_SINK_ENV, OCTOS_HARNESS_SESSION_ID_ENV, OCTOS_HARNESS_TASK_ID_ENV,
    OCTOS_SESSION_ID_ENV, OCTOS_TASK_ID_ENV, lookup_event_sink_context, write_event_to_sink,
};
use crate::policy::ApprovalPolicy;
use crate::progress::ProgressEvent;
use crate::subprocess_env::{
    EnvAllowlist, sanitize_command_env, sanitize_command_env_strict, should_forward_env_name,
    should_forward_env_name_strict,
};
use crate::tools::{
    TOOL_APPROVAL_CTX, TOOL_CTX, Tool, ToolApprovalDecision, ToolApprovalRequest, ToolContext,
    ToolResult,
};

use super::manifest::{ManifestRiskGate, PluginToolDef};

/// Synthesis LLM provider config injected into plugin args.
///
/// S2 plumbing: octos passes this struct under `synthesis_config` in the JSON
/// args (alongside `query`, `depth`, etc.) when the plugin's manifest opts in
/// via `x-octos-host-config-keys: ["synthesis_config"]`. Plugins that haven't
/// declared the key never see this struct, so secrets stay scoped to the
/// plugins that asked for them.
///
/// Token MUST NOT be logged. Audit `tracing::*` and `eprintln!` paths before
/// adding diagnostics that touch this struct.
#[derive(Clone, Debug)]
pub struct SynthesisConfig {
    /// OpenAI-compatible base URL (e.g. `https://api.deepseek.com/v1`).
    pub endpoint: String,
    /// Bearer token for the synthesis provider.
    pub api_key: String,
    /// Model id to request (e.g. `deepseek-chat`).
    pub model: String,
    /// Provider label for the v2 cost envelope (e.g. `deepseek`).
    pub provider: String,
}

impl SynthesisConfig {
    /// Whether all four fields are populated. Partial configs are dropped at
    /// the inject site so the plugin's env-fallback still works.
    pub fn is_complete(&self) -> bool {
        !self.endpoint.is_empty()
            && !self.api_key.is_empty()
            && !self.model.is_empty()
            && !self.provider.is_empty()
    }

    /// Encode the config as a JSON object suitable for inlining into plugin args.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": self.endpoint,
            "api_key": self.api_key,
            "model": self.model,
            "provider": self.provider,
        })
    }
}

/// A tool backed by a plugin executable.
///
/// Protocol: write JSON args to stdin, read JSON result from stdout.
/// Expected output: `{ "output": "...", "success": true/false }`
pub struct PluginTool {
    plugin_name: String,
    tool_def: PluginToolDef,
    executable: PathBuf,
    /// Environment variables to strip from the plugin's environment.
    blocked_env: Vec<String>,
    /// Extra environment variables to inject into the plugin's environment.
    /// Secret-like names require the tool manifest's explicit env allowlist.
    extra_env: Vec<(String, String)>,
    /// Working directory for plugin execution (created on first use).
    work_dir: Option<PathBuf>,
    /// Execution timeout.
    timeout: Duration,
    /// S2 plumbing: synthesis LLM provider config to inject into plugin args.
    /// Only honoured when the tool's manifest opts in via
    /// `x-octos-host-config-keys: ["synthesis_config"]`.
    synthesis_config: Option<SynthesisConfig>,
    /// Section C: SHA-256 (lowercase hex) of the verified-exe bytes computed
    /// at load time. Stored alongside the executable path so the pre-spawn
    /// re-hash gate in `execute()` can confirm the bytes have not been
    /// swapped between load and exec (closes the load→exec TOCTOU window).
    /// `None` when no hash was computed (legacy code paths).
    verified_exe_sha256: Option<String>,
    /// Section C (codex review round-5 P1.1): SHA-256 (lowercase hex) of
    /// the manifest.json bytes computed at load time. Under strict
    /// signing this acts as a "load-time tamper anchor" — manifest
    /// declarations (`risk`, `env`, tool schemas) are NOT covered by
    /// `manifest.sha256`, so we hash the manifest separately at load
    /// time and re-check on every invocation. A mismatch catches runtime
    /// tampering of the manifest after the runtime started. Tampering
    /// BEFORE the loader runs remains an operator responsibility
    /// (filesystem integrity tooling).
    manifest_sha256: Option<String>,
    /// Section C: the resolved manifest.json path so the pre-spawn
    /// re-hash gate can rehash it. Set alongside `manifest_sha256`.
    manifest_path: Option<PathBuf>,
    /// Section C: when `true`, the pre-spawn re-hash gate ALWAYS fires (and
    /// `verified_exe_sha256` must be `Some`). When `false`, the gate is
    /// skipped on unverified plugins to keep the legacy path cheap.
    require_signed: bool,
    /// yolo GAP #2: runtime approval behavior for the manifest risk gate.
    /// Threaded from the session's `EffectivePermissions::approval_policy`
    /// (same as `ShellTool`). Under [`ApprovalPolicy::Never`] a `high`/
    /// `critical`-risk plugin is DENIED without prompting — parity with
    /// shell.rs's fail-closed "approval_policy is never" — UNLESS
    /// `auto_approve_high_risk` is set (a DangerFullAccess / AllowAll
    /// context, which auto-allows the gate).
    approval_policy: ApprovalPolicy,
    /// yolo GAP #2: when `true`, the manifest risk gate auto-allows without
    /// prompting. Set for a DangerFullAccess / AllowAll ("yolo") context,
    /// mirroring how the same context swaps `SafePolicy` for `AllowAllPolicy`
    /// on the shell tools. Takes precedence over `approval_policy` so a
    /// dangerous session (whose `approval_policy` is `Never`) still runs
    /// high-risk plugins rather than denying them.
    auto_approve_high_risk: bool,
}

impl PluginTool {
    /// Default timeout for plugin execution.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

    pub fn new(plugin_name: String, tool_def: PluginToolDef, executable: PathBuf) -> Self {
        Self {
            plugin_name,
            tool_def,
            executable,
            blocked_env: vec![],
            extra_env: vec![],
            work_dir: None,
            timeout: Self::DEFAULT_TIMEOUT,
            synthesis_config: None,
            verified_exe_sha256: None,
            manifest_sha256: None,
            manifest_path: None,
            require_signed: false,
            approval_policy: ApprovalPolicy::Ask,
            auto_approve_high_risk: false,
        }
    }

    /// yolo GAP #2: set the runtime approval behavior for the manifest risk
    /// gate. Threaded from the session's
    /// [`EffectivePermissions::approval_policy`](crate::policy::EffectivePermissions),
    /// the same way `ShellTool::with_approval_policy` is wired. Under
    /// [`ApprovalPolicy::Never`] a `high`/`critical`-risk plugin is denied
    /// without prompting (unless [`Self::with_auto_approve_high_risk`] is set).
    pub fn with_approval_policy(mut self, approval_policy: ApprovalPolicy) -> Self {
        self.approval_policy = approval_policy;
        self
    }

    /// yolo GAP #2: when `true`, the manifest risk gate auto-allows without
    /// prompting. Set for a DangerFullAccess / AllowAll ("yolo") context —
    /// this takes precedence over the `approval_policy` so a dangerous
    /// session (whose policy is `Never`) still runs high-risk plugins.
    pub fn with_auto_approve_high_risk(mut self, auto_approve: bool) -> Self {
        self.auto_approve_high_risk = auto_approve;
        self
    }

    /// Attach the load-time SHA-256 of the verified-exe bytes so the pre-spawn
    /// re-hash gate in [`Self::execute`] can detect a swap between load and
    /// exec. Pass `require_signed = true` when the host config has enabled
    /// strict integrity — the gate will then run unconditionally and an
    /// invocation with a missing hash hard-errors.
    pub fn with_verified_sha256(mut self, hash: String, require_signed: bool) -> Self {
        self.verified_exe_sha256 = Some(hash);
        self.require_signed = require_signed;
        self
    }

    /// Section C (codex review round-5 P1.1): attach the load-time
    /// SHA-256 of the manifest.json bytes. Only consulted under
    /// `require_signed`. A mismatch at invocation refuses to spawn —
    /// catches manifest tampering between load and invocation (which
    /// could otherwise reduce `risk`, expand `env`, or alter tool
    /// schemas without invalidating the executable hash).
    pub fn with_manifest_sha256(mut self, hash: String, manifest_path: PathBuf) -> Self {
        self.manifest_sha256 = Some(hash);
        self.manifest_path = Some(manifest_path);
        self
    }

    /// Set environment variables to block from plugin execution.
    pub fn with_blocked_env(mut self, blocked: Vec<String>) -> Self {
        self.blocked_env = blocked;
        self
    }

    /// Set extra environment variables to inject into plugin execution.
    pub fn with_extra_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }

    /// Set the working directory for plugin processes.
    /// The directory is created automatically if it doesn't exist.
    pub fn with_work_dir(mut self, dir: PathBuf) -> Self {
        self.work_dir = Some(dir);
        self
    }

    /// Set custom execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The plugin's own execution timeout (seconds-resolution `Duration`).
    /// Exposed for the loader tests to assert the manifest-timeout clamp.
    #[cfg(test)]
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    /// yolo GAP #2 test accessor: the risk-gate approval policy this tool
    /// carries. Used by the registry wiring test to prove
    /// `apply_permissions_to_plugin_tools` threaded the session policy.
    #[cfg(test)]
    pub(crate) fn approval_policy(&self) -> ApprovalPolicy {
        self.approval_policy
    }

    /// yolo GAP #2 test accessor: whether this tool auto-allows the risk gate
    /// (a DangerFullAccess / AllowAll context).
    #[cfg(test)]
    pub(crate) fn auto_approve_high_risk(&self) -> bool {
        self.auto_approve_high_risk
    }

    /// The construction-time working directory bound to this tool (`None`
    /// when unbound). Mirrors the public [`Self::with_work_dir`] setter.
    ///
    /// Load-bearing for the chat/session cwd-rebind: a Host-scope ("yolo")
    /// session omits `session_scope`, so `execute` derives the plugin's
    /// `current_dir`/`OCTOS_WORK_DIR` from `work_dir` alone — it MUST be
    /// bound to the resolved `--cwd`, not left `None` (else plugins run in
    /// the process launch dir). Callers assert this to prove the binding.
    pub fn work_dir(&self) -> Option<&Path> {
        self.work_dir.as_deref()
    }

    /// S2 plumbing: set the synthesis LLM provider config injected into the
    /// plugin's args. Only honoured when the tool's manifest opts in via
    /// `x-octos-host-config-keys: ["synthesis_config"]`.
    pub fn with_synthesis_config(mut self, cfg: SynthesisConfig) -> Self {
        self.synthesis_config = Some(cfg);
        self
    }

    /// Section C: re-read the verified-exe bytes and recompute SHA-256.
    /// Returned as lowercase hex to match what the loader stored. Errors
    /// propagate as eyre wrappers so the caller can surface a precise
    /// reason in the refusal output.
    fn rehash_verified_exe(path: &Path) -> Result<String> {
        let bytes = std::fs::read(path).map_err(|e| eyre::eyre!("read {}: {e}", path.display()))?;
        Ok(format!("{:x}", Sha256::digest(&bytes)))
    }

    /// Section C (codex review round-5 P2 + P1.1): single source of truth
    /// for the pre-spawn re-hash gate. Returns `Some(ToolResult)` when
    /// the gate refuses (executable mismatch / manifest mismatch /
    /// missing hash under strict mode / I/O error); `None` when the
    /// gate passes or is intentionally skipped.
    ///
    /// Called twice in `execute()`: once before the approval round-trip
    /// (so a tampered-at-load binary or manifest is detected
    /// immediately) and once immediately before `cmd.spawn()` (so the
    /// approval delay window cannot be used to swap either file).
    fn check_verified_exe_hash(&self) -> Option<ToolResult> {
        // Executable check.
        if let Some(expected) = &self.verified_exe_sha256 {
            match Self::rehash_verified_exe(&self.executable) {
                Ok(actual) if actual == *expected => {
                    tracing::debug!(
                        plugin = %self.plugin_name,
                        tool = %self.tool_def.name,
                        "pre-spawn re-hash matched"
                    );
                }
                Ok(actual) => {
                    return Some(ToolResult {
                        output: format!(
                            "Plugin '{}' refused to spawn: verified executable hash mismatch \
                             (expected {expected}, got {actual}). The on-disk binary changed \
                             between load and invocation.",
                            self.plugin_name
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
                Err(err) => {
                    return Some(ToolResult {
                        output: format!(
                            "Plugin '{}' refused to spawn: failed to re-hash verified executable: {err}",
                            self.plugin_name
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        } else if self.require_signed {
            // Fail closed: strict policy is on but the load-time hash was
            // never recorded. This indicates a wiring bug — never let an
            // unhashed plugin invoke under `require_signed = true`.
            return Some(ToolResult {
                output: format!(
                    "Plugin '{}' refused to spawn: `plugins.require_signed` is enabled but \
                     no load-time hash was recorded for this tool (internal wiring error).",
                    self.plugin_name
                ),
                success: false,
                ..Default::default()
            });
        }

        // Section C (codex review round-5 P1.1): manifest check. Under
        // strict signing, we hashed manifest.json at load time and
        // stored the digest. A mismatch now means the manifest was
        // tampered with between load and invocation — refuse to spawn
        // because `manifest.tools[].risk` / `env` / schemas may have
        // been altered to bypass the approval gate or to widen the env
        // allowlist.
        if let (Some(expected), Some(path)) = (&self.manifest_sha256, &self.manifest_path) {
            match Self::rehash_verified_exe(path) {
                Ok(actual) if actual == *expected => {}
                Ok(actual) => {
                    return Some(ToolResult {
                        output: format!(
                            "Plugin '{}' refused to spawn: manifest.json hash mismatch \
                             (expected {expected}, got {actual}). The manifest changed \
                             between load and invocation.",
                            self.plugin_name
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
                Err(err) => {
                    return Some(ToolResult {
                        output: format!(
                            "Plugin '{}' refused to spawn: failed to re-hash manifest.json: {err}",
                            self.plugin_name
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        }

        None
    }

    /// Create a copy of this plugin tool with a different work directory.
    /// Used to give each user session its own workspace for plugin output.
    pub fn clone_with_work_dir(&self, work_dir: PathBuf) -> Self {
        Self {
            plugin_name: self.plugin_name.clone(),
            tool_def: self.tool_def.clone(),
            executable: self.executable.clone(),
            blocked_env: self.blocked_env.clone(),
            extra_env: self.extra_env.clone(),
            work_dir: Some(work_dir),
            timeout: self.timeout,
            synthesis_config: self.synthesis_config.clone(),
            verified_exe_sha256: self.verified_exe_sha256.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            manifest_path: self.manifest_path.clone(),
            require_signed: self.require_signed,
            approval_policy: self.approval_policy,
            auto_approve_high_risk: self.auto_approve_high_risk,
        }
    }

    /// yolo GAP #2: create a copy of this plugin tool carrying the session's
    /// risk-gate approval context (everything else, including the current
    /// `work_dir`, is preserved). The registry applies this per session in
    /// [`ToolRegistry::apply_permissions_to_plugin_tools`] so a `never` /
    /// DangerFullAccess session's plugin tools honor the same
    /// `ApprovalPolicy` the shell/coding tools already do.
    pub fn clone_with_permissions(
        &self,
        approval_policy: ApprovalPolicy,
        auto_approve_high_risk: bool,
    ) -> Self {
        Self {
            plugin_name: self.plugin_name.clone(),
            tool_def: self.tool_def.clone(),
            executable: self.executable.clone(),
            blocked_env: self.blocked_env.clone(),
            extra_env: self.extra_env.clone(),
            work_dir: self.work_dir.clone(),
            timeout: self.timeout,
            synthesis_config: self.synthesis_config.clone(),
            verified_exe_sha256: self.verified_exe_sha256.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            manifest_path: self.manifest_path.clone(),
            require_signed: self.require_signed,
            approval_policy,
            auto_approve_high_risk,
        }
    }

    /// Dispatch one line of plugin stderr to the host progress channel.
    ///
    /// Implements the plugin-protocol-v2 backward-compat shim:
    ///   1. Trim the line and try parsing as a [`ProtocolV2Event`].
    ///   2. On a known structured event, render a stable ToolProgress
    ///      message and (for cost events) write a structured cost
    ///      attribution to the harness sink so the ledger can pick it up.
    ///   3. On a JSON line with an unknown `type`, pass the raw JSON
    ///      through as ToolProgress (operator can still see the message).
    ///   4. On any other line, fall back to the v1 behavior — emit the
    ///      raw text as ToolProgress.
    ///
    /// The shim is intentionally side-effect-free aside from the reporter
    /// callback and the harness sink write so it is safe to call from a
    /// reader task without holding any locks.
    fn dispatch_stderr_line(
        plugin_name: &str,
        tool_name: &str,
        ctx: Option<&ToolContext>,
        line: &str,
    ) {
        use octos_plugin::protocol_v2::{LineParse, ProtocolV2Event};

        let parse = octos_plugin::protocol_v2::parse_event_line(line);
        let message = match parse {
            LineParse::Empty => return,
            LineParse::Event(ProtocolV2Event::Progress(progress)) => {
                let mut out = String::new();
                if !progress.stage.is_empty() {
                    out.push('[');
                    out.push_str(&progress.stage);
                    out.push(']');
                }
                if let Some(fraction) = progress.progress {
                    let pct = (fraction.clamp(0.0, 1.0) * 100.0).round();
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&format!("{pct:.0}%"));
                }
                if !progress.message.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&progress.message);
                }
                if out.is_empty() { progress.stage } else { out }
            }
            LineParse::Event(ProtocolV2Event::Phase(phase)) => {
                if phase.message.is_empty() {
                    format!("[phase] {}", phase.phase)
                } else {
                    format!("[{}] {}", phase.phase, phase.message)
                }
            }
            LineParse::Event(ProtocolV2Event::Cost(cost)) => {
                Self::record_cost_event(plugin_name, tool_name, ctx, &cost);
                if let Some(usd) = cost.usd {
                    format!(
                        "[cost] {}: in={} out={} (${usd:.4})",
                        cost.provider, cost.tokens_in, cost.tokens_out
                    )
                } else {
                    format!(
                        "[cost] {}: in={} out={}",
                        cost.provider, cost.tokens_in, cost.tokens_out
                    )
                }
            }
            LineParse::Event(ProtocolV2Event::Artifact(artifact)) => {
                if artifact.message.is_empty() {
                    format!("[artifact:{}] {}", artifact.kind, artifact.path)
                } else {
                    format!(
                        "[artifact:{}] {} ({})",
                        artifact.kind, artifact.message, artifact.path
                    )
                }
            }
            LineParse::Event(ProtocolV2Event::Log(log)) => {
                format!("[{}] {}", log.level, log.message)
            }
            LineParse::Event(ProtocolV2Event::Unknown) => {
                // Should not be reached because the parser converts
                // unknown variants to LineParse::UnknownEvent. Defensive
                // fallback: pass raw line through.
                line.to_string()
            }
            LineParse::UnknownEvent(raw) => raw,
            LineParse::Legacy(text) => text,
        };

        if let Some(ctx) = ctx {
            ctx.reporter.report(ProgressEvent::ToolProgress {
                name: tool_name.to_string(),
                tool_id: ctx.tool_id.clone(),
                message,
            });
        }
    }

    /// Forward a v2 cost event to the harness event sink if one is wired.
    ///
    /// Writes a `cost_attribution`-shaped JSON payload that mirrors
    /// `HarnessCostAttributionEvent` so existing ledger tooling can ingest
    /// plugin-level spend without a schema migration. The generated
    /// `attribution_id` is stable per (plugin, tool, provider, tokens) so
    /// duplicate sink writes can be detected downstream if needed.
    fn record_cost_event(
        plugin_name: &str,
        tool_name: &str,
        ctx: Option<&ToolContext>,
        cost: &octos_plugin::protocol_v2::CostEvent,
    ) {
        let Some(ctx) = ctx else {
            return;
        };
        let Some(sink) = ctx.harness_event_sink.as_deref() else {
            return;
        };
        let Some(sink_ctx) = lookup_event_sink_context(sink) else {
            return;
        };
        let attribution_id = format!(
            "plugin-cost-{}-{}-{}-{}-{}",
            plugin_name, tool_name, cost.provider, cost.tokens_in, cost.tokens_out
        );
        let payload = serde_json::json!({
            "schema": crate::harness_events::HARNESS_EVENT_SCHEMA_V1,
            "kind": "cost_attribution",
            "schema_version": 1,
            "session_id": sink_ctx.session_id,
            "task_id": sink_ctx.task_id,
            "workflow": null,
            "phase": null,
            "attribution_id": attribution_id,
            "contract_id": format!("plugin:{plugin_name}:{tool_name}"),
            "model": cost.model.clone().unwrap_or_else(|| "unknown".to_string()),
            "tokens_in": cost.tokens_in,
            "tokens_out": cost.tokens_out,
            "cost_usd": cost.usd.unwrap_or(0.0),
            "outcome": "ok",
            "provider": cost.provider,
            "source": "plugin_v2",
        });
        let line = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(error) => {
                tracing::debug!(
                    plugin = plugin_name,
                    tool = tool_name,
                    error = %error,
                    "failed to serialize plugin cost event"
                );
                return;
            }
        };
        if let Err(error) = crate::harness_events::write_event_line_to_sink(sink, &line) {
            tracing::debug!(
                plugin = plugin_name,
                tool = tool_name,
                error = %error,
                "failed to write plugin cost attribution to harness sink"
            );
        }
    }

    /// Record a `HarnessError` for this plugin tool: increments the
    /// `octos_loop_error_total{variant, recovery}` counter and writes a
    /// structured error event to the harness event sink (if one is wired
    /// via `ToolContext`). Keeps plugin error paths consistent with the
    /// in-process error boundary in `execution.rs`.
    fn emit_plugin_error(&self, ctx: Option<&ToolContext>, classified: &HarnessError) {
        classified.record_metric();
        let Some(sink) = ctx.and_then(|c| c.harness_event_sink.as_deref()) else {
            return;
        };
        let Some(sink_ctx) = lookup_event_sink_context(sink) else {
            return;
        };
        let event = classified.to_event(sink_ctx.session_id, sink_ctx.task_id, None, None);
        if let Err(error) = write_event_to_sink(sink, &event) {
            tracing::debug!(
                plugin = %self.plugin_name,
                tool = %self.tool_def.name,
                error = %error,
                "failed to write plugin error event to harness sink"
            );
        }
    }

    /// Phase 2-B of the SessionScope migration (PR #1198 follow-up to
    /// the bespoke #1186 / #1189 path-traversal saga): scope-aware
    /// rewriter. Replaces the entire `resolve_plugin_input_path` ->
    /// `has_unsafe_components_parent_only` -> `absolutize_path_in_work_dir`
    /// chain with a single
    /// [`SessionScope::classify_lexical_path`] call per argument.
    ///
    /// Policy:
    /// - Input keys (`audio_path`, `file_path`, `input`, `script_path`,
    ///   `video_path`, `text_path`, per-slide `source_image`): allow
    ///   `InWorkspace`, `InSharedZone` (multi-tenant read), `InGrantedDir`
    ///   (solo read). Refuse `OutOfScope`.
    /// - Output keys (`out`, `slide_dir`): allow `InWorkspace`,
    ///   `InGrantedDir`. Refuse `InSharedZone` (shared zones are
    ///   read-only) and `OutOfScope`.
    /// - `style`: same as input-path keys (it may resolve into
    ///   `<workspace>/styles/<name>.toml` or to an absolute path).
    ///
    /// After classification, paths land as ABSOLUTE strings in the
    /// rewritten args, so the spawned plugin (with
    /// `cmd.current_dir(scope.workspace())`) reads exactly what the
    /// host validated. `..` and other unsafe components are refused by
    /// `classify_lexical_path` itself (lexical normalise refuses
    /// `ParentDir`).
    ///
    /// Caller (`prepare_effective_args`) wires this for every scoped
    /// session — including those that have a rebound `self.work_dir`
    /// (codex round-2 P1 fix). `join_base` decides where relative paths
    /// land lexically before classification (= the registry-rebound
    /// `self.work_dir` when set, else `scope.workspace()`); scope
    /// validation runs against the absolute path UNCHANGED so the
    /// `OutOfScope` and `InSharedZone` write-refusal guards apply
    /// even when the plugin CWD is the hint.
    ///
    /// Basename rescue inside the scope path is bounded to
    /// `InWorkspace` classifications only (codex round-2 P2 fix). A
    /// missing `InSharedZone` / `InGrantedDir` path that happens to
    /// share its basename with a workspace file MUST NOT silently
    /// rewrite to the workspace file — the plugin would then process
    /// different input than the LLM requested. Out-of-`InWorkspace`
    /// paths flow through unchanged and the plugin's own
    /// `read_to_string` reports `os error 2`, which the LLM can act
    /// on.
    fn rewrite_args_with_scope(
        &self,
        args: &serde_json::Value,
        scope: &SessionScope,
        join_base: &std::path::Path,
    ) -> Result<serde_json::Value, eyre::Report> {
        let Some(obj) = args.as_object() else {
            return Ok(args.clone());
        };

        let mut rewritten = serde_json::Map::with_capacity(obj.len());
        for (key, value) in obj {
            if matches!(
                key.as_str(),
                "audio_path" | "file_path" | "input" | "script_path" | "video_path" | "text_path"
            ) {
                if let Some(path) = value.as_str() {
                    let absolute = absolutise_against_base(path, join_base);
                    // Codex round-2 BLOCKER 1: canonical-classify to
                    // close the ancestor-symlink escape — a `skill_dir`
                    // (or any zone root) containing
                    // `link -> /outside` previously let the lexical
                    // check accept `<skill_dir>/link/secret` as
                    // `InSkillDir`.
                    let (classification, normalised) =
                        classify_canonical_for_plugin_arg(scope, &absolute);
                    let resolved =
                        accept_for_intent(&classification, &normalised, path, PathIntent::Read)?;
                    // Codex round-1 P2 + round-2 P2 (scope review):
                    // basename rescue ONLY fires for `InWorkspace`.
                    // Shared zones and granted dirs (when missing)
                    // must report cleanly through the plugin's own
                    // `read_to_string` so the LLM sees "file not
                    // found" instead of being silently redirected to
                    // a same-basename workspace file.
                    let final_path = if matches!(classification, PathClassification::InWorkspace) {
                        rescue_workspace_input_existence(scope, join_base, path, &resolved)
                    } else {
                        resolved
                    };
                    rewritten.insert(key.clone(), serde_json::Value::String(final_path));
                    continue;
                }
            }
            if matches!(key.as_str(), "out" | "slide_dir") {
                if let Some(path) = value.as_str() {
                    let absolute = absolutise_against_base(path, join_base);
                    let (classification, normalised) =
                        classify_canonical_for_plugin_arg(scope, &absolute);
                    let resolved =
                        accept_for_intent(&classification, &normalised, path, PathIntent::Write)?;
                    rewritten.insert(key.clone(), serde_json::Value::String(resolved));
                    continue;
                }
            }
            if key == "style" {
                if let Some(style) = value.as_str() {
                    if self.tool_def.name.starts_with("mofa_") {
                        if let Some(normalized) = normalize_mofa_style_name(style) {
                            rewritten.insert(key.clone(), serde_json::Value::String(normalized));
                            continue;
                        }
                    }
                    // Same routing as `resolve_slides_style_in_work_dir`:
                    // if the style value looks like a path (absolute or
                    // contains a separator), classify it as an input
                    // path. Otherwise probe `<workspace>/styles/<style>.toml`
                    // and only rewrite when it exists; otherwise leave
                    // unchanged so the plugin can fall back to its own
                    // style registry (matching the legacy `Ok(None)`
                    // branch in `resolve_slides_style_in_work_dir`).
                    let trimmed = style.trim();
                    if trimmed.is_empty() {
                        rewritten.insert(key.clone(), value.clone());
                        continue;
                    }
                    let candidate = std::path::Path::new(trimmed);
                    let looks_like_path =
                        candidate.is_absolute() || trimmed.contains('/') || trimmed.contains('\\');
                    if looks_like_path {
                        let absolute = absolutise_against_base(trimmed, join_base);
                        // Codex round-2 BLOCKER 1: canonical-classify
                        // the style path so a symlink anywhere on the
                        // chain (including inside a skill_dir) is
                        // resolved before the prefix comparison.
                        let (classification, normalised) =
                            classify_canonical_for_plugin_arg(scope, &absolute);
                        let resolved = accept_for_intent(
                            &classification,
                            &normalised,
                            trimmed,
                            PathIntent::Read,
                        )?;
                        // Same `InWorkspace`-only basename rescue
                        // bound as the top-level input-path keys
                        // (codex round-2 P2).
                        let final_path =
                            if matches!(classification, PathClassification::InWorkspace) {
                                rescue_workspace_input_existence(
                                    scope, join_base, trimmed, &resolved,
                                )
                            } else {
                                resolved
                            };
                        rewritten.insert(key.clone(), serde_json::Value::String(final_path));
                        continue;
                    }
                    let filename = if trimmed.ends_with(".toml") {
                        trimmed.to_string()
                    } else {
                        format!("{trimmed}.toml")
                    };
                    // Probe `<join_base>/styles/<filename>` first so the
                    // registry-rebound work_dir wins (mirrors the legacy
                    // `resolve_slides_style_in_work_dir` behaviour),
                    // then `<scope.workspace>/styles/<filename>` as a
                    // secondary lookup for scope-only sessions.
                    for probe_root in [join_base, scope.workspace()] {
                        let probe = probe_root.join("styles").join(&filename);
                        if probe.exists() {
                            rewritten.insert(
                                key.clone(),
                                serde_json::Value::String(probe.to_string_lossy().into_owned()),
                            );
                            break;
                        }
                    }
                    if rewritten.contains_key(key) {
                        continue;
                    }
                }
            }
            if key == "slides" {
                if let Some(slides) = value.as_array() {
                    let mut rewritten_slides = Vec::with_capacity(slides.len());
                    for slide in slides {
                        let Some(slide_obj) = slide.as_object() else {
                            rewritten_slides.push(slide.clone());
                            continue;
                        };
                        let mut rewritten_slide = slide_obj.clone();
                        if let Some(source_image) = slide_obj
                            .get("source_image")
                            .and_then(|value| value.as_str())
                        {
                            let absolute = absolutise_against_base(source_image, join_base);
                            // Codex round-2 BLOCKER 1: canonical-classify
                            // per-slide source images for the same
                            // symlink-escape closure as the top-level
                            // input-path keys.
                            let (classification, normalised) =
                                classify_canonical_for_plugin_arg(scope, &absolute);
                            let resolved = accept_for_intent(
                                &classification,
                                &normalised,
                                source_image,
                                PathIntent::Read,
                            )?;
                            let final_path =
                                if matches!(classification, PathClassification::InWorkspace) {
                                    rescue_workspace_input_existence(
                                        scope,
                                        join_base,
                                        source_image,
                                        &resolved,
                                    )
                                } else {
                                    resolved
                                };
                            rewritten_slide.insert(
                                "source_image".into(),
                                serde_json::Value::String(final_path),
                            );
                        }
                        rewritten_slides.push(serde_json::Value::Object(rewritten_slide));
                    }
                    rewritten.insert(key.clone(), serde_json::Value::Array(rewritten_slides));
                    continue;
                }
            }
            rewritten.insert(key.clone(), value.clone());
        }
        Ok(serde_json::Value::Object(rewritten))
    }

    fn rewrite_workspace_file_args(
        &self,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, eyre::Report> {
        let Some(work_dir) = self.work_dir.as_ref() else {
            return Ok(args.clone());
        };
        let Some(obj) = args.as_object() else {
            return Ok(args.clone());
        };

        let mut rewritten = serde_json::Map::with_capacity(obj.len());
        for (key, value) in obj {
            if matches!(
                key.as_str(),
                "audio_path" | "file_path" | "input" | "script_path" | "video_path" | "text_path"
            ) {
                if let Some(path) = value.as_str() {
                    // Codex round-3 BLOCKER fix (PR #1186 review): propagate
                    // the resolver Err up to the call site (execute()) so
                    // a path with `..` components surfaces as a tool error
                    // envelope rather than being passed to the spawned
                    // plugin (which would resolve it relative to
                    // `work_dir` and escape the chroot).
                    rewritten.insert(
                        key.clone(),
                        serde_json::Value::String(resolve_plugin_input_path(path, work_dir)?),
                    );
                    continue;
                }
            }
            if matches!(key.as_str(), "out" | "slide_dir") {
                if let Some(path) = value.as_str() {
                    // Codex round-4 BLOCKER fix (PR #1186 review):
                    // propagate the absolutize Err for output-path keys
                    // so a `{"out":"../sneaky"}` or
                    // `{"slide_dir":"../escape"}` surfaces as a tool
                    // error envelope rather than being passed to the
                    // spawned plugin (which writes its output relative
                    // to `cmd.current_dir(work_dir)` and would escape
                    // the chroot). Matches the round-3 contract on
                    // input-path keys (resolve_plugin_input_path).
                    rewritten.insert(
                        key.clone(),
                        serde_json::Value::String(absolutize_path_in_work_dir(path, work_dir)?),
                    );
                    continue;
                }
            }
            if key == "style" {
                if let Some(style) = value.as_str() {
                    if self.tool_def.name.starts_with("mofa_") {
                        if let Some(normalized) = normalize_mofa_style_name(style) {
                            rewritten.insert(key.clone(), serde_json::Value::String(normalized));
                            continue;
                        }
                    }
                    // Codex round-4 BLOCKER fix (PR #1186 review):
                    // propagate the Err from
                    // `resolve_slides_style_in_work_dir` so a raw `..`
                    // in a style path fails closed at the rewrite step
                    // instead of being silently dropped (the previous
                    // `Option`-returning signature swallowed the
                    // unsafe-path case and fell through to the catch-
                    // all `.clone()` branch below, which would have
                    // passed the raw escape attempt straight to the
                    // plugin).
                    if let Some(resolved) = resolve_slides_style_in_work_dir(style, work_dir)? {
                        rewritten.insert(key.clone(), serde_json::Value::String(resolved));
                        continue;
                    }
                }
            }
            if key == "slides" {
                if let Some(slides) = value.as_array() {
                    let mut rewritten_slides = Vec::with_capacity(slides.len());
                    for slide in slides {
                        let Some(slide_obj) = slide.as_object() else {
                            rewritten_slides.push(slide.clone());
                            continue;
                        };
                        let mut rewritten_slide = slide_obj.clone();
                        if let Some(source_image) = slide_obj
                            .get("source_image")
                            .and_then(|value| value.as_str())
                        {
                            rewritten_slide.insert(
                                "source_image".into(),
                                serde_json::Value::String(resolve_plugin_input_path(
                                    source_image,
                                    work_dir,
                                )?),
                            );
                        }
                        rewritten_slides.push(serde_json::Value::Object(rewritten_slide));
                    }
                    rewritten.insert(key.clone(), serde_json::Value::Array(rewritten_slides));
                    continue;
                }
            }
            rewritten.insert(key.clone(), value.clone());
        }
        Ok(serde_json::Value::Object(rewritten))
    }

    pub(crate) fn prepare_effective_args(
        &self,
        args: &serde_json::Value,
        ctx: Option<&ToolContext>,
    ) -> Result<serde_json::Value, eyre::Report> {
        let mut effective_args = args.clone();
        if let Some(obj) = effective_args.as_object_mut() {
            let has_audio_path = obj
                .get("audio_path")
                .and_then(|value| value.as_str())
                .map(|value| !value.is_empty())
                .unwrap_or(false);
            if !has_audio_path
                && input_schema_has_property(&self.tool_def.input_schema, "audio_path")
            {
                if let Some(ctx) = ctx {
                    if ctx.audio_attachment_paths.len() == 1 {
                        obj.insert(
                            "audio_path".into(),
                            serde_json::Value::String(ctx.audio_attachment_paths[0].clone()),
                        );
                    }
                }
            }

            let has_file_path = obj
                .get("file_path")
                .and_then(|value| value.as_str())
                .map(|value| !value.is_empty())
                .unwrap_or(false);
            if !has_file_path && input_schema_has_property(&self.tool_def.input_schema, "file_path")
            {
                if let Some(ctx) = ctx {
                    if ctx.file_attachment_paths.len() == 1 {
                        obj.insert(
                            "file_path".into(),
                            serde_json::Value::String(ctx.file_attachment_paths[0].clone()),
                        );
                    }
                }
            }
        }

        // Phase 2-B (SessionScope migration, PR #1198 follow-up):
        // every scoped session funnels through `rewrite_args_with_scope`,
        // even when the registry rebound `self.work_dir` to a path
        // that the session's actual `SessionScope` doesn't enclose
        // (the hinted-workspace case codex round-3 P1 flagged). The
        // scope's `classify_lexical_path` collapses the 4-round #1186
        // traversal hardening + the #1189 workspace-root rescue + the
        // bespoke `resolve_plugin_input_path` /
        // `absolutize_path_in_work_dir` /
        // `resolve_slides_style_in_work_dir` validators into one gate.
        //
        // Routing policy (codex rounds 1+2+3+4):
        // - Scope absent: legacy rewriter (un-scoped fleet binaries,
        //   gateway sessions whose ids fail `is_safe_session_id`, all
        //   pre-Phase-1 callers).
        // - Scope present AND `self.work_dir` lives inside
        //   `scope.workspace()` (the typical un-hinted rebind: the
        //   registry rebound `<scope.workspace>/skill-output`): use
        //   the session scope directly. The rebound `self.work_dir`
        //   is the join base AND the rescue scan root.
        // - Scope present AND `self.work_dir` lives OUTSIDE
        //   `scope.workspace()` (the hinted-workspace path in
        //   `SessionRuntime::bootstrap` where scope is still the
        //   profile default but registry rebound a hint): substitute
        //   an AD-HOC solo scope rooted at `self.work_dir` so the
        //   plugin's read/write boundary still holds (absolute escapes
        //   like `/etc/passwd` still Err; bare `..` is still refused
        //   by `classify_lexical_path`'s lexical normalise step). The
        //   original session scope's `shared_zones` are NOT carried
        //   over — they're meaningless under the hint — but the
        //   security boundary is preserved. A follow-up will reconcile
        //   `SessionScope` construction with the hint; once that's
        //   done this branch collapses to the no-substitution case
        //   automatically. Round-4 codex flag fixed by replacing the
        //   round-3 legacy fallback that dropped the scope boundary.
        let effective_scope: Option<Arc<SessionScope>> =
            ctx.and_then(|c| c.session_scope.as_ref()).map(|scope| {
                match self.work_dir.as_deref() {
                    Some(wd) if !wd.starts_with(scope.workspace()) && wd.is_absolute() => {
                        // Codex round-5 P1 fix: real hinted bootstrap
                        // rebinds `self.work_dir` to
                        // `<hint>/skill-output`, so rooting the
                        // ad-hoc scope at `wd` directly would
                        // surrender the legacy workspace-root rescue
                        // (`script_path: "script.md"` with the file
                        // at `<hint>/script.md` would now resolve to
                        // `<hint>/skill-output/script.md` and miss).
                        // Promote the parent dir as the ad-hoc scope
                        // root when `wd` looks like the standard
                        // skill-output subdir so the workspace-root
                        // rescue keeps working. Absolute escapes
                        // (`/etc/passwd`) still Err because the
                        // parent is the hinted workspace root, not
                        // `/`.
                        let adhoc_root = if wd.file_name().and_then(|s| s.to_str())
                            == Some("skill-output")
                        {
                            wd.parent().unwrap_or(wd).to_path_buf()
                        } else {
                            wd.to_path_buf()
                        };
                        match SessionScope::solo(adhoc_root.clone(), vec![]) {
                            Ok(adhoc) => Arc::new(adhoc),
                            Err(err) => {
                                tracing::warn!(
                                    plugin = %self.plugin_name,
                                    tool = %self.tool_def.name,
                                    work_dir = %wd.display(),
                                    adhoc_root = %adhoc_root.display(),
                                    error = %err,
                                    "ad-hoc scope construction failed; falling back to session scope (validation may refuse legitimate hinted paths)"
                                );
                                scope.clone()
                            }
                        }
                    }
                    _ => scope.clone(),
                }
            });

        let mut effective_args = match effective_scope.as_deref() {
            Some(scope) => {
                let join_base: &std::path::Path = self
                    .work_dir
                    .as_deref()
                    .unwrap_or_else(|| scope.workspace());
                self.rewrite_args_with_scope(&effective_args, scope, join_base)?
            }
            None => self.rewrite_workspace_file_args(&effective_args)?,
        };
        if self.tool_def.name == "mofa_slides" {
            if let Some(obj) = effective_args.as_object_mut() {
                if !obj.contains_key("out")
                    || obj["out"].as_str().map(|s| s.is_empty()).unwrap_or(true)
                {
                    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
                    obj.insert(
                        "out".into(),
                        serde_json::Value::String(format!("slides_{ts}.pptx")),
                    );
                    tracing::info!("injected default 'out' for mofa_slides");
                }
            }
        }

        // Host workspace metadata is opt-in and HOST-OWNED. It is injected
        // after path rewriting so it is treated as metadata, not as a file
        // argument to normalize. The host-computed value ALWAYS wins: a
        // caller-supplied `workspace_root` is overwritten (or stripped when
        // the host cannot compute one) so a spoofed tool call can't point the
        // plugin at a workspace outside the session root.
        if self.tool_def.accepts_host_config_key("workspace_root") {
            if let Some(obj) = effective_args.as_object_mut() {
                let caller_supplied = obj.get("workspace_root").cloned();
                match self.workspace_root_for_host_injection(effective_scope.as_deref()) {
                    Some(root) => {
                        let host_value =
                            serde_json::Value::String(root.to_string_lossy().into_owned());
                        if let Some(prev) = caller_supplied.filter(|prev| *prev != host_value) {
                            tracing::warn!(
                                plugin = %self.plugin_name,
                                tool = %self.tool_def.name,
                                caller_workspace_root = %prev,
                                host_workspace_root = %root.display(),
                                "overriding caller-supplied workspace_root with host-computed value (host-owned metadata)"
                            );
                        }
                        obj.insert("workspace_root".into(), host_value);
                        tracing::info!(
                            plugin = %self.plugin_name,
                            tool = %self.tool_def.name,
                            workspace_root = %root.display(),
                            "injected workspace_root into plugin args"
                        );
                    }
                    None => {
                        if let Some(prev) = caller_supplied {
                            obj.remove("workspace_root");
                            tracing::warn!(
                                plugin = %self.plugin_name,
                                tool = %self.tool_def.name,
                                caller_workspace_root = %prev,
                                "stripping caller-supplied workspace_root; host has no computed value (host-owned metadata)"
                            );
                        }
                    }
                }
            }
        }

        // S2 plumbing: inject synthesis_config when the manifest opts in via
        // `x-octos-host-config-keys: ["synthesis_config"]` and the host has a
        // configured `SynthesisConfig`. The plugin still falls back to env if
        // the LLM happens to skip injection. NOTE: tokens MUST NOT be logged
        // — emit only the provider label.
        if self.tool_def.accepts_host_config_key("synthesis_config") {
            if let Some(cfg) = self.synthesis_config.as_ref() {
                if cfg.is_complete() {
                    if let Some(obj) = effective_args.as_object_mut() {
                        // Don't override an explicitly-provided synthesis_config.
                        // (The LLM should never set this, but we defend in depth
                        // so a misbehaving caller can't be silently overwritten.)
                        if !obj.contains_key("synthesis_config") {
                            obj.insert("synthesis_config".into(), cfg.to_json());
                            tracing::info!(
                                plugin = %self.plugin_name,
                                tool = %self.tool_def.name,
                                provider = %cfg.provider,
                                "injected synthesis_config into plugin args"
                            );
                        }
                    }
                }
            }
        }

        Ok(effective_args)
    }

    fn workspace_root_for_host_injection(
        &self,
        effective_scope: Option<&SessionScope>,
    ) -> Option<PathBuf> {
        if let Some(scope) = effective_scope {
            return Some(scope.workspace().to_path_buf());
        }
        let work_dir = self.work_dir.as_deref()?;
        if work_dir.file_name().and_then(|s| s.to_str()) == Some("skill-output") {
            return work_dir.parent().map(Path::to_path_buf);
        }
        Some(work_dir.to_path_buf())
    }

    async fn detect_output_file(
        &self,
        effective_args: &serde_json::Value,
        output: &str,
        files_to_send: &mut Vec<std::path::PathBuf>,
        effective_work_dir: Option<&std::path::Path>,
    ) -> Option<std::path::PathBuf> {
        // Phase 2-B (SessionScope migration): prefer the effective work
        // dir (= `scope.workspace()` when a scope was threaded) over
        // the construction-time `self.work_dir`. Falls back to the
        // legacy `self.work_dir` when no scope was supplied so the
        // backward-compat path is unchanged.
        let work_dir_owned: Option<std::path::PathBuf> = effective_work_dir
            .map(|p| p.to_path_buf())
            .or_else(|| self.work_dir.clone());
        let work_dir = work_dir_owned.as_deref();
        let out_file = effective_args
            .get("out")
            .and_then(|v| v.as_str())
            .and_then(|p| {
                let path = std::path::PathBuf::from(p);
                if path.is_absolute() && path.exists() {
                    return Some(path);
                }
                let candidates: Vec<std::path::PathBuf> = [
                    work_dir.map(|d| d.join(&path)),
                    std::env::current_dir().ok().map(|d| d.join(&path)),
                ]
                .into_iter()
                .flatten()
                .collect();
                candidates
                    .into_iter()
                    .find(|c| c.exists())
                    .or_else(|| work_dir.map(|d| d.join(&path)))
                    .or_else(|| std::env::current_dir().ok().map(|d| d.join(&path)))
                    .or(Some(path))
            });
        let from_output = if out_file.is_none() {
            output.lines().find_map(|line| {
                line.strip_prefix("Generated PPTX: ")
                    .or_else(|| line.strip_prefix("Generated: "))
                    .map(|p| std::path::PathBuf::from(p.trim()))
                    .and_then(|path| {
                        if path.exists() {
                            return Some(path.clone());
                        }
                        let in_work = work_dir.map(|d| d.join(&path));
                        let in_cwd = std::env::current_dir().ok().map(|d| d.join(&path));
                        in_work
                            .clone()
                            .filter(|p| p.exists())
                            .or_else(|| in_cwd.clone().filter(|p| p.exists()))
                            .or(in_work)
                            .or(in_cwd)
                            .or(Some(path))
                    })
            })
        } else {
            None
        };
        let found = match out_file.or(from_output) {
            Some(path) => {
                let resolved = if path.exists() {
                    path
                } else {
                    self.wait_for_output_file(path).await
                };
                if resolved.exists() {
                    Some(resolved)
                } else {
                    tracing::warn!(
                        file = %resolved.display(),
                        "auto-detected plugin output file was not created; skipping delivery"
                    );
                    None
                }
            }
            None => None,
        };
        if let Some(ref abs) = found {
            tracing::info!(file = %abs.display(), "auto-detected output file for delivery");
            files_to_send.push(abs.clone());
        }
        found
    }

    async fn wait_for_output_file(&self, path: std::path::PathBuf) -> std::path::PathBuf {
        if path.exists() {
            return path;
        }

        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if path.exists() {
                return path;
            }
        }

        path
    }
}

fn input_schema_has_property(schema: &serde_json::Value, property: &str) -> bool {
    schema
        .get("properties")
        .and_then(|properties| properties.as_object())
        .is_some_and(|properties| properties.contains_key(property))
}

/// Parse the optional `named_outputs` field from a spawn_only plugin's
/// stdout envelope.
///
/// Returns:
/// - `Ok(None)` when the field is absent or `null`.
/// - `Ok(Some(map))` when the field is a JSON object whose entries pass
///   validation (keys match `[a-z][a-z0-9_]*`, values are strings).
/// - `Err(message)` when the field is present but malformed: not an object,
///   contains a non-string value, an empty key, or a key shape violation.
///
/// The contract layer threads the returned map into `ValidatorInvocation`
/// so `${output.<key>}` interpolation can resolve against tool-emitted
/// values. Values are restricted to strings in v1; nested JSON support is
/// deferred.
fn parse_named_outputs(
    raw: Option<&serde_json::Value>,
) -> Result<Option<std::collections::HashMap<String, String>>, String> {
    let Some(value) = raw else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| "named_outputs must be a JSON object".to_string())?;
    if object.is_empty() {
        return Ok(None);
    }
    let mut map = std::collections::HashMap::with_capacity(object.len());
    for (key, entry) in object {
        if !is_valid_named_output_key(key) {
            return Err(format!(
                "named_outputs key '{key}' does not match required shape [a-z][a-z0-9_]*"
            ));
        }
        let string_value = entry.as_str().ok_or_else(|| {
            format!(
                "named_outputs value for '{key}' must be a string, got {}",
                value_kind_label(entry)
            )
        })?;
        map.insert(key.clone(), string_value.to_string());
    }
    Ok(Some(map))
}

/// Validate a `named_outputs` key matches `[a-z][a-z0-9_]*`.
fn is_valid_named_output_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn value_kind_label(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Phase 2-B (SessionScope migration): caller-declared intent used by
/// [`classify_for_intent`] to enforce per-zone read/write rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathIntent {
    /// Plugin will read this path (input keys + `source_image` +
    /// path-shaped `style`). Shared zones (multi-tenant) are readable
    /// with explicit intent; granted dirs (solo) are readable.
    Read,
    /// Plugin will write this path (`out`, `slide_dir`). Shared zones
    /// are refused per the [`PathClassification::InSharedZone`]
    /// contract; only the per-session workspace and solo granted dirs
    /// accept writes.
    Write,
}

/// Lexically join `raw_path` against `base` when relative; return it
/// unchanged when already absolute. Mirrors
/// [`absolutize_path_in_work_dir`] but without the `..` guard — the
/// downstream [`classify_for_canonical`] applies a strict lexical
/// `..` refusal before canonicalising.
///
/// `base` is the registry-rebound `self.work_dir` when set, else
/// `scope.workspace()` — see `rewrite_args_with_scope` doc and codex
/// round-2 P1 for why join base and scope are decoupled.
fn absolutise_against_base(raw_path: &str, base: &std::path::Path) -> std::path::PathBuf {
    let candidate = std::path::Path::new(raw_path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

/// Codex round-2 BLOCKER 1 fix (PR #1327 review): classify a plugin-arg
/// absolute path using the canonical containment guard
/// [`SessionScope::classify_canonical_path`] so a `skill_dir`
/// containing `link -> /outside` cannot smuggle
/// `<skill_dir>/link/secret` through as `InSkillDir`. Mirrors the
/// containment path that file tools use in `tools/mod.rs::resolve_for_scope`.
///
/// Pipeline:
/// 1. Lexically normalise (collapse `.`, refuse `..`). A traversal
///    surface returns `PathClassification::OutOfScope` immediately so
///    the canonicalize walk can't accidentally resurface inside a zone
///    after climbing out.
/// 2. Canonicalise the normalised candidate AND each zone root, then
///    apply the same workspace > granted_dirs > skill_read_zones >
///    shared_zones order as `classify_lexical_path`.
///
/// Returns the (lexically-normalised, NOT canonicalised) absolute path
/// alongside the classification so callers feed `accept_for_intent` and
/// the basename-rescue helper the same lexical form they used before
/// (the canonicalisation is for the classification check only). When
/// the path fails the `..` guard the classification is
/// `PathClassification::OutOfScope` and the returned path is the
/// best-effort lexical join (unused by `accept_for_intent` on the
/// refuse arm).
fn classify_canonical_for_plugin_arg(
    scope: &SessionScope,
    absolute: &std::path::Path,
) -> (PathClassification, std::path::PathBuf) {
    match lexical_normalise_strict_local(absolute) {
        Some(normalised) => {
            let classification = scope.classify_canonical_path(&normalised);
            (classification, normalised)
        }
        // Round-2 BLOCKER 1: refuse `..` here too. The bespoke
        // `classify_lexical_path` path used to swallow this via its
        // own lexical normalise; we replicate that contract so the
        // accept/refuse error message stays identical.
        None => (PathClassification::OutOfScope, absolute.to_path_buf()),
    }
}

/// Local copy of `tools::lexical_normalise_strict` (codex round-2
/// BLOCKER 1). The `tools` module's helper is `pub(crate)` but plugin
/// tooling can't import it directly without dragging in the entire
/// `tools/mod.rs` symbol set; this keeps the plugin-tool dependency
/// surface narrow.
fn lexical_normalise_strict_local(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::Normal(part) => out.push(part),
        }
    }
    Some(out)
}

/// Accept or refuse the absolute path based on the pre-computed
/// classification and the caller-declared [`PathIntent`]. Returns the
/// absolute path as a string on accept; returns a structured
/// `eyre::Report` (echoing the `raw_path` the LLM passed) on refuse.
/// The error message mirrors the "escapes plugin work dir" wording
/// from the bespoke validators so the LLM and downstream test
/// harnesses see consistent diagnostics.
///
/// Codex round-2 refactor: factored out from `classify_for_intent`
/// (now removed) so callers can inspect the classification both for
/// the accept/refuse decision AND for the basename-rescue gate (which
/// must only fire on `InWorkspace`).
fn accept_for_intent(
    classification: &PathClassification,
    absolute: &std::path::Path,
    raw_path: &str,
    intent: PathIntent,
) -> Result<String, eyre::Report> {
    match (classification, intent) {
        // Workspace: read + write both allowed.
        (PathClassification::InWorkspace, _) => Ok(absolute.to_string_lossy().into_owned()),
        // Solo granted dirs: read + write both allowed (the user has
        // explicitly granted access).
        (PathClassification::InGrantedDir { .. }, _) => Ok(absolute.to_string_lossy().into_owned()),
        // Shared zones: read allowed (multi-tenant explicit intent);
        // write refused per the `InSharedZone` doc contract.
        (PathClassification::InSharedZone { .. }, PathIntent::Read) => {
            Ok(absolute.to_string_lossy().into_owned())
        }
        (PathClassification::InSharedZone { zone }, PathIntent::Write) => Err(eyre::eyre!(
            "path '{raw_path}' rejected: shared zone '{}' is read-only — writes refused per SessionScope policy",
            zone.display()
        )),
        // PR-A: read-only plugin skill dirs follow the same policy as
        // shared zones — reads allowed, writes refused. Plugin tools
        // rarely touch their own skill_dir at runtime (skills usually
        // operate inside the host-provided work_dir), but the match
        // arm has to be exhaustive and the read/write split here is
        // consistent with `tools/mod.rs::resolve_for_scope`.
        (PathClassification::InSkillDir { .. }, PathIntent::Read) => {
            Ok(absolute.to_string_lossy().into_owned())
        }
        (PathClassification::InSkillDir { skill_dir }, PathIntent::Write) => Err(eyre::eyre!(
            "path '{raw_path}' rejected: plugin skill dir '{}' is read-only — writes refused per SessionScope policy",
            skill_dir.display()
        )),
        // Out of scope: refuse for both intents. Echo the raw path so
        // the LLM sees what was refused (matches the round-3/4
        // bespoke-validator error contract).
        (PathClassification::OutOfScope, _) => Err(eyre::eyre!(
            "path '{raw_path}' rejected: escapes plugin work dir"
        )),
    }
}

/// Phase 2-B effective-CWD policy (codex P1 fix): when the registry
/// rebound `self.work_dir` via `rebind_plugin_work_dirs` (the hinted-
/// workspace path inside `SessionRuntime::bootstrap`), the construction-
/// time work_dir is the SOURCE OF TRUTH and the scope is intentionally
/// ignored for CWD selection. The scope is only consulted to derive
/// the CWD when `self.work_dir` is `None` (un-hinted / non-registry-
/// rebound callers).
///
/// Rationale: today `SessionScope::multi_tenant_with_default_zones`
/// always derives `workspace = <data>/users/<id>/workspace`, ignoring
/// any `workspace_hint`. The fleet's coding-agent UI hands sessions
/// arbitrary repo paths via the hint; those sessions need their
/// plugin tools to run in the repo, not in the empty default. Until
/// a follow-up aligns scope construction with the hint, the
/// construction-time `self.work_dir` is the only source of truth that
/// reflects the hint.
///
/// When both are `None` we return `None` and the caller skips
/// `cmd.current_dir` (matches pre-Phase-2-B behaviour for plugins
/// never given a workspace).
fn effective_work_dir_for_execute(
    work_dir: Option<&std::path::Path>,
    scope: Option<&SessionScope>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = work_dir {
        return Some(dir.to_path_buf());
    }
    scope.map(|s| s.workspace().to_path_buf())
}

/// Phase 2-B basename-rescue helper (codex rounds 1-3 fixes): after
/// the scope gate accepted the lexically-joined path as `InWorkspace`,
/// this helper preserves the legacy
/// `resolve_plugin_input_path` rescue chain (#1186 `..`-guard +
/// #1189 workspace-root rescue + basename/`_<basename>` suffix scan +
/// redundant `skill-output/` prefix strip) so plugin calls that
/// worked under the bespoke resolver keep working under the
/// scope-aware path.
///
/// The rescue scan root is `join_base` — typically the registry-
/// rebound `self.work_dir` (`<scope.workspace>/skill-output`), so the
/// legacy `skill-output/<prefix>/<file>` doubling AND basename
/// rescues both work.
///
/// IMPORTANT: callers MUST only invoke this for paths classified as
/// `InWorkspace`. Round-2 P2 (codex): allowing the rescue for
/// `InSharedZone` / `InGrantedDir` would let a missing shared/granted
/// path silently rewrite to a workspace file with the same basename
/// — the plugin would then process different input than the LLM
/// requested.
///
/// Returns:
/// - `lexical_absolute` unchanged when it exists on disk (typical case)
/// - the rescued candidate from `resolve_plugin_input_path` when the
///   rescue lands back inside the scope (defence in depth: rejected
///   silently if the rescue escapes; the legacy resolver should never
///   produce that, but the guard catches a future refactor)
/// - `lexical_absolute` unchanged when no rescue applies — the
///   plugin's own `read_to_string` reports `os error 2` cleanly
fn rescue_workspace_input_existence(
    scope: &SessionScope,
    join_base: &std::path::Path,
    raw_path: &str,
    lexical_absolute: &str,
) -> String {
    if std::path::Path::new(lexical_absolute).exists() {
        return lexical_absolute.to_string();
    }
    // Hand off to the legacy resolver chain. It performs the same
    // four-layered rescue (`#1186` `..` guard + `#1189` workspace-root
    // rescue + basename scan + `skill-output/` prefix strip) the
    // pre-Phase-2-B path relied on. `raw_path` is what the LLM
    // passed (NOT the lexically-absolutised version) so the chain
    // can spot the `skill-output/<prefix>` redundancy.
    let Ok(rescued) = resolve_plugin_input_path(raw_path, join_base) else {
        return lexical_absolute.to_string();
    };
    if rescued == lexical_absolute {
        // No-op rescue (the legacy chain produced the same lexical
        // path); skip re-classification.
        return rescued;
    }
    // Defence in depth: re-classify the rescued candidate against
    // the scope. The legacy resolver's #1189 rescue can in principle
    // probe `<workspace>/skill-output/..`, which is still inside the
    // scope but a future widening could regress; reject silently
    // when it escapes.
    //
    // Codex round-2 BLOCKER 1: use canonical classification so a
    // symlinked rescue chain can't sneak the path back out of the
    // workspace. The rescue always reports `InWorkspace` today (the
    // resolver only emits workspace-relative paths) but the canonical
    // check is defence in depth for any future widening that lets the
    // resolver return scope-external paths.
    let rescued_abs = std::path::PathBuf::from(&rescued);
    let (classification, _normalised) = classify_canonical_for_plugin_arg(scope, &rescued_abs);
    match classification {
        PathClassification::InWorkspace => rescued,
        PathClassification::InGrantedDir { .. }
        | PathClassification::InSharedZone { .. }
        | PathClassification::InSkillDir { .. }
        | PathClassification::OutOfScope => lexical_absolute.to_string(),
    }
}

/// Lexically test whether `candidate` stays within `root` after
/// collapsing `.`/`..` components, WITHOUT touching the filesystem.
///
/// Used by the plugin input-path subdir rescue to refuse a
/// workspace-relative candidate that would climb out of the workspace
/// root (defence in depth — the caller already rejected raw `..` input,
/// but the `skill-output/`-stripped form is re-derived and re-checked
/// here). A `..` that pops above `root` makes the running depth go
/// negative → not within.
fn lexically_within(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let Ok(rel) = candidate.strip_prefix(root) else {
        return false; // not even lexically prefixed by root
    };
    let mut depth: i32 = 0;
    for comp in rel.components() {
        match comp {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            // CurDir / RootDir / Prefix: ignore (RootDir/Prefix can't
            // appear in a relative strip result; CurDir is a no-op).
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod subdir_rescue_tests {
    use super::lexically_within;
    use std::path::Path;

    #[test]
    fn lexically_within_accepts_subdir_paths() {
        let root = Path::new("/ws");
        assert!(lexically_within(
            root,
            Path::new("/ws/slides/deck/script.js")
        ));
        assert!(lexically_within(root, Path::new("/ws/file.txt")));
        assert!(lexically_within(root, Path::new("/ws"))); // root itself
    }

    #[test]
    fn lexically_within_rejects_escapes_and_foreign_roots() {
        let root = Path::new("/ws");
        // climbs above root
        assert!(!lexically_within(root, Path::new("/ws/../etc/passwd")));
        assert!(!lexically_within(root, Path::new("/ws/a/../../etc")));
        // not under root at all
        assert!(!lexically_within(root, Path::new("/etc/passwd")));
    }

    #[test]
    fn lexically_within_allows_interior_parent_that_stays_within() {
        let root = Path::new("/ws");
        // dips into a subdir then back up — net still inside
        assert!(lexically_within(root, Path::new("/ws/a/b/../c")));
    }
}

/// Resolve a plugin tool's input path (`audio_path` / `file_path` /
/// `input` / `script_path` / `video_path` / `text_path` / per-slide
/// `source_image`) to an absolute on-disk string.
///
/// Order:
///
/// 1. Try the shared
///    [`octos_bus::file_handle::resolve_tool_path`] resolver — the same
///    table that powers the file tools. This handles `up/...` /
///    `pf/...` handles (with both 3-segment and LLM-truncated
///    2-segment forms), and absolute paths inside the upload tmpdir.
///    Accept the result UNCONDITIONALLY when the resolver returned a
///    non-workspace scope (upload tmpdir / profile root), because those
///    scopes already include an existence check via canonicalize.
/// 2. For workspace scope, only accept if the resolved file actually
///    exists. Otherwise fall through to the plugin-specific filename
///    heuristics in [`resolve_path_in_work_dir`] — the legacy code
///    looks up `<work_dir>/<basename>` and `_<basename>` suffix
///    matches, which rescues live plugin calls where the LLM hallucinates
///    a directory prefix in front of a basename that exists at the
///    workspace root (codex review pin, 2026-05-13: `uploads/mark.wav`
///    when only `mark.wav` exists must still recover).
/// 3. Final fallback: lexically join with `work_dir` (the previous
///    behaviour of `absolutize_path_in_work_dir`) so the plugin never
///    sees an empty string.
fn resolve_plugin_input_path(
    raw_path: &str,
    work_dir: &std::path::Path,
) -> Result<String, eyre::Report> {
    use octos_bus::file_handle::ToolPathScope;
    // Codex round-3 BLOCKER fix (PR #1186 review): FAIL CLOSED on raw
    // `..` (`ParentDir`) components. The previous revision returned
    // `raw_path.to_string()` unchanged for unsafe inputs, but the
    // plugin process is then spawned with `cmd.current_dir(work_dir)`,
    // so when the plugin itself opens `../secret.md` (e.g. via
    // `fs::read`) the kernel resolves it relative to `work_dir` and
    // escapes the chroot. The host-side resolver MUST return an error
    // here so the call site short-circuits the entire spawn and
    // surfaces the rejection to the LLM as a tool error envelope.
    //
    // Absolute paths and Windows prefixes are NOT rejected at this
    // entry — `resolve_tool_path` will refuse out-of-scope absolutes,
    // and `resolve_path_in_work_dir`'s basename fallback discards
    // directory components safely. Only `..` poisons the resolution.
    if std::path::Path::new(raw_path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(eyre::eyre!(
            "path '{raw_path}' rejected: escapes plugin work dir"
        ));
    }
    // B1 fleet UX soak (mini2/iter1, mini5/iter2): when the host has
    // chrooted plugin `work_dir` into `<workspace>/skill-output/` (the
    // modern `runtime/session.rs` path), but the LLM passes a
    // workspace-relative path that still carries the `skill-output/`
    // prefix (because `write_file` resolves against the workspace
    // ROOT and the LLM mirrors that path), the naive
    // `work_dir.join(raw_path)` produces
    // `<workspace>/skill-output/skill-output/<rest>` and
    // `read_to_string` fails with `os error 2`. Strip the redundant
    // prefix the same way `mofa-podcast::resolve_output_dir` does for
    // output paths, then probe both forms — the stripped path wins
    // when it exists.
    let stripped = strip_redundant_skill_output_prefix(raw_path, work_dir);
    if let Some(ref stripped_path) = stripped {
        if let Ok(resolved) =
            octos_bus::file_handle::resolve_tool_path(work_dir, None, stripped_path)
        {
            if matches!(resolved.scope, ToolPathScope::Workspace) && resolved.absolute.exists() {
                return Ok(resolved.absolute.to_string_lossy().into_owned());
            }
        }
    }
    if let Ok(resolved) = octos_bus::file_handle::resolve_tool_path(work_dir, None, raw_path) {
        let accept = match resolved.scope {
            // Upload / profile scopes go through `canonicalize_under`,
            // so existence is already guaranteed.
            ToolPathScope::UploadTmpdir | ToolPathScope::Profile => true,
            // Workspace scope returns the LEXICAL workspace location
            // (so the tool's `O_NOFOLLOW` gate can refuse symlinks),
            // which means missing files slip through. Plugins need the
            // legacy filename fallback for those, so only accept the
            // workspace result when the file actually exists.
            ToolPathScope::Workspace => resolved.absolute.exists(),
        };
        if accept {
            return Ok(resolved.absolute.to_string_lossy().into_owned());
        }
    }
    // NEW-02 mini5 soak fix: when `write_file` resolves against the
    // workspace ROOT but plugin work_dir is chrooted to
    // `<workspace>/skill-output/`, the script lives one level ABOVE the
    // chroot. The shared resolver doesn't probe `work_dir.parent()`, so
    // a podcast script written to `<workspace>/octos_podcast_script.md`
    // never resolves and the plugin spawn fails with `os error 2`.
    //
    // This rescue branch is bounded by FOUR safety constraints (see
    // #1186 path-traversal review and codex review on #1189):
    //   1. `raw_path` is already guarded against raw `..` at the entry
    //      of this function — unsafe inputs returned Err above.
    //   2. We ONLY probe `work_dir.parent()` when the work_dir basename
    //      is exactly `skill-output`. The parent is then the workspace
    //      root by construction (runtime/session.rs always chroots
    //      `<workspace>/skill-output/`), NOT an arbitrary directory.
    //   3. We use `Path::file_name()` (not the raw path) so any
    //      directory components in `raw_path` are discarded — the only
    //      candidate we ever try is `<workspace>/<basename>`. This
    //      makes the rescue equivalent in scope to the basename scan
    //      in `resolve_path_in_work_dir`, just probing one directory
    //      level above the chroot instead of inside it.
    //   4. We use `symlink_metadata` + `is_file()` (NOT `exists()`,
    //      which follows symlinks). A `<workspace>/script.md` symlink
    //      pointing at `/etc/passwd` MUST NOT resolve. This matches
    //      the workspace's broader symlink-safety posture
    //      (`O_NOFOLLOW` in file tools, see CLAUDE.md). Directories
    //      are also rejected — the only acceptable candidate is a
    //      regular file at the workspace root.
    //
    // TOCTOU note (codex review #1189): the host checks
    // `symlink_metadata` before handing the path to the plugin
    // (which then opens the file itself). A race where the path is
    // swapped for a symlink AFTER the check would defeat this check
    // — but that race is shared with the rest of this resolver chain
    // (see `resolve_path_in_work_dir` line ~1116, which also uses
    // `exists()`) and is fundamental to the plugin-spawn model: the
    // host can't hold an `O_NOFOLLOW` fd that the plugin subprocess
    // will then open. Closing the race fully requires plumbing file
    // descriptors / O_NOFOLLOW opens through the plugin protocol,
    // which is out of scope here. The static-symlink rejection
    // implemented below CLOSES the realistic mistake (LLM-driven
    // symlink in the workspace from a prior tool call), even if it
    // doesn't fix the adversarial race.
    if work_dir.file_name().and_then(|s| s.to_str()) == Some("skill-output") {
        if let Some(parent) = work_dir.parent() {
            // #1377 slides fix: BEFORE the basename-only rescue, try the
            // FULL workspace-relative path under the workspace root. The
            // basename rescue below only finds a file at `<workspace>/
            // <basename>`, so a SUBDIR-prefixed input like
            // `slides/<deck>/script.js` (the documented mofa-slides
            // `input:` form, written by `write_file` against the workspace
            // ROOT) never resolves — the work_dir is chrooted to
            // `<workspace>/skill-output/`, so `work_dir.join(raw_path)`
            // probes `<workspace>/skill-output/slides/...` and misses.
            // Without this the agent's safe `input:` mode fails with
            // `os error 2` and it falls back to partial inline `slides`
            // arrays that overwrite earlier slides (position-based
            // filenames). Probe `raw_path` and its `skill-output/`-
            // stripped form against the workspace root, guarded by the
            // same symlink/regular-file check as the basename rescue PLUS
            // a lexical-containment check (raw `..` already returned Err
            // at this fn's entry; this rejects any candidate that still
            // escapes the workspace root after normalisation).
            let workspace_root = parent;
            for rel in [Some(raw_path), stripped.as_deref()].into_iter().flatten() {
                let candidate = workspace_root.join(rel);
                // Lexical containment: the normalised candidate must stay
                // under the workspace root. `lexically_within` collapses
                // `.`/`..` without touching disk; a candidate that climbs
                // out (despite the entry `..` guard, e.g. via the stripped
                // form) is refused.
                if !lexically_within(workspace_root, &candidate) {
                    continue;
                }
                // Guard 1 (final component): reject if the candidate's LEAF
                // is a symlink or non-regular file. `symlink_metadata` does
                // NOT follow the final component, so a `script.js -> …`
                // symlink at the candidate path is refused regardless of its
                // target — matching the basename rescue's target-agnostic,
                // TOCTOU-resistant posture.
                let leaf_is_regular_file = std::fs::symlink_metadata(&candidate)
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false);
                if !leaf_is_regular_file {
                    continue;
                }
                // Guard 2 (ancestors — codex round-1 P1): unlike the basename
                // rescue (single component), this candidate carries SUBDIR
                // components, so a symlinked ANCESTOR (`<workspace>/slides ->
                // /etc`) could let `slides/passwd` escape — `symlink_metadata`
                // above only checks the LEAF, and it traverses symlinked
                // parents. Canonicalize the full candidate (resolves every
                // ancestor symlink) and require it to stay under the canonical
                // workspace root.
                let (Ok(canon), Ok(canon_root)) = (
                    std::fs::canonicalize(&candidate),
                    std::fs::canonicalize(workspace_root),
                ) else {
                    continue;
                };
                if !canon.starts_with(&canon_root) {
                    continue; // a symlinked ancestor escaped the workspace
                }
                // Both guards passed: return the LEXICAL candidate (not
                // `canon`). Containment is proven, so the lexical path names
                // the same in-workspace file, and the legacy resolver
                // contract is to return the workspace-relative lexical form
                // (callers/tests rely on it; canonicalize would also rewrite
                // macOS `/var`->`/private/var`).
                return Ok(candidate.to_string_lossy().into_owned());
            }
            if let Some(basename) = std::path::Path::new(raw_path).file_name() {
                let candidate = parent.join(basename);
                // Reject symlinks AND non-regular files (directories,
                // sockets, FIFOs). symlink_metadata does not traverse,
                // so a `script.md -> /etc/passwd` symlink at the
                // workspace root returns FileType::is_symlink() == true
                // and is_file() == false — safely refused.
                let safe = std::fs::symlink_metadata(&candidate)
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false);
                if safe {
                    return Ok(candidate.to_string_lossy().into_owned());
                }
            }
        }
    }
    // Codex BLOCKER fix (PR #1186 review): the lexical-join branches
    // inside `resolve_path_in_work_dir` would otherwise let candidates
    // like `skill-output/../secret.md` resolve to `<workspace>/secret.md`
    // (escaping the chrooted `skill-output/` work_dir). The unsafe-
    // component guard inside `resolve_path_in_work_dir` skips those
    // branches but still permits the SAFE basename-only fallback,
    // and `strip_redundant_skill_output_prefix` independently refuses
    // to strip `..`-containing raw paths.
    if let Some(ref stripped_path) = stripped {
        if let Some(resolved) = resolve_path_in_work_dir(stripped_path, work_dir) {
            return Ok(resolved);
        }
    }
    // The round-3 `..` guard at the entry of `resolve_plugin_input_path`
    // already returned Err for any raw `..` input, so this fallback is
    // only reached for safe inputs. The Err arm of
    // `absolutize_path_in_work_dir` is therefore unreachable in practice;
    // we still `?`-propagate as defense in depth.
    if let Some(resolved) = resolve_path_in_work_dir(raw_path, work_dir) {
        return Ok(resolved);
    }
    absolutize_path_in_work_dir(raw_path, work_dir)
}

/// Reject paths that carry `..` (`ParentDir`) components or absolute
/// roots (`RootDir` / `Prefix`). Used as a defense-in-depth guard around
/// the lexical `work_dir.join(...)` fallback in
/// [`resolve_plugin_input_path`] — without it, a candidate like
/// `skill-output/../secret.md` would resolve to `<work_dir>/../secret.md`
/// (one level above the chrooted plugin work_dir) even though the
/// shared `resolve_tool_path` resolver would have rejected it.
fn has_unsafe_components(path: &std::path::Path) -> bool {
    use std::path::Component;
    path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

/// Returns `Some(stripped)` when `raw_path` carries a redundant
/// `skill-output/` prefix that should be removed before joining with
/// `work_dir` — i.e. `work_dir` itself terminates in a `skill-output`
/// component AND `raw_path` is relative and starts with `skill-output/`.
/// Mirrors the same guard the mofa-podcast skill applies for output
/// directories (see `resolve_output_dir` in mofa-podcast/src/main.rs).
fn strip_redundant_skill_output_prefix(
    raw_path: &str,
    work_dir: &std::path::Path,
) -> Option<String> {
    let raw = std::path::Path::new(raw_path);
    if raw.is_absolute() {
        return None;
    }
    // Codex BLOCKER fix (PR #1186 review): refuse to strip when the raw
    // path contains any `..` component. Otherwise
    // `skill-output/../secret.md` would strip to `../secret.md` and the
    // fallback `work_dir.join(...)` would escape the chrooted
    // `skill-output/` subdir.
    if has_unsafe_components(raw) {
        return None;
    }
    if work_dir.file_name().and_then(|s| s.to_str()) != Some("skill-output") {
        return None;
    }
    let stripped = raw.strip_prefix("skill-output").ok()?;
    let stripped_str = stripped.to_str()?.to_string();
    if stripped_str.is_empty() {
        return None;
    }
    Some(stripped_str)
}

fn resolve_path_in_work_dir(raw_path: &str, work_dir: &std::path::Path) -> Option<String> {
    let candidate = std::path::Path::new(raw_path);

    // Codex round-2 BLOCKER fix (PR #1186 review): fail fast for any
    // candidate carrying `..` (`ParentDir`) — before ANY branch. The
    // basename-fallback below joins `work_dir.join(file_name())`,
    // which CANNOT escape (file_name discards directory components),
    // so absolute paths and Windows prefixes are still allowed to flow
    // through to the basename fallback (legitimate use: LLM passes an
    // absolute path that doesn't exist on this host, but the basename
    // exists in `work_dir`). Only `..` poisons the resolution because
    // the upstream `resolve_plugin_input_path` would otherwise fall
    // back to `absolutize_path_in_work_dir` on a `None` here and
    // construct `<work_dir>/../foo` — escaping the chroot.
    if candidate
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }

    // Codex BLOCKER fix (PR #1186 review): the absolute, raw-relative,
    // and lexical-join branches below must NOT accept inputs that
    // would let a plugin arg escape `work_dir`. Skip them entirely
    // when the candidate carries `..` (`ParentDir`) components or is
    // absolute / has a Windows prefix. The basename-fallback branch
    // further down is still safe because it discards directory
    // components and only joins `file_name()` onto `work_dir`.
    let contained = !has_unsafe_components(candidate);
    if contained {
        if candidate.is_absolute() && candidate.exists() {
            return Some(raw_path.to_string());
        }

        let nested = work_dir.join(candidate);
        if nested.exists() {
            return Some(nested.to_string_lossy().into_owned());
        }

        if candidate.exists() {
            return Some(raw_path.to_string());
        }
    }

    let filename = candidate.file_name()?;
    let resolved = work_dir.join(filename);
    if resolved.exists() {
        return Some(resolved.to_string_lossy().into_owned());
    }

    let filename_str = filename.to_str()?;
    for entry in std::fs::read_dir(work_dir).ok()? {
        let entry = entry.ok()?;
        let entry_path = entry.path();
        let entry_name = entry_path.file_name()?.to_str()?;
        if entry_name == filename_str || entry_name.ends_with(&format!("_{filename_str}")) {
            return Some(entry_path.to_string_lossy().into_owned());
        }
    }

    None
}

/// Lexically join a raw plugin-arg path onto `work_dir` (or pass an
/// absolute path through unchanged).
///
/// Codex round-4 BLOCKER fix (PR #1186 review): FAIL CLOSED on raw `..`
/// (`ParentDir`) components. This helper is used to absolutize OUTPUT
/// path keys (`out`, `slide_dir`) inside `rewrite_workspace_file_args`,
/// as well as the slides-style and resolver-fallback paths. Plugins are
/// spawned with `cmd.current_dir(work_dir)`, so a path like
/// `../escape.txt` would otherwise have its `..` resolved by the kernel
/// relative to the chrooted work_dir when the plugin process WRITES the
/// output — escaping the chroot. The host-side rewriter MUST return an
/// error so the call site short-circuits the spawn and surfaces the
/// rejection to the LLM as a tool error envelope. Mirrors the fail-
/// closed contract in [`resolve_plugin_input_path`] (round-3).
fn absolutize_path_in_work_dir(
    raw_path: &str,
    work_dir: &std::path::Path,
) -> Result<String, eyre::Report> {
    let candidate = std::path::Path::new(raw_path);
    if has_unsafe_components_parent_only(candidate) {
        return Err(eyre::eyre!(
            "path '{raw_path}' rejected: escapes plugin work dir"
        ));
    }
    if candidate.is_absolute() {
        Ok(raw_path.to_string())
    } else {
        Ok(work_dir.join(candidate).to_string_lossy().into_owned())
    }
}

/// Like [`has_unsafe_components`] but only checks for `..` (`ParentDir`).
/// The full `has_unsafe_components` also rejects absolute roots, but
/// [`absolutize_path_in_work_dir`] intentionally allows absolutes through
/// (they are passed verbatim — sandbox / scope checks are the next gate).
fn has_unsafe_components_parent_only(path: &std::path::Path) -> bool {
    path.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Resolve a slides-style argument relative to `work_dir`.
///
/// Codex round-4 BLOCKER fix (PR #1186 review): now returns
/// `Result<Option<String>, eyre::Report>` instead of `Option<String>`.
/// When the style value carries raw `..` components, the underlying
/// `absolutize_path_in_work_dir` returns Err — we propagate that Err
/// up so `rewrite_workspace_file_args` short-circuits the spawn rather
/// than silently passing an escape attempt to the plugin. `Ok(None)`
/// still indicates "no resolution" (caller falls through to the next
/// rewrite branch); `Ok(Some(_))` is the successful resolution.
fn resolve_slides_style_in_work_dir(
    style: &str,
    work_dir: &std::path::Path,
) -> Result<Option<String>, eyre::Report> {
    let trimmed = style.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let candidate = std::path::Path::new(trimmed);
    if candidate.is_absolute() || trimmed.contains('/') || trimmed.contains('\\') {
        return Ok(Some(absolutize_path_in_work_dir(trimmed, work_dir)?));
    }

    let filename = if trimmed.ends_with(".toml") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.toml")
    };
    let resolved = work_dir.join("styles").join(filename);
    Ok(resolved
        .exists()
        .then(|| resolved.to_string_lossy().into_owned()))
}

fn normalize_mofa_style_name(style: &str) -> Option<String> {
    let trimmed = style.trim();
    if trimmed.is_empty() {
        return None;
    }

    let candidate = std::path::Path::new(trimmed);
    let filename = candidate.file_name()?.to_str()?.trim();
    let mut normalized = filename;
    while let Some(stripped) = normalized.strip_suffix(".toml") {
        normalized = stripped;
    }
    let normalized = normalized.trim();
    (!normalized.is_empty()).then(|| normalized.to_string())
}

/// Pre-flight validator for `mofa_slides`' `style` argument.
///
/// Mirrors the `RunPipelineTool::pre_flight_validate` pattern (PR #1015): catch
/// known-bad LLM-generated input synchronously in the foreground so the
/// spawn_only intercept records the failure on `iter_tool_success` and the LLM
/// sees a `[VALIDATION FAILED] …` tool_result in its next iteration. Without
/// this, the foreground intercept emits the synth-ack ("Background work
/// started for `mofa_slides`.") to the LLM, the plugin later writes
/// `{"success":false,"output":"style not found"}`, but the LLM-side
/// conversation has already moved on — only the UI sees the failure and the
/// model never retries with a corrected style.
///
/// Scope is deliberately narrow:
/// - missing / empty `style` → `Ok` (plugin's default-style path).
/// - any non-empty `style` (bare name, `name.toml`, absolute path, slash-
///   containing path, traversal) → normalize to a basename stem (same shape
///   `normalize_mofa_style_name` produces at the rewriter), then look for
///   `<dir>/styles/<stem>.toml` under each candidate directory.
///
/// Candidate directories searched, in order:
///   1. `<skill_dir>/styles/<stem>.toml` — built-in styles shipped with the
///      plugin.
///   2. `<work_dir>/styles/<stem>.toml` — `SessionRuntime` binds plugin
///      `work_dir` to `<workspace>/skill-output`, so this covers styles
///      authored under that subdirectory.
///   3. `<work_dir.parent()>/styles/<stem>.toml` — covers the workspace-root
///      `styles/` directory that `slides_default.txt:62` instructs the LLM
///      to author into. Without this probe, a valid custom style at
///      `<workspace>/styles/foo.toml` would be falsely rejected when the
///      plugin runs from `<workspace>/skill-output`.
///
/// Codex review on PR #1323:
/// - BLOCKER: previously only `<work_dir>/styles/`, falsely rejecting
///   workspace-root customs the prompt tells the LLM to create.
/// - MAJOR: previously bare `if path-like → Ok` skipped path-shaped values,
///   so `style: "../etc/passwd"` bypassed pre-flight and surfaced as a
///   background failure only the UI saw. Now the basename is normalized
///   first (matching the `normalize_mofa_style_name` rewriter) so traversal,
///   absolute paths, and slash-containing values are all validated against
///   the same on-disk lookup as bare names.
fn validate_mofa_slides_style(
    args: &serde_json::Value,
    skill_dir: Option<&std::path::Path>,
    work_dir: Option<&std::path::Path>,
) -> Result<(), String> {
    let Some(style) = args.get("style").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let trimmed = style.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    // Mirror the rewriter at tool.rs:609 / tool.rs:778: take the basename and
    // strip any `.toml` suffix. The rewriter will normalize a path-shaped
    // value to this same stem before the plugin sees it, so the pre-flight
    // MUST validate the post-normalization name — otherwise traversal /
    // absolute / slash-prefixed values slip past and fail in the background.
    let Some(stem) = normalize_mofa_style_name(trimmed) else {
        return Err(format!(
            "style '{trimmed}' is not a valid style name (must normalize to a non-empty basename). \
            See SKILL.md `Custom styles (full TOML)` section."
        ));
    };
    let filename = format!("{stem}.toml");

    let parent_probe = work_dir
        .filter(|wd| {
            wd.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == "skill-output")
                .unwrap_or(false)
        })
        .and_then(|wd| wd.parent());

    for dir in [skill_dir, work_dir, parent_probe].into_iter().flatten() {
        if dir.join("styles").join(&filename).exists() {
            return Ok(());
        }
    }

    let mut msg = format!("style '{trimmed}' not found");
    let builtin = list_available_styles(skill_dir);
    if !builtin.is_empty() {
        msg.push_str("\nAvailable built-in styles: ");
        msg.push_str(&builtin.join(", "));
    }
    let mut custom_dirs: Vec<&std::path::Path> = Vec::new();
    if let Some(wd) = work_dir {
        custom_dirs.push(wd);
    }
    if let Some(parent) = parent_probe {
        custom_dirs.push(parent);
    }
    let mut custom: Vec<String> = custom_dirs
        .iter()
        .flat_map(|dir| list_available_styles(Some(dir)))
        .collect();
    custom.sort();
    custom.dedup();
    if !custom.is_empty() {
        msg.push_str("\nAvailable workspace custom styles: ");
        msg.push_str(&custom.join(", "));
    }
    // Use the normalized stem in the authoring hint so a caller-supplied
    // `style: "foo.toml"` does not become `styles/foo.toml.toml`.
    let hint_root = parent_probe.or(work_dir);
    if let Some(wd) = hint_root {
        msg.push_str(&format!(
            "\nHint: author a workspace custom style at {}/styles/{stem}.toml.",
            wd.display()
        ));
    }
    msg.push_str("\nSee SKILL.md `Custom styles (full TOML)` section.");
    Err(msg)
}

/// List `*.toml` style filenames (stem only) under `<dir>/styles/`. Returns
/// `Vec::new()` when `dir` is `None`, when `styles/` does not exist, or when
/// the read fails — callers treat an empty list as "nothing to suggest" and
/// fall through to the path hint, so an IO error here degrades gracefully.
fn list_available_styles(dir: Option<&std::path::Path>) -> Vec<String> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    let styles_dir = dir.join("styles");
    let Ok(entries) = std::fs::read_dir(&styles_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .collect();
    names.sort();
    names
}

/// RAII guard that SIGKILLs a spawned plugin's entire PROCESS GROUP on drop.
///
/// Cancellation-safety (codex re-review of af3597ab — Gap 3's "limits must
/// degrade, never leak"): the registry's per-tool timeout wraps `execute()` in
/// `tokio::time::timeout`, which DROPS the future on elapse. `kill_on_drop(true)`
/// on the plugin `Command` reaps the DIRECT child on that drop, but NOT any
/// grandchildren the plugin spawned. This guard — owned by the `execute` future
/// alongside the `Child` — sends `kill -9 -<pid>` (negative pid = whole process
/// group, which exists because the plugin was spawned with `process_group(0)`)
/// when the future is dropped, reaping the entire tree.
///
/// On the normal-completion path the plugin has already exited and been reaped,
/// so the guard is `disarm()`ed to avoid a redundant kill. Even if it were not
/// disarmed, a group-kill after exit is a harmless no-op (the kernel returns
/// ESRCH for a vanished group), so the guard is purely best-effort: its `Drop`
/// never panics and ignores all errors.
#[cfg(unix)]
struct ProcessGroupKillGuard {
    /// pgid == the plugin's pid (it was spawned into its own group). 0 = unset
    /// / disarmed (no group to reap).
    pid: u32,
}

#[cfg(unix)]
impl ProcessGroupKillGuard {
    fn new(pid: u32) -> Self {
        Self { pid }
    }

    /// Disarm on the normal-completion path: the plugin already exited and was
    /// reaped, so there is no group left to kill.
    fn disarm(&mut self) {
        self.pid = 0;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKillGuard {
    fn drop(&mut self) {
        // Best-effort: never panic in Drop, ignore every error. A pid of 0
        // means disarmed (or never armed); skip. The negative pid targets the
        // whole process group established by `process_group(0)`, reaping any
        // grandchildren the plugin spawned. A group-kill after the leader has
        // already exited is a harmless ESRCH no-op.
        if self.pid == 0 {
            return;
        }
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{}", self.pid)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[async_trait]
impl Tool for PluginTool {
    fn name(&self) -> &str {
        &self.tool_def.name
    }

    fn description(&self) -> &str {
        &self.tool_def.description
    }

    fn concurrency_class(&self) -> super::super::tools::ConcurrencyClass {
        // Item 6 of OCTOS_M8_FIX_FIRST_CHECKLIST_2026-04-24:
        // honour the plugin manifest's optional `concurrency_class`
        // hint instead of inheriting the trait default `Safe`. When the
        // plugin author marks the tool as `"exclusive"` (e.g. it
        // mutates shared state, posts to a remote service, or writes
        // to disk) the M8.8 scheduler serialises it against siblings.
        //
        // Issue #718 follow-up: align with `McpServerConfig::resolved_concurrency_class`
        // — unknown literals fail-closed to `Exclusive` so a typo like
        // `"exclusve"` does not silently permit parallel writes. The
        // loader already emits a `warn!` on `Unknown` so misconfigurations
        // are visible; this resolver is the runtime safety net.
        match self
            .tool_def
            .concurrency_class
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("") | Some("safe") => super::super::tools::ConcurrencyClass::Safe,
            Some("exclusive") => super::super::tools::ConcurrencyClass::Exclusive,
            // Unknown values fail-safe to Exclusive — matches MCP behavior.
            Some(_) => super::super::tools::ConcurrencyClass::Exclusive,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut schema = self.tool_def.input_schema.clone();
        // Inject `timeout_secs` so the LLM can request longer timeouts for
        // complex tasks.  Only added when the schema is an object with
        // "properties" and doesn't already define the field.
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            if !props.contains_key("timeout_secs") {
                props.insert(
                    "timeout_secs".to_string(),
                    serde_json::json!({
                        "type": "integer",
                        "description": "Timeout in seconds. Estimate based on real execution times: single search (depth=2) ~3min → 300s; single search (depth=3) ~5min → 400s; research pipeline with 3 topics ~8min → 600s; research pipeline with 5-7 topics ~15-20min → 1200s; very complex multi-source analysis ~25min → 1500s. Max: 1800. Default: 600"
                    }),
                );
            }
        }
        schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// Synchronous foreground validation of LLM-generated arguments.
    ///
    /// Currently gated to `mofa_slides` only: catches `style="..."` bare-name
    /// values that don't resolve to a `<skill_dir>/styles/<name>.toml` or
    /// `<work_dir>/styles/<name>.toml` before the spawn_only intercept hands
    /// the call off to a background task. This closes the spawn_only
    /// synth-ack gap (LLM was told "started" while the plugin later wrote
    /// `success:false` only the UI ever saw — see the doc comment on
    /// `validate_mofa_slides_style`). The check is intentionally cheap (path
    /// existence + a single `read_dir` for the error message) so the
    /// foreground turn isn't blocked.
    ///
    /// Other plugin tools fall through to the trait default (`Ok`).
    async fn pre_flight_validate(&self, args: &serde_json::Value) -> Result<(), String> {
        if self.tool_def.name == "mofa_slides" {
            let skill_dir = self.executable.parent();
            let work_dir = self.work_dir.as_deref();
            validate_mofa_slides_style(args, skill_dir, work_dir)?;
        }
        Ok(())
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        tracing::info!(
            plugin = %self.plugin_name,
            tool = %self.tool_def.name,
            executable = %self.executable.display(),
            timeout_secs = self.timeout.as_secs(),
            args_size = args.to_string().len(),
            "spawning plugin process"
        );

        // Section C: pre-spawn re-hash gate. When a load-time hash was
        // recorded — either because the manifest declared `sha256` OR
        // because `require_signed` was on — re-read the verified-exe
        // bytes, recompute SHA-256, and compare against what we approved
        // at load time. A mismatch means the verified copy on disk has
        // been swapped between load and invocation; refuse to run.
        //
        // When neither path applied (no manifest hash AND
        // `require_signed = false`) the gate is skipped so the legacy
        // unverified path stays cheap. Under `require_signed = true` the
        // loader guarantees `verified_exe_sha256` is populated for every
        // tool that reached the registry.
        //
        // First call: detect a tampered-at-load binary before we issue an
        // approval prompt that the user might wait on for minutes. Cheap
        // up-front rejection of obvious tampering.
        if let Some(refusal) = self.check_verified_exe_hash() {
            return Ok(refusal);
        }

        // Phase 2-B: snapshot `ToolContext` up front so the approval
        // prompt below (P3 codex fix) can reflect the effective CWD,
        // not the construction-time `self.work_dir`.
        let ctx_snapshot: Option<ToolContext> = TOOL_CTX.try_with(|c| c.clone()).ok();
        let effective_work_dir = effective_work_dir_for_execute(
            self.work_dir.as_deref(),
            ctx_snapshot
                .as_ref()
                .and_then(|c| c.session_scope.as_deref()),
        );

        // M6 req 4: enforce manifest-declared `risk` field (UPCR-2026-001).
        // When the manifest declares `risk: "high"` or `risk: "critical"`,
        // request user approval before spawning the plugin process. `low`
        // and unspecified/unknown literals fall through (no enforced gate)
        // so existing skills that don't declare `risk` keep working
        // unchanged.
        let risk_gate = ManifestRiskGate::classify(self.tool_def.risk.as_deref());
        // yolo GAP #2: the manifest risk gate must honor `ApprovalPolicy`,
        // just like shell.rs's `Decision::Ask` path. Two overrides sit ahead
        // of the interactive prompt:
        //   1. A DangerFullAccess / AllowAll ("yolo") context auto-allows —
        //      parity with the shell tools swapping `SafePolicy` for
        //      `AllowAllPolicy` under danger. This takes precedence so a
        //      dangerous session (whose `approval_policy` is `Never`) still
        //      runs high-risk plugins instead of denying them.
        //   2. Otherwise `ApprovalPolicy::Never` denies WITHOUT prompting —
        //      fail-closed parity with shell.rs ("approval_policy is never").
        // Only when neither override applies (`ApprovalPolicy::Ask`) do we
        // fall through to the interactive approval round-trip below.
        if risk_gate.requires_approval() && self.auto_approve_high_risk {
            tracing::debug!(
                plugin = %self.plugin_name,
                tool = %self.tool_def.name,
                risk = ?self.tool_def.risk,
                "manifest risk gate auto-allowed (danger full access context)"
            );
        } else if risk_gate.requires_approval() && !self.approval_policy.allows_prompt() {
            tracing::warn!(
                plugin = %self.plugin_name,
                tool = %self.tool_def.name,
                risk = ?self.tool_def.risk,
                "plugin tool requires approval but approval_policy is never — denied"
            );
            return Ok(ToolResult {
                output: format!(
                    "Plugin tool '{}' requires approval (manifest risk={:?}) but approval_policy is never: denied without prompting.",
                    self.tool_def.name,
                    self.tool_def.risk.as_deref().unwrap_or("unspecified")
                ),
                success: false,
                ..Default::default()
            });
        } else if risk_gate.requires_approval() {
            let requester = TOOL_APPROVAL_CTX.try_with(Clone::clone).ok();
            let Some(requester) = requester else {
                tracing::warn!(
                    plugin = %self.plugin_name,
                    tool = %self.tool_def.name,
                    risk = ?self.tool_def.risk,
                    "plugin tool requires approval but no interactive approval bridge is in scope — denied"
                );
                return Ok(ToolResult {
                    output: format!(
                        "Plugin tool '{}' requires approval (manifest risk={:?}) and was denied: no interactive approval bridge available.",
                        self.tool_def.name,
                        self.tool_def.risk.as_deref().unwrap_or("unspecified")
                    ),
                    success: false,
                    ..Default::default()
                });
            };

            let tool_id = TOOL_CTX
                .try_with(|ctx| ctx.tool_id.clone())
                .unwrap_or_default();
            let title = format!(
                "Approve {} ({})",
                self.tool_def.name,
                self.tool_def
                    .risk
                    .as_deref()
                    .map(str::trim)
                    .filter(|risk| !risk.is_empty())
                    .unwrap_or("high")
            );
            let body = format!(
                "Plugin '{}' tool '{}' is declared {} risk in its manifest.",
                self.plugin_name,
                self.tool_def.name,
                self.tool_def
                    .risk
                    .as_deref()
                    .map(str::trim)
                    .filter(|risk| !risk.is_empty())
                    .unwrap_or("high")
            );
            let decision = requester
                .request_approval(ToolApprovalRequest {
                    tool_id,
                    tool_name: self.tool_def.name.clone(),
                    title,
                    body,
                    command: None,
                    // Codex P3 fix (Phase 2-B): the approval prompt
                    // MUST surface the directory the plugin will
                    // actually run in. Before Phase 2-B this was
                    // `self.work_dir`; in scoped sessions where the
                    // session_scope is the source of truth (and the
                    // registry didn't rebind via `clone_with_work_dir`)
                    // that's the scope workspace. Use the same
                    // `effective_work_dir` value `cmd.current_dir`
                    // sets further down so the user sees what they
                    // approved.
                    cwd: effective_work_dir
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                })
                .await;
            if matches!(decision, ToolApprovalDecision::Deny) {
                tracing::warn!(
                    plugin = %self.plugin_name,
                    tool = %self.tool_def.name,
                    "plugin tool denied by interactive approval"
                );
                return Ok(ToolResult {
                    output: format!(
                        "Plugin tool '{}' denied by user approval.",
                        self.tool_def.name
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        }

        let mut cmd = Command::new(&self.executable);
        cmd.arg(&self.tool_def.name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Cancellation-safety (codex review of 7c3e5eac): the registry's
            // per-tool timeout (`ToolRegistry::execute_with_context`) wraps
            // this future in `tokio::time::timeout`, which DROPS the future on
            // elapse. The spawned `Child` below is owned by this future, so
            // dropping the future drops the `Child`. With `kill_on_drop(true)`
            // tokio sends SIGKILL to the DIRECT child (and the runtime's reaper
            // collects it) on that drop. `kill_on_drop` alone, however, reaps
            // ONLY the direct child — a plugin that spawns its own children
            // (a worker, `sleep 600 &`, etc.) would leak those grandchildren on
            // a registry-timeout drop. The `ProcessGroupKillGuard` installed
            // after spawn closes that gap by SIGKILLing the whole PROCESS GROUP
            // on drop (see below); it is paired with `process_group(0)` to make
            // the group exist. The plugin's own kill branches below remain the
            // graceful path (process-group kill -9 -PID) when the plugin's own
            // `self.timeout` fires first.
            .kill_on_drop(true);
        // Cancellation-safety (codex re-review of af3597ab — Gap 3's "limits
        // must degrade, never leak"): put the plugin in its OWN process group
        // so its pgid == its pid. This is what makes a `kill -9 -<pid>`
        // (negative PID = process group) actually target the whole plugin tree
        // — the explicit kill branches below (and the drop guard) depend on it.
        // Without this, the negative-PID kills hit whatever group the harness
        // happens to be in (no dedicated group to reap), so grandchildren leak.
        // Windows has no process groups: leave it on the existing
        // `kill_on_drop` + `taskkill /T` behavior.
        #[cfg(unix)]
        {
            // `tokio::process::Command` exposes `process_group` as an inherent
            // method (mirroring `validators.rs`, which does the same on the same
            // `tokio::process::Command` type), so no `CommandExt` import is
            // needed here.
            cmd.process_group(0);
        }

        let env_allowlist = EnvAllowlist::from_strings(&self.tool_def.env);

        // M6 req 4: when the manifest declares a non-empty `env` list, treat
        // it as a strict allowlist and strip every other env var (only the
        // manifest's names + runtime essentials + harness-injected OCTOS_*
        // are retained). Empty list keeps the legacy "secret-only" gate so
        // existing skills that don't declare `env` continue working.
        let strict_env_gate = !env_allowlist.is_empty();
        if strict_env_gate {
            sanitize_command_env_strict(&mut cmd, &env_allowlist);
        } else {
            sanitize_command_env(&mut cmd, &env_allowlist);
        }

        // Remove blocked environment variables
        for var in &self.blocked_env {
            cmd.env_remove(var);
        }

        // Reuse the snapshot taken before the approval round-trip
        // (Phase 2-B): the prior code reread `TOOL_CTX` here, but the
        // approval gate is awaited above and there is no point at which
        // the snapshot would have grown stale. Sharing the snapshot
        // also keeps approval-prompt cwd and runtime cwd in lockstep.
        let ctx = ctx_snapshot;

        // Inject extra environment variables (e.g. provider base URLs, API keys)
        for (key, val) in &self.extra_env {
            let permitted = if strict_env_gate {
                should_forward_env_name_strict(key, &env_allowlist)
            } else {
                should_forward_env_name(key, &env_allowlist)
            };
            if permitted {
                cmd.env(key, val);
            } else {
                tracing::debug!(
                    plugin = %self.plugin_name,
                    tool = %self.tool_def.name,
                    env = %key,
                "skipping non-allowlisted environment variable for plugin tool"
                );
            }
        }

        if let Some(sink) = ctx
            .as_ref()
            .and_then(|ctx| ctx.harness_event_sink.as_deref())
        {
            cmd.env(OCTOS_EVENT_SINK_ENV, sink);
            if let Some(context) = lookup_event_sink_context(sink) {
                cmd.env(OCTOS_SESSION_ID_ENV, &context.session_id);
                cmd.env(OCTOS_TASK_ID_ENV, &context.task_id);
                cmd.env(OCTOS_HARNESS_SESSION_ID_ENV, &context.session_id);
                cmd.env(OCTOS_HARNESS_TASK_ID_ENV, &context.task_id);
            }
        }

        // Set working directory so relative paths in tool args (e.g.
        // input="slides/my-deck/script.js") resolve against the per-user
        // workspace — the same directory that write_file/read_file use.
        // OCTOS_WORK_DIR is kept for backward compat with plugins that
        // read it.
        //
        // Phase 2-B (SessionScope migration, PR #1198 follow-up): the
        // effective work dir was computed up front (see
        // `effective_work_dir_for_execute`). The policy is "registry-
        // rebound `self.work_dir` wins when set; otherwise fall back
        // to `scope.workspace()`" — this preserves correctness for
        // sessions with a `workspace_hint` (the
        // `SessionRuntime::bootstrap` path that calls
        // `rebind_plugin_work_dirs(<hint>/skill-output)`) where the
        // scope still points at the default
        // `<data>/users/<id>/workspace` (codex P1 fix). When a future
        // refactor aligns the scope with the hint, the override will
        // collapse to `scope.workspace()` naturally.
        if let Some(ref dir) = effective_work_dir {
            if let Err(e) = std::fs::create_dir_all(dir) {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to create plugin work_dir"
                );
            }
            cmd.current_dir(dir);
            cmd.env("OCTOS_WORK_DIR", dir);
        }

        // Codex round-3 BLOCKER fix (PR #1186 review): when
        // `prepare_effective_args` -> `rewrite_workspace_file_args` ->
        // `resolve_plugin_input_path` rejects a path with `..`
        // components, short-circuit BEFORE spawning the plugin so the
        // process is never started with a poisoned `script_path` /
        // `input` / etc. Surface the rejection to the LLM via the
        // tool's error envelope so the model sees a structured
        // failure rather than a silent escape attempt.
        let effective_args = match self.prepare_effective_args(args, ctx.as_ref()) {
            Ok(args) => args,
            Err(err) => {
                let message = err.to_string();
                tracing::warn!(
                    plugin = %self.plugin_name,
                    tool = %self.tool_def.name,
                    error = %message,
                    "plugin arg rewrite rejected unsafe path; refusing to spawn"
                );
                return Ok(ToolResult {
                    output: message,
                    success: false,
                    ..Default::default()
                });
            }
        };

        // Section C (codex review round-5 P2): RE-CHECK the verified-exe
        // hash immediately before spawn. The approval round-trip above
        // can take arbitrarily long; if the verified copy on disk was
        // swapped while the user was being prompted, we must NOT spawn
        // the swapped bytes. This second check closes the
        // approval→spawn TOCTOU window.
        if let Some(refusal) = self.check_verified_exe_hash() {
            return Ok(refusal);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let message = format!(
                    "failed to spawn plugin '{}' executable: {}: {err}",
                    self.plugin_name,
                    self.executable.display()
                );
                let classified = HarnessError::PluginSpawn {
                    plugin_name: self.plugin_name.clone(),
                    message: message.clone(),
                };
                self.emit_plugin_error(ctx.as_ref(), &classified);
                return Err(eyre::Report::new(err).wrap_err(message));
            }
        };

        let child_pid = child.id().unwrap_or(0);
        tracing::info!(
            plugin = %self.plugin_name,
            tool = %self.tool_def.name,
            pid = child_pid,
            "plugin process spawned"
        );

        // Arm the process-group kill guard (codex re-review of af3597ab). The
        // plugin was spawned into its own group via `process_group(0)`, so its
        // pgid == child_pid. This guard is owned by THIS future: if the registry
        // timeout drops the future, the guard's Drop SIGKILLs the whole group
        // (`kill -9 -<pid>`), reaping any grandchildren the plugin spawned —
        // not just the direct child (`kill_on_drop` only covers the latter). On
        // every normal-return path below we `disarm()` it, since the plugin has
        // already exited/been reaped by then. A non-zero pid is required for the
        // group to exist.
        #[cfg(unix)]
        let mut group_kill_guard = ProcessGroupKillGuard::new(child_pid);

        // Write args to stdin.
        //
        // Cancellation-safety (codex review of 7c3e5eac): this write happens
        // BEFORE the plugin's own timeout/kill branch below. A misbehaving
        // plugin that never drains stdin (and fills the OS pipe buffer by
        // streaming stdout before reading) could wedge `write_all` here
        // indefinitely. `kill_on_drop(true)` on `cmd` already guarantees the
        // child cannot be ORPHANED if the registry backstop drops this whole
        // future, but we ALSO bound the write under the plugin's own
        // `self.timeout` so the hang is caught by the plugin's graceful
        // kill path (process-group kill -9 -PID) rather than only by the
        // larger registry backstop. A timed-out write degrades to the same
        // structured timeout error as a hung wait.
        if let Some(mut stdin) = child.stdin.take() {
            let data = serde_json::to_vec(&effective_args)?;
            match tokio::time::timeout(self.timeout, stdin.write_all(&data)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    // Some plugins do not read stdin at all and exit after
                    // writing a best-effort stdout result. Treat an early pipe
                    // close as non-fatal so fallback stdout parsing can still
                    // succeed.
                    if err.kind() != ErrorKind::BrokenPipe {
                        return Err(err.into());
                    }
                }
                Err(_elapsed) => {
                    // stdin write wedged: kill the child via its process group
                    // (matches the wait-timeout branch below) and surface a
                    // structured timeout. The plugin was spawned with
                    // `process_group(0)`, so `kill -9 -<pid>` now reaps the
                    // whole tree (the leader AND any grandchildren), not just
                    // the leader. Dropping the future would also reap the group
                    // via the `ProcessGroupKillGuard`, but killing here keeps
                    // the error path symmetric. We disarm the guard afterward
                    // since the group is already reaped.
                    let _ = child.kill().await;
                    #[cfg(unix)]
                    if child_pid > 0 {
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &format!("-{child_pid}")])
                            .status();
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &child_pid.to_string()])
                            .status();
                    }
                    #[cfg(unix)]
                    group_kill_guard.disarm();
                    #[cfg(windows)]
                    if child_pid > 0 {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/F", "/T", "/PID", &child_pid.to_string()])
                            .status();
                    }
                    let timeout_secs = self.timeout.as_secs();
                    let message = format!(
                        "plugin '{}' tool '{}' timed out after {timeout_secs}s writing to stdin",
                        self.plugin_name, self.tool_def.name
                    );
                    let classified = HarnessError::PluginTimeout {
                        plugin_name: self.plugin_name.clone(),
                        timeout_secs,
                        message: message.clone(),
                    };
                    self.emit_plugin_error(ctx.as_ref(), &classified);
                    return Err(eyre::eyre!(message));
                }
            }
            // Drop stdin to signal EOF
        }

        // Take stdout and stderr handles for separate streaming
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Spawn stderr reader: streams lines as ToolProgress events.
        // Plugin protocol v2 (see `octos-plugin/docs/protocol-v2.md`):
        // each line is either a JSON-encoded `ProtocolV2Event` or legacy
        // free-form text. We try v2 first and fall back to legacy text on
        // any parse failure — this is the backward-compat shim required
        // for v1 plugins to keep working unchanged.
        let tool_name = self.tool_def.name.clone();
        // Clone ctx for the stderr reader so we can still consult the
        // original after the reader task is spawned (needed for
        // `emit_plugin_error` on spawn/timeout/protocol failures).
        let stderr_ctx = ctx.clone();
        let plugin_name_for_reader = self.plugin_name.clone();
        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            if let Some(stderr) = stderr_handle {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    Self::dispatch_stderr_line(
                        &plugin_name_for_reader,
                        &tool_name,
                        stderr_ctx.as_ref(),
                        &line,
                    );
                    if !collected.is_empty() {
                        collected.push('\n');
                    }
                    collected.push_str(&line);
                }
            }
            collected
        });

        // Spawn stdout reader: buffers full output for result parsing
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut stdout) = stdout_handle {
                let _ = stdout.read_to_end(&mut buf).await;
            }
            buf
        });

        // Wait for stdout/stderr to close (signals process exit) with timeout.
        // We join the reader tasks instead of child.wait() because child.wait()
        // can deadlock when pipe handles are held by spawned tasks.
        let all_done = async {
            let (stdout_res, stderr_res) = tokio::join!(stdout_task, stderr_task);
            (
                stdout_res.unwrap_or_default(),
                stderr_res.unwrap_or_default(),
            )
        };

        let (exit_status, stdout_bytes, stderr_text) =
            match tokio::time::timeout(self.timeout, async {
                let (stdout_bytes, stderr_text) = all_done.await;
                let status = child.wait().await;
                (status, stdout_bytes, stderr_text)
            })
            .await
            {
                Ok((Ok(status), stdout_bytes, stderr_text)) => (status, stdout_bytes, stderr_text),
                Ok((Err(e), _, _)) => {
                    let message = format!(
                        "plugin '{}' tool '{}' execution failed: {e}",
                        self.plugin_name, self.tool_def.name
                    );
                    let classified = HarnessError::PluginProtocol {
                        plugin_name: self.plugin_name.clone(),
                        message: message.clone(),
                    };
                    self.emit_plugin_error(ctx.as_ref(), &classified);
                    return Err(eyre::eyre!(message));
                }
                Err(_) => {
                    // Timeout — kill the child's whole process group. The plugin
                    // was spawned with `process_group(0)`, so `kill -9 -<pid>`
                    // reaps the leader AND any grandchildren it spawned, not just
                    // the leader. We disarm the drop guard afterward since the
                    // group is already reaped.
                    let _ = child.kill().await;
                    #[cfg(unix)]
                    if child_pid > 0 {
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &format!("-{child_pid}")])
                            .status();
                        let _ = std::process::Command::new("kill")
                            .args(["-9", &child_pid.to_string()])
                            .status();
                    }
                    #[cfg(unix)]
                    group_kill_guard.disarm();
                    #[cfg(windows)]
                    if child_pid > 0 {
                        let _ = std::process::Command::new("taskkill")
                            .args(["/F", "/T", "/PID", &child_pid.to_string()])
                            .status();
                    }
                    let timeout_secs = self.timeout.as_secs();
                    let message = format!(
                        "plugin '{}' tool '{}' timed out after {timeout_secs}s",
                        self.plugin_name, self.tool_def.name
                    );
                    let classified = HarnessError::PluginTimeout {
                        plugin_name: self.plugin_name.clone(),
                        timeout_secs,
                        message: message.clone(),
                    };
                    self.emit_plugin_error(ctx.as_ref(), &classified);
                    return Err(eyre::eyre!(message));
                }
            };

        // Normal-completion path: `child.wait()` above returned the exit status,
        // so the plugin leader has already exited and been reaped. Disarm the
        // process-group kill guard so a cleanly-finished plugin isn't redundantly
        // group-killed when this future returns. (A group-kill after exit would
        // be a harmless ESRCH no-op, but disarming is the clean approach.)
        #[cfg(unix)]
        group_kill_guard.disarm();
        let stdout = String::from_utf8_lossy(&stdout_bytes);

        tracing::info!(
            plugin = %self.plugin_name,
            tool = %self.tool_def.name,
            pid = child_pid,
            exit_code = exit_status.code().unwrap_or(-1),
            stdout_len = stdout.len(),
            stderr_len = stderr_text.len(),
            "plugin process completed"
        );

        // Try to parse structured output
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stdout) {
            let output = parsed
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or(&stdout)
                .to_string();
            let success = parsed
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(exit_status.success());
            // Check if plugin reported a file path
            let file_modified = parsed
                .get("file_modified")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    // Detect "Report saved to: <path>" pattern in output
                    output.lines().find_map(|line| {
                        line.strip_prefix("Report saved to: ")
                            .or_else(|| line.strip_prefix("Report saved to:"))
                            .map(|p| std::path::PathBuf::from(p.trim()))
                    })
                });
            // Parse files_to_send: plugin can request auto-delivery to chat
            let mut files_to_send: Vec<std::path::PathBuf> = parsed
                .get("files_to_send")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(std::path::PathBuf::from))
                        .collect()
                })
                .unwrap_or_default();

            // Parse named_outputs: spawn_only plugins can surface structured
            // values (e.g. `mofa_publish` emitting `deploy_url`) the contract
            // layer threads to validators for `${output.<key>}` interpolation.
            //
            // Malformed payloads must NOT silently drop the field — surface
            // a typed failure so the contract layer rejects the result.
            let named_outputs = match parse_named_outputs(parsed.get("named_outputs")) {
                Ok(value) => value,
                Err(reason) => {
                    tracing::warn!(
                        plugin = %self.plugin_name,
                        tool = %self.tool_def.name,
                        error = %reason,
                        "rejecting spawn_only result: malformed named_outputs"
                    );
                    return Ok(ToolResult {
                        output: format!("plugin emitted malformed named_outputs: {reason}"),
                        success: false,
                        ..Default::default()
                    });
                }
            };

            // Auto-deliver output file when plugin didn't report it.
            // Check multiple locations: work_dir, cwd, and the output text itself.
            let file_modified = if file_modified.is_none() && files_to_send.is_empty() {
                self.detect_output_file(
                    &effective_args,
                    &output,
                    &mut files_to_send,
                    effective_work_dir.as_deref(),
                )
                .await
            } else {
                file_modified
            };

            return Ok(ToolResult {
                output,
                success,
                file_modified,
                files_to_send,
                named_outputs,
                ..Default::default()
            });
        }

        // Fallback: raw stdout + stderr
        let mut output = stdout.to_string();
        if !stderr_text.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&stderr_text);
        }

        let mut files_to_send = Vec::new();
        let file_modified = self
            .detect_output_file(
                &effective_args,
                &output,
                &mut files_to_send,
                effective_work_dir.as_deref(),
            )
            .await;

        Ok(ToolResult {
            output,
            success: exit_status.success(),
            file_modified,
            files_to_send,
            ..Default::default()
        })
    }
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
