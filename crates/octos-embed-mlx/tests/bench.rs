//! Retrieval-quality parity + MRL + latency benchmarks (P2).
//!
//! `#[ignore]` — needs Apple Silicon, the `embed-mlx` feature, and the cached
//! model. The labeled corpus + Python metrics live in `tests/golden/` (produced
//! by `scripts/retrieval_bench.py`).
//!
//! Run:
//! ```bash
//! cargo test -p octos-embed-mlx --features embed-mlx --test bench -- --ignored --nocapture
//! ```
#![cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use octos_embed_mlx::{MlxEmbedder, set_default_device_cpu, set_default_device_gpu};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn model_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OCTOS_EMBED_MODEL_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    let snaps = Path::new(&home)
        .join(".cache/huggingface/hub/models--mlx-community--embeddinggemma-300m-8bit/snapshots");
    fs::read_dir(&snaps)
        .unwrap_or_else(|e| panic!("model not cached at {snaps:?}: {e}; set OCTOS_EMBED_MODEL_DIR"))
        .filter_map(|e| e.ok())
        .next()
        .expect("no snapshot dir")
        .path()
}

fn json(name: &str) -> serde_json::Value {
    let s = fs::read_to_string(Path::new(GOLDEN).join(name)).unwrap();
    serde_json::from_str(&s).unwrap()
}

fn strs(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

/// Truncate to `dim` and L2-renormalize (matches the provider + Python MRL).
fn mrl(v: &[f32], dim: usize) -> Vec<f32> {
    let head = &v[..dim.min(v.len())];
    let norm = head.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    head.iter().map(|x| x / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 1-indexed rank of `target` doc when docs are sorted by descending similarity.
fn rank_of(sims: &[f32], target: usize) -> usize {
    let mut idx: Vec<usize> = (0..sims.len()).collect();
    idx.sort_by(|&a, &b| sims[b].partial_cmp(&sims[a]).unwrap());
    idx.iter().position(|&d| d == target).unwrap() + 1
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_match_python_retrieval_and_mrl() {
    let _g = serial();
    let corpus = json("retrieval_corpus.json");
    let py = json("retrieval_python.json");
    let queries = strs(&corpus["queries"]);
    let docs = strs(&corpus["docs"]);
    let relevant: Vec<usize> = corpus["relevant"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap() as usize)
        .collect();

    let embedder = MlxEmbedder::from_model_dir(model_dir()).unwrap();
    let q_refs: Vec<&str> = queries.iter().map(|s| s.as_str()).collect();
    let d_refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
    // Full-width (768) embeddings; MRL dims are truncations of these.
    let q_emb = embedder.embed_texts(&q_refs, true).unwrap();
    let d_emb = embedder.embed_texts(&d_refs, false).unwrap();

    println!(
        "\nretrieval parity ({} queries, {} docs) — Rust vs Python:",
        queries.len(),
        docs.len()
    );
    println!("  dim   NDCG@10(rust/py)   Recall@1(rust/py)   ranks-match");
    for dim in [768usize, 512, 256, 128] {
        let qd: Vec<Vec<f32>> = q_emb.iter().map(|v| mrl(v, dim)).collect();
        let dd: Vec<Vec<f32>> = d_emb.iter().map(|v| mrl(v, dim)).collect();

        let mut ndcg_sum = 0.0f64;
        let mut recall_sum = 0.0f64;
        let mut ranks = Vec::with_capacity(queries.len());
        for (i, q) in qd.iter().enumerate() {
            let sims: Vec<f32> = dd.iter().map(|d| dot(q, d)).collect();
            let rank = rank_of(&sims, relevant[i]);
            ranks.push(rank as i64);
            if rank <= 10 {
                ndcg_sum += 1.0 / ((rank + 1) as f64).log2();
            }
            if rank == 1 {
                recall_sum += 1.0;
            }
        }
        let n = queries.len() as f64;
        let (ndcg, recall) = (ndcg_sum / n, recall_sum / n);

        let py_m = &py["metrics"][dim.to_string()];
        let py_ndcg = py_m["ndcg@10"].as_f64().unwrap();
        let py_recall = py_m["recall@1"].as_f64().unwrap();
        let py_ranks: Vec<i64> = py["ranks"][dim.to_string()]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_i64().unwrap())
            .collect();
        let ranks_match = ranks == py_ranks;

        println!(
            "  {dim:<4}  {ndcg:.4} / {py_ndcg:.4}      {recall:.4} / {py_recall:.4}      {ranks_match}"
        );
        assert!(
            (ndcg - py_ndcg).abs() < 1e-4,
            "dim {dim}: NDCG@10 {ndcg} vs Python {py_ndcg}"
        );
        assert!(
            (recall - py_recall).abs() < 1e-4,
            "dim {dim}: Recall@1 {recall} vs Python {py_recall}"
        );
        assert!(ranks_match, "dim {dim}: per-query ranks differ from Python");
    }
    println!("Rust retrieval NDCG@10 / Recall@1 match Python at every MRL dim.");
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_expose_mrl_output_dimension() {
    let _g = serial();
    let base = MlxEmbedder::from_model_dir(model_dir()).unwrap();
    assert_eq!(base.output_dim(), 768);

    let e256 = MlxEmbedder::from_model_dir(model_dir())
        .unwrap()
        .with_output_dim(256);
    let out = e256
        .embed_texts(&["title: none | text: hello"], false)
        .unwrap();
    assert_eq!(e256.output_dim(), 256);
    assert_eq!(out[0].len(), 256);
    // MRL vector is unit-norm after truncation.
    let norm: f32 = out[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-3,
        "MRL vector not unit-norm: {norm}"
    );
    println!("MRL provider path: 768 -> 256 truncation is unit-norm, dimension()=256");
}

#[test]
#[ignore = "latency benchmark — Metal (GPU) + MLX-CPU ms/embedding"]
fn bench_latency_metal_and_cpu() {
    let _g = serial();
    let embedder = MlxEmbedder::from_model_dir(model_dir()).unwrap();

    let probe = ["title: none | text: a short latency probe sentence for timing"];
    let bench = |label: &str, warmup: usize, n: usize| {
        for _ in 0..warmup {
            embedder.embed_texts(&probe, false).unwrap();
        }
        let t0 = Instant::now();
        for _ in 0..n {
            embedder.embed_texts(&probe, false).unwrap();
        }
        let per_ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("  {label:<12} {per_ms:.2} ms/embedding (warm, batch=1, n={n})");
        per_ms
    };

    println!("\nlatency (single embedding):");
    set_default_device_gpu();
    let gpu = bench("Metal/GPU", 5, 50);

    // The MLX-CPU 8-bit quantized-matmul path is far slower than Metal, so the
    // CPU sample is opt-in (set OCTOS_EMBED_BENCH_CPU=1) to keep the default run
    // fast. Reference from a direct run: ~312 ms/embedding.
    if std::env::var("OCTOS_EMBED_BENCH_CPU").is_ok() {
        set_default_device_cpu();
        let cpu = bench("MLX-CPU", 1, 10);
        set_default_device_gpu(); // restore
        println!("Metal ~{gpu:.2} ms/embed, CPU ~{cpu:.2} ms/embed");
    } else {
        println!("Metal ~{gpu:.2} ms/embed (CPU sample skipped; set OCTOS_EMBED_BENCH_CPU=1)");
    }
    // Loose guard: Metal path should be well under 100ms/embed on Apple Silicon.
    assert!(gpu < 100.0, "Metal latency unexpectedly high: {gpu} ms");
}
