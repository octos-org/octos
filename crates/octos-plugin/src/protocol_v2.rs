//! Plugin protocol v2 — structured events on stderr + result extensions.
//!
//! See `docs/protocol-v2.md` for the full specification. This module provides
//! the wire-typed Rust definitions used by both plugin authors (when emitting
//! events) and the host (when parsing them).
//!
//! Wire shape: each event is one JSON object terminated by `\n`. The
//! discriminator is the top-level `type` field. Lines that fail to parse as
//! a [`ProtocolV2Event`] are treated as legacy v1 text-progress lines by the
//! host — that is the backward-compatibility contract. See
//! [`parse_event_line`] for the parser used by the shim.
//!
//! ## Why a discriminated enum
//!
//! `serde(tag = "type")` matches the spec ("each line is `{type: ...}`")
//! and gives the host pattern-match exhaustiveness when handling events.
//! Unknown variants fall through to [`ProtocolV2Event::Unknown`] so a future
//! plugin emitting a new event kind doesn't kill the host parser.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Recommended progress-stage labels.
///
/// Plugins SHOULD use one of these when applicable so the host can render
/// stable badges. Custom stages (any other lowercase snake_case string) are
/// allowed and rendered as-is.
pub mod stage {
    pub const INIT: &str = "init";
    pub const VALIDATING: &str = "validating";
    pub const SEARCHING: &str = "searching";
    pub const FETCHING: &str = "fetching";
    pub const CRAWLING: &str = "crawling";
    pub const CHASING: &str = "chasing";
    pub const SYNTHESIZING: &str = "synthesizing";
    pub const BUILDING_REPORT: &str = "building_report";
    pub const DELIVERING: &str = "delivering";
    pub const CLEANUP: &str = "cleanup";
    pub const COMPLETE: &str = "complete";
}

/// One event emitted on a plugin's stderr stream.
///
/// Wire format: a single-line JSON object with a `type` discriminator.
/// Forward-compatible: unknown `type` values land in [`ProtocolV2Event::Unknown`]
/// and are passed through to the host as legacy progress lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProtocolV2Event {
    /// Per-step progress with a free-form message and an optional structured
    /// detail blob. Hosts render `{stage} - {message}` and may forward the
    /// fraction to a progress bar.
    Progress(ProgressEvent),

    /// Cost attribution for an internal LLM/API call. The host commits this
    /// to the ledger so per-task spend is visible.
    Cost(CostEvent),

    /// High-level state transition (kept for clients that prefer phase events
    /// over progress events). Equivalent semantically to `progress` with
    /// `progress = None`.
    Phase(PhaseEvent),

    /// File the plugin produced as a side-effect (e.g. saved a screenshot).
    Artifact(ArtifactEvent),

    /// Structured wrapper around a log line. Useful when a plugin wants to
    /// emit a leveled message instead of plain stderr text.
    Log(LogEvent),

    /// Catch-all for forward compatibility. The original JSON is preserved
    /// in `raw` so a host that does understand the new variant via a side
    /// channel can re-parse it.
    #[serde(other, deserialize_with = "deserialize_unit")]
    Unknown,
}

fn deserialize_unit<'de, D: serde::Deserializer<'de>>(_d: D) -> Result<(), D::Error> {
    Ok(())
}

/// Progress event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    /// Stable lowercase snake_case label. See [`stage`] for recommended values.
    pub stage: String,
    /// Free-form human-readable description of the current step.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Optional fraction of work complete in `[0, 1]`. The host clamps to
    /// `[0, 1]` defensively before forwarding to the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Free-form structured detail for advanced UI rendering. Hosts that do
    /// not understand the contents pass them through to the SSE event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// Cost-attribution event payload.
