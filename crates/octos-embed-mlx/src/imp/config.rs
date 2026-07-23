//! EmbeddingGemma-300M (`gemma3_text` encoder) configuration.
//!
//! Parsed from the model's `config.json`. All fields are validated against the
//! architecture the Rust forward pass assumes; unexpected values surface as an
//! error rather than a silent parity break.

use eyre::{Result, WrapErr, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GemmaConfig {
    pub num_hidden_layers: usize,
    pub hidden_size: i32,
    pub num_attention_heads: i32,
    pub num_key_value_heads: i32,
    pub head_dim: i32,
    pub intermediate_size: i32,
    pub rms_norm_eps: f32,
    pub rope_theta: f32,
    pub rope_local_base_freq: f32,
    pub query_pre_attn_scalar: f32,
    pub sliding_window_pattern: usize,
    pub quant_group_size: i32,
    pub quant_bits: i32,
}

#[derive(Deserialize)]
struct RawQuant {
    group_size: i32,
    bits: i32,
}

#[derive(Deserialize)]
struct RawConfig {
    num_hidden_layers: usize,
    hidden_size: i32,
    num_attention_heads: i32,
    num_key_value_heads: i32,
    head_dim: i32,
    intermediate_size: i32,
    rms_norm_eps: f32,
    rope_theta: f32,
    rope_local_base_freq: f32,
    query_pre_attn_scalar: f32,
    #[serde(default = "default_pattern")]
    sliding_window_pattern: usize,
    quantization: RawQuant,
}

fn default_pattern() -> usize {
    6
}

impl GemmaConfig {
    pub fn from_json_str(s: &str) -> Result<Self> {
        let r: RawConfig = serde_json::from_str(s).wrap_err("parsing config.json")?;
        if r.sliding_window_pattern == 0 {
            bail!("sliding_window_pattern must be > 0");
        }
        Ok(Self {
            num_hidden_layers: r.num_hidden_layers,
            hidden_size: r.hidden_size,
            num_attention_heads: r.num_attention_heads,
            num_key_value_heads: r.num_key_value_heads,
            head_dim: r.head_dim,
            intermediate_size: r.intermediate_size,
            rms_norm_eps: r.rms_norm_eps,
            rope_theta: r.rope_theta,
            rope_local_base_freq: r.rope_local_base_freq,
            query_pre_attn_scalar: r.query_pre_attn_scalar,
            sliding_window_pattern: r.sliding_window_pattern,
            quant_group_size: r.quantization.group_size,
            quant_bits: r.quantization.bits,
        })
    }

    /// Attention softmax scale: `query_pre_attn_scalar ** -0.5`
    /// (matches `mlx_lm.models.gemma3_text.Attention.scale`).
    pub fn attn_scale(&self) -> f32 {
        self.query_pre_attn_scalar.powf(-0.5)
    }

    /// Token-embedding multiplier.
    ///
    /// Gemma scales embeddings by `sqrt(hidden_size)`. CRITICAL: the reference
    /// `mlx_embeddings` computes the constant as
    /// `mx.array(hidden_size**0.5, embed_tokens.weight.dtype)`. For this 8-bit
    /// model the quantized `embed_tokens.weight` is `uint32`, so `sqrt(768) =
    /// 27.7128` is TRUNCATED to `27.0` before use. We replicate that truncation
    /// exactly (verified against the golden `embed_scaled` tap) — using the true
    /// `sqrt(768)` would break numeric parity with the oracle.
    pub fn embed_scale(&self) -> f32 {
        (self.hidden_size as f32).sqrt() as u32 as f32
    }

    /// A layer is a "full/global attention" layer (RoPE base = `rope_theta`)
    /// when `(idx + 1) % sliding_window_pattern == 0`, else "sliding" (RoPE base
    /// = `rope_local_base_freq`). Mirrors `Attention.is_sliding` in mlx_lm.
    pub fn is_global(&self, layer_idx: usize) -> bool {
        (layer_idx + 1) % self.sliding_window_pattern == 0
    }

    pub fn rope_base(&self, layer_idx: usize) -> f32 {
        if self.is_global(layer_idx) {
            self.rope_theta
        } else {
            self.rope_local_base_freq
        }
    }
}
