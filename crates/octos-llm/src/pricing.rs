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
    // A bare model id can be shared by several catalog rows — the native
    // provider (`moonshot/kimi-k2.5`) and its re-hosts
    // (`openrouter/moonshotai/kimi-k2.5`, `nvidia/…`). A plain last-writer-wins
    // insert would let a re-host clobber the native rate under the shared bare
    // alias `kimi-k2.5`, mispricing direct requests (which look up the bare id).
    // Award the bare alias to the row with the FEWEST path segments (the most
    // canonical/native), so the result is deterministic and order-independent.
    // On an EQUAL segment count the more-native HOST wins: compare the provider
    // prefix (segment before the first `/`) FIRST, then the full lowercased key —
    // `minimax/MiniMax-M3` ($0.15/$1.5) and `r9s/minimax-m3` ($0.5/$2) both have
    // one slash, so without a tie-breaker the bare `minimax-m3` rate would depend
    // on which row the router exported first. Comparing the provider prefix
    // (rather than the raw key) keeps a native `zai/…` row ahead of a
    // `zai-coding/…` re-host, whose `-` would otherwise sort before the native
    // row's `/` (mirrors `context::build_catalog_map`). (Tracking the owner's key
    // rather than only its depth is what lets the tie-break compare keys.)
    //
    // The bare alias is LOWERCASED (`catalog_pricing` lowercases the requested id
    // before lookup) so a case-variant native key like `minimax/MiniMax-M2.5` is
    // still reachable via `minimax-m2.5`. The full provider-qualified key,
    // however, is stored in its ORIGINAL case: lowercasing it would drop it into
    // the bare model-ID namespace, where a re-host's `model_id()` can exact-hit
    // it — `minimax/MiniMax-M2.5` (native, $0.50) lowercased to
    // `minimax/minimax-m2.5` is exactly what `OpenRouterProvider::model_id()`
    // returns for the re-hosted `openrouter/minimax/minimax-m2.5` ($0.29), so an
    // OpenRouter cost lookup would exact-select the native rate. NOTE: a bare
    // shared model name still resolves to the native (fewest-segments) rate for
    // COST ESTIMATION; distinguishing a re-host lane's own rate needs the
    // caller's provider family, which the `model_pricing(model_id)` API does not
    // carry (7 catalog model-ids are re-hosted by 2+ providers at different
    // rates, so a key-only heuristic cannot disambiguate them). That is a
    // deliberate, documented estimation limitation, not something this seeding
    // can fix on its own.
    let mut bare_owner: HashMap<String, (usize, String)> = HashMap::new();
    for (key, cost_in, cost_out) in entries {
        if *cost_in > 0.0 || *cost_out > 0.0 {
            let pricing = ModelPricing {
                input_per_million: *cost_in,
                output_per_million: *cost_out,
            };
            map.insert(key.clone(), pricing);
            if let Some(model) = key.split('/').next_back() {
                let bare = model.to_lowercase();
                let segments = key.matches('/').count();
                // Lowercased only for the deterministic tie-break comparison; the
                // full key itself is inserted above in its original case.
                let key_lower = key.to_lowercase();
                let take = match bare_owner.get(&bare) {
                    None => true,
                    Some((owned_seg, owner_key)) => {
                        segments < *owned_seg
                            || (segments == *owned_seg
                                && (
                                    crate::context::provider_prefix(&key_lower),
                                    key_lower.as_str(),
                                ) < (
                                    crate::context::provider_prefix(owner_key),
                                    owner_key.as_str(),
                                ))
                    }
                };
                if take {
                    map.insert(bare.clone(), pricing);
                    bare_owner.insert(bare, (segments, key_lower));
                }
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

/// Provider-specific prompt-cache billing multipliers on the base input rate.
///
/// #2194 review: cache economics are a PROTOCOL/provider property, not a
/// universal constant — pricing OpenAI or Gemini cache traffic at Anthropic's
/// 0.1x/1.25x misprices both. See [`cache_rates`] for the rate cards, their
/// sources, and the protocol keying.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheRates {
    /// Cache-read (cache-hit) tokens bill at this multiple of the input rate.
    pub read_multiplier: f64,
    /// Cache-write tokens bill at this multiple of the input rate. They are
    /// DISJOINT from `input_tokens`, so a 0.0 here makes a reported write
    /// VANISH from cost — only correct for a protocol that never reports
    /// writes (Google), never for one that does.
    pub write_multiplier: f64,
}

/// Whether the answering slot speaks the ANTHROPIC Messages API, keyed on the
/// resolved provider label + model. This is the protocol that reports
/// `cache_creation_input_tokens` (writes, 1.25x) and `cache_read_input_tokens`
/// (reads, 0.1x).
///
/// Native `anthropic` plus the relabeled proxies that construct an
/// `AnthropicProvider` under a custom label: `zai` / `zai-coding` (GLM over
/// the Anthropic API) and `r9s` when it is serving a `claude-*` model (r9s
/// auto-selects the Anthropic protocol for claude models and OpenAI for the
/// rest — see `registry/r9s.rs`). A label CONTAINING "anthropic" also counts,
/// covering custom Anthropic-compatible endpoints. zhipu / dashscope /
/// minimax / moonshot-coding are OpenAI-protocol re-hosts and are
/// deliberately excluded.
fn speaks_anthropic_protocol(provider: &str, model: &str) -> bool {
    let p = provider.to_ascii_lowercase();
    p.contains("anthropic")
        || p == "zai"
        || p == "zai-coding"
        // Mirror r9s construction EXACTLY (case-sensitive `starts_with("claude-")`
        // on the RAW model): r9s speaks the Anthropic protocol only for the models
        // it actually builds an `AnthropicProvider` for — see `registry::r9s`.
        || (p == "r9s" && crate::registry::r9s::prefers_anthropic(model))
}

/// The prompt-cache rate card for the answering slot, keyed on its
/// [`ProviderMetadata`] provider label + model (matched case-insensitively).
///
/// Sources, and why each bucket is what it is:
/// - ANTHROPIC protocol (native `anthropic`, plus relabeled Anthropic-API
///   proxies — see [`speaks_anthropic_protocol`]): cache reads 0.1x the input
///   rate, 5-minute ephemeral cache writes 1.25x — uniform across Claude
///   models per Anthropic's prompt-caching pricing docs, and consistent with
///   every catalog row that carries a cached rate (`catalog.rs`: sonnet-4
///   0.3/3.0, haiku-4.5 0.08/0.80 — both exactly 0.1x). This branch is keyed
///   on PROTOCOL, not on the family label, because a relabeled proxy (zai
///   serving GLM, r9s serving claude) still emits Anthropic cache accounting.
/// - `gemini` / `vertex` / `google`: implicit caching bills cached tokens at
///   25% of the input rate (catalog row gemini-2.5-flash: 0.0375/0.15 =
///   0.25x). No per-token write charge — explicit-cache STORAGE is
///   time-billed, octos never creates explicit caches, and the Gemini parser
///   never reports write tokens, so 0.0 writes can never make a real token
///   vanish.
/// - everything else (openai, openrouter, deepseek, local, unknown/empty):
///   no cached READ rate is knowable — the catalog's only OpenAI row carries
///   `cache_read_per_mtok: None`, and the public discount varies per model
///   FAMILY (0.5x for gpt-4o-era, deeper for newer), so a provider-wide
///   discount would be invented for some models. Reads bill at the FULL input
///   rate (1.0x — the never-understate bound; overstates where a real
///   discount exists). WRITES bill at 1.25x, NOT 0: `cache_write_tokens` is
///   populated ONLY by the Anthropic parser (`anthropic.rs`;
///   `TokenUsage`'s contract — no OpenAI/Gemini path sets it), so any write
///   token that reaches this residual bucket is an Anthropic-protocol write
///   from a proxy whose label we failed to recognize. Pricing it at 0 would
///   make a billed token vanish (an understatement); 1.25x is the honest
///   value. Tightening the read side needs cached-rate fields in the runtime
///   model catalog first.
pub fn cache_rates(provider: &str, model: &str) -> CacheRates {
    if speaks_anthropic_protocol(provider, model) {
        return CacheRates {
            read_multiplier: CACHE_READ_INPUT_MULTIPLIER,
            write_multiplier: CACHE_WRITE_INPUT_MULTIPLIER,
        };
    }
    let p = provider.to_ascii_lowercase();
    if p.contains("gemini") || p.contains("vertex") || p.contains("google") {
        return CacheRates {
            read_multiplier: 0.25,
            write_multiplier: 0.0,
        };
    }
    CacheRates {
        read_multiplier: 1.0,
        write_multiplier: CACHE_WRITE_INPUT_MULTIPLIER,
    }
}

impl ModelPricing {
    /// Calculate cost for given token counts.
    pub fn cost(&self, input_tokens: u32, output_tokens: u32) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_million
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_million
    }

    /// Cache-aware cost at ANTHROPIC's multipliers (0.1x read / 1.25x write)
    /// under the crate-wide DISJOINT accounting contract: `input_tokens`
    /// excludes cached tokens. Degenerates to [`Self::cost`] when both cache
    /// counts are zero.
    ///
    /// Token-count normalization is handled at every provider's parse
    /// boundary (see `TokenUsage`), but the MULTIPLIERS here are Anthropic's
    /// alone — call sites pricing arbitrary providers must use
    /// [`Self::cost_with_cache_for_provider`] instead (#2194 review).
    pub fn cost_with_cache(
        &self,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    ) -> f64 {
        self.cost_with_cache_rates(
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            CacheRates {
                read_multiplier: CACHE_READ_INPUT_MULTIPLIER,
                write_multiplier: CACHE_WRITE_INPUT_MULTIPLIER,
            },
        )
    }

    /// Cache-aware cost at the rate card of the slot that actually served the
    /// response ([`cache_rates`], keyed on its provider label + model). This
    /// is the entry point runtime pricing should use: `TokenUsage` counts are
    /// already disjoint-normalized for every provider, so the only
    /// provider-specific part left is the multipliers, and the model is
    /// needed to tell a relabeled Anthropic proxy (r9s serving claude) from
    /// the same label serving an OpenAI-protocol model.
    pub fn cost_with_cache_for_provider(
        &self,
        provider: &str,
        model: &str,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
    ) -> f64 {
        self.cost_with_cache_rates(
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cache_rates(provider, model),
        )
    }

    fn cost_with_cache_rates(
        &self,
        input_tokens: u32,
        output_tokens: u32,
        cache_read_tokens: u32,
        cache_write_tokens: u32,
        rates: CacheRates,
    ) -> f64 {
        self.cost(input_tokens, output_tokens)
            + (cache_read_tokens as f64 / 1_000_000.0)
                * self.input_per_million
                * rates.read_multiplier
            + (cache_write_tokens as f64 / 1_000_000.0)
                * self.input_per_million
                * rates.write_multiplier
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

    // Kimi / Moonshot — NOTE: kimi-k3 MUST be checked before the generic
    // kimi-k2/moonshot branch: the full provider key ("moonshot/kimi-k3")
    // contains both substrings. K3 official rates: $3.00/M input (cache
    // miss) / $15.00/M output.
    if m.contains("kimi-k3") {
        return Some(ModelPricing {
            input_per_million: 3.00,
            output_per_million: 15.0,
        });
    }
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
    fn should_mirror_r9s_construction_predicate_when_pricing_cache() {
        // #2194 R3: r9s auto-selects the Anthropic protocol ONLY for
        // `model.starts_with("claude-")` (case-sensitive; see registry/r9s.rs).
        // Cache pricing must classify by the SAME predicate, or a model r9s
        // actually serves over the OpenAI protocol (mixed-case "Claude-...", or
        // a "claude"-prefixed-but-not-"claude-" name) is handed Anthropic cache
        // rates it never earns. Before this fix pricing used a LOWERCASED
        // `starts_with("claude")` and diverged from construction.
        assert_eq!(
            cache_rates("r9s", "claude-3-5-sonnet").read_multiplier,
            CACHE_READ_INPUT_MULTIPLIER,
            "r9s + claude-* is Anthropic protocol -> 0.1x cache reads",
        );
        assert_eq!(
            cache_rates("r9s", "Claude-3-5-sonnet").read_multiplier,
            1.0,
            "r9s builds an OpenAIProvider for a non-'claude-' (mixed-case) model, \
             so pricing must NOT hand it Anthropic cache rates",
        );
        assert_eq!(
            cache_rates("r9s", "claude2-experimental").read_multiplier,
            1.0,
            "a 'claude'-prefixed-but-not-'claude-' model is OpenAI protocol at r9s",
        );
    }

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
    fn should_price_anthropic_cache_at_tenth_read_and_five_fourths_write() {
        let p = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        };
        let cost = p.cost_with_cache_for_provider(
            "anthropic",
            "claude-opus-4",
            100_000,
            10_000,
            10_000,
            2_000,
        );
        let naive = p.cost(100_000, 10_000);
        // 10k reads at 0.1x ($0.003) + 2k writes at 1.25x ($0.0075).
        assert!(
            (cost - (naive + 0.003 + 0.0075)).abs() < 1e-12,
            "got {cost}"
        );
        // The label match must also catch relabeled Anthropic slots that keep
        // the family in the label, and the direct primitive stays aligned.
        assert!((cost - p.cost_with_cache(100_000, 10_000, 10_000, 2_000)).abs() < 1e-12);
    }

    #[test]
    fn should_price_gemini_cache_reads_at_quarter_rate_with_no_write_premium() {
        let p = ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        };
        let cost = p.cost_with_cache_for_provider(
            "gemini",
            "gemini-2.5-flash",
            100_000,
            10_000,
            100_000,
            5_000,
        );
        let naive = p.cost(100_000, 10_000);
        // 100k cached reads at 0.25x of $0.15/M = $0.00375; writes free.
        assert!((cost - (naive + 0.00375)).abs() < 1e-12, "got {cost}");
        let vertex = p.cost_with_cache_for_provider(
            "vertex",
            "gemini-2.5-pro",
            100_000,
            10_000,
            100_000,
            5_000,
        );
        assert!(
            (vertex - cost).abs() < 1e-12,
            "vertex bills at Google rates"
        );
    }

    #[test]
    fn should_price_unknown_provider_cache_reads_at_full_input_rate_and_writes_never_free() {
        // No cached READ rate is knowable for these providers (the catalog's
        // only OpenAI row carries cache_read_per_mtok: None, and the public
        // discount varies per model family), so reads bill at the FULL input
        // rate — never an invented discount. WRITES, however, are never 0:
        // cache_write_tokens is only ever populated by the Anthropic parser,
        // so any write reaching this bucket is an Anthropic-protocol write
        // from an unrecognized proxy label and bills at 1.25x rather than
        // vanishing.
        let p = ModelPricing {
            input_per_million: 2.5,
            output_per_million: 10.0,
        };
        for (provider, model) in [
            ("openai", "gpt-4o"),
            ("openrouter", "anthropic/claude-3.5-sonnet"),
            ("deepseek", "deepseek-chat"),
            ("local", "qwen2.5"),
            ("", ""),
        ] {
            let cost =
                p.cost_with_cache_for_provider(provider, model, 100_000, 10_000, 40_000, 8_000);
            // reads at full input rate (folded into input) + writes at 1.25x.
            let expected = p.cost(100_000 + 40_000, 10_000)
                + (8_000.0 / 1_000_000.0) * p.input_per_million * 1.25;
            assert!(
                (cost - expected).abs() < 1e-12,
                "{provider}/{model}: reads at full input rate, writes at 1.25x, not free (got {cost})"
            );
            // A reported write must never vanish.
            let read_only =
                p.cost_with_cache_for_provider(provider, model, 100_000, 10_000, 40_000, 0);
            assert!(
                cost - read_only > 1e-9,
                "{provider}/{model}: cache-write tokens must add cost"
            );
        }
    }

    #[test]
    fn should_change_cache_cost_when_provider_changes() {
        // Mutation guard for the provider lookup itself: identical usage must
        // price differently across the three rate cards.
        let p = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        };
        let anthropic = p.cost_with_cache_for_provider(
            "anthropic",
            "claude-opus-4",
            100_000,
            10_000,
            50_000,
            5_000,
        );
        let gemini = p.cost_with_cache_for_provider(
            "gemini",
            "gemini-2.5-flash",
            100_000,
            10_000,
            50_000,
            5_000,
        );
        let unknown =
            p.cost_with_cache_for_provider("openai", "gpt-4o", 100_000, 10_000, 50_000, 5_000);
        assert!((anthropic - gemini).abs() > 1e-9);
        assert!((anthropic - unknown).abs() > 1e-9);
        assert!((gemini - unknown).abs() > 1e-9);
    }

    #[test]
    fn should_price_relabeled_anthropic_protocol_cache_writes_and_reads_at_anthropic_rates() {
        // #2194 review round 2: relabeled Anthropic-protocol providers
        // (zai / zai-coding serving GLM, r9s serving claude, custom
        // anthropic) emit cache_creation_input_tokens as WRITES. Their label
        // is not "anthropic", but they speak the Anthropic Messages API, so
        // writes bill at 1.25x and reads at 0.1x — NOT free, and not the
        // full-rate residual bucket.
        let p = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        };
        let anthropic_ref = p.cost_with_cache_for_provider(
            "anthropic",
            "claude-opus-4",
            100_000,
            10_000,
            40_000,
            8_000,
        );
        for (provider, model) in [
            ("zai", "glm-4.6"),
            ("zai-coding", "glm-4.6"),
            ("r9s", "claude-3-5-sonnet"),
            ("custom-anthropic", "claude-3-7-sonnet"),
        ] {
            let cost =
                p.cost_with_cache_for_provider(provider, model, 100_000, 10_000, 40_000, 8_000);
            assert!(
                (cost - anthropic_ref).abs() < 1e-12,
                "{provider}/{model}: Anthropic-protocol cache must price at 0.1x read / 1.25x write \
                 (got {cost}, anthropic {anthropic_ref})"
            );
            // And explicitly: the write is NOT free.
            let read_only =
                p.cost_with_cache_for_provider(provider, model, 100_000, 10_000, 40_000, 0);
            assert!(
                cost - read_only > 1e-9,
                "{provider}/{model}: 8k cache-write tokens must add cost, not vanish"
            );
        }
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
    fn should_price_kimi_k3_before_generic_moonshot_branch() {
        // kimi-k3 ($3.00/M in, $15.00/M out) must match before the generic
        // kimi-k2/moonshot branch — the full provider key contains BOTH
        // "kimi-k3" and "moonshot", and last-writer semantics would misprice
        // it at the k2 rates ($0.60/$2.40).
        for id in ["kimi-k3", "moonshot/kimi-k3"] {
            let p = model_pricing(id).unwrap();
            assert!((p.input_per_million - 3.0).abs() < f64::EPSILON, "{id}");
            assert!((p.output_per_million - 15.0).abs() < f64::EPSILON, "{id}");
        }
        // The k2 family keeps its own rates.
        let k2 = model_pricing("kimi-k2.6").unwrap();
        assert!((k2.input_per_million - 0.60).abs() < f64::EPSILON);
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

        // Section 4: a bare model id shared by a native provider and a re-host
        // resolves to the NATIVE (fewest-segments) rate. Re-host listed FIRST to
        // prove the award is order-independent, not last-writer-wins.
        seed_pricing_catalog(&[
            ("octrehost/octvendor/octshared-9".to_string(), 9.0, 9.0), // 2 segments
            ("octnative/octshared-9".to_string(), 1.5, 2.5),           // 1 segment (native)
        ]);
        let bare = model_pricing("octshared-9").unwrap();
        assert!(
            (bare.input_per_million - 1.5).abs() < f64::EPSILON,
            "native provider wins the bare alias, not the re-host"
        );
        assert!((bare.output_per_million - 2.5).abs() < f64::EPSILON);
        // The re-host's fully-qualified key still resolves to its own rate.
        let rehost = model_pricing("octrehost/octvendor/octshared-9").unwrap();
        assert!((rehost.input_per_million - 9.0).abs() < f64::EPSILON);

        // Section 5: case-variant bare ids. The native key is capitalized
        // (`OctCap-9`) and the re-host is lowercase (`octcap-9`); since the
        // lookup lowercases, the award must still go to the native rate rather
        // than splitting into two distinct aliases.
        seed_pricing_catalog(&[
            ("octrehost/octvendor/octcap-9".to_string(), 9.0, 9.0), // lowercase, 2 seg
            ("octnative/OctCap-9".to_string(), 1.5, 2.5),           // Capitalized, 1 seg (native)
        ]);
        let cap = model_pricing("OctCap-9").unwrap();
        assert!(
            (cap.input_per_million - 1.5).abs() < f64::EPSILON,
            "native rate wins the bare alias despite case variance"
        );

        // Section 6: two providers at EQUAL depth (both one segment) share a bare
        // id, so segment count can't break the tie. The lexicographically-smaller
        // lowercased full key wins — mirroring the real `minimax/MiniMax-M3`
        // ($0.15/$1.5) vs `r9s/minimax-m3` ($0.5/$2) collision. Listed
        // larger-key-first to prove the award is order-independent rather than
        // first-writer-wins.
        seed_pricing_catalog(&[
            ("octzeta/octtie-9".to_string(), 9.0, 9.0), // 1 seg, larger key
            ("octalpha/octtie-9".to_string(), 1.5, 2.5), // 1 seg, smaller key wins
        ]);
        let tie = model_pricing("octtie-9").unwrap();
        assert!(
            (tie.input_per_million - 1.5).abs() < f64::EPSILON,
            "equal-depth bare alias resolves to the lexicographically-smaller key, deterministically"
        );
        assert!((tie.output_per_million - 2.5).abs() < f64::EPSILON);

        // Restore an empty catalog so parallel tests keep hitting the
        // hardcoded ladder exactly as before this test ran.
        seed_pricing_catalog(&[]);
    }
}
