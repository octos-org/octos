//! Per-topic model lane routing (RFC-3, issue #1292).
//!
//! Routes session topics (e.g. `slides:*`, `code:*`, `research:*`) to a
//! capability lane (`instruction_strong`, `code_capable`, `general`,
//! `fast_chat`), which the [`crate::AdaptiveRouter`] uses to filter
//! candidate models before lane-scoring.
//!
//! Architecture lives in the [`Lane`] enum, the topic→lane resolver
//! ([`resolve_lane_for_topic`]), and the lane→candidate-list defaults
//! ([`default_lane_candidates`]). Both can be overridden per-profile
//! by carrying [`LaneRoutingConfig`] off [`crate::AdaptiveRouter`]'s
//! task-local scope ([`LANE_CONTEXT`]).
//!
//! # Backward compatibility
//!
//! Profiles without a `topic_lanes` block (the M9 status quo) see no
//! behavior change: `resolve_lane_for_topic` returns `None` for any
//! topic that doesn't carry one of the registered prefixes, and the
//! [`AdaptiveRouter::select_provider`](crate::AdaptiveRouter)
//! lane-filter is a no-op when [`LANE_CONTEXT`] is unset. The hot
//! `chat()` path still selects via priority + circuit-breaker logic
//! exactly as before for those callers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Capability lanes used to group models by aptitude on different task
/// classes. Each lane maps to an ordered list of `(provider, model_id)`
/// candidates via [`default_lane_candidates`] or
/// [`LaneRoutingConfig::lane_models`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    /// Models tuned for following long, structured prompts (slides
    /// builders, site builders, research synthesis pipelines). High
    /// instruction-following matters more than raw speed.
    InstructionStrong,
    /// Models specialized for code generation / refactoring / debugging.
    /// Code-specific fine-tunes outscore general chat models here.
    CodeCapable,
    /// Default lane. Falls through to the profile's configured model
    /// (existing behavior pre-RFC-3). Treat as "no preference".
    General,
    /// Cheap, fast models for short turns / one-shot Q&A. Used by
    /// default-chat sessions where latency dominates quality.
    FastChat,
}

impl Lane {
    /// Stable lowercase identifier used in profile config JSON
    /// (`lane_models["instruction_strong"]`) and for telemetry.
    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::InstructionStrong => "instruction_strong",
            Lane::CodeCapable => "code_capable",
            Lane::General => "general",
            Lane::FastChat => "fast_chat",
        }
    }

    /// Parse from the same lowercase identifier emitted by [`Self::as_str`].
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "instruction_strong" => Some(Lane::InstructionStrong),
            "code_capable" => Some(Lane::CodeCapable),
            "general" => Some(Lane::General),
            "fast_chat" => Some(Lane::FastChat),
            _ => None,
        }
    }
}

