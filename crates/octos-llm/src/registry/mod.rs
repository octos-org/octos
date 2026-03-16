//! Provider registry — one module per provider, single lookup for construction.
//!
//! Each sub-module exports a `pub const ENTRY: ProviderEntry` and a `create()`
//! factory.  Adding a new provider = add a file + one line in `ALL`.

use std::sync::Arc;

use eyre::Result;

use crate::openai::ModelHints;
use crate::provider::LlmProvider;

// ── Provider sub-modules ────────────────────────────────────────────────────

mod anthropic;
mod dashscope;
mod deepseek;
mod gemini;
mod groq;
mod minimax;
mod moonshot;
mod nvidia;
mod ollama;
mod openai;
mod openrouter;
mod r9s;
mod vllm;
mod zai;
mod zhipu;

// ── Public types ────────────────────────────────────────────────────────────

/// Parameters passed to a provider's `create` function.
pub struct CreateParams {
    /// Resolved API key (`None` for providers that don't need one).
    pub api_key: Option<String>,
    /// Model name override (`None` → use provider default).
    pub model: Option<String>,
    /// Base URL override (`None` → use provider default).
    pub base_url: Option<String>,
    /// Config-level hint overrides (`None` → auto-detect from model name).
    pub model_hints: Option<ModelHints>,
    /// HTTP request timeout in seconds (`None` → provider default).
    pub llm_timeout_secs: Option<u64>,
    /// HTTP connect timeout in seconds (`None` → provider default).
    pub llm_connect_timeout_secs: Option<u64>,
}

impl CreateParams {
    /// Returns `(timeout_secs, connect_timeout_secs)` if either is overridden.
    pub fn http_timeout(&self) -> Option<(u64, u64)> {
        match (self.llm_timeout_secs, self.llm_connect_timeout_secs) {
            (Some(t), Some(c)) => Some((t, c)),
            (Some(t), None) => Some((t, crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS)),
            (None, Some(c)) => Some((crate::provider::DEFAULT_LLM_TIMEOUT_SECS, c)),
            (None, None) => None,
        }
    }
}

/// Static metadata + factory for one LLM provider.
pub struct ProviderEntry {
    /// Canonical name (e.g. `"deepseek"`).
    pub name: &'static str,
    /// Alternative names that also resolve to this provider.
    pub aliases: &'static [&'static str],
    /// Default model when none is specified. `None` = must be provided by user.
    pub default_model: Option<&'static str>,
    /// Environment variable holding the API key. `None` = no key required.
    pub api_key_env: Option<&'static str>,
    /// Default base URL. `None` = must be provided by user.
    pub default_base_url: Option<&'static str>,
    /// Whether construction fails without an API key.
    pub requires_api_key: bool,
    /// Whether construction fails without a base URL from config.
    pub requires_base_url: bool,
    /// Whether construction fails without a model from config.
    pub requires_model: bool,
    /// Substrings in a model name that identify this provider (for auto-detection).
    pub detect_patterns: &'static [&'static str],
    /// Factory function with full control over provider construction.
    pub create: fn(CreateParams) -> Result<Arc<dyn LlmProvider>>,
}

// ── Master list ─────────────────────────────────────────────────────────────

/// All registered providers.  Order matters for `detect_provider` — more
/// specific patterns should come before catch-all providers like groq.
static ALL: &[ProviderEntry] = &[
    anthropic::ENTRY,
    openai::ENTRY,
    gemini::ENTRY,
    r9s::ENTRY,
    openrouter::ENTRY,
    deepseek::ENTRY,
    groq::ENTRY,
    moonshot::ENTRY,
    dashscope::ENTRY,
    minimax::ENTRY,
    zhipu::ENTRY,
    zai::ENTRY,
    nvidia::ENTRY,
    ollama::ENTRY,
    vllm::ENTRY,
];

// ── Public API ──────────────────────────────────────────────────────────────

/// Look up a provider by canonical name or alias (case-insensitive).
pub fn lookup(name: &str) -> Option<&'static ProviderEntry> {
    let lower = name.to_lowercase();
    ALL.iter()
        .find(|e| e.name == lower || e.aliases.iter().any(|a| a.eq_ignore_ascii_case(&lower)))
}

/// All registered provider entries.
pub fn all_entries() -> &'static [ProviderEntry] {
    ALL
}

/// All valid provider names (canonical + aliases).
pub fn all_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    for e in ALL {
        names.push(e.name);
        names.extend_from_slice(e.aliases);
    }
    names
}

/// Infer a provider from a model name (e.g. `"claude-sonnet-4"` → `"anthropic"`).
///
/// Returns the canonical provider name, or `None` if no match.
pub fn detect_provider(model: &str) -> Option<&'static str> {
    let m = model.to_lowercase();

    // OpenAI o-series needs prefix check, not substring match.
    if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        return Some("openai");
    }

    for entry in ALL {
        for pat in entry.detect_patterns {
            if m.contains(pat) {
                return Some(entry.name);
            }
        }
    }
    None
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_by_name() {
        assert!(lookup("anthropic").is_some());
        assert!(lookup("deepseek").is_some());
        assert!(lookup("vllm").is_some());
    }

    #[test]
    fn lookup_by_alias() {
        let e = lookup("google").unwrap();
        assert_eq!(e.name, "gemini");

        let e = lookup("kimi").unwrap();
        assert_eq!(e.name, "moonshot");

        let e = lookup("qwen").unwrap();
        assert_eq!(e.name, "dashscope");

        let e = lookup("glm").unwrap();
        assert_eq!(e.name, "zhipu");

        let e = lookup("z.ai").unwrap();
        assert_eq!(e.name, "zai");

        let e = lookup("nim").unwrap();
        assert_eq!(e.name, "nvidia");

        let e = lookup("r9s").unwrap();
        assert_eq!(e.name, "r9s");

        let e = lookup("r9s.ai").unwrap();
        assert_eq!(e.name, "r9s");
    }

    #[test]
    fn lookup_case_insensitive() {
        assert!(lookup("Anthropic").is_some());
        assert!(lookup("OPENAI").is_some());
        assert!(lookup("Google").is_some());
    }

    #[test]
    fn lookup_unknown() {
        assert!(lookup("foobar").is_none());
    }

    #[test]
    fn all_entries_count() {
        assert_eq!(all_entries().len(), 15);
    }

    #[test]
    fn detect_known_models() {
        assert_eq!(
            detect_provider("claude-sonnet-4-20250514"),
            Some("anthropic")
        );
        assert_eq!(detect_provider("gpt-4o"), Some("openai"));
        assert_eq!(detect_provider("o3-mini"), Some("openai"));
        assert_eq!(detect_provider("o4-mini"), Some("openai"));
        assert_eq!(detect_provider("gemini-2.5-flash"), Some("gemini"));
        assert_eq!(detect_provider("deepseek-chat"), Some("deepseek"));
        assert_eq!(detect_provider("kimi-k2.5"), Some("moonshot"));
        assert_eq!(detect_provider("qwen-max"), Some("dashscope"));
        assert_eq!(detect_provider("glm-4-plus"), Some("zhipu"));
        assert_eq!(detect_provider("MiniMax-M2.5"), Some("minimax"));
        assert_eq!(detect_provider("llama-3.3-70b"), Some("groq"));
    }

    #[test]
    fn detect_unknown_model() {
        assert_eq!(detect_provider("some-random-model"), None);
    }
}
