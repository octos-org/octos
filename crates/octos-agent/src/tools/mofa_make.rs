//! RFC-1 (issue #1290): the `mofa_make` content-generator dispatcher.
//!
//! Replaces N parallel `mofa_*` plugin tools (mofa_slides, mofa_site,
//! mofa_youtube, podcast_generate, mofa_cards, mofa_comic, mofa_frame,
//! mofa_infographic, mofa_publish, ...) with a single dispatcher tool:
//!
//! ```text
//! mofa_make({ content_type: "slides" | "site" | "podcast" | ..., args: {...} })
//! ```
//!
//! Why structural: the "weak model picks the wrong sibling" failure mode
//! (PR #1265, #1283, mini1 k2.6 misroutes on 2026-05-24/25, mini2
//! deepseek hand-rolled-deck incident on 2026-05-25) cannot occur if
//! there are no siblings — the LLM picks `content_type` from a
//! string enum, no ambiguity, no peer pressure between siblings.
//!
//! ## Schema discipline
//!
//! Per the 2026-05-25 mofa-slides v0.5.0 anyOf incident, the LLM-facing
//! schema is intentionally minimal:
//!
//! * `content_type` is a STRING ENUM populated at runtime from
//!   discovered skill `make_type` declarations. No `oneOf`/`anyOf`
//!   branches — strict provider validators (kimi-k2.5, kimi-k2.6,
//!   deepseek, claude, openai) all accept this shape.
//! * `args` is declared `type: "object"` with no `properties` — the
//!   per-skill shape is opaque to the dispatcher. The LLM uses the
//!   companion `mofa_describe_content_type({content_type: ...})` to
//!   fetch the shape as a TEXT description (also avoiding the anyOf
//!   trap by not subjecting per-skill schemas to provider validation).
//!
//! ## Registry suppression
//!
//! After plugin tools register, the loader calls
//! `ToolRegistry::defer()` on every name that resolved as the "make
//! target" for a discovered `make_type`. Deferred tools are hidden from
//! `specs()` (LLM never sees them) but remain reachable via
//! `ToolRegistry::get()` so the dispatcher can forward to them.
//! Catalog/lookup tools next to a make target (e.g. `mofa_list_styles`
//! next to `mofa_slides`, `podcast_voices` next to `podcast_generate`)
//! stay visible — only the EXACT target tool is hidden.
//!
//! Internal callers (the gateway path, backward-compat tests) can still
//! reach the hidden tools by name via the registry — `is_tool_visible`
//! only governs whether the spec is published, not callability.

use std::sync::{Mutex, Weak};

use async_trait::async_trait;
use eyre::Result;
use serde_json::json;

use super::{Tool, ToolContext, ToolRegistry, ToolResult};

/// One entry in the `mofa_make` dispatcher's content_type table.
///
/// Built by the plugin loader from each discovered skill's manifest
/// (`make_type`, `content_type_description`, plus the resolved target
/// tool name from [`crate::plugins::PluginManifest::make_target_tool_name`]).
/// The loader hands a `Vec<MakeTypeEntry>` to
/// [`make_dispatcher_with_entries`] which seeds the dispatcher's spec
/// enum + the describe-tool's catalog.
///
/// Per-profile shadowing is preserved automatically because the loader
/// is invoked per profile; entries from per-profile skills replace
/// global entries with the same `content_type` (`HashMap`-style
/// last-write semantics via `register_or_replace`).
#[derive(Debug, Clone)]
pub struct MakeTypeEntry {
    /// The `content_type` discriminator the LLM selects (e.g. "slides",
    /// "podcast", "video", "publish"). MUST be unique within a
    /// dispatcher's registry — the loader enforces last-write by
    /// content_type to honour per-profile shadowing.
    pub content_type: String,
    /// The skill the entry came from (e.g. "mofa-slides"). Informational —
    /// used in dispatcher error messages and debug logs.
    pub skill_id: String,
    /// Tool name the dispatcher forwards to (e.g. "mofa_slides",
    /// "podcast_generate"). Resolved by
    /// [`crate::plugins::PluginManifest::make_target_tool_name`] at
    /// load time.
    pub target_tool: String,
    /// Human-readable blurb describing what this content_type generates.
    /// Surfaced verbatim in the dispatcher's tool description (one line
    /// per enum value) so the LLM has enough context to pick the right
    /// `content_type`. Should be short (single sentence, <200 chars).
    pub description: String,
}

