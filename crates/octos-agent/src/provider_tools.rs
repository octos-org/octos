//! Provider-aligned toolset configuration.
//!
//! Different LLM providers work better with different tool configurations.
//! This module maps provider names to recommended tool adjustments:
//! - OpenAI: prefers `apply_patch` over `edit_file`, `multi_tool_use`
//! - Gemini: prefers `read_many_files` batched reads, larger context
//! - Anthropic: prefers `edit_file` with search/replace, `diff_edit`
//!
//! TODO: Wire into agent loop to auto-select toolsets based on active provider.

use std::collections::HashMap;

/// A tool adjustment for a specific provider.
#[derive(Debug, Clone, Default)]
pub struct ToolAdjustment {
    /// Tools to prefer (promote in ordering / system prompt).
    pub prefer: Vec<String>,
    /// Tools to demote or hide (still available but not advertised).
    pub demote: Vec<String>,
    /// Tool aliases (e.g. "apply_patch" -> "edit_file" for OpenAI).
    pub aliases: HashMap<String, String>,
    /// Extra tools to register for this provider.
    pub extras: Vec<String>,
}

/// Registry of provider-specific tool adjustments.
pub struct ProviderToolsets {
    adjustments: HashMap<String, ToolAdjustment>,
}

impl ProviderToolsets {
    pub fn new() -> Self {
        Self {
            adjustments: HashMap::new(),
        }
    }

    /// Register a tool adjustment for a provider.
    pub fn register(&mut self, provider: &str, adjustment: ToolAdjustment) {
        self.adjustments.insert(provider.to_string(), adjustment);
    }

    /// Get the adjustment for a provider (if any).
    pub fn get(&self, provider: &str) -> Option<&ToolAdjustment> {
        self.adjustments.get(provider)
    }

    /// Create with default provider-specific adjustments.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();

        registry.register(
            "openai",
            ToolAdjustment {
                prefer: vec!["write_file".into(), "glob".into(), "grep".into()],
                demote: vec!["diff_edit".into()],
                aliases: HashMap::new(),
                extras: vec![],
            },
        );

        registry.register(
            "anthropic",
            ToolAdjustment {
                prefer: vec!["edit_file".into(), "diff_edit".into(), "read_file".into()],
                demote: vec![],
                aliases: HashMap::new(),
                extras: vec![],
            },
        );

        registry.register(
            "google",
            ToolAdjustment {
                prefer: vec!["read_file".into(), "glob".into(), "grep".into()],
                demote: vec!["diff_edit".into()],
                aliases: HashMap::new(),
                extras: vec![],
            },
        );

        registry
    }

    /// Get preferred tool ordering for a provider.
    /// Returns tool names that should appear first in the tools list.
    pub fn preferred_order(&self, provider: &str) -> Vec<&str> {
        self.adjustments
            .get(provider)
            .map(|a| a.prefer.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }

    /// Get demoted tools for a provider (tools to hide from prominent listing).
    pub fn demoted(&self, provider: &str) -> Vec<&str> {
        self.adjustments
            .get(provider)
            .map(|a| a.demote.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }
}

impl Default for ProviderToolsets {
    fn default() -> Self {
        Self::new()
    }
}
