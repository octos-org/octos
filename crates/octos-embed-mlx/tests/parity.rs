//! Numeric-parity tests against the Python golden oracle (`golden_dump.py`).
//!
//! These are `#[ignore]` because they need (a) Apple Silicon, (b) the
//! `embed-mlx` feature, and (c) the cached model
//! `mlx-community/embeddinggemma-300m-8bit`. The golden tensors live in
//! `tests/golden/` (committed) and were produced by the SAME 8-bit weights.
//!
//! Run:
//! ```bash
//! # model auto-resolved from ~/.cache/huggingface, or set OCTOS_EMBED_MODEL_DIR
//! cargo test -p octos-embed-mlx --features embed-mlx --test parity -- --ignored --nocapture
//! ```
#![cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use octos_embed_mlx::{GemmaModel, GemmaTokenizer};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

/// MLX/Metal is not safe to drive from multiple test threads at once (it aborts
/// with SIGABRT). Serialize model-using tests so `cargo test ... --ignored`
/// works without `--test-threads=1`. Poison is ignored — a panicking test must
/// not wedge the rest.
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- helpers --------------------------------------------------------

/// Resolve the model dir: `$OCTOS_EMBED_MODEL_DIR`, else the first HF snapshot.
fn model_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OCTOS_EMBED_MODEL_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    let snaps = Path::new(&home).join(
        ".cache/huggingface/hub/models--mlx-community--embeddinggemma-300m-8bit/snapshots",
    );
    let entry = fs::read_dir(&snaps)
        .unwrap_or_else(|e| panic!("model not cached at {snaps:?}: {e}; set OCTOS_EMBED_MODEL_DIR"))
        .filter_map(|e| e.ok())
        .next()
        .expect("no snapshot dir");
    entry.path()
}

fn meta() -> serde_json::Value {
    let s = fs::read_to_string(Path::new(GOLDEN).join("meta.json")).expect("meta.json");
    serde_json::from_str(&s).expect("parse meta.json")
}

/// Minimal safetensors reader — extracts every F32 tensor as a flat `Vec<f32>`.
fn load_st_f32(path: &Path) -> HashMap<String, Vec<f32>> {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let hlen = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    let header: serde_json::Value = serde_json::from_slice(&bytes[8..8 + hlen]).unwrap();
    let data = &bytes[8 + hlen..];
    let mut out = HashMap::new();
    for (name, info) in header.as_object().unwrap() {
        if name == "__metadata__" {
            continue;
        }
        if info["dtype"].as_str() != Some("F32") {
            continue;
        }
        let offs = info["data_offsets"].as_array().unwrap();
        let (s, e) = (
            offs[0].as_u64().unwrap() as usize,
            offs[1].as_u64().unwrap() as usize,
        );
        let v = data[s..e]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        out.insert(name.clone(), v);
    }
    out
}

fn token_ids(m: &serde_json::Value, i: usize) -> Vec<i32> {
    m["token_ids"][i]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap() as i32)
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "length mismatch {} vs {}", a.len(), b.len());
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-12)
}

/// Relative L2: ‖a-b‖ / ‖b‖.
fn rel_l2(a: &[f32], b: &[f32]) -> f32 {
    let num: f32 = a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f32>().sqrt();
    let den: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    num / den
}

// ---------- tests ----------------------------------------------------------

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_match_tokenizer_ids_when_encoding_prompts() {
    let _g = serial();
    let m = meta();
    let tok = GemmaTokenizer::from_dir(&model_dir()).unwrap();
    let eval = m["eval"].as_array().unwrap();
    for (i, item) in eval.iter().enumerate() {
        let text = item["text"].as_str().unwrap();
        let role = item["role"].as_str().unwrap();
        let got = if role == "query" {
            tok.encode_query(text).unwrap()
        } else {
            tok.encode_document(text).unwrap()
        };
        let want = token_ids(&m, i);
        assert_eq!(got, want, "token ids differ for eval[{i}] ({role}): {text:?}");
    }
    println!("tokenizer: all {} eval prompts match Python ids", eval.len());
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_match_per_stage_activations_when_probe() {
    let _g = serial();
    let m = meta();
    let probe = m["probe_idx"].as_u64().unwrap() as usize;
    let ids = token_ids(&m, probe);

    let model = GemmaModel::from_dir(&model_dir()).unwrap();
    let taps = model.forward_taps(&ids).unwrap();
    let golden = load_st_f32(&Path::new(GOLDEN).join("intermediates.safetensors"));

    // Stages in forward order. embed_scaled/block0/... are localisers.
    let stages = [
        "embed_scaled",
        "block0",
        "blocks_all",
        "final_norm",
        "pooled",
        "dense0",
        "dense1",
        "normalized",
    ];
    println!("\nper-stage parity (probe idx {probe}, {} tokens):", ids.len());
    let mut worst_cos = 1.0f32;
    for stage in stages {
        let got = taps.get(stage).unwrap_or_else(|| panic!("missing tap {stage}"));
        let want = golden.get(stage).unwrap_or_else(|| panic!("missing golden {stage}"));
        let cos = cosine(got, want);
        let rl2 = rel_l2(got, want);
        println!("  {stage:<13} cos={cos:.6}  relL2={rl2:.2e}  (n={})", got.len());
        worst_cos = worst_cos.min(cos);
        assert!(cos >= 0.999, "stage {stage}: cosine {cos} < 0.999");
        assert!(rl2 < 5e-3, "stage {stage}: relL2 {rl2} >= 5e-3");
    }
    println!("worst per-stage cosine = {worst_cos:.6}");
}

#[test]
#[ignore = "diagnostic: per-block cosine drift localiser"]
fn diag_per_block_drift() {
    let _g = serial();
    let m = meta();
    let probe = m["probe_idx"].as_u64().unwrap() as usize;
    let ids = token_ids(&m, probe);
    let model = GemmaModel::from_dir(&model_dir()).unwrap();
    let taps = model.forward_taps(&ids).unwrap();
    let golden = load_st_f32(&Path::new(GOLDEN).join("intermediates.safetensors"));
    println!("\nper-block drift:");
    for i in 0..24 {
        let k = format!("block_{i}");
        if let (Some(g), Some(w)) = (taps.get(&k), golden.get(&k)) {
            println!("  block_{i:<2} cos={:.6} relL2={:.2e}", cosine(g, w), rel_l2(g, w));
        }
    }
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_match_end_to_end_embeddings_when_eval_set() {
    let _g = serial();
    let m = meta();
    let model = GemmaModel::from_dir(&model_dir()).unwrap();
    let golden = load_st_f32(&Path::new(GOLDEN).join("eval_embeds.safetensors"));
    let n = m["eval"].as_array().unwrap().len();

    println!("\nend-to-end parity (Rust embed_ids vs Python text_embeds):");
    let mut worst = 1.0f32;
    for i in 0..n {
        let ids = token_ids(&m, i);
        let got = model.embed_ids(&ids).unwrap();
        let want = golden.get(&format!("emb_{i}")).unwrap();
        let cos = cosine(&got, want);
        let rl2 = rel_l2(&got, want);
        println!("  emb_{i:<2} cos={cos:.6}  relL2={rl2:.2e}");
        worst = worst.min(cos);
        assert!(cos >= 0.9999, "emb_{i}: cosine {cos} < 0.9999");
    }
    println!("worst end-to-end cosine = {worst:.6} across {n} sentences");
}