impl MakeTypeEntry {
    /// Convenience constructor used by tests + the loader. All fields
    /// are required — the dispatcher relies on `content_type` /
    /// `target_tool` being non-empty.
    pub fn new(
        content_type: impl Into<String>,
        skill_id: impl Into<String>,
        target_tool: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            content_type: content_type.into(),
            skill_id: skill_id.into(),
            target_tool: target_tool.into(),
            description: description.into(),
        }
    }
}

/// Shared catalog the dispatcher and the describe tool consult.
///
/// Holds the `(content_type → MakeTypeEntry)` map and is shared between
/// `MofaMakeTool` and `MofaDescribeContentTypeTool` so a single loader
/// pass populates both. Entries are de-duplicated by `content_type`
/// (last-write wins) to preserve per-profile shadowing.
#[derive(Debug, Default)]
struct CatalogInner {
    /// Insertion-ordered list (for stable spec enum + describe output).
    ///
    /// Stored as a Vec instead of a HashMap so the dispatcher's
    /// `content_type` enum (visible to the LLM) is rendered in the
    /// load order, not HashMap iteration order. The dispatcher's
    /// `register_or_replace` linearly scans + replaces by content_type
    /// — fine for the typical 5-15 entry case (no big-O concern).
    entries: Vec<MakeTypeEntry>,
}

/// Dispatcher tool: forwards `mofa_make({content_type, args})` to the
/// concrete skill tool registered under `content_type`'s `make_type`.
///
/// ## Lifecycle
///
/// 1. [`MofaMakeTool::new`] mints a dispatcher with an empty catalog
///    and no registry back-reference.
/// 2. The loader calls [`MofaMakeTool::register_or_replace`] for every
///    discovered `MakeTypeEntry` to populate the catalog.
/// 3. The loader wraps the dispatcher in an `Arc` and registers it,
///    then calls [`MofaMakeTool::set_registry`] with a `Weak` ref so
///    the dispatcher can look up its forwarding target at execute time.
/// 4. The dispatcher is registered with `mark_spawn_only` so the
///    execution loop intercepts the call and runs the dispatcher in a
///    background tokio task. Inside that task, the dispatcher's
///    `execute` looks up the target tool and calls its `execute`
///    directly (bypassing the LLM loop, since we're already in the
///    background).
pub struct MofaMakeTool {
    catalog: Mutex<CatalogInner>,
    registry: Mutex<Option<Weak<ToolRegistry>>>,
}

impl Default for MofaMakeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MofaMakeTool {
    /// Mint a dispatcher with an empty catalog. Callers must populate
    /// via [`Self::register_or_replace`] and back-link via
    /// [`Self::set_registry`] before the tool is exercised.
    pub fn new() -> Self {
        Self {
            catalog: Mutex::new(CatalogInner::default()),
            registry: Mutex::new(None),
        }
    }

    /// Set (or replace) the registry back-reference after Arc wrapping.
    /// The Weak ref is upgraded on every `execute` call; if the registry
    /// has been dropped the call returns a `tool registry unavailable`
    /// error.
    pub fn set_registry(&self, weak: Weak<ToolRegistry>) {
        *self.registry.lock().unwrap_or_else(|e| e.into_inner()) = Some(weak);
    }

    /// Insert (or replace) a catalog entry. Replacement keys on
    /// `content_type` so per-profile loading can shadow a global skill
    /// by re-registering the same content_type with a different
    /// `target_tool`.
    pub fn register_or_replace(&self, entry: MakeTypeEntry) {
        let mut catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = catalog
            .entries
            .iter_mut()
            .find(|e| e.content_type == entry.content_type)
        {
            *existing = entry;
        } else {
            catalog.entries.push(entry);
        }
    }

    /// Replace the entire catalog with the given list.
    ///
    /// RFC-1 fixup (codex round 4 P2): used by `ToolRegistry::retain`
    /// to prune dispatcher entries whose forwarding targets have been
    /// evicted from the registry (e.g. slides-session retain pass).
    /// Without this prune the catalog's `content_type` enum would
    /// still advertise content types whose targets are gone, and the
    /// LLM would observe `[DISPATCHER_ERROR]` on dispatch.
    pub fn replace_entries(&self, entries: Vec<MakeTypeEntry>) {
        let mut catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        catalog.entries = entries;
    }

    /// Snapshot the catalog — used by tests and by the describe tool.
    pub fn entries(&self) -> Vec<MakeTypeEntry> {
        self.catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .clone()
    }

