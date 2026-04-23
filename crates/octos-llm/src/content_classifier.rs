//! Content-classified smart model routing (M6.6).
//!
//! A **pure** heuristic classifier that inspects the latest user turn and
//! returns a [`ModelTier`] — `Cheap` for short plain messages, `Strong` when
//! strong keywords, code fences, or long messages suggest a harder task.
//!
//! The classifier is deliberately:
//!
//! - **Pure**: no I/O, no LLM calls, deterministic for a given input.
//! - **Safe**: keyword matching is word-boundary-aware and case-insensitive,
//!   so `"debugger"` does not match the `"debug"` keyword.
//! - **Toggleable**: when [`RoutingConfig::enabled`] is `false`, every request
//!   returns [`ModelTier::Strong`] so the upstream router keeps its existing
//!   behavior (invariant #2 of issue #493).
//!
//! The per-decision payload is stable wire format for the
//! `octos.harness.event.v1 { kind: "routing.decision", tier, reasons }`
//! event — see [`HarnessRoutingDecisionPayload`].

use serde::{Deserialize, Serialize};

use crate::adaptive::ModelType;

/// Abstract routing tier used by the content classifier.
///
/// `Cheap` maps to the fast/cheap lane; `Strong` maps to the large model
/// lane. Keeping this a dedicated enum (rather than reusing [`ModelType`])
/// preserves the classifier's public vocabulary even if the underlying
/// router changes its capability labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Cheap,
    Strong,
}

impl ModelTier {
    /// Stable lowercase name used in harness events and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelTier::Cheap => "cheap",
            ModelTier::Strong => "strong",
        }
    }

    /// Map the tier to the adaptive router's [`ModelType`] capability label.
    pub fn to_model_type(self) -> ModelType {
        match self {
            ModelTier::Cheap => ModelType::Fast,
            ModelTier::Strong => ModelType::Strong,
        }
    }
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed routing config persisted per-profile (M4.6).
///
/// Missing config defaults to `enabled: false` (invariant #3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// Master switch. `false` (the default) leaves the router unchanged —
    /// every turn is classified as Strong so no cheap lane is taken.
    #[serde(default)]
    pub enabled: bool,

    /// Minimum character length that alone promotes a turn to Strong.
    #[serde(default = "default_min_strong_length")]
    pub min_strong_length: usize,

    /// Keywords that, when present as whole words, promote a turn to Strong.
    /// Matching is case-insensitive and word-boundary-aware.
    #[serde(default = "default_strong_keywords")]
    pub strong_keywords: Vec<String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_strong_length: default_min_strong_length(),
            strong_keywords: default_strong_keywords(),
        }
    }
}

fn default_min_strong_length() -> usize {
    400
}

fn default_strong_keywords() -> Vec<String> {
    vec![
        "debug".into(),
        "refactor".into(),
        "architecture".into(),
        "prove".into(),
        "proof".into(),
        "analyze".into(),
        "design".into(),
    ]
}

/// Per-decision payload surfaced to callers and harness events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassificationDecision {
    /// Tier chosen for this turn.
    pub tier: ModelTier,
    /// Ordered human-readable reasons (stable labels like `"keyword:debug"`).
    pub reasons: Vec<String>,
    /// Length of the classified input in chars.
    pub input_chars: usize,
}

impl ClassificationDecision {
    /// Build the stable `routing.decision` harness event payload.
    pub fn harness_event_payload(&self) -> HarnessRoutingDecisionPayload {
        HarnessRoutingDecisionPayload {
            kind: "routing.decision",
            tier: self.tier.as_str(),
            reasons: self.reasons.clone(),
            input_chars: self.input_chars,
        }
    }
}

/// Stable serialization shape for the harness `routing.decision` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarnessRoutingDecisionPayload {
    /// Always `"routing.decision"`.
    pub kind: &'static str,
    /// Lowercase tier name, `"cheap"` or `"strong"`.
    pub tier: &'static str,
    pub reasons: Vec<String>,
    pub input_chars: usize,
}

/// Pure heuristic classifier. Construction is cheap — there's no
/// persistent state beyond the config.
#[derive(Debug, Clone)]
pub struct ContentClassifier {
    config: RoutingConfig,
}