/// Built-in defaults: lane → ordered list of `(provider_name, model_id)`
/// candidates. The router filters its slots to this list (matching by
/// `provider_name() == provider && model_id() == model`) before
/// scoring. When the filter produces zero matches, the lane is treated
/// as inactive and the router falls back to its full slot list — i.e.
/// the existing pre-RFC-3 behavior. This guarantees built-in defaults
/// never make a profile worse off.
///
/// `InstructionStrong`, `CodeCapable`, and `FastChat` carry opinionated
/// lists. `General` returns an empty vec on purpose — its semantics are
/// "no filter; use the profile-default chain".
///
/// # Production fleet coverage (RFC-3 follow-up)
///
/// The dspfac fleet (mini1/3/5) runs `moonshot` + `deepseek` providers
/// — NOT `wisemodel`. The 2026-05-25 round-14 retest confirmed the
/// `provider_name == candidate_provider` filter found zero matches and
/// fell through to the per-profile primary (`deepseek-v4-pro`), which
/// doesn't follow long structured prompts strongly enough to invoke
/// `mofa_slides`. Production providers are now pinned into each
/// opinionated lane so RFC-3 is no longer inert on the live fleet.
/// `wisemodel` entries are kept for backward compatibility with
/// non-fleet profiles. Order: `anthropic` first (premium user pick),
/// then production providers (fleet match), then legacy compat.
pub fn default_lane_candidates(lane: Lane) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match lane {
        Lane::InstructionStrong => &[
            ("anthropic", "claude-sonnet-4-6"),
            // Production fleet declared primary (mini3/5). k2.5 is the
            // stronger instruction-following model in the kimi family;
            // pin it AHEAD of k2.6 so `select_provider` in Off/Hedge
            // mode (which walks the lane-eligible list in candidate
            // order) picks k2.5 first when both are configured. Codex
            // round-2 P2 flagged that reversing this would route mini3/5
            // turns to the faster fallback instead of the operator's
            // declared primary.
            ("moonshot", "kimi-k2.5"),
            // Production fleet fallback observed on 2026-05-25 mini1
            // round-14 retest (primary `deepseek-v4-pro`, fallback
            // `moonshot/kimi-k2.6`). MUST be present so the mini1 chain
            // matches; ordered AFTER k2.5 per the precedence above.
            ("moonshot", "kimi-k2.6"),
            // Legacy compat: non-fleet profiles routing via wisemodel.
            ("wisemodel", "kimi-k2.5"),
            ("openai", "gpt-4.1"),
        ],
        Lane::CodeCapable => &[
            ("anthropic", "claude-sonnet-4-6"),
            ("openai", "gpt-4.1"),
            ("deepseek", "deepseek-coder"),
            // Production fleet fallback: k2.5 is decent at code too.
            ("moonshot", "kimi-k2.5"),
        ],
        Lane::General => &[],
        Lane::FastChat => &[
            // Production fleet primary for short-turn chat.
            ("moonshot", "kimi-k2.6"),
            // Legacy compat: non-fleet profiles routing via wisemodel.
            ("wisemodel", "kimi-k2.6"),
            ("deepseek", "deepseek-chat"),
            ("openai", "gpt-4o-mini"),
        ],
    };
    pairs
        .iter()
        .map(|(p, m)| ((*p).to_string(), (*m).to_string()))
        .collect()
}

/// Built-in topic-prefix → lane mapping. The prefix is the substring
/// before the first whitespace/colon in `session.topic()`:
/// `slides:fastchain-demo` → `slides`, `code: refactor` → `code`,
/// `research/2026-q2` → `research` (we accept `/` as a separator too
/// because the slides bus uses it internally).
///
/// Unknown prefixes (or no topic at all) return `None`, which
/// [`resolve_lane_for_topic`] interprets as "fall through to General"
/// — i.e. no lane filter. This is the backward-compat path.
fn default_topic_prefix_lane(prefix: &str) -> Option<Lane> {
    match prefix {
        "slides" => Some(Lane::InstructionStrong),
        "site" => Some(Lane::InstructionStrong),
        "podcast" => Some(Lane::InstructionStrong),
        "research" => Some(Lane::InstructionStrong),
        "code" => Some(Lane::CodeCapable),
        _ => None,
    }
}

/// Extract the topic prefix used for lane resolution. Splits on the
/// first occurrence of `:`, `/`, ` ` (space), or `\t` (tab). An empty
/// topic returns `""`.
///
/// Examples:
/// - `"slides:fastchain-demo"` → `"slides"`
/// - `"slides fastchain"` → `"slides"`
/// - `"code/refactor"` → `"code"`
/// - `"plain-chat"` → `"plain-chat"` (no separator)
pub fn topic_prefix(topic: &str) -> &str {
    let end = topic
        .find(|c: char| c == ':' || c == '/' || c.is_whitespace())
        .unwrap_or(topic.len());
    &topic[..end]
}