    /// Build the spec description (with the per-content_type cheat
    /// sheet) so the LLM can pick the right `content_type` without
    /// guessing. Kept as a method so the spec emission stays in sync
    /// with the catalog even if the loader re-registers.
    fn build_description(&self) -> String {
        let entries = self.entries();
        if entries.is_empty() {
            return "Generate content via AI workflows. No content_type \
                    is registered yet — install a mofa-* skill (e.g. \
                    mofa-slides, mofa-podcast) to populate the enum."
                .to_string();
        }
        let mut s = String::from(
            "Generate content via AI workflows. Pick a content_type \
             that matches the user's request. Each content_type \
             dispatches to a dedicated skill. Use \
             mofa_describe_content_type to get the args shape for a \
             specific type before invoking.\n\nAvailable content types:",
        );
        for e in &entries {
            s.push_str(&format!("\n- {}: {}", e.content_type, e.description));
        }
        s
    }
}

/// Stand-in error returned to the LLM when the dispatcher cannot
/// resolve a content_type. Distinguished from regular tool failure by
/// the `[CONTENT_TYPE_NOT_FOUND]` prefix so prompt-tuning and tests
/// can match on it deterministically.
fn unknown_content_type_msg(requested: &str, known: &[MakeTypeEntry]) -> String {
    let mut known_list: Vec<&str> = known.iter().map(|e| e.content_type.as_str()).collect();
    known_list.sort_unstable();
    if known_list.is_empty() {
        format!(
            "[CONTENT_TYPE_NOT_FOUND] content_type {requested:?} is not registered. \
             No mofa-* skills are installed."
        )
    } else {
        format!(
            "[CONTENT_TYPE_NOT_FOUND] content_type {:?} is not registered. \
             Available: {}. Call mofa_describe_content_type to inspect any of them.",
            requested,
            known_list.join(", ")
        )
    }
}

#[async_trait]
impl Tool for MofaMakeTool {
    fn name(&self) -> &str {
        "mofa_make"
    }

    fn description(&self) -> &str {
        // Cache the description on every call so registry mutations
        // post-construction (per-profile shadowing) reflect in the next
        // spec rebuild. The registry already invalidates its spec cache
        // when a tool is registered, so this Box::leak runs at most
        // once per cache invalidation per tool instance. Acceptable
        // (single-digit allocations over a process lifetime); avoids
        // a self-referential `String -> &str` borrow lifetime.
        //
        // ToolRegistry::specs() consumes `description()` once per
        // cache rebuild and clones into ToolSpec, so the returned &str
        // does not need to outlive a single call.
        let desc = self.build_description();
        Box::leak(desc.into_boxed_str())
    }

