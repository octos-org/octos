//! Weight loading + quantized primitives (8-bit affine, group_size 64).
//!
//! The 8-bit MLX conversion stores every projection as three tensors:
//! `{prefix}.weight` (packed `u32`), `{prefix}.scales` and `{prefix}.biases`
//! (`f16`). We keep them as-is and run `quantized_matmul` / `dequantize`
//! directly — no dequantize-to-dense step, so the arithmetic matches
//! `mlx_embeddings` bit-for-bit.

use std::collections::HashMap;
use std::path::Path;

use eyre::{Result, eyre};
use mlx_rs::Array;
use mlx_rs::ops::indexing::IndexOp;
use mlx_rs::ops::{dequantize, quantized_matmul};

/// Load every tensor from the single `model.safetensors`.
pub fn load_safetensors(model_dir: &Path) -> Result<HashMap<String, Array>> {
    let path = model_dir.join("model.safetensors");
    Array::load_safetensors(&path).map_err(|e| eyre!("load_safetensors({path:?}): {e}"))
}

fn get(weights: &HashMap<String, Array>, key: &str) -> Result<Array> {
    weights
        .get(key)
        .cloned()
        .ok_or_else(|| eyre!("missing weight tensor: {key}"))
}

/// A pre-quantized affine linear projection: `y = quantized_matmul(x, W, s, b)`.
/// The stored weight is `[out, in]` packed, so `transpose = true`.
pub struct QuantizedLinear {
    weight: Array,
    scales: Array,
    biases: Array,
    group_size: i32,
    bits: i32,
}

impl QuantizedLinear {
    pub fn load(
        weights: &HashMap<String, Array>,
        prefix: &str,
        group_size: i32,
        bits: i32,
    ) -> Result<Self> {
        Ok(Self {
            weight: get(weights, &format!("{prefix}.weight"))?,
            scales: get(weights, &format!("{prefix}.scales"))?,
            biases: get(weights, &format!("{prefix}.biases"))?,
            group_size,
            bits,
        })
    }

    pub fn forward(&self, x: &Array) -> Result<Array> {
        Ok(quantized_matmul(
            x,
            &self.weight,
            &self.scales,
            &self.biases,
            true, // transpose: stored weight is [out, in]
            self.group_size,
            self.bits,
        )?)
    }
}

/// A pre-quantized token embedding table. Lookup gathers the packed rows for the
/// given token ids and dequantizes them (mirrors `QuantizedEmbedding.forward`).
pub struct QuantizedEmbedding {
    weight: Array,
    scales: Array,
    biases: Array,
    group_size: i32,
    bits: i32,
}

impl QuantizedEmbedding {
    pub fn load(
        weights: &HashMap<String, Array>,
        prefix: &str,
        group_size: i32,
        bits: i32,
    ) -> Result<Self> {
        Ok(Self {
            weight: get(weights, &format!("{prefix}.weight"))?,
            scales: get(weights, &format!("{prefix}.scales"))?,
            biases: get(weights, &format!("{prefix}.biases"))?,
            group_size,
            bits,
        })
    }

    /// `ids` is a 1-D `i32` array of token indices → returns `[len(ids), hidden]`.
    pub fn gather(&self, ids: &Array) -> Result<Array> {
        let w = self.weight.index(ids);
        let s = self.scales.index(ids);
        let b = self.biases.index(ids);
        Ok(dequantize(&w, &s, &b, self.group_size, self.bits)?)
    }
}