///
/// Plugins SHOULD emit one [`CostEvent`] per internal LLM/API call so the
/// host ledger can attribute spend. If the same call is reported on stderr
/// AND in the final stdout `cost` summary, the host de-duplicates by
/// preferring stderr (which has finer-grained attribution per call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostEvent {
    /// Provider lane (`"openai"`, `"deepseek"`, `"anthropic"`, ...). Free-form,
    /// used for grouping in the cost dashboard.
    pub provider: String,
    /// Optional model identifier (`"gpt-4o-mini"`, `"deepseek-chat"`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Prompt tokens consumed.
    pub tokens_in: u32,
    /// Completion tokens produced.
    pub tokens_out: u32,
    /// Optional dollar cost. If absent, the host computes it from the model
    /// catalog if the model is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    /// Free-form context (request id, lane choice rationale, ...). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// Phase event payload (state transition).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseEvent {
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// Artifact event payload (a file the plugin produced).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactEvent {
    /// Absolute or workspace-relative path to the file on disk.
    pub path: String,
    /// Free-form artifact kind, e.g. `"report"`, `"screenshot"`, `"log"`.
    pub kind: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// Log event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    /// `"debug"`, `"info"`, `"warn"`, or `"error"`. Other values are rendered
    /// as `"info"` by the host.
    pub level: String,
    pub message: String,
}

/// Wire shape for the optional `summary` field in the final stdout result.
///
/// The host's `SubAgentSummaryGenerator` consumes this to build the typed
/// summary the parent agent sees, without re-running an LLM. `kind` is the
/// discriminator; `extra` carries kind-specific fields the host can pass
/// through verbatim to the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultSummary {
    /// Discriminator for the summary variant. Reserved kinds documented in
    /// `protocol-v2.md`: `deep_research`, `crawl`, `plugin:<name>:<phase>`.
    pub kind: String,
    /// One-line headline rendered in the parent's tool-call pill.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub headline: String,
    /// Optional confidence score in `[0, 1]`. Plugin-specific semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Sources referenced. For research plugins this is the citation list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ResultSource>,
    /// Free-form additional fields keyed by name. Hosts pass these through
    /// verbatim. Use for kind-specific extensions without adding new
    /// top-level fields to [`ResultSummary`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A single source/citation entry inside a [`ResultSummary`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Whether this source was cited in the synthesized prose. `true` means
    /// the citation appears in the final report; `false` means the source was
    /// fetched but not used in the answer.
    #[serde(default)]
    pub cited: bool,
    /// Optional relevance score in `[0, 1]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// Roll-up cost reported in the final stdout result.
///
/// Equivalent to summing all stderr [`CostEvent`]s but supplied as a separate
/// field so v1 plugins that don't emit per-call events on stderr can still
/// report their total spend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResultCost {
    /// Provider lane for the dominant call. If the plugin made calls to
    /// multiple providers it should aggregate them into separate stderr
    /// `cost` events instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
}

/// Outcome of attempting to parse one stderr line as a v2 event.
///
/// The shim uses this to dispatch: `Event` paths flow into the structured
/// progress channel, `Legacy` paths flow into the v1 text-line channel.
#[derive(Debug, Clone, PartialEq)]
pub enum LineParse {
    /// Line was a valid v2 event.
    Event(ProtocolV2Event),
    /// Line was a valid v2 event with an unknown `type` discriminator.
    /// The host SHOULD pass the original line through to the v1 text-line
    /// channel (so the user sees the message) but MAY also surface the JSON
    /// to a generic event sink for forward compatibility.
    UnknownEvent(String),
    /// Line was not a v2 event — fall back to legacy text progress.
    Legacy(String),
    /// Line was empty after trimming. Ignore.
    Empty,
}

