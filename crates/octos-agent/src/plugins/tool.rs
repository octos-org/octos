//! Plugin tool: wraps a plugin executable as a Tool.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::harness_errors::HarnessError;
use crate::harness_events::{
    OCTOS_EVENT_SINK_ENV, OCTOS_HARNESS_SESSION_ID_ENV, OCTOS_HARNESS_TASK_ID_ENV,
    OCTOS_SESSION_ID_ENV, OCTOS_TASK_ID_ENV, lookup_event_sink_context, write_event_to_sink,
};
use crate::progress::ProgressEvent;
use crate::subprocess_env::{EnvAllowlist, sanitize_command_env, should_forward_env_name};
use crate::tools::{TOOL_CTX, Tool, ToolContext, ToolResult};

use super::manifest::PluginToolDef;

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
        }
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

    fn rewrite_workspace_file_args(&self, args: &serde_json::Value) -> serde_json::Value {
        let Some(work_dir) = self.work_dir.as_ref() else {
            return args.clone();
        };
        let Some(obj) = args.as_object() else {
            return args.clone();
        };

        let mut rewritten = serde_json::Map::with_capacity(obj.len());
        for (key, value) in obj {
            if matches!(key.as_str(), "audio_path" | "file_path" | "input") {
                if let Some(path) = value.as_str() {
                    rewritten.insert(
                        key.clone(),
                        serde_json::Value::String(
                            resolve_path_in_work_dir(path, work_dir)
                                .unwrap_or_else(|| absolutize_path_in_work_dir(path, work_dir)),
                        ),
                    );
                    continue;
                }
            }
            if matches!(key.as_str(), "out" | "slide_dir") {
                if let Some(path) = value.as_str() {
                    rewritten.insert(
                        key.clone(),
                        serde_json::Value::String(absolutize_path_in_work_dir(path, work_dir)),
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
                    if let Some(resolved) = resolve_slides_style_in_work_dir(style, work_dir) {
                        rewritten.insert(key.clone(), serde_json::Value::String(resolved));
                        continue;
                    }
                }
            }
            if key == "slides" {
                if let Some(slides) = value.as_array() {
                    let rewritten_slides = slides
                        .iter()
                        .map(|slide| {
                            let Some(slide_obj) = slide.as_object() else {
                                return slide.clone();
                            };
                            let mut rewritten_slide = slide_obj.clone();
                            if let Some(source_image) = slide_obj
                                .get("source_image")
                                .and_then(|value| value.as_str())
                            {
                                rewritten_slide.insert(
                                    "source_image".into(),
                                    serde_json::Value::String(
                                        resolve_path_in_work_dir(source_image, work_dir)
                                            .unwrap_or_else(|| {
                                                absolutize_path_in_work_dir(source_image, work_dir)
                                            }),
                                    ),
                                );
                            }
                            serde_json::Value::Object(rewritten_slide)
                        })
                        .collect::<Vec<_>>();
                    rewritten.insert(key.clone(), serde_json::Value::Array(rewritten_slides));
                    continue;
                }
            }
            rewritten.insert(key.clone(), value.clone());
        }
        serde_json::Value::Object(rewritten)
    }

    pub(crate) fn prepare_effective_args(
        &self,
        args: &serde_json::Value,
        ctx: Option<&ToolContext>,
    ) -> serde_json::Value {
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

        let mut effective_args = self.rewrite_workspace_file_args(&effective_args);
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

        effective_args
    }

    async fn detect_output_file(
        &self,
        effective_args: &serde_json::Value,
        output: &str,
        files_to_send: &mut Vec<std::path::PathBuf>,
    ) -> Option<std::path::PathBuf> {
        let out_file = effective_args
            .get("out")
            .and_then(|v| v.as_str())
            .and_then(|p| {
                let path = std::path::PathBuf::from(p);
                if path.is_absolute() && path.exists() {
                    return Some(path);
                }
                let candidates: Vec<std::path::PathBuf> = [
                    self.work_dir.as_ref().map(|d| d.join(&path)),
                    std::env::current_dir().ok().map(|d| d.join(&path)),
                ]
                .into_iter()
                .flatten()
                .collect();
                candidates
                    .into_iter()
                    .find(|c| c.exists())
                    .or_else(|| self.work_dir.as_ref().map(|d| d.join(&path)))
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
                        let in_work = self.work_dir.as_ref().map(|d| d.join(&path));
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

fn resolve_path_in_work_dir(raw_path: &str, work_dir: &std::path::Path) -> Option<String> {
    let candidate = std::path::Path::new(raw_path);
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

fn absolutize_path_in_work_dir(raw_path: &str, work_dir: &std::path::Path) -> String {
    let candidate = std::path::Path::new(raw_path);
    if candidate.is_absolute() {
        raw_path.to_string()
    } else {
        work_dir.join(candidate).to_string_lossy().into_owned()
    }
}

fn resolve_slides_style_in_work_dir(style: &str, work_dir: &std::path::Path) -> Option<String> {
    let trimmed = style.trim();
    if trimmed.is_empty() {
        return None;
    }

    let candidate = std::path::Path::new(trimmed);
    if candidate.is_absolute() || trimmed.contains('/') || trimmed.contains('\\') {
        return Some(absolutize_path_in_work_dir(trimmed, work_dir));
    }

    let filename = if trimmed.ends_with(".toml") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.toml")
    };
    let resolved = work_dir.join("styles").join(filename);
    resolved
        .exists()
        .then(|| resolved.to_string_lossy().into_owned())
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
        match self
            .tool_def
            .concurrency_class
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("exclusive") => super::super::tools::ConcurrencyClass::Exclusive,
            _ => super::super::tools::ConcurrencyClass::Safe,
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
                        "description": "Timeout in seconds. Estimate based on real execution times: single deep_search (depth=2) ~3min → 300s; single deep_search (depth=3) ~5min → 400s; research pipeline with 3 topics ~8min → 600s; research pipeline with 5-7 topics ~15-20min → 1200s; very complex multi-source analysis ~25min → 1500s. Max: 1800. Default: 600"
                    }),
                );
            }
        }
        schema
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
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

        let mut cmd = Command::new(&self.executable);
        cmd.arg(&self.tool_def.name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let env_allowlist = EnvAllowlist::from_strings(&self.tool_def.env);
        sanitize_command_env(&mut cmd, &env_allowlist);

        // Remove blocked environment variables
        for var in &self.blocked_env {
            cmd.env_remove(var);
        }

        let ctx: Option<ToolContext> = TOOL_CTX.try_with(|c| c.clone()).ok();

        // Inject extra environment variables (e.g. provider base URLs, API keys)
        for (key, val) in &self.extra_env {
            if should_forward_env_name(key, &env_allowlist) {
                cmd.env(key, val);
            } else {
                tracing::debug!(
                    plugin = %self.plugin_name,
                    tool = %self.tool_def.name,
                    env = %key,
                "skipping non-allowlisted secret environment variable for plugin tool"
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
        // OCTOS_WORK_DIR is kept for backward compat with plugins that read it.
        if let Some(ref dir) = self.work_dir {
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

        let effective_args = self.prepare_effective_args(args, ctx.as_ref());

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

            // Auto-deliver output file when plugin didn't report it.
            // Check multiple locations: work_dir, cwd, and the output text itself.
            let file_modified = if file_modified.is_none() && files_to_send.is_empty() {
                self.detect_output_file(&effective_args, &output, &mut files_to_send)
                    .await
            } else {
                file_modified
            };

            return Ok(ToolResult {
                output,
                success,
                file_modified,
                files_to_send,
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
            .detect_output_file(&effective_args, &output, &mut files_to_send)
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
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool.rewrite_workspace_file_args(&json!({
            "audio_path": "/home/user/uploads/mark.wav",
            "file_path": "deck.pdf",
        }));

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
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool.rewrite_workspace_file_args(&json!({
            "input": "slides/demo/script.js",
            "out": "slides/demo/output/deck.pptx",
            "slide_dir": "slides/demo/output/imgs"
        }));

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
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool.rewrite_workspace_file_args(&json!({
            "style": "cyberpunk-neon"
        }));

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
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool.rewrite_workspace_file_args(&json!({
            "style": style.to_string_lossy().to_string()
        }));

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
            spawn_only_message: None,
            concurrency_class: None,
        };
        let tool = PluginTool::new("plug".into(), def, PathBuf::from("/bin/true"))
            .with_work_dir(dir.path().to_path_buf());

        let rewritten = tool.rewrite_workspace_file_args(&json!({
            "style": "/tmp/styles/nb-pro.toml.toml"
        }));

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

        let prepared = tool.prepare_effective_args(&json!({}), Some(&ctx));

        assert_eq!(prepared["audio_path"], "/workspace/voice.ogg");
        assert_eq!(prepared["file_path"], "/workspace/report.pdf");
    }

    fn deep_search_def_with_opt_in() -> PluginToolDef {
        PluginToolDef {
            name: "deep_search".to_string(),
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

        let prepared = tool.prepare_effective_args(&json!({"query": "AI policy"}), None);
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

        let prepared = tool.prepare_effective_args(&json!({"query": "AI policy"}), None);
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

        let prepared = tool.prepare_effective_args(&json!({"query": "AI policy"}), None);
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

        let prepared = tool.prepare_effective_args(&json!({"query": "AI policy"}), None);
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

        let prepared = tool.prepare_effective_args(
            &json!({
                "query": "AI policy",
                "synthesis_config": {"api_key": "caller-supplied"}
            }),
            None,
        );
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
            "deep_search",
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
            "deep_search",
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
            "deep_search",
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
            "deep_search",
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
            "deep_search",
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
            "deep_search",
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
            "deep_search",
            Some(&ctx),
            r#"{"type":"cost","provider":"deepseek","model":"deepseek-chat","tokens_in":1024,"tokens_out":256,"usd":0.0034}"#,
        );

        let body = std::fs::read_to_string(&sink_path).expect("sink written");
        assert!(body.contains(r#""kind":"cost_attribution""#), "got: {body}");
        assert!(body.contains(r#""tokens_in":1024"#), "got: {body}");
        assert!(body.contains(r#""tokens_out":256"#), "got: {body}");
        assert!(body.contains(r#""cost_usd":0.0034"#), "got: {body}");
        assert!(body.contains(r#""contract_id":"plugin:deep-search:deep_search""#));
        assert!(body.contains(r#""provider":"deepseek""#));

        // Cleanup the sink registration.
        crate::harness_events::detach_event_sink_context(&ctx_path);
    }
}
