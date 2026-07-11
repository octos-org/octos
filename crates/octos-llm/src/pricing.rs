//! Model pricing for cost estimation.
//!
//! Prices are approximate and may become stale. Last updated: 2025-02.
//! Source: provider pricing pages. Update when models or prices change.

/// Pricing per 1M tokens (input, output) in USD.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

use std::collections::HashMap;
use std::sync::RwLock;

/// Cached pricing from runtime catalog.
static PRICING_CATALOG: RwLock<Option<HashMap<String, ModelPricing>>> = RwLock::new(None);

/// Seed pricing from model_catalog.json entries.
/// Called at startup alongside context::seed_from_catalog().
pub fn seed_pricing_catalog(entries: &[(String, f64, f64)]) {
    let mut map = HashMap::new();
    for (key, cost_in, cost_out) in entries {
        if *cost_in > 0.0 || *cost_out > 0.0 {
            let pricing = ModelPricing {
                input_per_million: *cost_in,
                output_per_million: *cost_out,
            };
            map.insert(key.clone(), pricing);
            if let Some(model) = key.split('/').next_back() {
                map.insert(model.to_string(), pricing);
            }
        }
    }
    *PRICING_CATALOG.write().unwrap_or_else(|e| e.into_inner()) = Some(map);
}

fn catalog_pricing(model_id: &str) -> Option<ModelPricing> {
    let guard = PRICING_CATALOG.read().ok()?;
    let map = guard.as_ref()?;
    let m = model_id.to_lowercase();
    if let Some(p) = map.get(&m) {
        return Some(*p);
    }
    // The substring fallback used to return the FIRST HashMap hit, which
    // made pricing nondeterministic whenever a model id matched several
    // keys ("gpt-5.2-codex" matches both "gpt-5.2" and "gpt-5" — which
    // one won differed per process). Deterministic rule instead:
    //   1. Keys the model id CONTAINS name a family the id extends; the
    //      LONGEST such key is the most specific family, so it wins.
    //   2. Otherwise, among keys that contain the model id, the SHORTEST
    //      is the closest match.
    // Ties break lexicographically so equal-length keys are stable too.
    let family = map
        .iter()
        .filter(|(key, _)| m.contains(key.as_str()))
        .max_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| b.cmp(a)));
    if let Some((_, p)) = family {
        return Some(*p);
    }
    map.iter()
        .filter(|(key, _)| key.contains(&m))
        .min_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
        .map(|(_, p)| *p)
}

/// Prompt-cache read multiplier on the input rate (Anthropic bills cache
/// hits at ~10% of the base input price).
const CACHE_READ_INPUT_MULTIPLIER: f64 = 0.1;
/// Prompt-cache write multiplier on the input rate (Anthropic bills 5-minute
/// ephemeral cache writes at 1.25x the base input price).
const CACHE_WRITE_INPUT_MULTIPLIER: f64 = 1.25;

impl ModelPricing {
    /// Calculate cost for given token counts.
    pub fn cost(&self, input_tokens: u32, output_tokens: u32) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_million
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_million
    }

    /// Cache-aware cost using Anthropic's DISJOINT accounting: `input_tokens`
    /// excludes cached tokens, `cache_read_tokens` bills at 0.1x the input
    /// rate and `cache_write_tokens` at 1.25x. Degenerates to [`Self::cost`]
    /// when both cache counts are zero.
    ///
    /// Do NOT feed this OpenAI/Gemini usage as-is: those providers report
    /// cached tokens INSIDE `input_tokens` (overlapping accounting), so the
    /// cached portion would be double-billed. Call sites that price mixed
    /// providers need per-provider normalization first — until then they
    /// keep using [`Self::cost`].
    pub fn cost_with_cache(
        &self,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    ) -> f64 {
        self.cost(input_tokens, output_tokens)
            + (cache_read_tokens as f64 / 1_000_000.0)
                * self.input_per_million
                * CACHE_READ_INPUT_MULTIPLIER
            + (cache_write_tokens as f64 / 1_000_000.0)
                * self.input_per_million
                * CACHE_WRITE_INPUT_MULTIPLIER
    }
}

