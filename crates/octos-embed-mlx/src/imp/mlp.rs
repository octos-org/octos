//! Gemma GeGLU MLP: `down(gelu_tanh(gate(x)) * up(x))`.
//!
//! `gelu_approximate` is the tanh ("pytorch_tanh") GELU — the same function
//! `mlx_lm` uses via `nn.gelu_approx`.

use std::collections::HashMap;

use eyre::Result;
use mlx_rs::Array;
use mlx_rs::nn::gelu_approximate;

use super::config::GemmaConfig;
use super::weights::QuantizedLinear;

pub struct Mlp {
    gate: QuantizedLinear,
    up: QuantizedLinear,
    down: QuantizedLinear,
}

impl Mlp {
    pub fn load(weights: &HashMap<String, Array>, cfg: &GemmaConfig, layer: usize) -> Result<Self> {
        let base = format!("model.layers.{layer}.mlp");
        let g = cfg.quant_group_size;
        let b = cfg.quant_bits;
        Ok(Self {
            gate: QuantizedLinear::load(weights, &format!("{base}.gate_proj"), g, b)?,
            up: QuantizedLinear::load(weights, &format!("{base}.up_proj"), g, b)?,
            down: QuantizedLinear::load(weights, &format!("{base}.down_proj"), g, b)?,
        })
    }

    pub fn forward(&self, x: &Array) -> Result<Array> {
        // `mlx-rs`'s `gelu_approximate` multiplies by 0-d f32 constant arrays,
        // which PROMOTES an f16 input to f32 (mlx-python keeps f16 via weak
        // scalars). Cast the gelu result back to the input dtype so the gated
        // product and `down_proj` stay f16 — otherwise f32 leaks through the MLP
        // and breaks parity with the f16 oracle.
        let gate = gelu_approximate(&self.gate.forward(x)?)?.as_dtype(x.dtype())?;
        let up = self.up.forward(x)?;
        let gated = gate.multiply(&up)?;
        self.down.forward(&gated)
    }
}
