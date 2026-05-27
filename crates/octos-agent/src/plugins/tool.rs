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
        }
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
                    let classification = scope.classify_lexical_path(&absolute);
                    let resolved =
                        accept_for_intent(&classification, &absolute, path, PathIntent::Read)?;
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
                    let classification = scope.classify_lexical_path(&absolute);
                    let resolved =
                        accept_for_intent(&classification, &absolute, path, PathIntent::Write)?;
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
                        let classification = scope.classify_lexical_path(&absolute);
                        let resolved = accept_for_intent(
                            &classification,
                            &absolute,
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
                            let classification = scope.classify_lexical_path(&absolute);
                            let resolved = accept_for_intent(
                                &classification,
                                &absolute,
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
/// downstream [`SessionScope::classify_lexical_path`] already refuses
/// `ParentDir` components via its lexical normalisation step.
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
    let rescued_abs = std::path::PathBuf::from(&rescued);
    match scope.classify_lexical_path(&rescued_abs) {
        PathClassification::InWorkspace => rescued,
        PathClassification::InGrantedDir { .. }
        | PathClassification::InSharedZone { .. }
        | PathClassification::OutOfScope => lexical_absolute.to_string(),
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
        if risk_gate.requires_approval() {
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
            .stderr(Stdio::piped());

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

        // Write args to stdin
        if let Some(mut stdin) = child.stdin.take() {
            let data = serde_json::to_vec(&effective_args)?;
            if let Err(err) = stdin.write_all(&data).await {
                // Some plugins do not read stdin at all and exit after writing a
                // best-effort stdout result. Treat an early pipe close as
                // non-fatal so fallback stdout parsing can still succeed.
                if err.kind() != ErrorKind::BrokenPipe {
                    return Err(err.into());
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
                    // Timeout — kill the child process
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
mod tests {
    use super::*;
    use crate::SilentReporter;
    use serde_json::json;
    use std::sync::Arc;

    fn make_tool_def(name: &str, desc: &str) -> PluginToolDef {
        PluginToolDef {
            name: name.to_string(),
            description: desc.to_string(),
            input_schema: json!({"type": "object", "properties": {"msg": {"type": "string"}}}),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        }
    }

    #[test]
    fn new_sets_defaults() {
        let def = make_tool_def("greet", "Say hello");
        let tool = PluginTool::new("my-plugin".into(), def, PathBuf::from("/bin/echo"));

        assert_eq!(tool.plugin_name, "my-plugin");
        assert_eq!(tool.timeout, PluginTool::DEFAULT_TIMEOUT);
        assert_eq!(tool.timeout, Duration::from_secs(600));
        assert!(tool.blocked_env.is_empty());
    }

    #[test]
    fn with_blocked_env_sets_list() {
        let def = make_tool_def("t", "d");
        let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"))
            .with_blocked_env(vec!["SECRET".into(), "TOKEN".into()]);

        assert_eq!(tool.blocked_env, vec!["SECRET", "TOKEN"]);
    }

    #[test]
    fn with_extra_env_sets_vars() {
        let def = make_tool_def("t", "d");
        let tool =
            PluginTool::new("p".into(), def, PathBuf::from("/bin/echo")).with_extra_env(vec![
                (
                    "GEMINI_BASE_URL".into(),
                    "https://api.r9s.ai/gemini/v1beta".into(),
                ),
                ("GEMINI_API_KEY".into(), "test-key".into()),
            ]);

        assert_eq!(tool.extra_env.len(), 2);
        assert_eq!(tool.extra_env[0].0, "GEMINI_BASE_URL");
        assert_eq!(tool.extra_env[1].0, "GEMINI_API_KEY");
    }

    #[test]
    fn with_timeout_sets_custom() {
        let def = make_tool_def("t", "d");
        let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"))
            .with_timeout(Duration::from_secs(120));

        assert_eq!(tool.timeout, Duration::from_secs(120));
    }

    #[test]
    fn trait_methods_delegate_to_tool_def() {
        let def = make_tool_def("my_tool", "A fine tool");
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

        assert_eq!(tool.name(), "my_tool");
        assert_eq!(tool.description(), "A fine tool");
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["msg"].is_object());
    }

    #[test]
    fn rewrite_workspace_file_args_updates_audio_and_file_paths() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("mark.wav");
        let pdf = dir.path().join("deck.pdf");
        std::fs::write(&wav, b"wav").unwrap();
        std::fs::write(&pdf, b"pdf").unwrap();

        let def = PluginToolDef {
            name: "voice_tool".to_string(),
            description: "Voice tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "audio_path": {"type": "string"},
                    "file_path": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "audio_path": "/home/user/uploads/mark.wav",
                "file_path": "deck.pdf",
            }))
            .unwrap();

        // `audio_path` (a fictional absolute path) cannot resolve
        // through the unified table — it's outside every allowed root
        // — and falls back to the legacy `resolve_path_in_work_dir`
        // filename match. `file_path` (`deck.pdf`) is workspace-relative
        // and resolves through the unified resolver, which returns the
        // lexical workspace path on purpose (the tool's `O_NOFOLLOW`
        // open is the symlink-safety gate; canonicalising here would
        // bypass it).
        assert_eq!(rewritten["audio_path"], wav.to_string_lossy().to_string());
        assert_eq!(rewritten["file_path"], pdf.to_string_lossy().to_string());
    }

    #[test]
    fn rewrite_workspace_file_args_preserves_nested_workspace_paths() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("slides").join("demo");
        std::fs::create_dir_all(&nested).unwrap();
        let script = nested.join("script.js");
        std::fs::write(&script, b"export default [];").unwrap();

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "Slides tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string"},
                    "out": {"type": "string"},
                    "slide_dir": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "input": "slides/demo/script.js",
                "out": "slides/demo/output/deck.pptx",
                "slide_dir": "slides/demo/output/imgs"
            }))
            .unwrap();

        // All three keys end up as lexical workspace paths: `input`
        // resolves through the unified resolver (workspace scope keeps
        // the lexical form so the leaf `O_NOFOLLOW` gate can refuse
        // symlinks), and `out` / `slide_dir` go through the
        // absolutize-only branch which has always been lexical.
        assert_eq!(rewritten["input"], script.to_string_lossy().to_string());
        assert_eq!(
            rewritten["out"],
            dir.path()
                .join("slides/demo/output/deck.pptx")
                .to_string_lossy()
                .to_string()
        );
        assert_eq!(
            rewritten["slide_dir"],
            dir.path()
                .join("slides/demo/output/imgs")
                .to_string_lossy()
                .to_string()
        );
    }

    #[test]
    fn rewrite_workspace_file_args_recovers_basename_when_workspace_relative_missing() {
        // Codex review P2 (2026-05-13): when the LLM hallucinates a
        // directory prefix in front of a basename that exists at the
        // workspace root, the plugin filename fallback must rescue it.
        // The unified resolver succeeds for any syntactically valid
        // workspace-relative path even when the file is missing, so
        // the plugin code must require existence on the workspace scope
        // before accepting the resolver's result.
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("mark.wav");
        std::fs::write(&mark, b"wav").unwrap();
        // Note: `uploads/mark.wav` deliberately does NOT exist.

        let def = PluginToolDef {
            name: "voice_tool".to_string(),
            description: "Voice tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"audio_path": {"type": "string"}}
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "audio_path": "uploads/mark.wav",
            }))
            .unwrap();

        // Must recover `<work_dir>/mark.wav` via the legacy filename
        // fallback, NOT return the missing `<work_dir>/uploads/mark.wav`.
        assert_eq!(rewritten["audio_path"], mark.to_string_lossy().to_string());
    }

    #[test]
    fn rewrite_workspace_file_args_strips_redundant_skill_output_prefix_for_script_path() {
        // B1 fleet UX soak (mini2/iter1 + mini5/iter2): the modern
        // `runtime/session.rs` path chroots plugin `work_dir` into
        // `<workspace>/skill-output/`, while `write_file`'s base_dir
        // is the workspace ROOT. When the LLM passes the same
        // `skill-output/mofa-podcast/<file>.md` path to both, the
        // naive `work_dir.join(...)` doubles the prefix and the
        // plugin's `read_to_string` fails with `No such file or
        // directory (os error 2)`. The rewrite must detect this and
        // resolve the path against `work_dir` WITHOUT the redundant
        // prefix.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        let podcast_dir = skill_output.join("mofa-podcast");
        std::fs::create_dir_all(&podcast_dir).unwrap();
        let script = podcast_dir.join("octos_intro_script.md");
        std::fs::write(&script, b"# Podcast script").unwrap();

        let def = PluginToolDef {
            name: "podcast_generate".to_string(),
            description: "Podcast generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "script_path": {"type": "string"}
                }
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        // Plugin's work_dir mirrors the modern `runtime/session.rs`
        // path: `<workspace>/skill-output/`.
        let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(skill_output.clone());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "script_path": "skill-output/mofa-podcast/octos_intro_script.md",
            }))
            .unwrap();

        assert_eq!(
            rewritten["script_path"],
            script.to_string_lossy().to_string(),
            "script_path must resolve to <work_dir>/mofa-podcast/<file>.md, \
             NOT the doubled <work_dir>/skill-output/mofa-podcast/<file>.md"
        );
    }

    #[test]
    fn rewrite_workspace_file_args_keeps_skill_output_prefix_when_work_dir_is_workspace_root() {
        // Symmetric guard for the legacy `session_actor.rs` path:
        // when `work_dir` IS the workspace root (not chrooted into
        // `skill-output/`), the LLM's `skill-output/<file>` path is
        // correct as-is and must resolve to
        // `<workspace>/skill-output/<file>` — NOT have its prefix
        // stripped.
        let workspace = tempfile::tempdir().unwrap();
        let podcast_dir = workspace.path().join("skill-output").join("mofa-podcast");
        std::fs::create_dir_all(&podcast_dir).unwrap();
        let script = podcast_dir.join("intro.md");
        std::fs::write(&script, b"# script").unwrap();

        let def = PluginToolDef {
            name: "podcast_generate".to_string(),
            description: "Podcast generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"script_path": {"type": "string"}}
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(workspace.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "script_path": "skill-output/mofa-podcast/intro.md",
            }))
            .unwrap();

        assert_eq!(
            rewritten["script_path"],
            script.to_string_lossy().to_string(),
        );
    }

    #[test]
    fn strip_redundant_skill_output_prefix_rejects_parent_dir_escape() {
        // Codex BLOCKER fix (PR #1186 review): malicious input like
        // `skill-output/../secret.md` must NOT slip through the
        // `strip_redundant_skill_output_prefix` helper, AND the
        // unsafe-component guard inside `resolve_path_in_work_dir`
        // must skip the existence-check branches so the lexical
        // `work_dir.join(...)` fallback cannot escape the chrooted
        // `skill-output/` subdir.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Bait file ABOVE the chroot — escape attempt would land here.
        let secret = workspace.path().join("secret.md");
        std::fs::write(&secret, b"SECRET").unwrap();

        // 1. The helper itself refuses the unsafe candidate.
        assert!(
            strip_redundant_skill_output_prefix("skill-output/../secret.md", &skill_output)
                .is_none(),
            "stripped output of a `..`-containing raw path must be None"
        );

        // 2. resolve_path_in_work_dir must return None (so the
        //    existence-check branches are bypassed) — i.e. the EXISTENCE
        //    of the bait file must not drive the result.
        assert!(
            resolve_path_in_work_dir("skill-output/../secret.md", &skill_output).is_none(),
            "resolve_path_in_work_dir must return None for `..` escape, \
             NOT the existing bait file's resolved path"
        );

        // 3. End-to-end: codex round-3 fail-closed contract. The full
        //    resolver MUST return Err for any input carrying `..`. The
        //    prior behaviour (returning the raw string unchanged) was
        //    unsafe because the spawned plugin has
        //    `cmd.current_dir(skill_output)`, so when the plugin's own
        //    process opens `skill-output/../secret.md` (or worse, the
        //    raw `../secret.md`) the kernel resolves it relative to the
        //    chrooted work_dir and escapes. We must surface the
        //    rejection to the caller (which propagates a tool error
        //    envelope), NOT pass through.
        let err = resolve_plugin_input_path("skill-output/../secret.md", &skill_output)
            .expect_err("parent-dir escape must return Err, not pass-through");
        let msg = err.to_string();
        assert!(
            msg.contains("escapes plugin work dir"),
            "error message must explain why the path was rejected: {msg}"
        );
        // Defense in depth: even if a future refactor returns Ok, the
        // resolved string must never point at the bait file.
        let _ = secret; // suppress unused warning under future refactors
    }

    #[test]
    fn resolve_plugin_input_path_returns_err_on_raw_parent_dir() {
        // Codex round-3 BLOCKER fix (PR #1186 review): the round-2 fix
        // returned the raw string unchanged when `..` was present, but
        // the plugin process is spawned with
        // `cmd.current_dir(work_dir)`. Passing `../secret.md` through
        // unchanged lets the plugin itself open the path relative to
        // the chrooted work_dir and escape. The resolver must FAIL
        // CLOSED with an explicit error so the call site
        // (`rewrite_workspace_file_args` -> `prepare_effective_args`
        // -> `execute`) short-circuits the spawn and surfaces the
        // rejection to the LLM as a tool error envelope.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Bait file ABOVE the chroot.
        let secret = workspace.path().join("secret.md");
        std::fs::write(&secret, b"SECRET").unwrap();

        // 1. The low-level helper must still return None — the
        //    fail-closed guarantee at the entry of the lexical-join
        //    helpers is unchanged.
        assert!(
            resolve_path_in_work_dir("../secret.md", &skill_output).is_none(),
            "resolve_path_in_work_dir must return None for raw `..` escape"
        );

        // 2. Top-level resolver returns Err — NOT a pass-through
        //    string — for every raw form of `..` escape.
        for raw in ["../secret.md", "..", "foo/../bar", "a/b/../../c"] {
            let err = resolve_plugin_input_path(raw, &skill_output).expect_err(&format!(
                "raw `..` input {raw:?} must return Err, not a pass-through string"
            ));
            let msg = err.to_string();
            assert!(
                msg.contains(raw),
                "error must echo the rejected raw path so the LLM sees what was refused: {msg}",
            );
            assert!(
                msg.contains("escapes plugin work dir"),
                "error must explain the rejection reason: {msg}",
            );
        }

        // 3. End-to-end via `rewrite_workspace_file_args`: any raw
        //    `..` path on a workspace-file key (`script_path`,
        //    `input`, `audio_path`, `file_path`, `video_path`,
        //    `text_path`) must abort the rewrite. The caller
        //    (execute()) returns a tool error envelope instead of
        //    spawning the plugin with a poisoned arg.
        let def = PluginToolDef {
            name: "podcast_generate".to_string(),
            description: "Podcast generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"script_path": {"type": "string"}}
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(skill_output.clone());
        let rewrite_err = tool
            .rewrite_workspace_file_args(&json!({
                "script_path": "../secret.md",
            }))
            .expect_err("rewrite must propagate the resolver Err");
        assert!(
            rewrite_err.to_string().contains("../secret.md"),
            "rewrite error must echo the offending path: {rewrite_err}"
        );

        // Defense in depth: even if a future refactor accidentally
        // returns Ok, the resolved string must never point at the
        // bait file.
        let _ = secret;
    }

    #[test]
    fn rewrite_workspace_file_args_rejects_raw_parent_dir_on_output_keys() {
        // Codex round-4 BLOCKER fix (PR #1186 review): the round-3
        // fail-closed Err contract on input-path keys (`audio_path`,
        // `file_path`, `input`, `script_path`, `video_path`,
        // `text_path`) did NOT cover OUTPUT-path keys. The
        // `out` / `slide_dir` keys are routed through
        // `absolutize_path_in_work_dir`, which previously did a naive
        // lexical join. A `{"out":"../sneaky"}` or
        // `{"slide_dir":"../escape"}` therefore produced a
        // `<work_dir>/../sneaky` string that the plugin (spawned with
        // `cmd.current_dir(work_dir)`) WOULD then write to — escaping
        // the chroot. Round-4 extends the fail-closed Err contract to
        // these keys: `absolutize_path_in_work_dir` now returns
        // `Result<String, Err>` and rejects raw `..` at the entry, and
        // `rewrite_workspace_file_args` `?`-propagates so the
        // `execute()` boundary returns a tool error envelope BEFORE
        // spawn.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Bait file above the chroot (would-be victim of escape).
        let bait = workspace.path().join("sneaky");
        std::fs::write(&bait, b"BAIT").unwrap();

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "Slides generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "out": {"type": "string"},
                    "slide_dir": {"type": "string"}
                }
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-slides".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(skill_output.clone());

        // Every output-key + escape-pattern combination MUST produce
        // an Err propagated through `rewrite_workspace_file_args`.
        let cases = [
            ("out", "../sneaky"),
            ("slide_dir", "../escape"),
            ("out", "subdir/../../../etc/passwd"),
            // Trailing-`..` escape: legacy `work_dir.join("..")` would
            // resolve to the parent of `work_dir`.
            ("slide_dir", ".."),
            // Mid-path `..` escape: `work_dir.join("a/../../escape")`
            // would resolve one level above `work_dir`.
            ("out", "a/../../escape"),
        ];

        for (key, raw) in cases {
            let err = tool
                .rewrite_workspace_file_args(&json!({ key: raw }))
                .expect_err(&format!(
                    "output-path key {key:?} with raw `..` input {raw:?} \
                     must propagate Err from absolutize_path_in_work_dir"
                ));
            let msg = err.to_string();
            assert!(
                msg.contains(raw),
                "error for {key:?}={raw:?} must echo the offending path: {msg}",
            );
            assert!(
                msg.contains("escapes plugin work dir"),
                "error for {key:?}={raw:?} must explain rejection reason: {msg}",
            );
        }

        // Defense in depth: the underlying helper itself must Err so a
        // future refactor that bypasses `rewrite_workspace_file_args`
        // (e.g. a new call site) still fails closed.
        let helper_err = absolutize_path_in_work_dir("../sneaky", &skill_output)
            .expect_err("absolutize must Err on raw `..`");
        assert!(
            helper_err.to_string().contains("escapes plugin work dir"),
            "helper error must explain rejection: {helper_err}"
        );

        // Safe inputs still flow through unchanged (regression guard):
        // a relative path without `..` produces a lexical join, and an
        // absolute path is passed verbatim. Without this, a refactor
        // could over-zealously reject legitimate output args.
        let safe = absolutize_path_in_work_dir("sub/dir/out.toml", &skill_output)
            .expect("safe relative path must succeed");
        assert_eq!(
            safe,
            skill_output.join("sub/dir/out.toml").to_string_lossy()
        );
        let abs_in = "/tmp/explicit-out.toml";
        let abs_out = absolutize_path_in_work_dir(abs_in, &skill_output)
            .expect("absolute path must succeed (sandbox is the next gate)");
        assert_eq!(abs_out, abs_in);

        let _ = bait;
    }

    #[test]
    fn rewrite_workspace_file_args_recovers_workspace_root_script_for_podcast_generate() {
        // NEW-02 mini5 soak fix: when `write_file` lands the podcast
        // script at the workspace ROOT (because write_file's base_dir is
        // `<workspace>/`, not `<workspace>/skill-output/`), but the
        // plugin's `work_dir` is chrooted to
        // `<workspace>/skill-output/`, the script lives one level ABOVE
        // the chroot. Before this fix the resolver only probed inside
        // `work_dir`, so #1186's shared resolver returned a non-existent
        // path and the plugin spawn failed with `os error 2`.
        //
        // The rescue branch in `resolve_plugin_input_path` now probes
        // `work_dir.parent()` (the workspace root) for the basename
        // when `work_dir` ends in `skill-output`. Both raw forms the
        // LLM tends to emit MUST recover:
        //   * `script.md`                — bare basename
        //   * `skill-output/script.md`   — with the redundant prefix
        //     (mirrors write_file's workspace-root resolution)
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Mimic write_file landing the script at the workspace ROOT.
        let script = workspace.path().join("script.md");
        std::fs::write(&script, b"# podcast script\n").unwrap();

        // Form 1: bare basename. The legacy resolver would return
        // `<skill-output>/script.md` (lexical join, doesn't exist) or
        // fall through to the basename-scan inside the chroot (also
        // empty). Rescue must promote the workspace-root candidate.
        let resolved_bare = resolve_plugin_input_path("script.md", &skill_output)
            .expect("bare basename must resolve to workspace-root script");
        assert_eq!(
            std::path::Path::new(&resolved_bare),
            &script,
            "bare basename rescue must point at the workspace-root script",
        );

        // Form 2: `skill-output/`-prefixed path. The first strip-probe
        // would yield `script.md`, which doesn't exist inside the
        // chroot either. Same rescue must apply.
        let resolved_prefixed = resolve_plugin_input_path("skill-output/script.md", &skill_output)
            .expect("prefixed path must resolve to workspace-root script");
        assert_eq!(
            std::path::Path::new(&resolved_prefixed),
            &script,
            "prefixed-form rescue must point at the workspace-root script",
        );

        // End-to-end via `rewrite_workspace_file_args`: the
        // `script_path` key must be rewritten to the absolute
        // workspace-root path so the plugin spawn opens the right file.
        let def = PluginToolDef {
            name: "podcast_generate".to_string(),
            description: "Podcast generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"script_path": {"type": "string"}}
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(skill_output.clone());
        let rewritten = tool
            .rewrite_workspace_file_args(&json!({"script_path": "script.md"}))
            .expect("rewrite must succeed for workspace-root script");
        let rewritten_path = rewritten
            .get("script_path")
            .and_then(|v| v.as_str())
            .expect("script_path must remain a string after rewrite");
        assert_eq!(
            std::path::Path::new(rewritten_path),
            &script,
            "rewrite must point script_path at the workspace-root file",
        );

        // SECURITY GUARANTEE — #1186 fail-closed contract for raw `..`
        // must STILL hold. The rescue is bounded to `work_dir.parent()`
        // via `Path::file_name()` (basename only), so directory
        // components in the raw path are discarded. But the entry
        // guard rejects `..` long before we get there, and that
        // behaviour must NOT regress.
        let traversal_err = resolve_plugin_input_path("../../etc/passwd", &skill_output)
            .expect_err("raw `..` traversal must still fail-closed per #1186");
        let msg = traversal_err.to_string();
        assert!(
            msg.contains("../../etc/passwd"),
            "error must echo the rejected raw path: {msg}",
        );
        assert!(
            msg.contains("escapes plugin work dir"),
            "error must explain rejection reason: {msg}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_root_rescue_rejects_symlink_to_outside_workspace() {
        // Codex review on PR #1189 (BLOCKER): the rescue branch
        // originally used `candidate.exists()`, which FOLLOWS symlinks.
        // A `<workspace>/script.md -> /etc/passwd` symlink would have
        // satisfied `exists()` and the plugin would have received an
        // absolute path to a host file outside the workspace. The
        // hardened branch uses `symlink_metadata` + `is_file()` so
        // symlinks are caught before the path is handed off.
        //
        // This regression test creates a symlink at the workspace root
        // pointing at `/etc/passwd` and asserts the resolver REFUSES
        // to promote it via the rescue branch. The expected behaviour
        // is that the resolver falls through to the deeper fallbacks
        // (which either succeed inside `skill-output/` or, on absence,
        // produce the lexical-join string — neither lands on the
        // outside-workspace file).
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Symlink at the workspace ROOT pointing OUTSIDE the workspace.
        let bait = workspace.path().join("script.md");
        std::os::unix::fs::symlink("/etc/passwd", &bait).unwrap();
        // Sanity check: the symlink target exists on a real host
        // (so `exists()` would have succeeded), but it MUST NOT drive
        // the rescue.
        assert!(
            std::fs::symlink_metadata(&bait)
                .unwrap()
                .file_type()
                .is_symlink(),
            "test setup: bait must be a symlink"
        );

        let resolved = resolve_plugin_input_path("script.md", &skill_output)
            .expect("resolver still returns a path (lexical fallback) but NOT the bait");
        let resolved_path = std::path::Path::new(&resolved);
        // Critical: the resolved path MUST NOT be the workspace-root
        // symlink. Any value pointing at `/etc/passwd` (directly or
        // through the symlink) would be a security failure.
        assert_ne!(
            resolved_path, bait,
            "rescue must not return the symlinked workspace-root path"
        );
        assert!(
            !resolved_path.starts_with("/etc"),
            "resolved path must not escape into /etc: {resolved}",
        );
        // The expected fall-through is `<skill_output>/script.md`
        // (lexical join from the basename scan inside work_dir, or the
        // final absolutize step). That path doesn't exist either — but
        // it's CONTAINED to the chroot, so the plugin will hit a clean
        // os error 2 instead of reading the bait.
        assert!(
            resolved_path.starts_with(&skill_output) || resolved_path.starts_with(workspace.path()),
            "resolver must stay within the workspace: {resolved}",
        );
        // Defense in depth.
        let _ = bait;
    }

    #[cfg(unix)]
    #[test]
    fn workspace_root_rescue_rejects_symlink_to_inside_workspace() {
        // Defense-in-depth: codex review #1189 noted that symlinks
        // pointing INSIDE the workspace should also be rejected by
        // the rescue branch. The check is symlink-target-agnostic —
        // any symlink at the rescue candidate path fails because
        // `is_file()` (on `symlink_metadata`) returns false for the
        // symlink itself, regardless of what it points at. This test
        // pins that behaviour so a future refactor doesn't loosen
        // the predicate to e.g. follow symlinks within the workspace
        // and re-introduce TOCTOU swap risk.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Real file lives inside skill-output.
        let real = skill_output.join("real.md");
        std::fs::write(&real, b"real").unwrap();
        // Symlink at the workspace root pointing INSIDE the workspace.
        let aliased = workspace.path().join("script.md");
        std::os::unix::fs::symlink(&real, &aliased).unwrap();

        let resolved = resolve_plugin_input_path("script.md", &skill_output)
            .expect("resolver still returns a path via fallback");
        let resolved_path = std::path::Path::new(&resolved);
        assert_ne!(
            resolved_path, aliased,
            "rescue must NOT return the workspace-root symlink even when it points inside",
        );
    }

    #[test]
    fn workspace_root_rescue_rejects_directory_at_workspace_root() {
        // The rescue branch must also reject non-file candidates
        // (directories, sockets, FIFOs). A directory at
        // `<workspace>/script.md` should NOT satisfy the rescue —
        // plugins expect to read a file, and handing them a directory
        // path is at best confusing, at worst exploitable.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // Create a DIRECTORY (not a file) at the rescue candidate.
        let dir_at_root = workspace.path().join("script.md");
        std::fs::create_dir(&dir_at_root).unwrap();

        let resolved = resolve_plugin_input_path("script.md", &skill_output)
            .expect("resolver still returns a path via fallback");
        let resolved_path = std::path::Path::new(&resolved);
        // The directory MUST NOT be promoted by the rescue branch.
        assert_ne!(
            resolved_path, dir_at_root,
            "rescue must not return a directory at the workspace root"
        );
    }

    #[test]
    fn strip_redundant_skill_output_prefix_rejects_absolute_paths() {
        // Codex BLOCKER fix (PR #1186 review): absolute paths (e.g.
        // `/etc/passwd`) must not be accepted by the strip helper or
        // by the existence-check branches of
        // `resolve_path_in_work_dir`. The shared `resolve_tool_path`
        // resolver in the upstream caller already rejects them, but
        // the legacy fallback chain must not silently accept them
        // either: the EXISTENCE of the absolute file on disk must
        // never drive the result.
        let workspace = tempfile::tempdir().unwrap();
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();

        assert!(
            strip_redundant_skill_output_prefix("/etc/passwd", &skill_output).is_none(),
            "absolute paths must not be stripped"
        );

        // Critical security guarantee: the existence-check branches of
        // resolve_path_in_work_dir are skipped for absolute paths.
        // Returns None (so the caller falls back to a lexical join or
        // the absolutize fallback — both safe in that the sandbox
        // gates the real subprocess), NOT Some("/etc/passwd").
        assert!(
            resolve_path_in_work_dir("/etc/passwd", &skill_output).is_none(),
            "resolve_path_in_work_dir must return None for absolute paths, \
             NOT the raw path because the file exists on disk"
        );
    }

    #[test]
    fn rewrite_workspace_file_args_rewrites_video_path_and_text_path() {
        // Codex MAJOR fix (PR #1186 review): mofa-frame uses
        // `video_path` and the (unpublished) mofa-videolizer uses
        // `text_path` for their input args. Both must be subject to
        // the same workspace-relative rewrite as `audio_path` /
        // `file_path` / `script_path`.
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        let text = dir.path().join("transcript.txt");
        std::fs::write(&video, b"mp4").unwrap();
        std::fs::write(&text, b"hello").unwrap();

        let def = PluginToolDef {
            name: "frame_tool".to_string(),
            description: "mofa-frame style tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "video_path": {"type": "string"},
                    "text_path": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-frame".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "video_path": "clip.mp4",
                "text_path": "transcript.txt",
            }))
            .unwrap();

        assert_eq!(
            rewritten["video_path"],
            video.to_string_lossy().to_string(),
            "mofa-frame video_path must be rewritten to absolute work_dir path"
        );
        assert_eq!(
            rewritten["text_path"],
            text.to_string_lossy().to_string(),
            "mofa-videolizer text_path must be rewritten to absolute work_dir path"
        );
    }

    #[test]
    fn rewrite_workspace_file_args_keeps_mofa_style_as_name() {
        let dir = tempfile::tempdir().unwrap();
        let styles = dir.path().join("styles");
        std::fs::create_dir_all(&styles).unwrap();
        let style = styles.join("cyberpunk-neon.toml");
        std::fs::write(&style, b"[meta]\nname='Cyberpunk'\n").unwrap();

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "Slides tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "style": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "style": "cyberpunk-neon"
            }))
            .unwrap();

        assert_eq!(rewritten["style"], "cyberpunk-neon");
    }

    #[test]
    fn rewrite_workspace_file_args_strips_mofa_style_toml_paths_to_name() {
        let dir = tempfile::tempdir().unwrap();
        let styles = dir.path().join("styles");
        std::fs::create_dir_all(&styles).unwrap();
        let style = styles.join("cyberpunk-neon.toml");
        std::fs::write(&style, b"[meta]\nname='Cyberpunk'\n").unwrap();

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "Slides tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "style": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "style": style.to_string_lossy().to_string()
            }))
            .unwrap();

        assert_eq!(rewritten["style"], "cyberpunk-neon");
    }

    #[test]
    fn rewrite_workspace_file_args_strips_repeated_mofa_style_toml_suffixes() {
        let dir = tempfile::tempdir().unwrap();

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "Slides tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "style": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool
            .rewrite_workspace_file_args(&json!({
                "style": "/tmp/styles/nb-pro.toml.toml"
            }))
            .unwrap();

        assert_eq!(rewritten["style"], "nb-pro");
    }

    #[test]
    fn prepare_effective_args_injects_attachment_defaults() {
        let def = PluginToolDef {
            name: "voice_tool".to_string(),
            description: "Voice tool".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "audio_path": {"type": "string"},
                    "file_path": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));
        let ctx = ToolContext {
            tool_id: "tool-1".to_string(),
            reporter: Arc::new(SilentReporter),
            harness_event_sink: None,
            attachment_paths: vec![
                "/workspace/voice.ogg".to_string(),
                "/workspace/report.pdf".to_string(),
            ],
            audio_attachment_paths: vec!["/workspace/voice.ogg".to_string()],
            file_attachment_paths: vec!["/workspace/report.pdf".to_string()],
            ..ToolContext::zero()
        };

        let prepared = tool.prepare_effective_args(&json!({}), Some(&ctx)).unwrap();

        assert_eq!(prepared["audio_path"], "/workspace/voice.ogg");
        assert_eq!(prepared["file_path"], "/workspace/report.pdf");
    }

    fn deep_search_def_with_opt_in() -> PluginToolDef {
        PluginToolDef {
            name: "search".to_string(),
            description: "Deep research".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "synthesis_config": {"type": "object"}
                },
                "x-octos-host-config-keys": ["synthesis_config"]
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        }
    }

    fn full_synthesis_config() -> SynthesisConfig {
        SynthesisConfig {
            endpoint: "https://api.deepseek.com/v1".to_string(),
            api_key: "sk-host-injected".to_string(),
            model: "deepseek-chat".to_string(),
            provider: "deepseek".to_string(),
        }
    }

    #[test]
    fn synthesis_config_is_complete_only_when_all_fields_populated() {
        let cfg = full_synthesis_config();
        assert!(cfg.is_complete());

        let mut partial = cfg.clone();
        partial.api_key.clear();
        assert!(!partial.is_complete());

        let mut partial = cfg.clone();
        partial.endpoint.clear();
        assert!(!partial.is_complete());
    }

    #[test]
    fn prepare_effective_args_injects_synthesis_config_when_opted_in() {
        let tool = PluginTool::new(
            "deep-search".into(),
            deep_search_def_with_opt_in(),
            PathBuf::from("/bin/true"),
        )
        .with_synthesis_config(full_synthesis_config());

        let prepared = tool
            .prepare_effective_args(&json!({"query": "AI policy"}), None)
            .unwrap();
        let cfg = &prepared["synthesis_config"];
        assert_eq!(cfg["endpoint"], "https://api.deepseek.com/v1");
        assert_eq!(cfg["api_key"], "sk-host-injected");
        assert_eq!(cfg["model"], "deepseek-chat");
        assert_eq!(cfg["provider"], "deepseek");
    }

    #[test]
    fn prepare_effective_args_skips_synthesis_config_when_manifest_does_not_opt_in() {
        // Same tool but without the x-octos-host-config-keys extension.
        let mut def = deep_search_def_with_opt_in();
        def.input_schema = json!({
            "type": "object",
            "properties": {"query": {"type": "string"}}
        });
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_synthesis_config(full_synthesis_config());

        let prepared = tool
            .prepare_effective_args(&json!({"query": "AI policy"}), None)
            .unwrap();
        assert!(
            prepared.get("synthesis_config").is_none(),
            "tools without opt-in must not receive synthesis_config: {prepared}",
        );
    }

    #[test]
    fn prepare_effective_args_skips_synthesis_config_when_host_did_not_set_one() {
        let tool = PluginTool::new(
            "deep-search".into(),
            deep_search_def_with_opt_in(),
            PathBuf::from("/bin/true"),
        );

        let prepared = tool
            .prepare_effective_args(&json!({"query": "AI policy"}), None)
            .unwrap();
        assert!(prepared.get("synthesis_config").is_none());
    }

    #[test]
    fn prepare_effective_args_skips_synthesis_config_when_partial() {
        let mut cfg = full_synthesis_config();
        cfg.api_key.clear(); // Partial → fall through to env path.
        let tool = PluginTool::new(
            "deep-search".into(),
            deep_search_def_with_opt_in(),
            PathBuf::from("/bin/true"),
        )
        .with_synthesis_config(cfg);

        let prepared = tool
            .prepare_effective_args(&json!({"query": "AI policy"}), None)
            .unwrap();
        assert!(prepared.get("synthesis_config").is_none());
    }

    #[test]
    fn prepare_effective_args_does_not_overwrite_explicit_synthesis_config() {
        // Defense in depth: if a caller already set synthesis_config (e.g. a
        // unit test or a future LLM-controlled override), don't silently
        // replace it.
        let tool = PluginTool::new(
            "deep-search".into(),
            deep_search_def_with_opt_in(),
            PathBuf::from("/bin/true"),
        )
        .with_synthesis_config(full_synthesis_config());

        let prepared = tool
            .prepare_effective_args(
                &json!({
                    "query": "AI policy",
                    "synthesis_config": {"api_key": "caller-supplied"}
                }),
                None,
            )
            .unwrap();
        assert_eq!(prepared["synthesis_config"]["api_key"], "caller-supplied");
        assert!(
            prepared["synthesis_config"].get("endpoint").is_none(),
            "host config must not be merged into caller-supplied synthesis_config",
        );
    }

    /// Write a script to a file and make it executable, with fsync to avoid ETXTBSY
    /// on Linux overlayfs (Docker containers).
    #[cfg(unix)]
    fn write_test_script(path: &std::path::Path, content: &str) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // On Linux overlayfs (Docker), the kernel may still report ETXTBSY
        // briefly after closing. A short sleep allows the inode to settle.
        // macOS doesn't use overlayfs so this is skipped there.
        #[cfg(target_os = "linux")]
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_spawns_subprocess_and_captures_output() {
        // Create a temp script that reads stdin and writes structured JSON to stdout.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT\necho '{\"output\": \"got: '\"$INPUT\"'\", \"success\": true}'\n",
        );

        let def = make_tool_def("echo_tool", "echoes input");
        let tool = PluginTool::new("test-plugin".into(), def, script_path)
            .with_timeout(Duration::from_secs(5));

        let args = json!({"msg": "hello"});
        let result = tool.execute(&args).await.expect("execute should succeed");

        assert!(result.success);
        assert!(
            result.output.contains("got:"),
            "output should contain echoed input, got: {}",
            result.output
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_structured_progress_event_updates_task_supervisor() {
        use crate::task_supervisor::TaskSupervisor;
        use serde_json::json;

        let dir = tempfile::tempdir().expect("create temp dir");
        let supervisor = Arc::new(TaskSupervisor::new());
        let task_id = supervisor.register("structured_tool", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);

        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\ncat >/dev/null\nprintf '{\"schema\":\"octos.harness.event.v1\",\"kind\":\"progress\",\"session_id\":\"%s\",\"task_id\":\"%s\",\"workflow\":\"deep_research\",\"phase\":\"fetching_sources\",\"message\":\"Fetching source 3/12\",\"progress\":0.42}\\n' \"$OCTOS_SESSION_ID\" \"$OCTOS_TASK_ID\" >> \"$OCTOS_EVENT_SINK\"\nprintf '{\"output\":\"ok\",\"success\":true}'\n",
        );

        let def = make_tool_def("structured_tool", "writes harness events");
        let tool = PluginTool::new("test-plugin".into(), def, script_path)
            .with_timeout(Duration::from_secs(5));

        let sink = crate::harness_events::HarnessEventSink::new(
            supervisor.clone(),
            task_id.clone(),
            "api:session",
        )
        .expect("create sink");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        supervisor.set_on_change(move |task| {
            let _ = tx.send(task.clone());
        });

        let ctx = ToolContext {
            tool_id: "tool-1".to_string(),
            reporter: Arc::new(SilentReporter),
            harness_event_sink: Some(sink.path().display().to_string()),
            attachment_paths: vec![],
            audio_attachment_paths: vec![],
            file_attachment_paths: vec![],
            ..ToolContext::zero()
        };

        let result = crate::tools::TOOL_CTX
            .scope(ctx, tool.execute(&json!({})))
            .await
            .expect("tool execution should succeed");
        assert!(result.success);

        let updated = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("callback should fire")
            .expect("task snapshot should be sent");

        let detail: serde_json::Value =
            serde_json::from_str(updated.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(detail["workflow_kind"], "deep_research");
        assert_eq!(detail["current_phase"], "fetching_sources");
        assert_eq!(detail["progress_message"], "Fetching source 3/12");
        assert_eq!(updated.status, crate::task_supervisor::TaskStatus::Running);
        assert_eq!(
            updated.lifecycle_state(),
            crate::task_supervisor::TaskLifecycleState::Running
        );

        let task = supervisor.get_task(&task_id).expect("task missing");
        let task_detail: serde_json::Value =
            serde_json::from_str(task.runtime_detail.as_deref().unwrap()).unwrap();
        assert_eq!(task_detail["current_phase"], "fetching_sources");
        assert_eq!(task_detail["progress_message"], "Fetching source 3/12");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_does_not_expose_secret_extra_env_without_tool_allowlist() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\nVALUE=${OPENAI_API_KEY:-missing}\necho '{\"output\":\"'\"$VALUE\"'\",\"success\":true}'\n",
        );

        let def = make_tool_def("env_tool", "prints env");
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_extra_env(vec![(
                "OPENAI_API_KEY".into(),
                "sk-octos-plugin-regression".into(),
            )])
            .with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");

        assert!(result.success);
        assert_eq!(result.output, "missing");
        assert!(!result.output.contains("sk-octos-plugin-regression"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_exposes_secret_extra_env_with_tool_allowlist() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\nVALUE=${OPENAI_API_KEY:-missing}\necho '{\"output\":\"'\"$VALUE\"'\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("env_tool", "prints env");
        def.env.push("OPENAI_API_KEY".into());
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_extra_env(vec![(
                "OPENAI_API_KEY".into(),
                "sk-octos-plugin-allowed".into(),
            )])
            .with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");

        assert!(result.success);
        assert_eq!(result.output, "sk-octos-plugin-allowed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_fallback_on_non_json_stdout() {
        // Script that outputs plain text (not JSON).
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(&script_path, "#!/bin/sh\necho 'plain text output'\n");

        let def = make_tool_def("plain_tool", "plain output");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");

        assert!(result.success);
        assert!(result.output.contains("plain text output"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_fallback_detects_generated_pptx_as_file_to_send() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let output_rel = "slides/demo/output/deck.pptx";
        let output_abs = dir.path().join(output_rel);
        std::fs::create_dir_all(output_abs.parent().unwrap()).unwrap();
        std::fs::write(&output_abs, b"fake pptx").unwrap();

        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho 'Generated PPTX: slides/demo/output/deck.pptx'\n",
        );

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "slides output".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "out": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_work_dir(dir.path().to_path_buf())
            .with_timeout(Duration::from_secs(5));

        let result = tool
            .execute(&json!({"out": output_rel}))
            .await
            .expect("should succeed");

        assert!(result.success);
        assert_eq!(result.file_modified.as_deref(), Some(output_abs.as_path()));
        assert_eq!(result.files_to_send, vec![output_abs]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_fallback_waits_briefly_for_generated_pptx_to_appear() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let output_rel = "slides/demo/output/deck.pptx";
        let output_abs = dir.path().join(output_rel);

        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nnohup sh -c 'sleep 0.2; mkdir -p slides/demo/output; printf fake > slides/demo/output/deck.pptx' >/dev/null 2>&1 &\necho 'Generated PPTX: slides/demo/output/deck.pptx'\n",
        );

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "slides output".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "out": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_work_dir(dir.path().to_path_buf())
            .with_timeout(Duration::from_secs(5));

        let result = tool
            .execute(&json!({"out": output_rel}))
            .await
            .expect("should succeed");

        assert!(result.success);
        assert_eq!(result.file_modified.as_deref(), Some(output_abs.as_path()));
        assert_eq!(result.files_to_send, vec![output_abs.clone()]);
        assert!(
            output_abs.exists(),
            "generated deck should appear after fallback wait"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_fallback_skips_missing_generated_pptx() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let output_rel = "slides/demo/output/deck.pptx";

        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho 'Generated PPTX: slides/demo/output/deck.pptx'\n",
        );

        let def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "slides output".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "out": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_work_dir(dir.path().to_path_buf())
            .with_timeout(Duration::from_secs(5));

        let result = tool
            .execute(&json!({"out": output_rel}))
            .await
            .expect("should succeed");

        assert!(result.success);
        assert_eq!(result.file_modified, None);
        assert!(result.files_to_send.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_timeout_returns_error() {
        // Skip in Docker containers where pid/process management can cause hangs.
        // This test passes on macOS and bare-metal Linux.
        if std::path::Path::new("/.dockerenv").exists()
            || std::fs::read_to_string("/proc/1/cgroup")
                .map(|s| s.contains("docker") || s.contains("kubepods"))
                .unwrap_or(false)
        {
            eprintln!("skipping execute_timeout_returns_error: container detected");
            return;
        }

        // Script that sleeps longer than the timeout.
        // multi_thread needed because execute() spawns reader tasks that must run
        // concurrently with the timeout future.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(&script_path, "#!/bin/sh\nsleep 60\n");

        let def = make_tool_def("slow_tool", "too slow");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(1));

        match tool.execute(&json!({})).await {
            Err(e) => assert!(
                e.to_string().contains("timed out"),
                "expected timeout error, got: {e}"
            ),
            Ok(_) => panic!("expected timeout error, but execute succeeded"),
        }
    }

    // -------------------------------------------------------------------
    // Plugin protocol v2 stderr dispatch tests (W3.F2).
    // -------------------------------------------------------------------

    use crate::progress::ProgressReporter;
    use std::sync::Mutex as StdMutex;

    /// Captures every reported event so tests can assert on the ToolProgress
    /// messages the v2 shim emits.
    struct CapturingReporter {
        events: Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
    }

    impl ProgressReporter for CapturingReporter {
        fn report(&self, event: crate::progress::ProgressEvent) {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        }
    }

    fn make_capturing_ctx() -> (
        ToolContext,
        Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
    ) {
        let events = Arc::new(StdMutex::new(Vec::<crate::progress::ProgressEvent>::new()));
        let mut ctx = ToolContext::zero();
        ctx.tool_id = "tool-1".to_string();
        ctx.reporter = Arc::new(CapturingReporter {
            events: Arc::clone(&events),
        });
        (ctx, events)
    }

    fn last_progress_message(
        events: &Arc<StdMutex<Vec<crate::progress::ProgressEvent>>>,
    ) -> Option<String> {
        events.lock().unwrap().last().and_then(|event| match event {
            crate::progress::ProgressEvent::ToolProgress { message, .. } => Some(message.clone()),
            _ => None,
        })
    }

    #[test]
    fn v2_progress_event_renders_stage_and_message() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            r#"{"type":"progress","stage":"searching","message":"round 1/3","progress":0.25}"#,
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert!(msg.contains("[searching]"), "expected stage badge: {msg}");
        assert!(msg.contains("25%"), "expected percent: {msg}");
        assert!(msg.contains("round 1/3"), "expected message: {msg}");
    }

    #[test]
    fn v2_phase_event_renders_phase_label() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            r#"{"type":"phase","phase":"synthesizing","message":"calling LLM"}"#,
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert!(msg.starts_with("[synthesizing]"), "got {msg}");
        assert!(msg.contains("calling LLM"), "got {msg}");
    }

    #[test]
    fn v2_cost_event_renders_cost_summary() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            r#"{"type":"cost","provider":"deepseek","model":"deepseek-chat","tokens_in":1024,"tokens_out":256,"usd":0.0034}"#,
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert!(msg.contains("[cost]"), "got {msg}");
        assert!(msg.contains("deepseek"), "got {msg}");
        assert!(msg.contains("in=1024"), "got {msg}");
        assert!(msg.contains("out=256"), "got {msg}");
        assert!(msg.contains("0.0034"), "got {msg}");
    }

    #[test]
    fn v2_log_event_renders_level() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            r#"{"type":"log","level":"warn","message":"low disk"}"#,
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert_eq!(msg, "[warn] low disk");
    }

    #[test]
    fn v2_artifact_event_renders_kind_and_path() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            r#"{"type":"artifact","path":"/tmp/x.md","kind":"report","message":"final"}"#,
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert!(msg.contains("[artifact:report]"), "got {msg}");
        assert!(msg.contains("/tmp/x.md"), "got {msg}");
    }

    #[test]
    fn legacy_v1_text_passes_through_unchanged() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "old-plugin",
            "old_tool",
            Some(&ctx),
            "[deep_crawl] launched chrome on port 9222",
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert_eq!(msg, "[deep_crawl] launched chrome on port 9222");
    }

    #[test]
    fn legacy_starting_with_bracket_does_not_lose_data() {
        // Plugins emitting `[1/3] Searching ...` style text must still flow
        // through unchanged — they are not JSON, the shim must not eat them.
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            "[1/3] Searching: \"foo\"",
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        assert_eq!(msg, "[1/3] Searching: \"foo\"");
    }

    #[test]
    fn malformed_json_falls_back_to_legacy() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "p",
            "t",
            Some(&ctx),
            r#"{"type":"progress""#, // truncated, parse fails
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        // Falls back to the raw line (trimmed).
        assert_eq!(msg, r#"{"type":"progress""#);
    }

    #[test]
    fn empty_line_emits_no_progress() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line("p", "t", Some(&ctx), "");
        PluginTool::dispatch_stderr_line("p", "t", Some(&ctx), "   \r\n");
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_event_type_passes_raw_through() {
        let (ctx, events) = make_capturing_ctx();
        PluginTool::dispatch_stderr_line(
            "p",
            "t",
            Some(&ctx),
            r#"{"type":"future_event","data":42}"#,
        );
        let msg = last_progress_message(&events).expect("emitted progress");
        // The raw JSON is forwarded so the operator can still see it.
        assert!(msg.contains("future_event"), "got {msg}");
    }

    #[test]
    fn dispatch_with_no_ctx_is_noop() {
        // No assertion — just confirm there's no panic. With no ctx the
        // shim cannot dispatch but it must not crash.
        PluginTool::dispatch_stderr_line(
            "p",
            "t",
            None,
            r#"{"type":"progress","stage":"init","message":"go"}"#,
        );
    }

    #[test]
    fn cost_event_writes_to_harness_sink() {
        let dir = tempfile::tempdir().unwrap();
        let sink_path = dir.path().join("events.ndjson");

        // Wire up a sink context so record_cost_event has a session+task to
        // attribute against.
        let ctx_path = sink_path.display().to_string();
        crate::harness_events::attach_event_sink_context(
            ctx_path.clone(),
            crate::harness_events::HarnessEventSinkContext {
                session_id: "session-1".to_string(),
                task_id: "task-1".to_string(),
            },
        );

        let mut ctx = ToolContext::zero();
        ctx.tool_id = "tool-1".to_string();
        ctx.harness_event_sink = Some(ctx_path.clone());

        PluginTool::dispatch_stderr_line(
            "deep-search",
            "search",
            Some(&ctx),
            r#"{"type":"cost","provider":"deepseek","model":"deepseek-chat","tokens_in":1024,"tokens_out":256,"usd":0.0034}"#,
        );

        let body = std::fs::read_to_string(&sink_path).expect("sink written");
        assert!(body.contains(r#""kind":"cost_attribution""#), "got: {body}");
        assert!(body.contains(r#""tokens_in":1024"#), "got: {body}");
        assert!(body.contains(r#""tokens_out":256"#), "got: {body}");
        assert!(body.contains(r#""cost_usd":0.0034"#), "got: {body}");
        assert!(body.contains(r#""contract_id":"plugin:deep-search:search""#));
        assert!(body.contains(r#""provider":"deepseek""#));

        // Cleanup the sink registration.
        crate::harness_events::detach_event_sink_context(&ctx_path);
    }

    // -------------------------------------------------------------------
    // M6 req 4: env allowlist + risk approval enforcement tests
    // -------------------------------------------------------------------

    /// Manifest declares `env: ["FOO_ALLOWED_PLUGIN"]`. With strict gate
    /// active, an extra_env entry that's NOT on the manifest list is
    /// dropped — even though it isn't a secret name, the legacy gate
    /// would forward it. Pin the new strict semantics.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn strict_env_allowlist_drops_non_listed_extra_env() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\nA=${FOO_ALLOWED_PLUGIN:-missing}\nN=${FOO_BLOCKED_PLUGIN:-missing}\necho '{\"output\":\"a='\"$A\"';n='\"$N\"'\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("env_strict_tool", "prints env");
        def.env.push("FOO_ALLOWED_PLUGIN".into());
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_extra_env(vec![
                ("FOO_ALLOWED_PLUGIN".into(), "yes".into()),
                ("FOO_BLOCKED_PLUGIN".into(), "should_be_stripped".into()),
            ])
            .with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");

        assert!(result.success);
        assert!(
            result.output.contains("a=yes"),
            "listed extra env should reach subprocess; got: {}",
            result.output
        );
        assert!(
            result.output.contains("n=missing"),
            "non-listed extra env must be stripped under strict allowlist; got: {}",
            result.output
        );
    }

    /// When the manifest declares an empty `env` list, legacy semantics
    /// apply: non-secret extra_env entries pass through unfiltered. This
    /// pins the no-regression contract: skills that don't declare `env`
    /// see no behavior change from this PR.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn empty_env_allowlist_keeps_legacy_extra_env_passthrough() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        // Use a name that isn't flagged as secret-like (no token match
        // for SECRET/TOKEN/KEY/PASSWORD/etc).
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\nVALUE=${MY_BASE_URL:-missing}\necho '{\"output\":\"'\"$VALUE\"'\",\"success\":true}'\n",
        );

        let def = make_tool_def("legacy_env_tool", "prints env");
        // No `env` allowlist declared → empty list → legacy gate.
        let tool = PluginTool::new("p".into(), def, script_path)
            .with_extra_env(vec![("MY_BASE_URL".into(), "passes_through".into())])
            .with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");

        assert!(result.success);
        assert!(
            result.output.contains("passes_through"),
            "non-secret extra_env should pass through under legacy gate; got: {}",
            result.output
        );
    }

    /// Strict allowlist must still permit runtime essentials like PATH
    /// even if they aren't listed in the manifest, otherwise the
    /// subprocess can't find binaries it needs (sh, etc.). PATH is
    /// inherited from the parent process, not injected via extra_env.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn strict_env_allowlist_retains_path() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\nVALUE=${PATH:-missing}\nif [ \"$VALUE\" = \"missing\" ]; then echo '{\"output\":\"NO_PATH\",\"success\":true}'; else echo '{\"output\":\"HAS_PATH\",\"success\":true}'; fi\n",
        );

        let mut def = make_tool_def("path_tool", "prints PATH");
        def.env.push("FOO_ALLOWED_PLUGIN".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("should succeed");
        assert!(result.success);
        assert!(
            result.output.contains("HAS_PATH"),
            "PATH must be retained under strict allowlist; got: {}",
            result.output
        );
    }

    // ---- risk approval gate ----

    use async_trait::async_trait;
    use std::sync::Mutex;

    use crate::tools::ToolApprovalRequester;

    struct RecordingRequester {
        decision: ToolApprovalDecision,
        last: Arc<Mutex<Option<ToolApprovalRequest>>>,
    }

    impl RecordingRequester {
        fn new(
            decision: ToolApprovalDecision,
        ) -> (Arc<Self>, Arc<Mutex<Option<ToolApprovalRequest>>>) {
            let last = Arc::new(Mutex::new(None));
            let r = Arc::new(Self {
                decision,
                last: last.clone(),
            });
            (r, last)
        }
    }

    #[async_trait]
    impl ToolApprovalRequester for RecordingRequester {
        async fn request_approval(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
            *self.last.lock().unwrap() = Some(request);
            self.decision
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn high_risk_plugin_tool_requests_approval() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("danger_tool", "danger");
        def.risk = Some("high".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Approve);
        let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

        let result = TOOL_APPROVAL_CTX
            .scope(requester_arc, tool.execute(&json!({})))
            .await
            .expect("execute should succeed");

        assert!(result.success);
        assert_eq!(result.output, "ran");
        let req = last
            .lock()
            .unwrap()
            .clone()
            .expect("approval was requested");
        assert_eq!(req.tool_name, "danger_tool");
        assert!(req.title.contains("high"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn high_risk_plugin_tool_denied_returns_deny_message() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"output\":\"should_not_run\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("danger_tool_deny", "danger");
        def.risk = Some("critical".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let (requester, _last) = RecordingRequester::new(ToolApprovalDecision::Deny);
        let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

        let result = TOOL_APPROVAL_CTX
            .scope(requester_arc, tool.execute(&json!({})))
            .await
            .expect("execute returns Ok with deny message");

        assert!(!result.success, "denied call must report failure");
        assert!(
            result.output.contains("denied"),
            "deny message should be returned; got: {}",
            result.output
        );
        assert!(!result.output.contains("should_not_run"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn low_risk_plugin_tool_does_not_request_approval() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"output\":\"ran_without_prompt\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("safe_tool", "safe");
        def.risk = Some("low".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
        let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

        let result = TOOL_APPROVAL_CTX
            .scope(requester_arc, tool.execute(&json!({})))
            .await
            .expect("execute should succeed");

        assert!(result.success);
        assert_eq!(result.output, "ran_without_prompt");
        assert!(
            last.lock().unwrap().is_none(),
            "approval must not be requested for low risk"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn unspecified_risk_plugin_tool_does_not_request_approval() {
        // Default behavior — pinning that skills without `risk` declared
        // continue to run without ever prompting (no breakage).
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"output\":\"unprompted\",\"success\":true}'\n",
        );

        let def = make_tool_def("plain_tool", "plain");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
        let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

        let result = TOOL_APPROVAL_CTX
            .scope(requester_arc, tool.execute(&json!({})))
            .await
            .expect("execute should succeed");

        assert!(result.success);
        assert_eq!(result.output, "unprompted");
        assert!(last.lock().unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn high_risk_without_approval_bridge_denies_safely() {
        // Mirrors shell.rs behavior: if there's no interactive bridge,
        // a high-risk plugin tool must NOT silently run.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"output\":\"should_not_run\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("danger_tool_no_bridge", "danger");
        def.risk = Some("HIGH".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        // No TOOL_APPROVAL_CTX scoped → try_with returns Err → deny.
        let result = tool
            .execute(&json!({}))
            .await
            .expect("returns Ok with deny");
        assert!(!result.success);
        assert!(result.output.contains("denied"));
        assert!(!result.output.contains("should_not_run"));
    }

    #[test]
    fn concurrency_class_trims_whitespace_and_returns_exclusive() {
        // Codex review #1 regression test: `"exclusive "` (trailing
        // whitespace) previously silently downgraded to Safe. After the
        // trim added at the parse site, it must classify as Exclusive.
        let mut def = make_tool_def("excl_tool", "exclusive");
        def.concurrency_class = Some("exclusive ".to_string());
        let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"));
        let class = tool.concurrency_class();
        assert!(matches!(class, crate::tools::ConcurrencyClass::Exclusive));
    }

    #[test]
    fn plugin_unknown_concurrency_class_falls_back_to_exclusive() {
        // Issue #718 follow-up: align with MCP's
        // `McpServerConfig::resolved_concurrency_class`. The previous
        // behavior was fail-open (unknown → Safe), which silently
        // permitted parallel writes when a manifest author typoed
        // `"exclusve"`. After the fix, unknown literals fail-closed to
        // Exclusive — same behavior as MCP — so a typo still serialises
        // execution.
        let mut def = make_tool_def("excl_tool", "exclusive");
        def.concurrency_class = Some("highly-exclusive".to_string());
        let tool = PluginTool::new("p".into(), def, PathBuf::from("/bin/echo"));
        assert!(matches!(
            tool.concurrency_class(),
            crate::tools::ConcurrencyClass::Exclusive,
        ));

        // The exact typo called out in #718.
        let mut typo_def = make_tool_def("typo_tool", "exclusive");
        typo_def.concurrency_class = Some("exclusve".to_string());
        let typo_tool = PluginTool::new("p".into(), typo_def, PathBuf::from("/bin/echo"));
        assert!(matches!(
            typo_tool.concurrency_class(),
            crate::tools::ConcurrencyClass::Exclusive,
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn unknown_risk_literal_does_not_force_approval() {
        // medium / weird literals fall through to "no enforced gate"
        // (semantics ambiguous; documented as Tier-2/3 follow-up).
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"output\":\"ran\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("medium_tool", "medium");
        def.risk = Some("medium".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Deny);
        let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

        let result = TOOL_APPROVAL_CTX
            .scope(requester_arc, tool.execute(&json!({})))
            .await
            .expect("execute should succeed");

        assert!(result.success);
        assert_eq!(result.output, "ran");
        assert!(last.lock().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // Wave-3b: spawn_only stdout envelope extension — `named_outputs`.
    // -------------------------------------------------------------------

    #[test]
    fn parse_named_outputs_returns_none_when_field_absent() {
        // Tool that doesn't emit named_outputs should parse cleanly to None
        // so existing spawn_only callers stay byte-identical.
        let envelope = json!({"success": true, "output": "ok"});
        let parsed = parse_named_outputs(envelope.get("named_outputs")).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_named_outputs_returns_none_when_field_is_null() {
        let envelope = json!({"named_outputs": null});
        let parsed = parse_named_outputs(envelope.get("named_outputs")).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_named_outputs_returns_none_when_object_is_empty() {
        let envelope = json!({"named_outputs": {}});
        let parsed = parse_named_outputs(envelope.get("named_outputs")).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_named_outputs_maps_string_values() {
        let envelope = json!({
            "named_outputs": {
                "deploy_url": "https://example.com/site",
                "repo": "octos/site",
            }
        });
        let parsed = parse_named_outputs(envelope.get("named_outputs"))
            .unwrap()
            .expect("expected Some(map)");
        assert_eq!(
            parsed.get("deploy_url").map(String::as_str),
            Some("https://example.com/site")
        );
        assert_eq!(parsed.get("repo").map(String::as_str), Some("octos/site"));
    }

    #[test]
    fn parse_named_outputs_rejects_non_object_payload() {
        let envelope = json!({"named_outputs": ["a", "b"]});
        let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
        assert!(err.contains("must be a JSON object"), "{err}");
    }

    #[test]
    fn parse_named_outputs_rejects_non_string_value() {
        // v1: nested JSON not supported. Numbers, bools, arrays, objects
        // must surface as errors so the contract layer sees a typed
        // failure rather than silently dropping the field.
        let envelope = json!({
            "named_outputs": {"deploy_count": 42}
        });
        let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
        assert!(err.contains("must be a string"), "{err}");
        assert!(err.contains("deploy_count"), "{err}");
    }

    #[test]
    fn parse_named_outputs_rejects_key_starting_with_digit() {
        let envelope = json!({"named_outputs": {"1deploy": "x"}});
        let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
        assert!(err.contains("required shape"), "{err}");
    }

    #[test]
    fn parse_named_outputs_rejects_uppercase_key() {
        let envelope = json!({"named_outputs": {"DeployUrl": "x"}});
        let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
        assert!(err.contains("required shape"), "{err}");
    }

    #[test]
    fn parse_named_outputs_rejects_key_with_hyphen() {
        let envelope = json!({"named_outputs": {"deploy-url": "x"}});
        let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
        assert!(err.contains("required shape"), "{err}");
    }

    #[test]
    fn parse_named_outputs_rejects_empty_key() {
        let envelope = json!({"named_outputs": {"": "x"}});
        let err = parse_named_outputs(envelope.get("named_outputs")).unwrap_err();
        assert!(err.contains("required shape"), "{err}");
    }

    #[test]
    fn parse_named_outputs_accepts_underscore_and_digits_after_first_char() {
        let envelope = json!({"named_outputs": {"deploy_url_v2": "x", "out1": "y"}});
        let parsed = parse_named_outputs(envelope.get("named_outputs"))
            .unwrap()
            .expect("expected map");
        assert_eq!(parsed.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_with_named_outputs_threads_field_into_tool_result() {
        // End-to-end: plugin emits {"named_outputs": {...}} on stdout, the
        // PluginTool wrapper forwards it through ToolResult so the
        // spawn_only contract path can read it.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"success\":true,\"output\":\"deployed\",\"named_outputs\":{\"deploy_url\":\"http://example.com/site\"}}'\n",
        );

        let def = make_tool_def("publish_tool", "publish");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("execute should ok");
        assert!(result.success);
        assert_eq!(result.output, "deployed");
        let named = result.named_outputs.expect("named_outputs should be set");
        assert_eq!(
            named.get("deploy_url").map(String::as_str),
            Some("http://example.com/site")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_with_malformed_named_outputs_returns_failure() {
        // A plugin emitting a non-string value in named_outputs must
        // surface as a typed failure so the contract layer rejects it.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"success\":true,\"output\":\"ok\",\"named_outputs\":{\"count\":42}}'\n",
        );

        let def = make_tool_def("bad_tool", "emits bad named outputs");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("execute should ok");
        assert!(!result.success);
        assert!(
            result.output.contains("named_outputs") || result.output.contains("must be a string"),
            "unexpected output: {}",
            result.output
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn execute_without_named_outputs_leaves_tool_result_none() {
        // Backward compat: legacy plugins that don't emit named_outputs
        // must continue to produce ToolResult.named_outputs = None.
        let dir = tempfile::tempdir().expect("create temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\necho '{\"success\":true,\"output\":\"done\"}'\n",
        );

        let def = make_tool_def("legacy_tool", "legacy");
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let result = tool.execute(&json!({})).await.expect("execute should ok");
        assert!(result.success);
        assert!(result.named_outputs.is_none());
    }

    // ---------------------------------------------------------------
    // Phase 2-B SessionScope migration tests (PR #1198 follow-up).
    //
    // These pin the new scope-aware code path. They collapse the
    // bespoke `resolve_plugin_input_path` / `absolutize_path_in_work_dir`
    // / `resolve_slides_style_in_work_dir` validators behind a single
    // `classify_lexical_path` gate so the 4-round #1186 traversal
    // hardening + the #1189 workspace-root rescue have one home.
    //
    // The legacy fallback path (no scope) is independently exercised
    // by the existing `rewrite_workspace_file_args_*` tests above,
    // plus the `legacy_workspace_root_rescue_still_works_when_no_scope`
    // pin further down which calls the resolver directly.
    // ---------------------------------------------------------------

    fn input_path_def(key: &str) -> PluginToolDef {
        PluginToolDef {
            name: format!("phase2b_{key}_tool"),
            description: "Phase 2-B fixture".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {key: {"type": "string"}}
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        }
    }

    fn solo_scope_at(root: &std::path::Path) -> SessionScope {
        SessionScope::solo(root.to_path_buf(), vec![]).expect("build solo scope")
    }

    fn multi_tenant_scope_at(
        data: &std::path::Path,
        tenant: &str,
        session: &str,
        shared_zones: Vec<std::path::PathBuf>,
    ) -> SessionScope {
        SessionScope::multi_tenant(
            data.to_path_buf(),
            tenant.into(),
            session.into(),
            shared_zones,
        )
        .expect("build multi-tenant scope")
    }

    fn ctx_with_scope(scope: SessionScope) -> ToolContext {
        let mut ctx = ToolContext::zero();
        ctx.session_scope = Some(Arc::new(scope));
        ctx
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn plugin_uses_scope_workspace_when_present() {
        // Phase 2-B contract: when a `SessionScope` is threaded via the
        // `ToolContext` AND `self.work_dir` is `None` (no registry
        // rebind happened, so the scope is the source of truth), the
        // plugin spawns with `OCTOS_WORK_DIR = scope.workspace()`. The
        // workspace dir is created on the fly so
        // `SessionScope::multi_tenant`'s no-create-on-construction
        // promise still holds and the spawner takes care of it.
        //
        // The "self.work_dir wins when set" path (the hinted-workspace
        // case that codex P1 flagged) is pinned separately by
        // `plugin_prefers_registry_rebound_work_dir_over_scope` below.
        let data = tempfile::tempdir().expect("data dir");
        // Use a session id that has not been created yet — Phase 2-B
        // must `create_dir_all(scope.workspace())` before spawn.
        let scope = multi_tenant_scope_at(data.path(), "dspfac", "web-phase2b", vec![]);
        let session_workspace = scope.workspace().to_path_buf();
        assert!(
            !session_workspace.exists(),
            "test fixture sanity: workspace must not pre-exist"
        );

        // The executable lives outside the scope workspace because the
        // test cannot pre-create the scope's session dir without
        // defeating the assertion below. Both dirs must exist before
        // `write_test_script` so we use an unrelated tempdir for the
        // binary.
        let bin_dir = tempfile::tempdir().expect("bin dir");
        let script_path = bin_dir.path().join("script.sh");
        // Script echoes its CWD via `pwd` inside the JSON envelope so
        // the test can inspect it.
        write_test_script(
            &script_path,
            "#!/bin/sh\nDIR=$(pwd)\nprintf '{\"output\":\"%s\",\"success\":true}' \"$DIR\"\n",
        );

        let def = make_tool_def("scope_cwd", "echo CWD");
        // Crucially: NO `.with_work_dir(...)`. The scope is the only
        // source of truth.
        let tool =
            PluginTool::new("plug".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let ctx = ctx_with_scope(scope);
        let result = crate::tools::TOOL_CTX
            .scope(ctx, tool.execute(&json!({})))
            .await
            .expect("execute should succeed");

        assert!(result.success, "scope-aware execute should succeed");
        assert!(
            session_workspace.exists(),
            "Phase 2-B must create scope.workspace() before spawn"
        );
        // macOS prefixes tempdirs with `/private`, so canonicalise both
        // sides before comparing (the shell's `pwd` resolves symlinks).
        let actual = std::fs::canonicalize(result.output.trim())
            .expect("CWD echoed by plugin should resolve");
        let expected = std::fs::canonicalize(&session_workspace)
            .expect("scope workspace should resolve after create_dir_all");
        assert_eq!(
            actual, expected,
            "plugin CWD must equal scope.workspace() when self.work_dir is None"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn high_risk_plugin_approval_cwd_reflects_scope_workspace() {
        // Codex P3 pin (Phase 2-B): the approval prompt's `cwd` field
        // must reflect the directory the plugin will ACTUALLY run in,
        // not the construction-time `self.work_dir`. In a scope-only
        // wiring (no registry rebind), that's `scope.workspace()` —
        // before this fix the prompt would have shown `None` (or the
        // bogus construction work_dir), so users approving a
        // high/critical-risk plugin would see the wrong directory.
        let data = tempfile::tempdir().expect("data dir");
        let scope = multi_tenant_scope_at(data.path(), "dspfac", "web-approval", vec![]);
        let scope_workspace = scope.workspace().to_path_buf();

        // Place the binary outside the scope (we need it on disk so
        // `write_test_script` works) and DO NOT pass it via
        // `with_work_dir` — scope-only wiring.
        let bin_dir = tempfile::tempdir().expect("bin dir");
        let script_path = bin_dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
        );

        let mut def = make_tool_def("approval_cwd_tool", "danger");
        def.risk = Some("high".into());
        let tool =
            PluginTool::new("p".into(), def, script_path).with_timeout(Duration::from_secs(5));

        let (requester, last) = RecordingRequester::new(ToolApprovalDecision::Approve);
        let requester_arc: Arc<dyn ToolApprovalRequester> = requester;

        let ctx = ctx_with_scope(scope);
        let _ = crate::tools::TOOL_CTX
            .scope(
                ctx,
                TOOL_APPROVAL_CTX.scope(requester_arc, tool.execute(&json!({}))),
            )
            .await
            .expect("execute should succeed");

        let req = last
            .lock()
            .unwrap()
            .clone()
            .expect("approval was requested");
        let cwd = req
            .cwd
            .as_deref()
            .expect("approval cwd must be Some when scope is present");
        assert_eq!(
            std::path::Path::new(cwd),
            &scope_workspace,
            "approval cwd MUST reflect the effective work dir (scope.workspace() when scope-only)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn plugin_rescues_workspace_root_under_hinted_skill_output_rebind() {
        // Codex round-5 P1 pin (Phase 2-B): real hinted bootstrap
        // rebinds `self.work_dir = <hint>/skill-output`. The
        // workspace-root rescue (LLM passes `script_path: "script.md"`
        // when the file is at `<hint>/script.md`) must still resolve.
        // Round-4 rooted the ad-hoc scope at `wd` directly, which
        // surrendered that rescue; round-5 promotes the parent dir
        // when `wd` ends in `skill-output`.
        let hint = tempfile::tempdir().expect("hint");
        let skill_output = hint.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        // The script lives at the hinted workspace ROOT, not inside
        // `skill-output/` (mirrors the soak workflow where write_file
        // lands the script at the workspace root).
        let script = hint.path().join("script.md");
        std::fs::write(&script, b"# podcast script").unwrap();

        let scope_workspace = tempfile::tempdir().expect("scope workspace");
        let scope = solo_scope_at(scope_workspace.path());

        let bin = tempfile::tempdir().expect("bin");
        let bin_path = bin.path().join("script.sh");
        // The plugin echoes the script_path it received so the test
        // can inspect the rewrite.
        write_test_script(
            &bin_path,
            "#!/bin/sh\nINPUT=$(cat)\nVALUE=$(echo \"$INPUT\" | sed -n 's/.*\"script_path\":\"\\([^\"]*\\)\".*/\\1/p')\nprintf '{\"output\":\"%s\",\"success\":true}' \"$VALUE\"\n",
        );
        let tool = PluginTool::new("plug".into(), input_path_def("script_path"), bin_path)
            .with_work_dir(skill_output.clone())
            .with_timeout(Duration::from_secs(5));

        let ctx = ctx_with_scope(scope);
        let result = crate::tools::TOOL_CTX
            .scope(ctx, tool.execute(&json!({"script_path": "script.md"})))
            .await
            .expect("execute should succeed");

        assert!(result.success, "hinted workspace-root rescue must succeed");
        let echoed_path = result.output.trim();
        let echoed_canon = std::fs::canonicalize(echoed_path).expect("echoed path resolves");
        let expected_canon = std::fs::canonicalize(&script).expect("script resolves");
        assert_eq!(
            echoed_canon, expected_canon,
            "rescue must promote `<hint>/script.md` (the parent rescue), \
             NOT rewrite to `<hint>/skill-output/script.md`"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn plugin_refuses_absolute_escape_in_hinted_session() {
        // Codex round-4 P1 pin (Phase 2-B): when a scoped session has
        // a workspace_hint whose path falls outside the session scope
        // (the `SessionRuntime::bootstrap` reality today), the round-3
        // routing fell back to the legacy rewriter — which accepted
        // absolute paths anywhere on disk after `resolve_tool_path`
        // failed. The round-4 fix substitutes an AD-HOC solo scope
        // rooted at the hinted work_dir so the read/write boundary
        // still holds: an `audio_path: "/etc/passwd"` from a hinted
        // session MUST still Err.
        let scope_workspace = tempfile::tempdir().expect("scope workspace");
        let hinted_work_dir = tempfile::tempdir().expect("hinted work_dir");
        // Bait file outside the hinted work_dir.
        let bait_outside = tempfile::tempdir().expect("bait");
        let bait_path = bait_outside.path().join("escape.txt");
        std::fs::write(&bait_path, b"BAIT").unwrap();

        let scope = solo_scope_at(scope_workspace.path());
        // Mirror the hinted bootstrap shape: scope is at
        // `scope_workspace`, but `self.work_dir` is the hint
        // (`hinted_work_dir`), which is OUTSIDE `scope.workspace()`.
        // Round-3 would have routed this through the legacy rewriter
        // and accepted `/escape/path`. Round-4 substitutes an ad-hoc
        // solo scope rooted at the hint, so the absolute escape Errs.
        let bin = tempfile::tempdir().expect("bin");
        let script = bin.path().join("script.sh");
        write_test_script(
            &script,
            "#!/bin/sh\nread INPUT || true\necho '{\"output\":\"ran\",\"success\":true}'\n",
        );
        let tool = PluginTool::new("plug".into(), input_path_def("audio_path"), script.clone())
            .with_work_dir(hinted_work_dir.path().to_path_buf())
            .with_timeout(Duration::from_secs(5));

        let ctx = ctx_with_scope(scope);
        let bait_abs = bait_path.to_string_lossy().to_string();
        let result = crate::tools::TOOL_CTX
            .scope(ctx, tool.execute(&json!({"audio_path": bait_abs.clone()})))
            .await
            .expect("execute should return Ok with error envelope");

        assert!(
            !result.success,
            "absolute out-of-scope path under hint must produce a tool error envelope (success=false), got success=true output={}",
            result.output,
        );
        assert!(
            result.output.contains(&bait_abs),
            "tool error envelope must echo the rejected path: {}",
            result.output
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn plugin_prefers_registry_rebound_work_dir_over_scope() {
        // Codex P1 pin (Phase 2-B): when `SessionRuntime::bootstrap`
        // honours a `workspace_hint`, it calls
        // `tools.rebind_plugin_work_dirs(<hint>/skill-output)` so every
        // `PluginTool` clone carries `self.work_dir = <hint>/...`. The
        // `SessionScope` constructed alongside still derives its
        // workspace from `profile.data_dir` (= the un-hinted default),
        // so the two disagree. Phase 2-B MUST honour the registry
        // rebind (the hint is the source of truth in that wiring) and
        // NOT silently redirect the plugin to the empty default scope
        // workspace. This pin guards the regression codex flagged.
        let data = tempfile::tempdir().expect("data dir");
        // Multi-tenant scope: workspace lands at
        // `<data>/users/web-codex-p1/workspace`. We deliberately
        // never create it; the test asserts it stays absent because the
        // plugin runs in the registry-rebound dir instead.
        let scope = multi_tenant_scope_at(data.path(), "dspfac", "web-codex-p1", vec![]);
        let scope_workspace = scope.workspace().to_path_buf();
        assert!(
            !scope_workspace.exists(),
            "test fixture sanity: scope workspace must not pre-exist"
        );

        // Registry-rebound work_dir mirrors the hinted-workspace path.
        let hinted_work_dir = tempfile::tempdir().expect("hinted work dir");
        let script_path = hinted_work_dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nDIR=$(pwd)\nprintf '{\"output\":\"%s\",\"success\":true}' \"$DIR\"\n",
        );

        let def = make_tool_def("hint_cwd", "echo CWD");
        let tool = PluginTool::new("plug".into(), def, script_path)
            .with_work_dir(hinted_work_dir.path().to_path_buf())
            .with_timeout(Duration::from_secs(5));

        let ctx = ctx_with_scope(scope);
        let result = crate::tools::TOOL_CTX
            .scope(ctx, tool.execute(&json!({})))
            .await
            .expect("execute should succeed");

        assert!(result.success, "hinted execute should succeed");
        let actual = std::fs::canonicalize(result.output.trim())
            .expect("CWD echoed by plugin should resolve");
        let expected =
            std::fs::canonicalize(hinted_work_dir.path()).expect("hinted work_dir should resolve");
        assert_eq!(
            actual, expected,
            "registry-rebound self.work_dir MUST win over scope.workspace()"
        );
        // Defence in depth: the scope workspace must STILL be absent
        // because Phase 2-B did NOT redirect the spawn there.
        assert!(
            !scope_workspace.exists(),
            "scope workspace must NOT be created when self.work_dir wins"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn plugin_falls_back_to_self_work_dir_when_no_scope() {
        // Backward compat: legacy callers (no scope threaded) must keep
        // the construction-time `self.work_dir` as the plugin's CWD.
        let dir = tempfile::tempdir().expect("temp dir");
        let script_path = dir.path().join("script.sh");
        write_test_script(
            &script_path,
            "#!/bin/sh\nDIR=$(pwd)\nprintf '{\"output\":\"%s\",\"success\":true}' \"$DIR\"\n",
        );

        let def = make_tool_def("legacy_cwd", "echo CWD");
        let tool = PluginTool::new("plug".into(), def, script_path)
            .with_work_dir(dir.path().to_path_buf())
            .with_timeout(Duration::from_secs(5));

        // No scope threaded — execute via the default `TOOL_CTX::zero`
        // shape (the global TOOL_CTX::try_with returns Err so the
        // legacy path is taken).
        let result = tool
            .execute(&json!({}))
            .await
            .expect("execute should succeed");
        assert!(result.success);
        let actual = std::fs::canonicalize(result.output.trim())
            .expect("CWD echoed by plugin should resolve");
        let expected = std::fs::canonicalize(dir.path()).expect("construction dir should resolve");
        assert_eq!(
            actual, expected,
            "no-scope path must use construction-time work_dir"
        );
    }

    #[test]
    fn plugin_refuses_out_of_scope_input_path() {
        // Phase 2-B: with scope, every input-path key (`audio_path`,
        // `file_path`, `input`, `script_path`, `video_path`,
        // `text_path`) MUST refuse paths that `classify_lexical_path`
        // resolves to `OutOfScope`. This collapses the round-1..round-4
        // bespoke `..`-guards into one gate.
        let workspace = tempfile::tempdir().expect("workspace dir");
        // Bait file outside the workspace — escape attempts would
        // otherwise resolve here.
        let outside = tempfile::tempdir().expect("outside dir");
        let bait = outside.path().join("passwd");
        std::fs::write(&bait, b"ROOT:x:0:0::/root:/bin/sh").unwrap();

        let scope = solo_scope_at(workspace.path());
        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("audio_path"),
            PathBuf::from("/bin/true"),
        );

        let outside_abs = outside.path().join("passwd").to_string_lossy().into_owned();
        for raw in [
            "../passwd",
            "../../etc/passwd",
            "foo/../../bar",
            outside_abs.as_str(),
        ] {
            let err = tool
                .rewrite_args_with_scope(&json!({"audio_path": raw}), &scope, scope.workspace())
                .expect_err(&format!("scope refuse must Err for {raw:?}"));
            let msg = err.to_string();
            assert!(
                msg.contains(raw),
                "error must echo the rejected raw path: {msg}",
            );
        }
        let _ = bait;
    }

    #[test]
    fn plugin_refuses_out_of_scope_output_path() {
        // Phase 2-B: output-path keys (`out`, `slide_dir`) must refuse
        // `OutOfScope` paths with the same one-shot
        // `classify_lexical_path` gate. Collapses the round-4
        // `absolutize_path_in_work_dir` Err contract on output keys
        // into the unified scope policy.
        let workspace = tempfile::tempdir().expect("workspace dir");
        let scope = solo_scope_at(workspace.path());

        let def = PluginToolDef {
            name: "phase2b_output_tool".to_string(),
            description: "fixture".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "out": {"type": "string"},
                    "slide_dir": {"type": "string"}
                }
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

        for (key, raw) in [
            ("out", "../sneaky"),
            ("slide_dir", "../escape"),
            ("out", ".."),
            ("slide_dir", "subdir/../../../escape"),
        ] {
            let err = tool
                .rewrite_args_with_scope(&json!({ key: raw }), &scope, scope.workspace())
                .expect_err(&format!("output key {key:?} with {raw:?} must Err"));
            let msg = err.to_string();
            assert!(
                msg.contains(raw),
                "error for {key:?}={raw:?} must echo offending path: {msg}",
            );
        }
    }

    #[test]
    fn plugin_reads_from_shared_zone_research_dir_when_scope_present() {
        // Phase 2-B: multi-tenant `shared_zones` (e.g. `<root>/research/`,
        // `<root>/skills/`) classify as `InSharedZone`. The plugin tool
        // must allow READ from those zones with explicit intent —
        // input-path keys carry read intent by construction. Mirrors
        // the `PathClassification::InSharedZone` doc contract.
        let data = tempfile::tempdir().expect("data dir");
        let research = data.path().join("research");
        std::fs::create_dir_all(&research).expect("create research zone");
        let report = research.join("dossier.md");
        std::fs::write(&report, b"# shared dossier").unwrap();

        let scope = multi_tenant_scope_at(
            data.path(),
            "dspfac",
            "web-read-shared",
            vec![research.clone()],
        );

        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("input"),
            PathBuf::from("/bin/true"),
        );

        let rewritten = tool
            .rewrite_args_with_scope(
                &json!({"input": report.to_string_lossy().to_string()}),
                &scope,
                scope.workspace(),
            )
            .expect("read from shared zone must succeed");
        assert_eq!(
            rewritten["input"].as_str().unwrap(),
            report.to_string_lossy().to_string(),
            "shared-zone read must pass through as absolute path"
        );
    }

    #[test]
    fn plugin_refuses_write_to_shared_zone() {
        // Phase 2-B: `InSharedZone` doc contract says reads allowed,
        // writes refused. Output-path keys (`out`, `slide_dir`) must
        // therefore Err when the path lands in a shared zone.
        let data = tempfile::tempdir().expect("data dir");
        let research = data.path().join("research");
        std::fs::create_dir_all(&research).expect("create research zone");

        let scope = multi_tenant_scope_at(
            data.path(),
            "dspfac",
            "web-write-shared",
            vec![research.clone()],
        );

        let def = PluginToolDef {
            name: "phase2b_write_shared".to_string(),
            description: "fixture".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"out": {"type": "string"}}
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));

        let write_target = research.join("forbidden_output.pptx");
        let raw = write_target.to_string_lossy().to_string();
        let err = tool
            .rewrite_args_with_scope(&json!({"out": raw.clone()}), &scope, scope.workspace())
            .expect_err("writes to shared zone must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains(&raw),
            "error must echo the rejected raw path: {msg}"
        );
        assert!(
            msg.contains("shared zone") && msg.contains("read-only"),
            "error must explain shared-zone read-only policy: {msg}"
        );
    }

    #[test]
    fn plugin_scope_path_rescues_basename_when_workspace_relative_missing() {
        // Codex P2 pin (Phase 2-B): the scope-aware rewriter must
        // preserve the legacy `resolve_path_in_work_dir` basename
        // rescue. LLMs commonly hallucinate a directory prefix in
        // front of a basename that exists at the workspace root
        // (e.g. `audio_path: "uploads/mark.wav"` when only
        // `<workspace>/mark.wav` exists from a prior attachment
        // copy). Before this fix the scope path would have rewritten
        // to the lexically-joined `<workspace>/uploads/mark.wav` and
        // the plugin's `fs::read` would fail with `os error 2`,
        // breaking attachment workflows under scoped sessions.
        let workspace = tempfile::tempdir().expect("workspace");
        let mark = workspace.path().join("mark.wav");
        std::fs::write(&mark, b"wav").unwrap();
        // `uploads/mark.wav` deliberately does NOT exist.

        let scope = solo_scope_at(workspace.path());
        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("audio_path"),
            PathBuf::from("/bin/true"),
        );

        let rewritten = tool
            .rewrite_args_with_scope(
                &json!({"audio_path": "uploads/mark.wav"}),
                &scope,
                scope.workspace(),
            )
            .expect("scope rewrite must succeed with basename rescue");
        assert_eq!(
            rewritten["audio_path"].as_str().unwrap(),
            mark.to_string_lossy().to_string(),
            "scope-aware path must rescue `<workspace>/<basename>` when the lexically-joined path is missing"
        );
    }

    #[test]
    fn scope_still_validates_out_of_scope_when_self_work_dir_is_rebound() {
        // Codex round-2 P1 pin (Phase 2-B): the round-1 fix routed
        // hinted/rebound sessions through the legacy rewriter, which
        // only blocked `..`. The intended invariant is that
        // `SessionScope` validation applies to ALL scoped sessions,
        // EVEN when the registry rebound `self.work_dir`. Only the
        // join base for relative paths shifts; absolute or workspace-
        // relative paths still get scope-checked.
        //
        // Concretely: a hinted session with scope X and rebound
        // work_dir Y must still refuse an absolute path that escapes
        // the scope (`/etc/passwd`), even though Y is honoured for
        // CWD.
        let scope_workspace = tempfile::tempdir().expect("scope workspace");
        let rebound_work_dir = tempfile::tempdir().expect("rebound work_dir");
        // Bait file deliberately outside both dirs.
        let bait_outside = tempfile::tempdir().expect("bait");
        let bait_path = bait_outside.path().join("escape.txt");
        std::fs::write(&bait_path, b"BAIT").unwrap();

        let scope = solo_scope_at(scope_workspace.path());
        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("audio_path"),
            PathBuf::from("/bin/true"),
        );

        // Even with the rebound dir as the join base, an absolute
        // path outside the scope must still Err.
        let bait_abs = bait_path.to_string_lossy().to_string();
        let err = tool
            .rewrite_args_with_scope(
                &json!({"audio_path": bait_abs.clone()}),
                &scope,
                rebound_work_dir.path(),
            )
            .expect_err("absolute out-of-scope path must still Err under hinted wiring");
        assert!(
            err.to_string().contains(&bait_abs),
            "error must echo the rejected path: {err}",
        );

        // Defence in depth: shared-zone write refusal must also still
        // apply when the rebound work_dir would otherwise mask the
        // scope. We use a multi-tenant scope here so a shared zone
        // exists; the rebound dir is unrelated to either.
        let data = tempfile::tempdir().expect("data");
        let research = data.path().join("research");
        std::fs::create_dir_all(&research).unwrap();
        let multi_scope = multi_tenant_scope_at(
            data.path(),
            "dspfac",
            "web-codex-r2",
            vec![research.clone()],
        );
        let target_in_shared = research.join("forbidden.txt").to_string_lossy().to_string();
        let def = PluginToolDef {
            name: "phase2b_r2_output".to_string(),
            description: "fixture".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"out": {"type": "string"}}
            }),
            spawn_only: false,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool_out = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"));
        let err = tool_out
            .rewrite_args_with_scope(
                &json!({"out": target_in_shared.clone()}),
                &multi_scope,
                rebound_work_dir.path(),
            )
            .expect_err("shared-zone write must Err even under hinted wiring");
        assert!(
            err.to_string().contains(&target_in_shared) && err.to_string().contains("shared zone"),
            "shared-zone write error must echo the path and explain: {err}",
        );
    }

    #[test]
    fn scope_basename_rescue_does_not_fire_for_shared_zone_misses() {
        // Codex round-2 P2 pin (Phase 2-B): the basename rescue must
        // be bounded to `InWorkspace`. A missing `InSharedZone` path
        // whose basename happens to match a workspace file MUST NOT
        // silently rewrite to the workspace file — the plugin would
        // then process different input than the LLM asked for.
        let data = tempfile::tempdir().expect("data");
        let research = data.path().join("research");
        std::fs::create_dir_all(&research).unwrap();
        let scope = multi_tenant_scope_at(
            data.path(),
            "dspfac",
            "web-rescue-bound",
            vec![research.clone()],
        );
        let workspace_file = scope.workspace().join("report.md");
        std::fs::create_dir_all(scope.workspace()).unwrap();
        std::fs::write(&workspace_file, b"# workspace report").unwrap();

        // The LLM asks for `<shared>/report.md` (missing on disk).
        // The basename `report.md` matches the workspace file. The
        // round-2 fix MUST NOT promote the workspace file.
        let missing_shared = research.join("report.md").to_string_lossy().to_string();
        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("input"),
            PathBuf::from("/bin/true"),
        );
        let rewritten = tool
            .rewrite_args_with_scope(
                &json!({"input": missing_shared.clone()}),
                &scope,
                scope.workspace(),
            )
            .expect("shared-zone read should succeed (file may be missing)");
        assert_eq!(
            rewritten["input"].as_str().unwrap(),
            missing_shared,
            "rescue MUST NOT redirect a missing shared-zone path to a basename-matching workspace file"
        );
    }

    #[test]
    fn scope_path_rescues_skill_output_prefix_under_un_hinted_rebind() {
        // Codex round-3 P2 pin (Phase 2-B): for scoped sessions whose
        // registry rebound `self.work_dir` to
        // `<scope.workspace>/skill-output` (the typical un-hinted
        // bootstrap path), the rescue must scan the rebound work_dir.
        // Inputs like `script_path: "skill-output/mofa-podcast/intro.md"`
        // with the file actually at
        // `<scope.workspace>/skill-output/mofa-podcast/intro.md` must
        // resolve correctly — the legacy `strip_redundant_skill_output_prefix`
        // logic that `resolve_plugin_input_path` performs MUST still
        // be reachable from the scope-aware path.
        let workspace = tempfile::tempdir().expect("workspace");
        let skill_output = workspace.path().join("skill-output");
        let podcast_dir = skill_output.join("mofa-podcast");
        std::fs::create_dir_all(&podcast_dir).unwrap();
        let script = podcast_dir.join("intro.md");
        std::fs::write(&script, b"# podcast").unwrap();

        let scope = solo_scope_at(workspace.path());
        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("script_path"),
            PathBuf::from("/bin/true"),
        );

        // Mimic the routing in `prepare_effective_args`: scope is
        // present AND `self.work_dir == <scope.workspace>/skill-output`
        // (inside scope), so the join_base shifts to the rebound dir.
        let rewritten = tool
            .rewrite_args_with_scope(
                &json!({"script_path": "skill-output/mofa-podcast/intro.md"}),
                &scope,
                &skill_output,
            )
            .expect("scope rewrite must succeed");
        let resolved = rewritten["script_path"].as_str().unwrap();
        let resolved_canon = std::fs::canonicalize(resolved).unwrap_or_else(|_| {
            panic!("resolved path must exist on disk, got: {resolved}");
        });
        let expected_canon = std::fs::canonicalize(&script).expect("expected exists");
        assert_eq!(
            resolved_canon, expected_canon,
            "scoped rebind must rescue the redundant `skill-output/` prefix \
             via the legacy resolver chain"
        );
    }

    #[test]
    fn scope_path_rescues_basename_under_un_hinted_rebind() {
        // Codex round-3 P2 pin (Phase 2-B): basename rescue inside the
        // scope path must scan the rebound `self.work_dir`, not just
        // `scope.workspace()`. When the registry rebound
        // `<scope.workspace>/skill-output` and the LLM hallucinates a
        // directory prefix in front of a basename that exists at the
        // REBOUND work_dir (`audio_path: "uploads/mark.wav"` when
        // `<scope.workspace>/skill-output/mark.wav` is the actual
        // file), the rescue must promote the rebound-dir candidate.
        let workspace = tempfile::tempdir().expect("workspace");
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        let mark = skill_output.join("mark.wav");
        std::fs::write(&mark, b"wav").unwrap();
        // `uploads/mark.wav` deliberately does NOT exist.

        let scope = solo_scope_at(workspace.path());
        let tool = PluginTool::new(
            "plug".into(),
            input_path_def("audio_path"),
            PathBuf::from("/bin/true"),
        );

        let rewritten = tool
            .rewrite_args_with_scope(
                &json!({"audio_path": "uploads/mark.wav"}),
                &scope,
                &skill_output,
            )
            .expect("scope rewrite must succeed");
        assert_eq!(
            rewritten["audio_path"].as_str().unwrap(),
            mark.to_string_lossy().to_string(),
            "basename rescue must scan the rebound work_dir, not just scope.workspace()"
        );
    }

    #[test]
    fn legacy_workspace_root_rescue_still_works_when_no_scope() {
        // Backward compat: when NO scope is threaded, the legacy
        // `resolve_plugin_input_path` chain (including the #1189
        // workspace-root rescue for plugins chrooted to
        // `<workspace>/skill-output/`) must still rescue
        // `<workspace>/<basename>` candidates. This pin makes sure the
        // Phase 2-B migration didn't accidentally delete the legacy
        // fallback that production fleet binaries still rely on.
        let workspace = tempfile::tempdir().expect("workspace");
        let skill_output = workspace.path().join("skill-output");
        std::fs::create_dir_all(&skill_output).unwrap();
        let script = workspace.path().join("script.md");
        std::fs::write(&script, b"# script").unwrap();

        // Resolver-level rescue still kicks in.
        let resolved = resolve_plugin_input_path("script.md", &skill_output)
            .expect("workspace-root rescue must still resolve");
        assert_eq!(std::path::Path::new(&resolved), &script);

        // End-to-end via `rewrite_workspace_file_args` (the legacy
        // entry point used when `prepare_effective_args` sees no
        // scope on the ToolContext).
        let def = PluginToolDef {
            name: "podcast_generate".to_string(),
            description: "Podcast generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"script_path": {"type": "string"}}
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-podcast".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(skill_output.clone());
        let rewritten = tool
            .rewrite_workspace_file_args(&json!({"script_path": "script.md"}))
            .expect("legacy rewrite must succeed");
        assert_eq!(
            rewritten["script_path"].as_str().unwrap(),
            script.to_string_lossy().to_string(),
            "legacy rescue must continue to bridge workspace-root scripts"
        );
    }

    // ---- mofa_slides style pre-flight validator ----
    //
    // These cover the synth-ack gap closed in
    // `Tool::pre_flight_validate` for `mofa_slides`: invalid `style=`
    // values used to slip past the spawn_only intercept and the LLM
    // never saw the plugin's later `success:false`. The pre-flight now
    // catches bare-name styles synchronously so the LLM gets a
    // `[VALIDATION FAILED]` tool_result instead of the misleading
    // synth-ack.

    /// Helper: build a temp `skill_dir` with `styles/<name>.toml` entries.
    fn make_skill_dir_with_styles(styles: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create skill_dir");
        let styles_dir = dir.path().join("styles");
        std::fs::create_dir_all(&styles_dir).expect("mkdir styles");
        for name in styles {
            std::fs::write(styles_dir.join(format!("{name}.toml")), b"").expect("write style");
        }
        dir
    }

    #[test]
    fn mofa_slides_preflight_accepts_builtin_style() {
        let skill_dir =
            make_skill_dir_with_styles(&["nb-pro", "puer-tea", "modern-cn", "vintage-jp"]);

        let result =
            validate_mofa_slides_style(&json!({"style": "nb-pro"}), Some(skill_dir.path()), None);

        assert!(
            result.is_ok(),
            "built-in style must pass pre-flight: {result:?}"
        );
    }

    #[test]
    fn mofa_slides_preflight_accepts_workspace_custom_style() {
        let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
        let work_dir = make_skill_dir_with_styles(&["custom-brand"]);

        let result = validate_mofa_slides_style(
            &json!({"style": "custom-brand"}),
            Some(skill_dir.path()),
            Some(work_dir.path()),
        );

        assert!(
            result.is_ok(),
            "workspace custom style must pass pre-flight: {result:?}"
        );
    }

    #[test]
    fn mofa_slides_preflight_rejects_missing_style() {
        let skill_dir = make_skill_dir_with_styles(&["nb-pro", "puer-tea"]);
        let work_dir = make_skill_dir_with_styles(&["custom-brand"]);

        let result = validate_mofa_slides_style(
            &json!({"style": "puer-woodcut"}),
            Some(skill_dir.path()),
            Some(work_dir.path()),
        );

        let Err(msg) = result else {
            panic!("expected pre-flight to reject invalid style, got Ok");
        };
        assert!(
            msg.contains("not found"),
            "error must mention 'not found': {msg}"
        );
        assert!(
            msg.contains("Available built-in styles"),
            "error must list available built-in styles: {msg}"
        );
        // Built-in names should be present, sorted/joined.
        assert!(msg.contains("nb-pro"), "error must list nb-pro: {msg}");
        assert!(msg.contains("puer-tea"), "error must list puer-tea: {msg}");
        // Workspace custom styles listed separately.
        assert!(
            msg.contains("Available workspace custom styles"),
            "error must list workspace customs: {msg}"
        );
        assert!(
            msg.contains("custom-brand"),
            "error must list custom-brand: {msg}"
        );
        // Hint to author under work_dir/styles/.
        assert!(
            msg.contains(&format!(
                "{}/styles/puer-woodcut.toml",
                work_dir.path().display()
            )),
            "error must hint at the workspace authoring path: {msg}"
        );
    }

    #[test]
    fn mofa_slides_preflight_passes_when_no_style_arg() {
        // No styles dir at all on disk — pre-flight must NOT touch the
        // filesystem when the LLM omits `style`. The plugin's
        // default-style fallback path is what runs in production.
        let skill_dir = tempfile::tempdir().expect("create skill_dir");
        let work_dir = tempfile::tempdir().expect("create work_dir");

        for args in [json!({}), json!({"style": ""}), json!({"style": "   "})] {
            let result =
                validate_mofa_slides_style(&args, Some(skill_dir.path()), Some(work_dir.path()));
            assert!(
                result.is_ok(),
                "missing/empty style must pass pre-flight (args={args:?}): {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn mofa_slides_preflight_only_fires_for_mofa_slides_tool() {
        // A plugin tool with a different name must NOT be gated by the
        // mofa_slides style check, even when it carries a bogus `style`
        // arg — the pre-flight is intentionally scoped to one tool.
        let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
        let executable = skill_dir.path().join("other-binary");
        std::fs::write(&executable, b"").expect("write fake exe");

        let def = PluginToolDef {
            name: "podcast_generate".to_string(),
            description: "Podcast generator".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"style": {"type": "string"}}
            }),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("mofa-podcast".into(), def, executable);

        let result = tool
            .pre_flight_validate(&json!({"style": "does-not-exist"}))
            .await;
        assert!(
            result.is_ok(),
            "non-mofa_slides tool must skip pre-flight even with bad style: {result:?}"
        );

        // And the mofa_slides tool with the same bogus style MUST fail.
        let mofa_def = PluginToolDef {
            name: "mofa_slides".to_string(),
            description: "Slides".to_string(),
            input_schema: json!({"type": "object", "properties": {"style": {"type": "string"}}}),
            spawn_only: true,
            env: vec![],
            risk: None,
            spawn_only_message: None,
            concurrency_class: None,
        };
        let mofa_executable = skill_dir.path().join("mofa-slides");
        std::fs::write(&mofa_executable, b"").expect("write fake mofa exe");
        let mofa_tool = PluginTool::new("mofa-slides".into(), mofa_def, mofa_executable);
        let mofa_result = mofa_tool
            .pre_flight_validate(&json!({"style": "does-not-exist"}))
            .await;
        assert!(
            mofa_result.is_err(),
            "mofa_slides MUST reject bad style at pre-flight: {mofa_result:?}"
        );
    }

    // ---- Codex review on PR #1323 regression tests ----
    //
    // These guard the BLOCKER + MAJOR + MINOR findings: workspace-root
    // custom styles when `work_dir` is `<workspace>/skill-output`,
    // path-shaped style values that the mofa rewriter would otherwise
    // normalize to a missing basename, and the `.toml.toml` hint bug.

    #[test]
    fn mofa_slides_preflight_accepts_workspace_root_custom_style_when_work_dir_is_skill_output() {
        // SessionRuntime binds the plugin work_dir to
        // `<workspace>/skill-output` (see runtime/session.rs:222), but the
        // slides prompt tells the LLM to author custom styles at
        // workspace-root `styles/{name}.toml` (slides_default.txt:62). The
        // pre-flight must probe `work_dir.parent()/styles/` when work_dir
        // basename is `skill-output`, otherwise a valid workspace-root
        // custom is falsely rejected.
        let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
        let workspace = tempfile::tempdir().expect("create workspace");
        let workspace_styles = workspace.path().join("styles");
        std::fs::create_dir_all(&workspace_styles).expect("mkdir workspace styles");
        std::fs::write(workspace_styles.join("foo.toml"), b"").expect("write workspace style");
        let work_dir = workspace.path().join("skill-output");
        std::fs::create_dir_all(&work_dir).expect("mkdir skill-output");

        let result = validate_mofa_slides_style(
            &json!({"style": "foo"}),
            Some(skill_dir.path()),
            Some(&work_dir),
        );

        assert!(
            result.is_ok(),
            "workspace-root custom style at <ws>/styles/foo.toml must pass pre-flight \
             when work_dir=<ws>/skill-output: {result:?}"
        );
    }

    #[test]
    fn mofa_slides_preflight_rejects_traversal_style() {
        // The mofa rewriter normalizes "../etc/passwd" to basename "passwd"
        // (see normalize_mofa_style_name + tool.rs:609). Pre-flight must
        // validate that normalized basename so the bypass doesn't surface
        // as a background `success:false` the LLM never sees.
        let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
        let work_dir = tempfile::tempdir().expect("create work_dir");

        let result = validate_mofa_slides_style(
            &json!({"style": "../etc/passwd"}),
            Some(skill_dir.path()),
            Some(work_dir.path()),
        );

        let Err(msg) = result else {
            panic!("expected pre-flight to reject traversal style, got Ok");
        };
        assert!(
            msg.contains("not found") || msg.contains("not a valid style name"),
            "error must signal rejection: {msg}"
        );
    }

    #[test]
    fn mofa_slides_preflight_rejects_absolute_path_style() {
        // The rewriter normalizes "/tmp/missing.toml" to basename
        // "missing" before the plugin runs (tool.rs:778). Pre-flight must
        // validate that, not skip path-shaped values.
        let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
        let work_dir = tempfile::tempdir().expect("create work_dir");

        let result = validate_mofa_slides_style(
            &json!({"style": "/tmp/missing.toml"}),
            Some(skill_dir.path()),
            Some(work_dir.path()),
        );

        assert!(
            result.is_err(),
            "absolute-path style with missing basename must fail pre-flight: {result:?}"
        );
    }

    #[test]
    fn mofa_slides_preflight_hint_does_not_double_toml_suffix() {
        // When the LLM passes `style: "foo.toml"` and the file doesn't
        // exist, the authoring hint must say `styles/foo.toml`, not
        // `styles/foo.toml.toml`. The hint formatter must use the
        // normalized stem.
        let skill_dir = make_skill_dir_with_styles(&["nb-pro"]);
        let work_dir = tempfile::tempdir().expect("create work_dir");

        let result = validate_mofa_slides_style(
            &json!({"style": "foo.toml"}),
            Some(skill_dir.path()),
            Some(work_dir.path()),
        );

        let Err(msg) = result else {
            panic!("expected pre-flight to reject missing 'foo.toml', got Ok");
        };
        assert!(
            !msg.contains("foo.toml.toml"),
            "authoring hint must not double the .toml suffix: {msg}"
        );
        assert!(
            msg.contains("styles/foo.toml"),
            "authoring hint must reference styles/foo.toml: {msg}"
        );
        assert!(
            msg.contains("SKILL.md"),
            "error must reference SKILL.md custom-style authoring: {msg}"
        );
    }
}