/// Look up pricing for a model. Checks the runtime catalog first,
/// falls back to hardcoded defaults for models not in the catalog.
pub fn model_pricing(model_id: &str) -> Option<ModelPricing> {
    // Check runtime catalog first (populated from model_catalog.json)
    if let Some(pricing) = catalog_pricing(model_id) {
        return Some(pricing);
    }
    // Fallback to hardcoded defaults for models not in catalog
    let m = model_id.to_lowercase();

    // Anthropic
    if m.contains("claude-opus-4") || m.contains("claude-4-opus") {
        return Some(ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
        });
    }
    if m.contains("claude-sonnet-4") || m.contains("claude-4-sonnet") {
        return Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        });
    }
    if m.contains("claude-3-5-sonnet") {
        return Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        });
    }
    if m.contains("claude-3-5-haiku") || m.contains("claude-haiku") {
        return Some(ModelPricing {
            input_per_million: 0.80,
            output_per_million: 4.0,
        });
    }

    // OpenAI — NOTE: gpt-4o-mini MUST be checked before gpt-4o (substring match)
    if m.contains("gpt-4o-mini") {
        return Some(ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        });
    }
    if m.contains("gpt-4o") {
        return Some(ModelPricing {
            input_per_million: 2.50,
            output_per_million: 10.0,
        });
    }
    if m.starts_with("o3") || m.starts_with("o4") {
        return Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 40.0,
        });
    }

    // Gemini
    if m.contains("gemini-2") || m.contains("gemini-1.5") {
        return Some(ModelPricing {
            input_per_million: 0.075,
            output_per_million: 0.30,
        });
    }

    // DeepSeek
    if m.contains("deepseek-r1") {
        return Some(ModelPricing {
            input_per_million: 0.55,
            output_per_million: 2.19,
        });
    }
    if m.contains("deepseek") {
        return Some(ModelPricing {
            input_per_million: 0.27,
            output_per_million: 1.10,
        });
    }

    // Qwen
    if m.contains("qwen3-coder") || m.contains("qwen3-235b") || m.contains("qwen3.5") {
        return Some(ModelPricing {
            input_per_million: 0.30,
            output_per_million: 1.20,
        });
    }
    if m.contains("qwen") {
        return Some(ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        });
    }

    // Llama (via NVIDIA NIM / Groq — pricing varies by host, using NVIDIA NIM rates)
    if m.contains("llama-3.1-405b") || m.contains("llama-3.1-nemotron-ultra") {
        return Some(ModelPricing {
            input_per_million: 5.00,
            output_per_million: 15.0,
        });
    }
    if m.contains("llama-3.3-70b") || m.contains("llama-3.1-70b") || m.contains("llama-4-maverick")
    {
        return Some(ModelPricing {
            input_per_million: 0.40,
            output_per_million: 1.60,
        });
    }
    if m.contains("llama-4-scout") || m.contains("llama3-70b") {
        return Some(ModelPricing {
            input_per_million: 0.30,
            output_per_million: 1.20,
        });
    }
    // Match "llama" but not "ollama" (local runner, no pricing)
    if (m.contains("llama") && !m.contains("ollama")) || m.contains("meta/llama") {
        return Some(ModelPricing {
            input_per_million: 0.10,
            output_per_million: 0.40,
        });
    }

    // Mistral
    if m.contains("mistral-large") {
        return Some(ModelPricing {
            input_per_million: 2.00,
            output_per_million: 6.00,
        });
    }
    if m.contains("mistral") || m.contains("mixtral") {
        return Some(ModelPricing {
            input_per_million: 0.20,
            output_per_million: 0.60,
        });
    }

    // Kimi / Moonshot
    if m.contains("kimi-k2") || m.contains("moonshot") {
        return Some(ModelPricing {
            input_per_million: 0.60,
            output_per_million: 2.40,
        });
    }
    if m.contains("kimi") {
        return Some(ModelPricing {
            input_per_million: 0.30,
            output_per_million: 1.20,
        });
    }

    // MiniMax
    if m.contains("minimax-m1") || m.contains("minimax-m2") {
        return Some(ModelPricing {
            input_per_million: 0.50,
            output_per_million: 2.00,
        });
    }
    if m.contains("minimax") {
        return Some(ModelPricing {
            input_per_million: 0.20,
            output_per_million: 1.10,
        });
    }

    // Zhipu GLM
    if m.contains("glm-5") || m.contains("glm5") {
        return Some(ModelPricing {
            input_per_million: 0.50,
            output_per_million: 2.00,
        });
    }
    if m.contains("glm-4") || m.contains("glm4") {
        return Some(ModelPricing {
            input_per_million: 0.30,
            output_per_million: 1.20,
        });
    }

    // NVIDIA Nemotron
    if m.contains("nemotron-super") || m.contains("nemotron-ultra") {
        return Some(ModelPricing {
            input_per_million: 1.50,
            output_per_million: 5.00,
        });
    }
    if m.contains("nemotron") {
        return Some(ModelPricing {
            input_per_million: 0.20,
            output_per_million: 0.80,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_model_pricing() {
        let p = model_pricing("claude-sonnet-4-20250514").unwrap();
        assert!((p.input_per_million - 3.0).abs() < f64::EPSILON);
        assert!((p.output_per_million - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cost_calculation() {
        let p = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        };
        let cost = p.cost(1_000_000, 100_000);
        // $3.00 input + $1.50 output = $4.50
        assert!((cost - 4.5).abs() < 0.001);
    }

    #[test]
    fn test_cost_with_cache_applies_read_and_write_multipliers() {
        let p = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        };
        // 100k uncached in ($0.30) + 10k out ($0.15)
        // + 900k cache-read at 0.1x ($0.27) + 50k cache-write at 1.25x ($0.1875)
        let cost = p.cost_with_cache(100_000, 10_000, 900_000, 50_000);
        assert!((cost - 0.9075).abs() < 1e-9, "got {cost}");

        // Zero cache counts degenerate to the plain cost().
        let plain = p.cost_with_cache(100_000, 10_000, 0, 0);
        assert!((plain - p.cost(100_000, 10_000)).abs() < 1e-12);
    }

    #[test]
    fn test_gpt4o_mini_before_gpt4o() {
        // gpt-4o-mini must match before gpt-4o
        let mini = model_pricing("gpt-4o-mini").unwrap();
        assert!((mini.input_per_million - 0.15).abs() < f64::EPSILON);
        let full = model_pricing("gpt-4o").unwrap();
        assert!((full.input_per_million - 2.50).abs() < f64::EPSILON);
    }

    #[test]
    fn test_unknown_model_returns_none() {
        assert!(model_pricing("my-local-model").is_none());
        assert!(model_pricing("ollama/phi-custom").is_none());
    }

    #[test]
    fn test_nvidia_model_pricing() {
        // Llama models should have pricing
        let llama = model_pricing("meta/llama-3.3-70b-instruct").unwrap();
        assert!(llama.input_per_million > 0.0);

        // Mistral models
        let mistral = model_pricing("mistralai/mistral-small-3.1-24b-instruct-2503").unwrap();
        assert!(mistral.input_per_million > 0.0);

        // Qwen models
        let qwen = model_pricing("qwen/qwen3-coder-480b-a35b-instruct").unwrap();
        assert!(qwen.input_per_million > 0.0);

        // DeepSeek R1 should be more expensive than base deepseek
        let r1 = model_pricing("deepseek-ai/deepseek-r1").unwrap();
        let base = model_pricing("deepseek-chat").unwrap();
        assert!(r1.input_per_million > base.input_per_million);
    }

    /// Catalog substring fallback must be deterministic. The old scan
    /// returned the FIRST HashMap hit, so a model id matching several
    /// catalog keys ("octestfam-5.2-codex" matches both "octestfam-5"
    /// and "octestfam-5.2") got a random sibling's pricing per process.
    ///
    /// One #[test] on purpose: these sections share the process-global
    /// PRICING_CATALOG, so splitting them into parallel tests would race.
    /// Key names are deliberately weird so no other test's probe can
    /// substring-match them while the seed is live.
    #[test]
    fn should_match_catalog_keys_deterministically_when_no_exact_hit() {
        // Section 1: model id EXTENDS several family keys — the longest
        // (most specific) family must win, not HashMap iteration order.
        seed_pricing_catalog(&[
            ("octestprov/octestfam-5".to_string(), 1.0, 2.0),
            ("octestprov/octestfam-5.2".to_string(), 3.0, 4.0),
        ]);
        let p = model_pricing("octestfam-5.2-codex").unwrap();
        assert!((p.input_per_million - 3.0).abs() < f64::EPSILON);
        assert!((p.output_per_million - 4.0).abs() < f64::EPSILON);

        // Section 2: exact key still wins over any substring candidate.
        let exact = model_pricing("octestfam-5").unwrap();
        assert!((exact.input_per_million - 1.0).abs() < f64::EPSILON);

        // Section 3: model id is a PREFIX of several keys — the shortest
        // (closest) super-key must win deterministically.
        seed_pricing_catalog(&[
            ("octestprov/octestfam-7.2".to_string(), 5.0, 6.0),
            ("octestprov/octestfam-7-mini-preview".to_string(), 7.0, 8.0),
        ]);
        let sup = model_pricing("octestfam-7").unwrap();
        assert!((sup.input_per_million - 5.0).abs() < f64::EPSILON);

        // Restore an empty catalog so parallel tests keep hitting the
        // hardcoded ladder exactly as before this test ran.
        seed_pricing_catalog(&[]);
    }
}
