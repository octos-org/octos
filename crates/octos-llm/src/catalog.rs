//! Model catalog with capabilities, costs, and aliases.
//!
//! Provides programmatic model discovery: check if a model supports
//! vision, tool use, streaming, and look up cost per token.

use std::collections::HashMap;

/// Capabilities a model may support.
#[derive(Debug, Clone, Default)]
pub struct ModelCapabilities {
    pub vision: bool,
    pub tool_use: bool,
    pub streaming: bool,
    pub structured_output: bool,
    pub reasoning: bool,
}

/// Cost per million tokens (in USD).
#[derive(Debug, Clone, Default)]
pub struct ModelCost {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: Option<f64>,
}

/// Metadata for a single model.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Canonical model ID (e.g. "claude-sonnet-4-20250514").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Provider name (e.g. "anthropic", "openai").
    pub provider: String,
    /// Context window in tokens.
    pub context_window: u32,
    /// Maximum output tokens.
    pub max_output_tokens: Option<u32>,
    /// Capabilities.
    pub capabilities: ModelCapabilities,
    /// Cost per million tokens.
    pub cost: ModelCost,
    /// Alternative names / aliases (e.g. "sonnet", "strong").
    pub aliases: Vec<String>,
}

/// Registry of known models with lookup by ID or alias.
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
    index: HashMap<String, usize>,
}

impl ModelCatalog {
    pub fn new() -> Self {
        Self {
            models: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Register a model. ID and all aliases are indexed for lookup.
    pub fn register(&mut self, info: ModelInfo) {
        let idx = self.models.len();
        self.index.insert(info.id.clone(), idx);
        for alias in &info.aliases {
            self.index.insert(alias.clone(), idx);
        }
        self.models.push(info);
    }

    /// Look up a model by ID or alias.
    pub fn get(&self, id_or_alias: &str) -> Option<&ModelInfo> {
        self.index.get(id_or_alias).map(|&idx| &self.models[idx])
    }

    /// List all registered models.
    pub fn all(&self) -> &[ModelInfo] {
        &self.models
    }

    /// Find models by provider.
    pub fn by_provider(&self, provider: &str) -> Vec<&ModelInfo> {
        self.models
            .iter()
            .filter(|m| m.provider == provider)
            .collect()
    }

    /// Find models with a specific capability.
    pub fn with_capability(&self, filter: impl Fn(&ModelCapabilities) -> bool) -> Vec<&ModelInfo> {
        self.models
            .iter()
            .filter(|m| filter(&m.capabilities))
            .collect()
    }

    /// Number of registered models.
    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Create a catalog with well-known models pre-registered.
    /// Prices as of 2025-06-01. Update periodically as provider pricing changes.
    pub fn with_defaults() -> Self {
        let mut catalog = Self::new();

        catalog.register(ModelInfo {
            id: "claude-sonnet-4-20250514".into(),
            name: "Claude Sonnet 4".into(),
            provider: "anthropic".into(),
            context_window: 200_000,
            max_output_tokens: Some(16_384),
            capabilities: ModelCapabilities {
                vision: true,
                tool_use: true,
                streaming: true,
                structured_output: false,
                reasoning: true,
            },
            cost: ModelCost {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: Some(0.3),
            },
            aliases: vec!["sonnet".into(), "claude-sonnet".into()],
        });

        catalog.register(ModelInfo {
            id: "claude-haiku-4-5-20251001".into(),
            name: "Claude Haiku 4.5".into(),
            provider: "anthropic".into(),
            context_window: 200_000,
            max_output_tokens: Some(8_192),
            capabilities: ModelCapabilities {
                vision: true,
                tool_use: true,
                streaming: true,
                structured_output: false,
                reasoning: false,
            },
            cost: ModelCost {
                input_per_mtok: 0.80,
                output_per_mtok: 4.0,
                cache_read_per_mtok: Some(0.08),
            },
            aliases: vec!["haiku".into(), "claude-haiku".into(), "cheap".into()],
        });

        catalog.register(ModelInfo {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            context_window: 128_000,
            max_output_tokens: Some(16_384),
            capabilities: ModelCapabilities {
                vision: true,
                tool_use: true,
                streaming: true,
                structured_output: true,
                reasoning: false,
            },
            cost: ModelCost {
                input_per_mtok: 2.50,
                output_per_mtok: 10.0,
                cache_read_per_mtok: None,
            },
            aliases: vec!["4o".into()],
        });

        catalog.register(ModelInfo {
            id: "gemini-2.5-flash".into(),
            name: "Gemini 2.5 Flash".into(),
            provider: "google".into(),
            context_window: 1_048_576,
            max_output_tokens: Some(65_536),
            capabilities: ModelCapabilities {
                vision: true,
                tool_use: true,
                streaming: true,
                structured_output: true,
                reasoning: true,
            },
            cost: ModelCost {
                input_per_mtok: 0.15,
                output_per_mtok: 0.60,
                cache_read_per_mtok: Some(0.0375),
            },
            aliases: vec!["flash".into(), "gemini-flash".into()],
        });

        catalog
    }
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_lookup_by_id() {
        let catalog = ModelCatalog::with_defaults();
        let model = catalog.get("gpt-4o").unwrap();
        assert_eq!(model.provider, "openai");
        assert_eq!(model.context_window, 128_000);
    }

    #[test]
    fn should_lookup_by_alias() {
        let catalog = ModelCatalog::with_defaults();
        let model = catalog.get("sonnet").unwrap();
        assert_eq!(model.id, "claude-sonnet-4-20250514");

        let model = catalog.get("cheap").unwrap();
        assert_eq!(model.id, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn should_return_none_for_unknown() {
        let catalog = ModelCatalog::with_defaults();
        assert!(catalog.get("nonexistent-model").is_none());
    }

    #[test]
    fn should_filter_by_provider() {
        let catalog = ModelCatalog::with_defaults();
        let anthropic = catalog.by_provider("anthropic");
        assert_eq!(anthropic.len(), 2);
        assert!(anthropic.iter().all(|m| m.provider == "anthropic"));
    }

    #[test]
    fn should_filter_by_capability() {
        let catalog = ModelCatalog::with_defaults();
        let reasoning = catalog.with_capability(|c| c.reasoning);
        assert!(reasoning.len() >= 2); // sonnet + flash
        assert!(reasoning.iter().all(|m| m.capabilities.reasoning));
    }

    #[test]
    fn should_list_all() {
        let catalog = ModelCatalog::with_defaults();
        assert!(catalog.len() >= 4);
        assert!(!catalog.is_empty());
    }

    #[test]
    fn should_register_custom_model() {
        let mut catalog = ModelCatalog::new();
        catalog.register(ModelInfo {
            id: "custom-v1".into(),
            name: "Custom Model".into(),
            provider: "local".into(),
            context_window: 4096,
            max_output_tokens: Some(2048),
            capabilities: ModelCapabilities::default(),
            cost: ModelCost::default(),
            aliases: vec!["custom".into()],
        });

        assert_eq!(catalog.len(), 1);
        assert!(catalog.get("custom-v1").is_some());
        assert!(catalog.get("custom").is_some());
    }
}