/// Per-profile override of lane defaults. Both fields are optional;
/// when absent or empty the built-in defaults in this module apply.
///
/// Serialized in the profile config as:
///
/// ```json
/// {
///     "topic_lanes": { "slides": "instruction_strong", "blog": "code_capable" },
///     "lane_models": {
///         "instruction_strong": [["anthropic", "claude-sonnet-4-6"]]
///     }
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LaneRoutingConfig {
    /// Topic-prefix overrides. Keys are bare prefixes (no `:` or `*`),
    /// values are lane identifiers (`instruction_strong`, etc.).
    /// Unknown lane strings are ignored at resolve time.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub topic_lanes: HashMap<String, Lane>,
    /// Lane → candidate list overrides. When a lane has an explicit
    /// (possibly empty) entry, that list wins outright over
    /// [`default_lane_candidates`] for that lane. Use an empty list to
    /// say "this lane is intentionally unconfigured" — the router will
    /// then fall through to the profile default.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub lane_models: HashMap<Lane, Vec<(String, String)>>,
}

impl LaneRoutingConfig {
    /// Resolve a topic to its lane using this config as the override
    /// layer over the built-in defaults. Profile config wins, then
    /// built-in defaults, then fall through to `Lane::General`.
    pub fn resolve_lane(&self, topic: Option<&str>) -> Lane {
        let Some(topic) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
            return Lane::General;
        };
        let prefix = topic_prefix(topic);
        if let Some(lane) = self.topic_lanes.get(prefix).copied() {
            return lane;
        }
        default_topic_prefix_lane(prefix).unwrap_or(Lane::General)
    }

    /// Resolve a lane to its candidate `(provider, model)` list. When
    /// the profile config carries a non-empty entry for `lane`, that
    /// wins; otherwise the built-in defaults apply.
    pub fn candidates_for_lane(&self, lane: Lane) -> Vec<(String, String)> {
        if let Some(entries) = self.lane_models.get(&lane) {
            if !entries.is_empty() {
                return entries.clone();
            }
        }
        default_lane_candidates(lane)
    }
}

/// Standalone resolver for callers that don't have a profile config
/// in scope. Identical to `LaneRoutingConfig::default().resolve_lane`.
pub fn resolve_lane_for_topic(topic: Option<&str>) -> Lane {
    LaneRoutingConfig::default().resolve_lane(topic)
}

/// Per-turn lane context. Passed by the session-actor / WS handler
/// into the chat() task-local so [`crate::AdaptiveRouter::select_provider`]
/// can filter its slots to the lane's candidate list before scoring.
#[derive(Debug, Clone, Default)]
pub struct LaneContext {
    /// The lane selected for this turn. `None` (or `Some(Lane::General)`)
    /// means "no filter — use the profile-default chain".
    pub lane: Option<Lane>,
    /// Profile-level lane overrides (topic→lane and lane→models).
    /// `None` means "use the built-in defaults". Carried alongside the
    /// lane itself so the router doesn't need to consult any profile
    /// state — keeping the lane filter agnostic of the CLI crate.
    pub config: Option<LaneRoutingConfig>,
}

impl LaneContext {
    /// Build a context from a session topic + optional profile config.
    /// Use this at the session-actor / WS handler call site to bake
    /// both inputs (topic, profile override) into the one task-local.
    pub fn for_topic(topic: Option<&str>, config: Option<&LaneRoutingConfig>) -> Self {
        let resolved = match config {
            Some(cfg) => cfg.resolve_lane(topic),
            None => resolve_lane_for_topic(topic),
        };
        Self {
            lane: Some(resolved),
            config: config.cloned(),
        }
    }

    /// Return the ordered list of `(provider, model)` candidates for
    /// the resolved lane. Returns an empty vec for `General` or when
    /// `lane` is `None` — both meaning "no filter".
    pub fn candidates(&self) -> Vec<(String, String)> {
        let Some(lane) = self.lane else {
            return Vec::new();
        };
        if lane == Lane::General {
            return Vec::new();
        }
        match self.config {
            Some(ref cfg) => cfg.candidates_for_lane(lane),
            None => default_lane_candidates(lane),
        }
    }
}

