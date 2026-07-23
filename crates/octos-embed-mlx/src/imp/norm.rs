//! Gemma RMSNorm.
//!
//! `mlx_lm.models.gemma3_text.RMSNorm` is `mx.fast.rms_norm(x, 1.0 + weight, eps)`
//! — the learnable gamma is stored centered at zero and used as `(1 + weight)`.
//! We call the same fused `fast::rms_norm` (which upcasts to f32 internally) with
//! `(1 + weight)` precomputed at load, so the norm matches the oracle exactly.

use eyre::Result;
use mlx_rs::fast::rms_norm;
use mlx_rs::{Array, Dtype};

pub struct RmsNorm {
    one_plus_weight: Array,
    eps: f32,
}

impl RmsNorm {
    pub fn new(weight: &Array, eps: f32) -> Result<Self> {
        // Gemma stores gamma centered at zero; the effective weight is
        // `1.0 + weight`. The reference runs the transformer in f16 with f16 norm
        // weights, so we force f16 here (mlx-rs `load_safetensors` upcasts the
        // stored f16 tensors to f32 — casting back keeps `fast::rms_norm` f16 and
        // in numeric lockstep with the oracle).
        let one = Array::from_f32(1.0);
        Ok(Self {
            one_plus_weight: weight.add(&one)?.as_dtype(Dtype::Float16)?,
            eps,
        })
    }

    pub fn forward(&self, x: &Array) -> Result<Array> {
        Ok(rms_norm(x, &self.one_plus_weight, self.eps)?)
    }
}
