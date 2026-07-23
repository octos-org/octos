//! Gemma3 self-attention (GQA), bidirectional / no cache.
//!
//! Mirrors `mlx_lm.models.gemma3_text.Attention.__call__`:
//! ```text
//! q = q_proj(x).reshape(B,L,H,hd).T(0,2,1,3);  q = q_norm(q); q = rope(q)
//! k = k_proj(x).reshape(B,L,Hkv,hd).T(0,2,1,3); k = k_norm(k); k = rope(k)
//! v = v_proj(x).reshape(B,L,Hkv,hd).T(0,2,1,3)
//! o = sdpa(q, k, v, scale=query_pre_attn_scalar**-0.5, mask)
//! o = o.T(0,2,1,3).reshape(B,L,H*hd);  o_proj(o)
//! ```
//! q_norm/k_norm are per-head RMSNorm over `head_dim`, applied BEFORE RoPE.
//! The RoPE base is per layer (sliding=10000, global=1e6). For the batch=1,
//! unpadded encoder path the attention mask is all-visible, so we pass `None`
//! (bidirectional full attention) — validated against the golden oracle.

use std::collections::HashMap;

use eyre::Result;
use mlx_rs::Array;
use mlx_rs::fast::{rope, scaled_dot_product_attention};

use super::config::GemmaConfig;
use super::norm::RmsNorm;
use super::weights::QuantizedLinear;

pub struct Attention {
    q_proj: QuantizedLinear,
    k_proj: QuantizedLinear,
    v_proj: QuantizedLinear,
    o_proj: QuantizedLinear,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    n_heads: i32,
    n_kv_heads: i32,
    head_dim: i32,
    scale: f32,
    rope_base: f32,
}

impl Attention {
    pub fn load(weights: &HashMap<String, Array>, cfg: &GemmaConfig, layer: usize) -> Result<Self> {
        let base = format!("model.layers.{layer}.self_attn");
        let g = cfg.quant_group_size;
        let b = cfg.quant_bits;
        Ok(Self {
            q_proj: QuantizedLinear::load(weights, &format!("{base}.q_proj"), g, b)?,
            k_proj: QuantizedLinear::load(weights, &format!("{base}.k_proj"), g, b)?,
            v_proj: QuantizedLinear::load(weights, &format!("{base}.v_proj"), g, b)?,
            o_proj: QuantizedLinear::load(weights, &format!("{base}.o_proj"), g, b)?,
            q_norm: RmsNorm::new(
                weights
                    .get(&format!("{base}.q_norm.weight"))
                    .ok_or_else(|| eyre::eyre!("missing {base}.q_norm.weight"))?,
                cfg.rms_norm_eps,
            )?,
            k_norm: RmsNorm::new(
                weights
                    .get(&format!("{base}.k_norm.weight"))
                    .ok_or_else(|| eyre::eyre!("missing {base}.k_norm.weight"))?,
                cfg.rms_norm_eps,
            )?,
            n_heads: cfg.num_attention_heads,
            n_kv_heads: cfg.num_key_value_heads,
            head_dim: cfg.head_dim,
            scale: cfg.attn_scale(),
            rope_base: cfg.rope_base(layer),
        })
    }

    /// `x`: `[B, L, hidden]` → `[B, L, hidden]`.
    pub fn forward(&self, x: &Array) -> Result<Array> {
        let shape = x.shape();
        let (b, l) = (shape[0], shape[1]);

        let q = self
            .q_proj
            .forward(x)?
            .reshape(&[b, l, self.n_heads, self.head_dim])?
            .transpose_axes(&[0, 2, 1, 3])?;
        let q = self.q_norm.forward(&q)?;
        let q = rope(&q, self.head_dim, false, self.rope_base, 1.0, 0, None)?;

        let k = self
            .k_proj
            .forward(x)?
            .reshape(&[b, l, self.n_kv_heads, self.head_dim])?
            .transpose_axes(&[0, 2, 1, 3])?;
        let k = self.k_norm.forward(&k)?;
        let k = rope(&k, self.head_dim, false, self.rope_base, 1.0, 0, None)?;

        let v = self
            .v_proj
            .forward(x)?
            .reshape(&[b, l, self.n_kv_heads, self.head_dim])?
            .transpose_axes(&[0, 2, 1, 3])?;

        let o = scaled_dot_product_attention(&q, &k, &v, self.scale, None)?;
        let o = o.transpose_axes(&[0, 2, 1, 3])?.reshape(&[b, l, -1])?;
        self.o_proj.forward(&o)
    }
}