tokio::task_local! {
    /// Per-turn lane scope read by [`crate::AdaptiveRouter::select_provider`].
    /// Defaults to [`LaneContext::default()`] (no lane, no filter) when
    /// the caller hasn't wrapped the chat() future. Mirrors
    /// [`crate::adaptive::ROUTER_CONTEXT`] in shape.
    pub static LANE_CONTEXT: LaneContext;
}

/// Run `fut` inside a [`LANE_CONTEXT`] scope. The session-actor and
/// the WS turn handler wrap `provider.chat()` / `process_message()`
/// with this so the router sees the resolved lane.
pub async fn with_lane_context<F, T>(ctx: LaneContext, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    LANE_CONTEXT.scope(ctx, fut).await
}

/// Snapshot of the active [`LANE_CONTEXT`] for callers outside the
/// task-local scope (test paths, debug logging, etc.). Returns
/// [`LaneContext::default()`] when no scope is active.
pub fn current_lane_context() -> LaneContext {
    LANE_CONTEXT.try_with(|ctx| ctx.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Topic prefix extraction ────────────────────────────────────

    // ── Topic → lane resolution (the RFC-3 test suite) ──────────────

    // ── Profile override behavior ───────────────────────────────────

    // ── Default candidate lists smoke test ──────────────────────────

    #[test]
    fn default_candidates_match_rfc3_spec() {
        let strong = default_lane_candidates(Lane::InstructionStrong);
        assert!(
            strong
                .iter()
                .any(|(p, m)| p == "anthropic" && m == "claude-sonnet-4-6")
        );
        assert!(
            strong
                .iter()
                .any(|(p, m)| p == "wisemodel" && m == "kimi-k2.5")
        );

        let code = default_lane_candidates(Lane::CodeCapable);
        assert!(
            code.iter()
                .any(|(p, m)| p == "anthropic" && m == "claude-sonnet-4-6")
        );

        let fast = default_lane_candidates(Lane::FastChat);
        assert!(
            fast.iter()
                .any(|(p, m)| p == "wisemodel" && m == "kimi-k2.6")
        );

        // General is intentionally empty (no filter).
        assert!(default_lane_candidates(Lane::General).is_empty());
    }

    // ── Production fleet coverage (RFC-3 follow-up) ─────────────────
    //
    // The dspfac fleet (mini1/3/5) runs `moonshot` + `deepseek` providers
    // — not `wisemodel`. The 2026-05-25 round-14 retest confirmed the
    // lane filter found zero matches and fell through to the per-profile
    // primary (deepseek-v4-pro), which doesn't follow long structured
    // prompts strongly enough to invoke `mofa_slides`. These tests pin
    // the production providers into the built-in defaults so RFC-3 is
    // no longer inert on the live fleet.

    #[test]
    fn instruction_strong_orders_k25_before_k26() {
        // Codex round-2 P2: `select_provider` in Off/Hedge mode walks the
        // lane-eligible list in candidate-declaration order. If a profile
        // has BOTH moonshot/kimi-k2.5 (mini3/5 declared primary, stronger
        // instruction-following) and moonshot/kimi-k2.6 (mini1 fallback,
        // faster) in its chain, the lane filter must pick k2.5 first so
        // the operator's declared primary wins. Pin the relative order
        // of these two entries here so a future re-shuffle can't silently
        // demote the primary.
        let strong = default_lane_candidates(Lane::InstructionStrong);
        let k25_idx = strong
            .iter()
            .position(|(p, m)| p == "moonshot" && m == "kimi-k2.5")
            .expect("k2.5 must be present");
        let k26_idx = strong
            .iter()
            .position(|(p, m)| p == "moonshot" && m == "kimi-k2.6")
            .expect("k2.6 must be present");
        assert!(
            k25_idx < k26_idx,
            "moonshot/kimi-k2.5 (primary) must precede moonshot/kimi-k2.6 \
             (fallback) so Off/Hedge mode picks k2.5 when both are in the \
             chain; got k2.5_idx={k25_idx} k2.6_idx={k26_idx}, list={strong:?}",
        );
    }

    // ── LaneContext shape ───────────────────────────────────────────
}
