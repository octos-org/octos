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

impl ModelPricing {
    /// Calculate cost for given token counts.
    pub fn cost(&self, input_tokens: u32, output_tokens: u32) -> f64 {
        (input_tokens as f64 / 1_000_000.0) * self.input_per_million
            + (output_tokens as f64 / 1_000_000.0) * self.output_per_million
    }
}

/// Look up pricing for a model. Returns None for unknown/local models.
pub fn model_pricing(model_id: &str) -> Option<ModelPricing> {
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
}
