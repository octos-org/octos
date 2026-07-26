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

    /// `mask` is the optional additive attention mask (padded batches only);
    /// see [`Attention::forward`].
    pub fn forward(&self, x: &Array, mask: Option<&Array>) -> Result<Array> {
        let r = self.attn.forward(&self.input_layernorm.forward(x)?, mask)?;
        let h = clip_residual(x, &self.post_attention_layernorm.forward(&r)?)?;
        let r = self
            .mlp
            .forward(&self.pre_feedforward_layernorm.forward(&h)?)?;
        clip_residual(&h, &self.post_feedforward_layernorm.forward(&r)?)
    }
}

/// Residual add matching `mlx_lm.models.gemma3_text.clip_residual`.
///
/// Gemma's late layers (here blocks 22-23) produce activations that exceed the
/// f16 range (±65504). A plain f16 add overflows to `inf`; mlx clips instead:
/// for f16 inputs it adds in f32, clamps to ±f16::MAX, then casts back to f16.
/// Skipping this diverged hard at the last two blocks (cos 0.98 → 1.0000).
///
/// NEGATIVE RESULT — do not re-try this. As written this is six mlx-rs calls
/// (two casts, add, two clamps, cast back), running twice per block over 24
/// blocks, which looks like ~288 kernel launches on a model that is
/// dispatch-bound at batch=1. Replacing it with one hand-written fused Metal
/// kernel via `mlx_fast_metal_kernel` was implemented and measured: numerically
/// EXACT (golden parity unchanged to the digit) and **flat on speed** —
/// batch=1 6.85 ms fused vs 6.88 ms graph, batch=16 17.7 ms vs 18.2 ms, both
/// inside run-to-run spread over 3 A/B pairs.
///
/// A follow-up probe found why, and bounds any future attempt: replacing this
/// whole chain with a bare `x.add(y)` — 1 op instead of 6, numerically wrong but
/// timing-valid — measured 6.60/6.87/6.65 ms against a 6.86/6.95/6.83 ms
/// baseline. **Deleting five sixths of the chain is worth ~3%, at the edge of
/// noise.** No fusion of it could ever have won more than that.
///
/// So the mistake was picking the target by counting ops rather than measuring
/// where time goes: mlx-rs calls build a lazy GRAPH, and op-count at the binding
/// layer is not GPU launch-count. The batch=1 cost lives in the quantized
/// matmuls and SDPA. Optimize there, or raise the batch size.
fn clip_residual(x: &Array, y: &Array) -> Result<Array> {
    if x.dtype() != Dtype::Float16 {
        return Ok(x.add(y)?);
    }
    let bound = 65504.0_f32; // f16::MAX
    let sum = x
        .as_dtype(Dtype::Float32)?
        .add(&y.as_dtype(Dtype::Float32)?)?;
    let clipped = minimum(
        &maximum(&sum, Array::from_f32(-bound))?,
        Array::from_f32(bound),
    )?;
    Ok(clipped.as_dtype(Dtype::Float16)?)
}