    fn input_schema(&self) -> serde_json::Value {
        let entries = self.entries();
        let enum_values: Vec<serde_json::Value> =
            entries.iter().map(|e| json!(e.content_type)).collect();
        // When no entries are registered, omit the `enum` constraint so
        // provider validators don't reject an empty list (some treat it
        // as an unsatisfiable schema). The description still tells the
        // LLM the dispatcher is unusable in this state.
        let content_type_schema = if enum_values.is_empty() {
            json!({
                "type": "string",
                "description": "Type of content to generate. NO content_types are currently registered — install a mofa-* skill to populate the enum."
            })
        } else {
            json!({
                "type": "string",
                "enum": enum_values,
                "description": "Type of content to generate. Use mofa_describe_content_type to get the args schema for a specific type before invoking."
            })
        };
        json!({
            "type": "object",
            "properties": {
                "content_type": content_type_schema,
                "args": {
                    "type": "object",
                    "description": "Skill-specific arguments. Opaque — call mofa_describe_content_type({content_type: \"...\"}) to see the shape per content_type."
                }
            },
            "required": ["content_type", "args"]
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn pre_flight_validate(&self, args: &serde_json::Value) -> Result<(), String> {
        // Synchronous catches that surface as a regular tool_result so
        // the LLM can retry in the same turn — vs. failing in the
        // background where the error is decoupled from the agent loop.
        let content_type = args.get("content_type").and_then(|v| v.as_str());
        let Some(ct) = content_type else {
            return Err(
                "missing required field `content_type` (must be a string from the enum)".into(),
            );
        };
        if ct.is_empty() {
            return Err("`content_type` must not be empty".into());
        }
        let entries = self.entries();
        if !entries.iter().any(|e| e.content_type == ct) {
            return Err(unknown_content_type_msg(ct, &entries));
        }
        if args.get("args").is_none() {
            return Err(
                "missing required field `args` (object) — pass {} for skills with no required args"
                    .into(),
            );
        }
        if !args.get("args").is_some_and(|v| v.is_object()) {
            return Err("`args` must be a JSON object".into());
        }
        Ok(())
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let entries = self.entries();
        let content_type = args
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let entry = entries
            .iter()
            .find(|e| e.content_type == content_type)
            .cloned();
        let Some(entry) = entry else {
            // pre_flight_validate should normally catch this in the
            // foreground, but defend in depth — the dispatcher might be
            // invoked from a context that bypasses pre-flight (tests,
            // future internal callers).
            return Ok(ToolResult {
                output: unknown_content_type_msg(content_type, &entries),
                success: false,
                ..Default::default()
            });
        };

        let forwarded_args = args.get("args").cloned().unwrap_or_else(|| json!({}));

        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|w| w.upgrade());
        let Some(registry) = registry else {
            return Ok(ToolResult {
                output: "[DISPATCHER_ERROR] tool registry is not available — mofa_make cannot resolve its forwarding target.".into(),
                success: false,
                ..Default::default()
            });
        };

        let Some(target) = registry.get(&entry.target_tool).cloned() else {
            return Ok(ToolResult {
                output: format!(
                    "[DISPATCHER_ERROR] content_type {:?} resolves to tool {:?} (from skill {:?}), \
                     but that tool is not in the registry. Re-check skill installation.",
                    content_type, entry.target_tool, entry.skill_id
                ),
                success: false,
                ..Default::default()
            });
        };

        tracing::info!(
            content_type = %entry.content_type,
            target = %entry.target_tool,
            skill = %entry.skill_id,
            "mofa_make dispatching"
        );

        // Forward via `execute_with_context` so any TLS-tracked
        // synthesis_config / TOOL_CTX state propagates to the target
        // tool — PluginTool::execute reads TOOL_CTX directly. Default
        // impl on Tool routes through `execute(args)` for tools that
        // don't override, so the call shape is uniform.
        target.execute_with_context(ctx, &forwarded_args).await
    }
}

/// Companion read-only tool: returns the args schema (as a text blob)
/// for a single content_type so the LLM can construct the `args` JSON.
///
/// Why text and not JSON-schema: per the 2026-05-25 mofa-slides v0.5.0
/// anyOf incident, returning per-skill JSON-schema as a top-level
/// `oneOf`/`anyOf` provoked strict provider validators (kimi-k2.6) to
/// reject the dispatcher tool entirely. Surfacing the schema as a
/// PRE-RENDERED TEXT description sidesteps that — the LLM consumes
/// text, the validator never sees the per-skill shape.
pub struct MofaDescribeContentTypeTool {
    catalog: Mutex<CatalogInner>,
    registry: Mutex<Option<Weak<ToolRegistry>>>,
}

impl Default for MofaDescribeContentTypeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MofaDescribeContentTypeTool {
    pub fn new() -> Self {
        Self {
            catalog: Mutex::new(CatalogInner::default()),
            registry: Mutex::new(None),
        }
    }

    pub fn set_registry(&self, weak: Weak<ToolRegistry>) {
        *self.registry.lock().unwrap_or_else(|e| e.into_inner()) = Some(weak);
    }

    pub fn register_or_replace(&self, entry: MakeTypeEntry) {
        let mut catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = catalog
            .entries
            .iter_mut()
            .find(|e| e.content_type == entry.content_type)
        {
            *existing = entry;
        } else {
            catalog.entries.push(entry);
        }
    }

    /// Replace the entire catalog with the given list. Mirrors
    /// [`MofaMakeTool::replace_entries`] — used by
    /// `ToolRegistry::retain` to keep the describe tool in sync with
    /// the dispatcher when target tools are evicted.
    pub fn replace_entries(&self, entries: Vec<MakeTypeEntry>) {
        let mut catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        catalog.entries = entries;
    }

    /// Snapshot the catalog. Mirrors [`MofaMakeTool::entries`].
    /// `pub` so per-turn snapshot paths can copy catalog entries when
    /// minting fresh tool instances (RFC-1 fixup, codex P2).
    pub fn entries(&self) -> Vec<MakeTypeEntry> {
        self.catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .clone()
    }
}

#[async_trait]
impl Tool for MofaDescribeContentTypeTool {
    fn name(&self) -> &str {
        "mofa_describe_content_type"
    }

    fn description(&self) -> &str {
        "Describe the args schema for a single mofa_make content_type. \
         Pass {content_type: \"slides\"} to see the args shape for the \
         slides generator. Returns a text description of required and \
         optional fields (NOT a JSON-Schema-validated payload, to avoid \
         the kimi-k2.6 anyOf-rejection trap)."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content_type": {
                    "type": "string",
                    "description": "The content_type to describe (e.g. \"slides\", \"podcast\", \"video\")."
                }
            },
            "required": ["content_type"]
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let content_type = args
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if content_type.is_empty() {
            return Ok(ToolResult {
                output: "missing required field `content_type` (string)".into(),
                success: false,
                ..Default::default()
            });
        }
        let entries = self.entries();
        let Some(entry) = entries.iter().find(|e| e.content_type == content_type) else {
            return Ok(ToolResult {
                output: unknown_content_type_msg(content_type, &entries),
                success: false,
                ..Default::default()
            });
        };

        // Look up the target tool's input_schema directly from the
        // registry so the description reflects whatever the loaded
        // plugin actually declared (post env-allowlist tweaks, after
        // any operator-side schema overrides).
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|w| w.upgrade());
        let Some(registry) = registry else {
            return Ok(ToolResult {
                output: format!(
                    "[DESCRIBE_ERROR] tool registry unavailable; can only \
                     report static blurb for content_type {:?}: {}",
                    entry.content_type, entry.description
                ),
                success: false,
                ..Default::default()
            });
        };

