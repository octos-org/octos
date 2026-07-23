//! Gemma3 decoder block with sandwich norms and two residuals.
//!
//! Mirrors `mlx_lm.models.gemma3_text.TransformerBlock.__call__`:
//! ```text
//! r = self_attn(input_layernorm(x))
//! h = x + post_attention_layernorm(r)
//! r = mlp(pre_feedforward_layernorm(h))
//! out = h + post_feedforward_layernorm(r)
//! ```
//! (`clip_residual` is a plain add in f32 — no clipping needed here.)

use std::collections::HashMap;

use eyre::Result;
use mlx_rs::ops::{maximum, minimum};
use mlx_rs::{Array, Dtype};

use super::attention::Attention;
use super::config::GemmaConfig;
use super::mlp::Mlp;
use super::norm::RmsNorm;

pub struct Block {
    input_layernorm: RmsNorm,
    post_attention_layernorm: RmsNorm,
    pre_feedforward_layernorm: RmsNorm,
    post_feedforward_layernorm: RmsNorm,
    attn: Attention,
    mlp: Mlp,
}

impl Block {
    pub fn load(weights: &HashMap<String, Array>, cfg: &GemmaConfig, layer: usize) -> Result<Self> {
        let base = format!("model.layers.{layer}");
        let norm = |name: &str| -> Result<RmsNorm> {
            let w = weights
                .get(&format!("{base}.{name}.weight"))
                .ok_or_else(|| eyre::eyre!("missing {base}.{name}.weight"))?;
            RmsNorm::new(w, cfg.rms_norm_eps)
        };
        Ok(Self {
            input_layernorm: norm("input_layernorm")?,
            post_attention_layernorm: norm("post_attention_layernorm")?,
            pre_feedforward_layernorm: norm("pre_feedforward_layernorm")?,
            post_feedforward_layernorm: norm("post_feedforward_layernorm")?,
            attn: Attention::load(weights, cfg, layer)?,
            mlp: Mlp::load(weights, cfg, layer)?,
        })
    }

    pub fn forward(&self, x: &Array) -> Result<Array> {
        let r = self.attn.forward(&self.input_layernorm.forward(x)?)?;
        let h = clip_residual(x, &self.post_attention_layernorm.forward(&r)?)?;
        let r = self.mlp.forward(&self.pre_feedforward_layernorm.forward(&h)?)?;
        clip_residual(&h, &self.post_feedforward_layernorm.forward(&r)?)
    }
}

/// Residual add matching `mlx_lm.models.gemma3_text.clip_residual`.
///
/// Gemma's late layers (here blocks 22-23) produce activations that exceed the
/// f16 range (±65504). A plain f16 add overflows to `inf`; mlx clips instead:
/// for f16 inputs it adds in f32, clamps to ±f16::MAX, then casts back to f16.
/// Skipping this diverged hard at the last two blocks (cos 0.98 → 1.0000).
fn clip_residual(x: &Array, y: &Array) -> Result<Array> {
    if x.dtype() != Dtype::Float16 {
        return Ok(x.add(y)?);
    }
    let bound = 65504.0_f32; // f16::MAX
    let sum = x.as_dtype(Dtype::Float32)?.add(&y.as_dtype(Dtype::Float32)?)?;
    let clipped = minimum(&maximum(&sum, &Array::from_f32(-bound))?, &Array::from_f32(bound))?;
    Ok(clipped.as_dtype(Dtype::Float16)?)
}
