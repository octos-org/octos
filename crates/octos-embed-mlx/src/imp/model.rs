//! Full EmbeddingGemma forward: embed·scale → 24 blocks → final norm →
//! mean-pool → dense.0 → dense.1 → L2 normalize.
//!
//! SentenceTransformers pipeline (from modules.json): Transformer → Pooling(mean)
//! → Dense.0 (768→3072, no activation) → Dense.1 (3072→768, no activation) →
//! Normalize. The two Dense layers are plain linear projections (Identity
//! activation) — confirmed against `mlx_embeddings`' `self.dense`.

use std::collections::HashMap;
use std::path::Path;

use eyre::{Result, WrapErr};
use mlx_rs::ops::maximum;
use mlx_rs::{Array, Dtype};

use super::block::Block;
use super::config::GemmaConfig;
use super::norm::RmsNorm;
use super::weights::{QuantizedEmbedding, QuantizedLinear, load_safetensors};

/// Filler token for right-padding a ragged batch. Gemma's `<pad>` is id 0;
/// padded positions are masked out of both attention and pooling, so the value
/// only has to be in-vocabulary.
const PAD_ID: i32 = 0;

/// Additive attention-mask value for padded key positions. Deliberately
/// `-f16::MAX` rather than `-inf`: mlx runs the softmax in f32, where
/// `exp(-65504)` underflows to exactly 0, so padded keys contribute nothing —
/// but an all-masked row degrades to a uniform average instead of NaN.
const MASK_NEG: f32 = -65504.0;

pub struct GemmaModel {
    embed: QuantizedEmbedding,
    blocks: Vec<Block>,
    final_norm: RmsNorm,
    dense0: QuantizedLinear,
    dense1: QuantizedLinear,
    embed_scale_f16: Array,
    hidden: i32,
    dim: usize,
}

impl GemmaModel {
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let cfg_str =
            std::fs::read_to_string(dir.join("config.json")).wrap_err("reading config.json")?;
        let cfg = GemmaConfig::from_json_str(&cfg_str)?;
        let weights = load_safetensors(dir)?;

        let g = cfg.quant_group_size;
        let b = cfg.quant_bits;

        let embed = QuantizedEmbedding::load(&weights, "model.embed_tokens", g, b)?;
        let mut blocks = Vec::with_capacity(cfg.num_hidden_layers);
        for i in 0..cfg.num_hidden_layers {
            blocks.push(Block::load(&weights, &cfg, i)?);
        }
        let final_norm = RmsNorm::new(
            weights
                .get("model.norm.weight")
                .ok_or_else(|| eyre::eyre!("missing model.norm.weight"))?,
            cfg.rms_norm_eps,
        )?;
        let dense0 = QuantizedLinear::load(&weights, "dense.0", g, b)?;
        let dense1 = QuantizedLinear::load(&weights, "dense.1", g, b)?;