/// Parse a single stderr line into a structured event or a legacy fallback.
///
/// The shim trims trailing CR/LF and a leading BOM, then attempts JSON
/// parsing only when the line starts with `{`. This keeps the hot path
/// allocation-free for legacy plugins (which never start lines with `{`).
///
/// # Examples
///
/// ```
/// use octos_plugin::protocol_v2::{parse_event_line, LineParse, ProtocolV2Event};
///
/// // v2 progress event
/// match parse_event_line(r#"{"type":"progress","stage":"init","message":"go"}"#) {
///     LineParse::Event(ProtocolV2Event::Progress(p)) => {
///         assert_eq!(p.stage, "init");
///         assert_eq!(p.message, "go");
///     }
///     other => panic!("expected progress event, got {other:?}"),
/// }
///
/// // legacy text line
/// match parse_event_line("hello there") {
///     LineParse::Legacy(s) => assert_eq!(s, "hello there"),
///     other => panic!("expected legacy line, got {other:?}"),
/// }
///
/// // empty line
/// assert_eq!(parse_event_line(""), LineParse::Empty);
/// assert_eq!(parse_event_line("   "), LineParse::Empty);
/// ```
pub fn parse_event_line(line: &str) -> LineParse {
    // Strip a leading UTF-8 BOM if present (rare but observed when plugins
    // pipe stderr through tools that prepend one).
    let trimmed = line.strip_prefix('\u{feff}').unwrap_or(line);
    let trimmed = trimmed.trim_end_matches(['\r', '\n']);
    let trimmed = trimmed.trim();

    if trimmed.is_empty() {
        return LineParse::Empty;
    }

    // Fast path: only attempt JSON parse if the line plausibly starts with
    // a JSON object. Saves a parse attempt per legacy line.
    if !trimmed.starts_with('{') {
        return LineParse::Legacy(trimmed.to_string());
    }

    match serde_json::from_str::<ProtocolV2Event>(trimmed) {
        Ok(ProtocolV2Event::Unknown) => LineParse::UnknownEvent(trimmed.to_string()),
        Ok(event) => LineParse::Event(event),
        Err(_) => LineParse::Legacy(trimmed.to_string()),
    }
}

/// Emit a v2 event on stderr.
///
/// Convenience helper for plugin authors. Serializes to a single line with
/// a trailing newline. If serialization fails (which would be a bug) the
/// helper falls back to writing nothing — a missing event is preferable to
/// a malformed line that confuses the host parser.
pub fn emit_event(event: &ProtocolV2Event) {
    if let Ok(json) = serde_json::to_string(event) {
        eprintln!("{json}");
    }
}

/// Convenience helper for `progress` events.
pub fn emit_progress(stage: &str, message: &str, progress: Option<f64>) {
    let event = ProtocolV2Event::Progress(ProgressEvent {
        stage: stage.to_string(),
        message: message.to_string(),
        progress,
        detail: None,
    });
    emit_event(&event);
}

/// Convenience helper for `cost` events.
pub fn emit_cost(
    provider: &str,
    model: Option<&str>,
    tokens_in: u32,
    tokens_out: u32,
    usd: Option<f64>,
) {
    let event = ProtocolV2Event::Cost(CostEvent {
        provider: provider.to_string(),
        model: model.map(String::from),
        tokens_in,
        tokens_out,
        usd,
        detail: None,
    });
    emit_event(&event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_json_falls_back_to_legacy() {
        let line = r#"{"type":"progress""#; // truncated
        match parse_event_line(line) {
            LineParse::Legacy(s) => assert_eq!(s, line.trim()),
            other => panic!("expected legacy line, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_is_unknown_event() {
        let line = r#"{"type":"newkind","data":42}"#;
        match parse_event_line(line) {
            LineParse::UnknownEvent(s) => assert_eq!(s, line),
            other => panic!("expected unknown event, got {other:?}"),
        }
    }

    #[test]
    fn very_long_legacy_line_is_returned_as_is() {
        let big = "x".repeat(10_000);
        match parse_event_line(&big) {
            LineParse::Legacy(s) => assert_eq!(s, big),
            other => panic!("expected legacy, got {other:?}"),
        }
    }
}
