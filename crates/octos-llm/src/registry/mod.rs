//! Provider registry — one module per provider, single lookup for construction.
//!
//! Each sub-module exports a `pub const ENTRY: ProviderEntry` and a `create()`
//! factory.  Adding a new provider = add a file + one line in `ALL`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use eyre::Result;

use crate::openai::ModelHints;
use crate::provider::LlmProvider;

// ── Family defaults, from the catalog ───────────────────────────────────────

/// The canonical `model_catalog.json`, compiled in — the same SSOT the CLI
/// serves for onboarding. It is the single place a model NAME is written down;
/// no provider module hardcodes one.
const CANONICAL_MODEL_CATALOG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../model_catalog.json"
));

/// `family -> default model`, built once from the rows flagged `"default": true`.
///
/// A family's default used to be a `&'static str` on each `ProviderEntry`, which
/// meant the same fact lived in two places and drifted: four families (deepseek,
/// minimax, openrouter, vertex) had hardcoded defaults that no catalog row even
/// mentioned. Reading it from the catalog makes drift impossible by construction.
fn family_defaults() -> &'static HashMap<String, String> {
    static DEFAULTS: OnceLock<HashMap<String, String>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        let catalog: crate::adaptive::QosCatalog =
            match serde_json::from_str(CANONICAL_MODEL_CATALOG) {
                Ok(catalog) => catalog,
                Err(error) => {
                    // The catalog is compiled in, so this is a build-time-shaped
                    // failure surfacing at runtime. Degrade to "no defaults" rather
                    // than panicking a live process: every caller already handles
                    // `None` by demanding an explicit model.
                    tracing::error!(%error, "canonical model catalog failed to parse; \
                     provider families will report no default model");
                    return HashMap::new();
                }
            };
        catalog
            .models
            .iter()
            .filter(|entry| entry.is_family_default)
            .filter_map(|entry| {
                // `provider` is `"<family>/<model>"`, and the model half may
                // itself contain slashes (`openrouter/anthropic/claude-...`),
                // so split ONCE.
                let (family, model) = entry.provider.split_once('/')?;
                Some((family.to_string(), model.to_string()))
            })
            .collect()
    })
}

/// The catalog-declared default model for a provider family, if it has one.
///
/// `None` means the family requires an explicit model.
pub fn catalog_default_model(family: &str) -> Option<&'static str> {
    family_defaults().get(family).map(String::as_str)
}

// ── Provider sub-modules ────────────────────────────────────────────────────

mod anthropic;
mod dashscope;
mod deepseek;
mod gemini;
mod groq;
mod local;
mod minimax;
mod moonshot;
mod moonshot_coding;
mod nvidia;
mod ollama;
mod openai;
mod openrouter;
mod r9s;
mod vertex;
mod vllm;
mod zai;
mod zai_coding;
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
    /// Environment variable holding the API key. `None` = no key required.
    pub api_key_env: Option<&'static str>,
    /// Additional accepted API-key env var names for this provider, beyond
    /// `api_key_env` (e.g. Moonshot accepts both `MOONSHOT_API_KEY` and
    /// `KIMI_API_KEY`). Only consulted when the configured key var is already
    /// one of the provider's known names — an arbitrary custom `api_key_env`
    /// override stays exclusive and never falls back to these.
    pub key_env_aliases: &'static [&'static str],
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

impl ProviderEntry {
    /// The model this family uses when a profile names none, read from the
    /// catalog row flagged `"default": true`. `None` = the user must supply one.
    ///
    /// Was a `&'static str` const on every entry until the catalog became the
    /// single place a model name is written down.
    pub fn default_model(&self) -> Option<&'static str> {
        catalog_default_model(self.name)
    }

    /// Every API-key env var name this provider accepts: the primary
    /// `api_key_env` plus any declared `key_env_aliases`.
    pub fn key_env_names(&self) -> impl Iterator<Item = &'static str> {
        self.api_key_env
            .into_iter()
            .chain(self.key_env_aliases.iter().copied())
    }

    /// Whether `name` is one of this provider's declared key env var names.
    /// Comparison is case-SENSITIVE: environment variable names are
    /// case-sensitive on Unix, so a genuinely custom `kimi_api_key` must NOT be
    /// treated as the declared `KIMI_API_KEY` (doing so would let a missing
    /// custom override fall back to an ambient sibling credential).
    pub fn is_known_key_env(&self, name: &str) -> bool {
        self.key_env_names().any(|k| k == name)
    }
}

// ── Master list ─────────────────────────────────────────────────────────────

