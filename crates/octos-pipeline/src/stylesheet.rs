//! Model stylesheet: CSS-like LLM configuration by selector.
//!
//! Allows pipeline-level model defaults to be overridden per-node using
//! handler-kind or node-id selectors. Selectors are evaluated in order;
//! most-specific match wins.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::graph::HandlerKind;

/// A single stylesheet rule that maps a selector to model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleRule {
    /// Selector: `"*"` (all), `"handler:codergen"`, `"handler:shell"`, or `"node:my_node_id"`.
    pub selector: String,
    /// Model key to use (e.g. "cheap", "strong", "claude-sonnet-4-20250514").
    pub model: Option<String>,
    /// Max tokens override.
    pub max_tokens: Option<u32>,
    /// Temperature override.
    pub temperature: Option<f32>,
}

/// A collection of style rules applied to a pipeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStylesheet {
    pub rules: Vec<StyleRule>,
}

/// Resolved model configuration for a specific node.
#[derive(Debug, Clone, Default)]
pub struct ResolvedStyle {
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl ModelStylesheet {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a rule.
    pub fn add_rule(&mut self, rule: StyleRule) {
        self.rules.push(rule);
    }

    /// Resolve the effective style for a given node.
    /// Rules are applied in order; later rules override earlier ones.
    /// Specificity: `node:id` > `handler:kind` > `*`.
    pub fn resolve(&self, node_id: &str, handler: &HandlerKind) -> ResolvedStyle {
        let handler_str = handler_kind_str(handler);
        let mut result = ResolvedStyle::default();

        // Group by specificity: apply wildcards first, then handler, then node
        let mut wildcard_rules = Vec::new();
        let mut handler_rules = Vec::new();
        let mut node_rules = Vec::new();

        for rule in &self.rules {
            match classify_selector(&rule.selector) {
                SelectorKind::Wildcard => wildcard_rules.push(rule),
                SelectorKind::Handler(h) if h == handler_str => handler_rules.push(rule),
                SelectorKind::Node(id) if id == node_id => node_rules.push(rule),
                _ => {} // non-matching selectors
            }
        }

        for rule in wildcard_rules
            .iter()
            .chain(&handler_rules)
            .chain(&node_rules)
        {
            if let Some(ref m) = rule.model {
                result.model = Some(m.clone());
            }
            if let Some(t) = rule.max_tokens {
                result.max_tokens = Some(t);
            }
            if let Some(t) = rule.temperature {
                result.temperature = Some(t);
            }
        }

        result
    }

    /// Parse from a map: `{ "handler:codergen": { model: "strong" } }`.
    /// Uses `BTreeMap` for deterministic rule ordering (sorted by selector).
    pub fn from_map(map: &BTreeMap<String, HashMap<String, String>>) -> Self {
        let mut stylesheet = Self::new();
        for (selector, props) in map {
            stylesheet.add_rule(StyleRule {
                selector: selector.clone(),
                model: props.get("model").cloned(),
                max_tokens: props.get("max_tokens").and_then(|s| s.parse().ok()),
                temperature: props.get("temperature").and_then(|s| s.parse().ok()),
            });
        }
        stylesheet
    }
}

#[derive(Debug)]
enum SelectorKind<'a> {
    Wildcard,
    Handler(&'a str),
    Node(&'a str),
    Other,
}

fn classify_selector(s: &str) -> SelectorKind<'_> {
    if s == "*" {
        SelectorKind::Wildcard
    } else if let Some(h) = s.strip_prefix("handler:") {
        SelectorKind::Handler(h)
    } else if let Some(n) = s.strip_prefix("node:") {
        SelectorKind::Node(n)
    } else {
        SelectorKind::Other
    }
}

fn handler_kind_str(kind: &HandlerKind) -> &'static str {
    match kind {
        HandlerKind::Codergen => "codergen",
        HandlerKind::Shell => "shell",
        HandlerKind::Gate => "gate",
        HandlerKind::Noop => "noop",
        HandlerKind::Parallel => "parallel",
        HandlerKind::DynamicParallel => "dynamic_parallel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_resolve_wildcard() {
        let mut ss = ModelStylesheet::new();
        ss.add_rule(StyleRule {
            selector: "*".into(),
            model: Some("cheap".into()),
            max_tokens: Some(4096),
            temperature: None,
        });

        let style = ss.resolve("any_node", &HandlerKind::Codergen);
        assert_eq!(style.model.as_deref(), Some("cheap"));
        assert_eq!(style.max_tokens, Some(4096));
    }