        let mut out = format!(
            "content_type: {}\nskill: {}\ntarget_tool: {}\n\n{}\n",
            entry.content_type, entry.skill_id, entry.target_tool, entry.description
        );

        if let Some(target) = registry.get(&entry.target_tool) {
            out.push_str("\nForwarded-tool description:\n");
            out.push_str(target.description());
            out.push_str("\n\nForwarded-tool input_schema (JSON):\n");
            // Pretty-print as text so the LLM consumes it without
            // having to parse it; the value is informational, not
            // validated against by the dispatcher.
            let schema = target.input_schema();
            match serde_json::to_string_pretty(&schema) {
                Ok(pretty) => out.push_str(&pretty),
                Err(_) => out.push_str(&schema.to_string()),
            }
            out.push('\n');
        } else {
            out.push_str(
                "\n[DESCRIBE_WARNING] target tool not loaded in this registry; \
                 only the static blurb above is available.\n",
            );
        }

        Ok(ToolResult {
            output: out,
            success: true,
            ..Default::default()
        })
    }
}

/// Convenience constructor: build a paired `(dispatcher, describe)` from
/// a list of `MakeTypeEntry`. Both tools share the same catalog seed —
/// callers must wrap them in `Arc` and call `set_registry` after
/// registration.
///
/// Returns `None` when `entries` is empty so callers can skip
/// registration entirely (avoids publishing a dispatcher with no
/// targets, which would just be noise in the LLM's tool list).
pub fn make_dispatcher_with_entries(
    entries: Vec<MakeTypeEntry>,
) -> Option<(MofaMakeTool, MofaDescribeContentTypeTool)> {
    if entries.is_empty() {
        return None;
    }
    let dispatcher = MofaMakeTool::new();
    let describe = MofaDescribeContentTypeTool::new();
    for entry in entries {
        dispatcher.register_or_replace(entry.clone());
        describe.register_or_replace(entry);
    }
    Some((dispatcher, describe))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tools::ToolRegistry;

    /// Trivial fake tool that records the args it was called with so
    /// dispatcher tests can assert on forwarding behaviour without
    /// needing a real PluginTool process.
    struct RecordingTool {
        name: &'static str,
        last_args: Arc<Mutex<Option<serde_json::Value>>>,
    }

    impl RecordingTool {
        fn new(name: &'static str) -> (Self, Arc<Mutex<Option<serde_json::Value>>>) {
            let last_args = Arc::new(Mutex::new(None));
            (
                Self {
                    name,
                    last_args: last_args.clone(),
                },
                last_args,
            )
        }
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "recording test tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
            *self.last_args.lock().unwrap_or_else(|e| e.into_inner()) = Some(args.clone());
            Ok(ToolResult {
                output: format!("recorded by {}", self.name),
                success: true,
                ..Default::default()
            })
        }
    }

    fn cards_entry() -> MakeTypeEntry {
        MakeTypeEntry::new(
            "cards",
            "mofa-cards",
            "mofa_cards",
            "Trading-card / greeting-card style images.",
        )
    }
    fn slides_entry() -> MakeTypeEntry {
        MakeTypeEntry::new(
            "slides",
            "mofa-slides",
            "mofa_slides",
            "PPTX presentation decks.",
        )
    }
    fn podcast_entry() -> MakeTypeEntry {
        MakeTypeEntry::new(
            "podcast",
            "mofa-podcast",
            "podcast_generate",
            "Multi-speaker podcast audio.",
        )
    }

    #[test]
    fn make_type_entry_new_carries_all_fields() {
        let e = cards_entry();
        assert_eq!(e.content_type, "cards");
        assert_eq!(e.skill_id, "mofa-cards");
        assert_eq!(e.target_tool, "mofa_cards");
        assert!(e.description.contains("Trading-card"));
    }

    #[test]
    fn register_or_replace_keys_on_content_type() {
        let tool = MofaMakeTool::new();
        tool.register_or_replace(cards_entry());
        // Replace cards with a different target (simulates per-profile
        // shadow).
        tool.register_or_replace(MakeTypeEntry::new(
            "cards",
            "mofa-cards-fork",
            "mofa_cards_fork",
            "Forked cards.",
        ));
        let entries = tool.entries();
        assert_eq!(entries.len(), 1, "replacement should not duplicate");
        assert_eq!(entries[0].skill_id, "mofa-cards-fork");
        assert_eq!(entries[0].target_tool, "mofa_cards_fork");
    }

    #[test]
    fn mofa_make_enum_populated_from_discovered_skills() {
        // Test from the task spec — the dispatcher's input_schema must
        // emit a `content_type` enum derived from the registered entries.
        let tool = MofaMakeTool::new();
        tool.register_or_replace(slides_entry());
        tool.register_or_replace(podcast_entry());
        tool.register_or_replace(cards_entry());
        let schema = tool.input_schema();
        let enum_vals = schema["properties"]["content_type"]["enum"]
            .as_array()
            .expect("content_type must declare an enum");
        let names: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["slides", "podcast", "cards"]);
    }

    #[test]
    fn mofa_make_schema_omits_enum_when_no_entries_registered() {
        // Empty enum lists trigger validator rejection on some
        // providers — fall back to a free-form string.
        let tool = MofaMakeTool::new();
        let schema = tool.input_schema();
        assert!(schema["properties"]["content_type"]["enum"].is_null());
        assert_eq!(schema["properties"]["content_type"]["type"], "string");
    }

    #[test]
    fn mofa_make_schema_is_bare_string_enum_no_anyof_oneof() {
        // Hard constraint from the task spec: the LLM-facing schema
        // MUST NOT use bare anyOf/oneOf branches without type — strict
        // provider validators (kimi-k2.6 etc) reject those.
        let tool = MofaMakeTool::new();
        tool.register_or_replace(slides_entry());
        let schema = tool.input_schema();
        // No top-level anyOf / oneOf on either the root or content_type.
        assert!(schema.get("anyOf").is_none(), "root must not declare anyOf");
        assert!(schema.get("oneOf").is_none(), "root must not declare oneOf");
        assert!(schema["properties"]["content_type"].get("anyOf").is_none());
        assert!(schema["properties"]["content_type"].get("oneOf").is_none());
        // Args is declared as a plain object (no per-skill branches).
        assert_eq!(schema["properties"]["args"]["type"], "object");
        assert!(schema["properties"]["args"].get("oneOf").is_none());
        assert!(schema["properties"]["args"].get("anyOf").is_none());
    }

    #[tokio::test]
    async fn mofa_make_returns_helpful_error_when_content_type_missing() {
        let tool = MofaMakeTool::new();
        tool.register_or_replace(slides_entry());
        // Args missing content_type — pre_flight_validate must reject.
        let err = tool
            .pre_flight_validate(&json!({"args": {}}))
            .await
            .expect_err("missing content_type must be rejected");
        assert!(
            err.contains("content_type"),
            "error must mention the field: {err}"
        );
    }

    #[tokio::test]
    async fn mofa_make_returns_helpful_error_when_content_type_unknown() {
        let tool = MofaMakeTool::new();
        tool.register_or_replace(slides_entry());
        tool.register_or_replace(cards_entry());
        let err = tool
            .pre_flight_validate(&json!({
                "content_type": "completely_made_up",
                "args": {}
            }))
            .await
            .expect_err("unknown content_type must be rejected");
        assert!(err.contains("[CONTENT_TYPE_NOT_FOUND]"));
        // Helpful error lists the known content_types so the LLM can recover.
        assert!(err.contains("slides"));
        assert!(err.contains("cards"));
    }

    #[tokio::test]
    async fn mofa_make_rejects_non_object_args() {
        let tool = MofaMakeTool::new();
        tool.register_or_replace(slides_entry());
        let err = tool
            .pre_flight_validate(&json!({
                "content_type": "slides",
                "args": "not an object"
            }))
            .await
            .expect_err("non-object args must be rejected");
        assert!(err.contains("must be a JSON object"));
    }

    #[tokio::test]
    async fn mofa_make_dispatches_to_correct_skill_binary_by_content_type() {
        // Build a registry with two recording targets representing
        // mofa_slides and mofa_cards. Dispatcher must forward to the
        // correct one based on content_type.
        let mut registry = ToolRegistry::new();
        let (slides_tool, slides_args) = RecordingTool::new("mofa_slides");
        let (cards_tool, cards_args) = RecordingTool::new("mofa_cards");
        registry.register(slides_tool);
        registry.register(cards_tool);

        let dispatcher = MofaMakeTool::new();
        dispatcher.register_or_replace(slides_entry());
        dispatcher.register_or_replace(cards_entry());

        let registry_arc = Arc::new(registry);
        dispatcher.set_registry(Arc::downgrade(&registry_arc));

        // Dispatch slides.
        let result = dispatcher
            .execute(&json!({
                "content_type": "slides",
                "args": { "topic": "rust ownership", "n_slides": 5 }
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("mofa_slides"));
        let recorded = slides_args
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("slides target must have been invoked");
        assert_eq!(recorded["topic"], "rust ownership");
        assert_eq!(recorded["n_slides"], 5);
        // Cards target must NOT have been called.
        assert!(
            cards_args
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
        );

        // Dispatch cards.
        let result = dispatcher
            .execute(&json!({
                "content_type": "cards",
                "args": { "prompt": "holiday greeting" }
            }))
            .await
            .unwrap();
        assert!(result.success);
        let recorded = cards_args
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("cards target must have been invoked");
        assert_eq!(recorded["prompt"], "holiday greeting");
    }

    #[tokio::test]
    async fn mofa_make_returns_dispatcher_error_when_target_not_in_registry() {
        // Catalog has an entry but the registry doesn't have the
        // target tool — must surface a clear DISPATCHER_ERROR rather
        // than silently succeeding or panicking.
        let registry = Arc::new(ToolRegistry::new());
        let dispatcher = MofaMakeTool::new();
        dispatcher.register_or_replace(slides_entry());
        dispatcher.set_registry(Arc::downgrade(&registry));
        let result = dispatcher
            .execute(&json!({
                "content_type": "slides",
                "args": {}
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("[DISPATCHER_ERROR]"));
        assert!(result.output.contains("mofa_slides"));
    }

    #[tokio::test]
    async fn mofa_make_returns_dispatcher_error_when_registry_dropped() {
        // Loader → Arc<registry>; if the Arc has been dropped, the
        // Weak ref cannot upgrade and we must error cleanly.
        let dispatcher = MofaMakeTool::new();
        dispatcher.register_or_replace(slides_entry());
        // No set_registry call → no weak ref at all → upgrade returns None.
        let result = dispatcher
            .execute(&json!({
                "content_type": "slides",
                "args": {}
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("[DISPATCHER_ERROR]"));
    }

    #[tokio::test]
    async fn describe_content_type_returns_target_schema() {
        let mut registry = ToolRegistry::new();
        let (slides_tool, _) = RecordingTool::new("mofa_slides");
        registry.register(slides_tool);

        let describe = MofaDescribeContentTypeTool::new();
        describe.register_or_replace(slides_entry());
        let registry_arc = Arc::new(registry);
        describe.set_registry(Arc::downgrade(&registry_arc));

        let result = describe
            .execute(&json!({"content_type": "slides"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("content_type: slides"));
        assert!(result.output.contains("mofa_slides"));
        assert!(result.output.contains("PPTX presentation decks"));
        // The forwarded tool's schema must be inlined as TEXT (json
        // pretty-printed) — NOT validated as a JSON-Schema branch.
        assert!(result.output.contains("input_schema"));
    }

    #[tokio::test]
    async fn describe_content_type_rejects_unknown_type() {
        let describe = MofaDescribeContentTypeTool::new();
        describe.register_or_replace(slides_entry());
        let registry = Arc::new(ToolRegistry::new());
        describe.set_registry(Arc::downgrade(&registry));
        let result = describe
            .execute(&json!({"content_type": "made_up"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("[CONTENT_TYPE_NOT_FOUND]"));
    }

    #[test]
    fn make_dispatcher_with_entries_returns_none_for_empty_input() {
        // When the loader discovers zero mofa-* skills, we should
        // NOT publish a dispatcher tool — otherwise the LLM sees a
        // tool with no usable content_type, which clutters the spec
        // and degrades model performance.
        assert!(make_dispatcher_with_entries(vec![]).is_none());
    }

    #[test]
    fn make_dispatcher_with_entries_seeds_both_tools() {
        let (dispatcher, describe) =
            make_dispatcher_with_entries(vec![slides_entry(), cards_entry()])
                .expect("should mint dispatcher pair");
        let d_entries = dispatcher.entries();
        let desc_entries = describe.entries();
        assert_eq!(d_entries.len(), 2);
        assert_eq!(desc_entries.len(), 2);
        // Same content_type ordering on both.
        assert_eq!(d_entries[0].content_type, desc_entries[0].content_type);
        assert_eq!(d_entries[1].content_type, desc_entries[1].content_type);
    }

    /// RFC-1 task-spec test: after the loader hides target tools via
    /// `defer`, they MUST NOT appear in `specs()` (which is what gets
    /// shipped to the LLM). But `get(name)` MUST still succeed so the
    /// dispatcher / internal gateway can forward to them.
    #[test]
    fn individual_mofa_tools_not_in_llm_visible_registry_and_still_callable() {
        let mut registry = ToolRegistry::new();
        let (slides_tool, _) = RecordingTool::new("mofa_slides");
        let (cards_tool, _) = RecordingTool::new("mofa_cards");
        registry.register(slides_tool);
        registry.register(cards_tool);

        // Simulate what the loader does for every make_type entry.
        registry.mark_internal_hidden("mofa_slides");
        registry.mark_internal_hidden("mofa_cards");

        // LLM-visible specs MUST NOT include the target tools.
        let visible: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert!(
            !visible.contains(&"mofa_slides".to_string()),
            "mofa_slides must NOT appear in LLM-visible specs after defer; got {:?}",
            visible
        );
        assert!(
            !visible.contains(&"mofa_cards".to_string()),
            "mofa_cards must NOT appear in LLM-visible specs after defer; got {:?}",
            visible
        );

        // BUT `registry.get(name)` must still resolve — that's how the
        // dispatcher forwards, and that's how the gateway path can
        // call these tools internally without LLM cooperation.
        assert!(
            registry.get("mofa_slides").is_some(),
            "mofa_slides must remain callable via get() after defer"
        );
        assert!(
            registry.get("mofa_cards").is_some(),
            "mofa_cards must remain callable via get() after defer"
        );
    }

    /// Per-profile shadowing requirement (per
    /// `feedback_profile_scoped_skill_shadow.md`): when the loader
    /// registers two skills with the same content_type (global +
    /// per-profile), the per-profile one MUST win at `mofa_make`
    /// dispatch time. The aggregator's `register_or_replace` is the
    /// linchpin — verify end-to-end through the catalog seed AND
    /// the spec description.
    #[test]
    fn make_dispatcher_per_profile_shadow_replaces_global() {
        let dispatcher = MofaMakeTool::new();
        // First registration: simulates the global skill.
        dispatcher.register_or_replace(MakeTypeEntry::new(
            "slides",
            "mofa-slides-global",
            "mofa_slides",
            "Global slides description.",
        ));
        // Second registration with same content_type: simulates the
        // per-profile skill loaded later (e.g. <profile>/data/skills/).
        dispatcher.register_or_replace(MakeTypeEntry::new(
            "slides",
            "mofa-slides-profile",
            "mofa_slides_profile_fork",
            "Per-profile slides description.",
        ));
        let entries = dispatcher.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].skill_id, "mofa-slides-profile");
        assert_eq!(entries[0].target_tool, "mofa_slides_profile_fork");
        // The spec description must reflect the per-profile blurb.
        let desc = dispatcher.description();
        assert!(
            desc.contains("Per-profile slides description"),
            "dispatcher description must surface the shadowing entry: {}",
            desc
        );
        assert!(
            !desc.contains("Global slides description"),
            "dispatcher description must NOT carry the shadowed entry: {}",
            desc
        );
    }
}