impl ContentClassifier {
    pub fn new(config: RoutingConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &RoutingConfig {
        &self.config
    }

    /// Classify a single input string. Deterministic and side-effect free.
    pub fn classify(&self, input: &str) -> ClassificationDecision {
        let input_chars = input.chars().count();

        if !self.config.enabled {
            // Invariant #2 / #3 — disabled or absent config keeps the router's
            // current behavior (everything routes Strong).
            return ClassificationDecision {
                tier: ModelTier::Strong,
                reasons: vec!["disabled".into()],
                input_chars,
            };
        }

        let mut reasons: Vec<String> = Vec::new();
        let mut tier = ModelTier::Cheap;

        // Code fences are a strong signal.
        if contains_code_fence(input) {
            reasons.push("code_fence".into());
            tier = ModelTier::Strong;
        }

        // Length-based escalation.
        if input_chars >= self.config.min_strong_length {
            reasons.push(format!("length>={}", self.config.min_strong_length));
            tier = ModelTier::Strong;
        }

        // Whole-word keyword match (case-insensitive).
        if let Some(hit) = first_keyword_match(input, &self.config.strong_keywords) {
            reasons.push(format!("keyword:{hit}"));
            tier = ModelTier::Strong;
        }

        // URLs are surfaced as a reason but are not themselves sufficient
        // to upgrade tier — they often appear in "open this link" turns that
        // do not need a strong model.
        if contains_url(input) {
            reasons.push("url".into());
        }

        if reasons.is_empty() {
            reasons.push("default_cheap".into());
        }

        ClassificationDecision {
            tier,
            reasons,
            input_chars,
        }
    }
}

fn contains_code_fence(input: &str) -> bool {
    // Matches "```" anywhere. Also recognizes "~~~" fences (markdown allows
    // them as a code-fence delimiter).
    input.contains("```") || input.contains("~~~")
}

fn contains_url(input: &str) -> bool {
    // Cheap, dependency-free URL signal. No network calls, no allocations
    // beyond the borrowed input.
    let lower = input.to_ascii_lowercase();
    lower.contains("http://") || lower.contains("https://")
}

/// Return the first keyword that matches `input` as a whole-word,
/// case-insensitive substring. Word boundaries are non-alphanumeric chars
/// (so `"debugger"` never matches `"debug"`).
fn first_keyword_match<'a>(input: &str, keywords: &'a [String]) -> Option<&'a str> {
    if keywords.is_empty() {
        return None;
    }
    let haystack = input.to_ascii_lowercase();
    for kw in keywords {
        if kw.is_empty() {
            continue;
        }
        let needle = kw.to_ascii_lowercase();
        if is_word_boundary_match(&haystack, &needle) {
            return Some(kw.as_str());
        }
    }
    None
}

/// True if `needle` appears in `haystack` bounded on both sides by a
/// non-alphanumeric character (or start/end of string).
fn is_word_boundary_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let nlen = needle.len();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let absolute = start + pos;
        let before_ok = absolute == 0 || !is_word_char_byte(bytes[absolute - 1]);
        let end = absolute + nlen;
        let after_ok = end == bytes.len() || !is_word_char_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        start = absolute + 1;
    }
    false
}

fn is_word_char_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> RoutingConfig {
        RoutingConfig {
            enabled,
            min_strong_length: 200,
            strong_keywords: vec!["debug".into(), "refactor".into()],
        }
    }

    #[test]
    fn default_routing_config_is_disabled() {
        assert!(!RoutingConfig::default().enabled);
    }

    #[test]
    fn classifier_is_pure_same_input_same_output() {
        let c = ContentClassifier::new(cfg(true));
        let a = c.classify("please debug this");
        let b = c.classify("please debug this");
        assert_eq!(a, b);
    }

    #[test]
    fn word_boundary_match_rejects_substrings() {
        assert!(is_word_boundary_match("please debug this", "debug"));
        assert!(!is_word_boundary_match("this debugger rocks", "debug"));
        assert!(is_word_boundary_match("debug", "debug"));
        assert!(!is_word_boundary_match("prelude", "prel"));
    }

    #[test]
    fn tier_str_labels_are_stable() {
        assert_eq!(ModelTier::Cheap.as_str(), "cheap");
        assert_eq!(ModelTier::Strong.as_str(), "strong");
    }

    #[test]
    fn model_tier_maps_to_adaptive_model_type() {
        assert_eq!(ModelTier::Cheap.to_model_type(), ModelType::Fast);
        assert_eq!(ModelTier::Strong.to_model_type(), ModelType::Strong);
    }

    #[test]
    fn disabled_config_always_returns_strong() {
        let c = ContentClassifier::new(cfg(false));
        let d = c.classify("hi");
        assert_eq!(d.tier, ModelTier::Strong);
        assert!(d.reasons.iter().any(|r| r == "disabled"));
    }

    #[test]
    fn code_fence_upgrades_tier() {
        let c = ContentClassifier::new(cfg(true));
        let d = c.classify("quick\n```\nfoo\n```\n");
        assert_eq!(d.tier, ModelTier::Strong);
        assert!(d.reasons.iter().any(|r| r == "code_fence"));
    }

    #[test]
    fn harness_payload_shape_is_stable() {
        let c = ContentClassifier::new(cfg(true));
        let d = c.classify("please refactor this very long ".repeat(20).as_str());
        let p = d.harness_event_payload();
        assert_eq!(p.kind, "routing.decision");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["kind"], "routing.decision");
        assert!(json["reasons"].is_array());
    }
}