/// All registered providers.  Order matters for `detect_provider` — more
/// specific patterns should come before catch-all providers like groq.
static ALL: &[ProviderEntry] = &[
    anthropic::ENTRY,
    openai::ENTRY,
    gemini::ENTRY,
    vertex::ENTRY,
    r9s::ENTRY,
    openrouter::ENTRY,
    deepseek::ENTRY,
    groq::ENTRY,
    // Coding-plan families FIRST so an explicit `moonshot-coding` / `zai-coding`
    // resolves to the coding endpoint before the base family's name/aliases.
    moonshot_coding::ENTRY,
    moonshot::ENTRY,
    dashscope::ENTRY,
    minimax::ENTRY,
    zai_coding::ENTRY,
    zhipu::ENTRY,
    zai::ENTRY,
    nvidia::ENTRY,
    ollama::ENTRY,
    vllm::ENTRY,
    local::ENTRY,
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

    /// The catalog is the only place a default model is written down, so every
    /// family that claims one must actually find it there. A family silently
    /// losing its default would make `octos chat -p <family>` start demanding an
    /// explicit `--model`.
    #[test]
    fn every_family_with_a_default_resolves_it_from_the_catalog() {
        // Families that must have one, with the value the catalog declares.
        for (family, expected) in [
            ("anthropic", "claude-sonnet-4-20250514"),
            ("deepseek", "deepseek-v4-flash"),
            ("gemini", "gemini-2.5-flash"),
            ("local", "default"),
            ("minimax", "MiniMax-M3"),
            ("moonshot-coding", "k3"),
            ("openai", "gpt-4o"),
            ("openrouter", "anthropic/claude-sonnet-4-6"),
            ("vertex", "gemini-2.5-flash"),
            ("zai", "glm-5-turbo"),
            ("zai-coding", "glm-5.3"),
        ] {
            let entry = lookup(family).unwrap_or_else(|| panic!("{family} registered"));
            assert_eq!(
                entry.default_model(),
                Some(expected),
                "{family} must resolve its default from the catalog"
            );
        }
    }

    /// A family's default model must be unambiguous: two rows flagged `default`
    /// under one family would make the resolved value depend on catalog order,
    /// which is exactly the fragility this replaced.
    #[test]
    fn the_catalog_declares_at_most_one_default_per_family() {
        let catalog: crate::adaptive::QosCatalog =
            serde_json::from_str(CANONICAL_MODEL_CATALOG).expect("canonical catalog parses");

        let mut by_family: HashMap<&str, Vec<&str>> = HashMap::new();
        for entry in catalog.models.iter().filter(|e| e.is_family_default) {
            let (family, _) = entry
                .provider
                .split_once('/')
                .unwrap_or((entry.provider.as_str(), ""));
            by_family.entry(family).or_default().push(&entry.provider);
        }

        let duplicated: Vec<_> = by_family.iter().filter(|(_, v)| v.len() > 1).collect();
        assert!(
            duplicated.is_empty(),
            "each family may flag at most one default: {duplicated:?}"
        );
        assert!(
            !by_family.is_empty(),
            "the catalog must declare family defaults at all"
        );
    }

    /// `openrouter` and `nvidia` models carry a slash of their own
    /// (`anthropic/claude-sonnet-4-6`), so the family split must take the FIRST
    /// separator only — splitting on the last would yield family `anthropic`
    /// and silently give OpenRouter no default.
    #[test]
    fn a_model_id_containing_a_slash_still_resolves_to_its_family() {
        assert_eq!(
            catalog_default_model("openrouter"),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(
            catalog_default_model("nvidia"),
            Some("meta/llama-3.3-70b-instruct")
        );
    }

    /// An unregistered family has no default rather than a wrong one.
    #[test]
    fn an_unknown_family_has_no_default() {
        assert_eq!(catalog_default_model("not-a-provider"), None);
    }

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

    /// Every engine name a user might type for a local OpenAI-compatible
    /// server resolves to the ONE unified `local` family.
    #[test]
    fn should_resolve_local_engine_aliases_to_the_local_family() {
        for alias in [
            "local",
            "llamacpp",
            "llama.cpp",
            "llama-server",
            "llama_server",
            "lmstudio",
            "lm-studio",
            "openai-compatible",
        ] {
            let e = lookup(alias).unwrap_or_else(|| panic!("{alias} must resolve"));
            assert_eq!(e.name, "local", "{alias} must resolve to the local family");
        }
    }

    /// `local` is fully optional: no key, no base URL, no model required —
    /// zero-config onboarding is the family's contract.
    #[test]
    fn should_require_nothing_for_the_local_family() {
        let e = lookup("local").unwrap();
        assert!(e.api_key_env.is_none());
        assert!(!e.requires_api_key);
        assert!(!e.requires_base_url);
        assert!(!e.requires_model);
        assert_eq!(e.default_base_url, Some("http://127.0.0.1:8080/v1"));
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
        assert_eq!(all_entries().len(), 19);
    }

    /// The coding-plan families resolve to their coding endpoints + default
    /// models, distinct from the regular families, and their name/alias lookups
    /// don't shadow the base families.
    #[test]
    fn coding_plan_families_resolve_to_coding_endpoints() {
        // Kimi coding plan: OpenAI-compat, api.kimi.com/coding/v1, default k3.
        let mc = lookup("moonshot-coding").expect("moonshot-coding registered");
        assert_eq!(mc.default_model(), Some("k3"));
        assert_eq!(mc.default_base_url, Some("https://api.kimi.com/coding/v1"));
        assert!(mc.is_known_key_env("KIMI_API_KEY"));
        assert_eq!(
            lookup("kimi-coding").map(|e| e.name),
            Some("moonshot-coding")
        );

        // Z.AI GLM coding plan: Anthropic-compat coding endpoint.
        let zc = lookup("zai-coding").expect("zai-coding registered");
        assert_eq!(zc.default_base_url, Some("https://api.z.ai/api/anthropic"));
        assert_eq!(lookup("z.ai-coding").map(|e| e.name), Some("zai-coding"));

        // The base families are unshadowed by the coding families.
        assert_eq!(lookup("kimi").map(|e| e.name), Some("moonshot"));
        assert_eq!(lookup("z.ai").map(|e| e.name), Some("zai"));
    }

    #[test]
    fn vertex_entry_is_registered_with_sa_json_credential() {
        let e = lookup("vertex").expect("vertex provider should be registered");
        assert_eq!(e.name, "vertex");
        // Credential is the SA JSON, resolved through the VERTEX_SA_JSON channel.
        assert_eq!(e.api_key_env, Some("VERTEX_SA_JSON"));
        assert!(e.requires_api_key);
        // bare gemini model names must NOT auto-route to vertex.
        assert_eq!(detect_provider("gemini-2.5-flash"), Some("gemini"));
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