        Ok(Self {
            embed,
            blocks,
            final_norm,
            dense0,
            dense1,
            embed_scale_f16: Array::from_f32(cfg.embed_scale()).as_dtype(Dtype::Float16)?,
            hidden: cfg.hidden_size,
            dim: cfg.hidden_size as usize,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Run the transformer stack, returning the post-final-norm hidden states
    /// `[B, L, hidden]` for the flattened, right-padded token ids `ids_flat`.
    ///
    /// `mask` is `None` for an unpadded batch (every sequence the same length),
    /// which is the exact code path the golden parity tests validated.
    fn hidden_states(
        &self,
        ids_flat: &[i32],
        b: i32,
        l: i32,
        mask: Option<&Array>,
    ) -> Result<Array> {
        let ids_arr = Array::from_slice(ids_flat, &[b * l]);
        let embedded = self
            .embed
            .gather(&ids_arr)?
            .reshape(&[b, l, self.hidden])?
            .as_dtype(Dtype::Float16)?;
        let mut h = embedded.multiply(&self.embed_scale_f16)?;
        for block in &self.blocks {
            h = block.forward(&h, mask)?;
        }
        self.final_norm.forward(&h)
    }

    /// Full embedding for one sequence → L2-normalized `[dim]` vector.
    pub fn embed_ids(&self, ids: &[i32]) -> Result<Vec<f32>> {
        let batch = [ids.to_vec()];
        self.embed_batch(&batch)?
            .pop()
            .ok_or_else(|| eyre::eyre!("empty embedding batch"))
    }

    /// Embed a whole batch in ONE forward pass → one L2-normalized `[dim]`
    /// vector per input, in input order.
    ///
    /// Sequences are right-padded to the longest in the batch. When they are
    /// already uniform (notably `len == 1`) no mask is built and the arithmetic
    /// is identical to the single-sequence path. Otherwise padded key positions
    /// get an additive attention mask and mean-pooling divides by the true
    /// token count, so a sequence's embedding does not depend on what it was
    /// batched with.
    pub fn embed_batch(&self, batch: &[Vec<i32>]) -> Result<Vec<Vec<f32>>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(i) = batch.iter().position(|ids| ids.is_empty()) {
            eyre::bail!("batch[{i}] has no tokens");
        }

        let b = batch.len();
        let max_l = batch.iter().map(Vec::len).max().unwrap_or(0);
        let uniform = batch.iter().all(|ids| ids.len() == max_l);

        let mut flat = Vec::with_capacity(b * max_l);
        for ids in batch {
            flat.extend_from_slice(ids);
            flat.resize(flat.len() + (max_l - ids.len()), PAD_ID);
        }

        // `keep` is the [B, L] 1/0 token mask; `additive` is its attention form.
        let (attn_mask, pool_mask) = if uniform {
            (None, None)
        } else {
            let mut keep = Vec::with_capacity(b * max_l);
            for ids in batch {
                keep.extend((0..max_l).map(|i| if i < ids.len() { 1.0f32 } else { 0.0 }));
            }
            let additive: Vec<f32> = keep
                .iter()
                .map(|&k| if k > 0.0 { 0.0 } else { MASK_NEG })
                .collect();
            let b32 = b as i32;
            let l32 = max_l as i32;
            (
                // [B, 1, 1, L] broadcasts over heads and query positions.
                Some(Array::from_slice(&additive, &[b32, 1, 1, l32]).as_dtype(Dtype::Float16)?),
                Some(Array::from_slice(&keep, &[b32, l32, 1])),
            )
        };

        let h = self.hidden_states(&flat, b as i32, max_l as i32, attn_mask.as_ref())?;
        // mean-pool in f32: mlx promotes here (f16 tokens × f32 mask), and the
        // SentenceTransformers head (dense.0/dense.1/normalize) runs in f32.
        let h32 = h.as_dtype(Dtype::Float32)?;
        let pooled = match &pool_mask {
            None => h32.mean_axes(&[1], false)?, // [B, hidden]
            Some(m) => {
                let summed = h32.multiply(m)?.sum_axes(&[1], false)?; // [B, hidden]
                let counts = m.sum_axes(&[1], false)?; // [B, 1]
                summed.divide(&counts)?
            }
        };
        let d0 = self.dense0.forward(&pooled)?; // [B, 4*hidden]
        let d1 = self.dense1.forward(&d0)?; // [B, hidden]
        let normed = l2_normalize(&d1)?;
        normed.eval()?;
        let out: Vec<Vec<f32>> = normed
            .as_slice::<f32>()
            .chunks(self.dim)
            .map(<[f32]>::to_vec)
            .collect();
        if out.len() != b {
            eyre::bail!("batched forward returned {} rows, expected {b}", out.len());
        }
        Ok(out)
    }

    /// Per-stage intermediates for parity testing (keys match the golden dump).
    /// Each value is flattened to a `Vec<f32>` for the probe (batch=1) sequence.
    pub fn forward_taps(&self, ids: &[i32]) -> Result<HashMap<String, Vec<f32>>> {
        let l = ids.len() as i32;
        let ids_arr = Array::from_slice(ids, &[l]);
        let mut taps: HashMap<String, Vec<f32>> = HashMap::new();

        let mut h = self
            .embed
            .gather(&ids_arr)?
            .reshape(&[1, l, self.hidden])?
            .as_dtype(Dtype::Float16)?
            .multiply(&self.embed_scale_f16)?;
        taps.insert("embed_scaled".into(), to_vec(&h)?);

        for (i, block) in self.blocks.iter().enumerate() {
            h = block.forward(&h, None)?;
            taps.insert(format!("block_{i}"), to_vec(&h)?);
            if i == 0 {
                taps.insert("block0".into(), to_vec(&h)?);
            }
        }
        taps.insert("blocks_all".into(), to_vec(&h)?);

        h = self.final_norm.forward(&h)?;
        taps.insert("final_norm".into(), to_vec(&h)?);

        let pooled = h.as_dtype(Dtype::Float32)?.mean_axes(&[1], false)?;
        taps.insert("pooled".into(), to_vec(&pooled)?);

        let d0 = self.dense0.forward(&pooled)?;
        taps.insert("dense0".into(), to_vec(&d0)?);

        let d1 = self.dense1.forward(&d0)?;
        taps.insert("dense1".into(), to_vec(&d1)?);

        let normed = l2_normalize(&d1)?;
        taps.insert("normalized".into(), to_vec(&normed)?);

        Ok(taps)
    }
}

/// L2-normalize along the last axis with eps=1e-9 (matches `normalize_embeddings`).
fn l2_normalize(x: &Array) -> Result<Array> {
    let norm = x.square()?.sum_axes(&[-1], true)?.sqrt()?;
    let denom = maximum(&norm, Array::from_f32(1e-9))?;
    Ok(x.divide(&denom)?)
}

/// Extract a flat `Vec<f32>`, upcasting f16 activations so the raw bytes are
/// interpreted correctly (the golden intermediates are stored as f32).
fn to_vec(a: &Array) -> Result<Vec<f32>> {
    let a = a.as_dtype(Dtype::Float32)?;
    a.eval()?;
    Ok(a.as_slice::<f32>().to_vec())
}