    #[test]
    fn should_resolve_handler_over_wildcard() {
        let mut ss = ModelStylesheet::new();
        ss.add_rule(StyleRule {
            selector: "*".into(),
            model: Some("cheap".into()),
            max_tokens: None,
            temperature: None,
        });
        ss.add_rule(StyleRule {
            selector: "handler:codergen".into(),
            model: Some("strong".into()),
            max_tokens: None,
            temperature: None,
        });

        let style = ss.resolve("analyze", &HandlerKind::Codergen);
        assert_eq!(style.model.as_deref(), Some("strong"));

        // Shell nodes still get wildcard
        let shell_style = ss.resolve("build", &HandlerKind::Shell);
        assert_eq!(shell_style.model.as_deref(), Some("cheap"));
    }

    #[test]
    fn should_resolve_node_over_handler() {
        let mut ss = ModelStylesheet::new();
        ss.add_rule(StyleRule {
            selector: "handler:codergen".into(),
            model: Some("cheap".into()),
            max_tokens: None,
            temperature: None,
        });
        ss.add_rule(StyleRule {
            selector: "node:critical_analysis".into(),
            model: Some("strong".into()),
            max_tokens: Some(8192),
            temperature: Some(0.2),
        });

        let style = ss.resolve("critical_analysis", &HandlerKind::Codergen);
        assert_eq!(style.model.as_deref(), Some("strong"));
        assert_eq!(style.max_tokens, Some(8192));
        assert_eq!(style.temperature, Some(0.2));

        // Other codergen nodes get handler default
        let other = ss.resolve("other_node", &HandlerKind::Codergen);
        assert_eq!(other.model.as_deref(), Some("cheap"));
    }

    #[test]
    fn should_return_empty_when_no_rules() {
        let ss = ModelStylesheet::new();
        let style = ss.resolve("node", &HandlerKind::Gate);
        assert!(style.model.is_none());
        assert!(style.max_tokens.is_none());
        assert!(style.temperature.is_none());
    }

    #[test]
    fn should_parse_from_map() {
        let mut map = BTreeMap::new();
        map.insert("*".into(), {
            let mut m = HashMap::new();
            m.insert("model".into(), "default".into());
            m
        });
        map.insert("handler:shell".into(), {
            let mut m = HashMap::new();
            m.insert("model".into(), "fast".into());
            m.insert("max_tokens".into(), "2048".into());
            m
        });

        let ss = ModelStylesheet::from_map(&map);
        assert_eq!(ss.rules.len(), 2);

        let style = ss.resolve("cmd", &HandlerKind::Shell);
        assert_eq!(style.model.as_deref(), Some("fast"));
        assert_eq!(style.max_tokens, Some(2048));
    }

    #[test]
    fn should_merge_partial_overrides() {
        let mut ss = ModelStylesheet::new();
        ss.add_rule(StyleRule {
            selector: "*".into(),
            model: Some("default".into()),
            max_tokens: Some(4096),
            temperature: Some(0.7),
        });
        ss.add_rule(StyleRule {
            selector: "node:special".into(),
            model: None, // don't override model
            max_tokens: Some(16384),
            temperature: None, // don't override temperature
        });

        let style = ss.resolve("special", &HandlerKind::Codergen);
        assert_eq!(style.model.as_deref(), Some("default")); // from wildcard
        assert_eq!(style.max_tokens, Some(16384)); // overridden
        assert_eq!(style.temperature, Some(0.7)); // from wildcard
    }
}
