//! Cross-backend agreement: llama.cpp vs the MLX port.
//!
//! `octos-embed-mlx` is verified against a Python oracle — golden per-stage
//! activations, worst end-to-end cosine 0.999997, NDCG@10 1.0000 vs the
//! reference at every MRL dim. It is, in effect, a known-good implementation of
//! this exact model. Showing that the llama.cpp provider agrees with it
//! transfers that evidence rather than re-deriving it, and it is the only check
//! here that could catch a *semantic* mistake — wrong pooling, a missing task
//! prefix, an un-normalized vector — as opposed to a crash.
//!
//! The two are NOT expected to be bit-identical: they run different 8-bit
//! quantizations of the same weights (MLX affine group-64 vs GGUF Q8_0) through
//! different kernels. So the assertions are about the properties that actually
//! matter downstream — direction and ranking — not exact equality.
//!
//! # The vectors are NOT interchangeable
//!
//! Measured agreement is 0.962–0.991 cosine — high enough to prove the two
//! implement the same semantics, and far too low to mix in one index. A stored
//! embedding produced by one backend, compared against a query embedded by the
//! other, carries ~0.02–0.04 of pure backend noise, which is the same order as
//! the gap between genuinely related documents.
//!
//! So switching backends invalidates a populated index exactly the way
//! switching models would: the episodes must be re-embedded. This is why the
//! agreement here is asserted on RANKING (which survives the drift) rather than
//! on vector equality (which does not).
//!
//! Run (Apple Silicon, both models cached):
//! ```bash
//! cargo test -p octos-embed-llama --features cross-backend --test cross_backend -- --ignored --nocapture
//! ```
#![cfg(feature = "cross-backend")]

use std::path::PathBuf;

use octos_embed_llama::LlamaEmbedder;
use octos_embed_mlx::MlxEmbedder;

fn gguf() -> PathBuf {
    if let Ok(p) = std::env::var("OCTOS_EMBED_GGUF") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    let base = PathBuf::from(home)
        .join(".cache/huggingface/hub/models--ggml-org--embeddinggemma-300M-GGUF/snapshots");
    std::fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("GGUF not cached at {base:?}: {e}; set OCTOS_EMBED_GGUF"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find_map(|p| {
            std::fs::read_dir(&p)
                .ok()?
                .filter_map(|e| e.ok())
                .find_map(|e| {
                    let p = e.path();
                    (p.extension().is_some_and(|x| x == "gguf")).then_some(p)
                })
        })
        .expect("no .gguf under the snapshot dir")
}

fn mlx_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OCTOS_EMBED_MODEL_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME");
    let base = PathBuf::from(home)
        .join(".cache/huggingface/hub/models--mlx-community--embeddinggemma-300m-8bit/snapshots");
    std::fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("MLX model not cached at {base:?}: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("model.safetensors").exists())
        .expect("no snapshot with model.safetensors")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "backends disagree on dimension");
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-9)
}

/// Deliberately varied: lengths from 1 token to several hundred, plus a
/// near-duplicate pair, so a pooling or prefix bug cannot hide behind uniform
/// inputs.
fn corpus() -> Vec<String> {
    vec![
        "octos".into(),
        "BM25 is a lexical ranking function.".into(),
        "BM25 is a keyword ranking function.".into(),
        "Grep needs no index at all, so it stays fresh as files change.".into(),
        "The mitochondria is the powerhouse of the cell.".into(),
        "Re-embedding on every commit is the main maintenance cost of a vector database.".into(),
        "The quick brown fox jumps over the lazy dog. ".repeat(20),
        "Apple Silicon runs MLX on the unified memory architecture, which removes \
         the host-to-device copy that dominates discrete GPU inference."
            .into(),
    ]
}

fn both_backends() -> (LlamaEmbedder, MlxEmbedder) {
    let llama = LlamaEmbedder::from_model_file(gguf(), 99).expect("load GGUF");
    let mlx = MlxEmbedder::from_model_dir(mlx_dir()).expect("load MLX model");
    assert_eq!(
        llama.native_dim(),
        mlx.native_dim(),
        "backends must agree on the native dimension"
    );
    (llama, mlx)
}

/// Same text through both backends must point the same direction. Different
/// quantizations, so not bit-identical — but a semantic bug (mean vs CLS
/// pooling, dropped prefix, missing normalization) would show up here as a
/// cosine nowhere near 1.
#[test]
#[ignore = "needs Apple Silicon + both models cached + --features cross-backend"]
fn should_agree_on_direction_for_the_same_text() {
    let (llama, mlx) = both_backends();
    let owned = corpus();
    let texts: Vec<&str> = owned.iter().map(String::as_str).collect();

    let a = llama.embed_texts(&texts, false).expect("llama");
    let b = mlx.embed_texts(&texts, false).expect("mlx");

    println!("\nper-text agreement (llama.cpp Q8_0 vs MLX 8-bit):");
    let mut worst = 1.0f32;
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        let cos = cosine(x, y);
        println!("  text_{i} (len {:>4})  cos={cos:.5}", owned[i].len());
        worst = worst.min(cos);
    }
    println!("worst = {worst:.5}");
    assert!(
        worst >= 0.95,
        "backends diverge on direction (worst cosine {worst:.5}) — suspect pooling, \
         prefix, or normalization rather than quantization noise"
    );
}

/// The property that actually matters: both backends must retrieve the same
/// document for the same query. Absolute vectors may drift with quantization;
/// the ranking must not.
#[test]
#[ignore = "needs Apple Silicon + both models cached + --features cross-backend"]
fn should_produce_the_same_retrieval_ranking() {
    let (llama, mlx) = both_backends();
    let owned = corpus();
    let docs: Vec<&str> = owned.iter().map(String::as_str).collect();

    let queries = [
        "how do I stop my search index going stale?",
        "what does a cell use for energy?",
        "keyword based document ranking",
        "why is unified memory good for inference?",
    ];

    let l_docs = llama.embed_texts(&docs, false).expect("llama docs");
    let m_docs = mlx.embed_texts(&docs, false).expect("mlx docs");
    let l_q = llama.embed_texts(&queries, true).expect("llama queries");
    let m_q = mlx.embed_texts(&queries, true).expect("mlx queries");

    let rank = |q: &[f32], d: &[Vec<f32>]| -> Vec<usize> {
        let mut idx: Vec<usize> = (0..d.len()).collect();
        idx.sort_by(|&x, &y| {
            cosine(q, &d[y])
                .partial_cmp(&cosine(q, &d[x]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        idx
    };

    println!("\nretrieval agreement:");
    for (i, query) in queries.iter().enumerate() {
        let rl = rank(&l_q[i], &l_docs);
        let rm = rank(&m_q[i], &m_docs);
        println!("  q{i}: llama top3={:?}  mlx top3={:?}", &rl[..3], &rm[..3]);
        assert_eq!(
            rl[0], rm[0],
            "backends retrieve different top-1 for {query:?}: \
             llama picked {:?}, mlx picked {:?}",
            owned[rl[0]], owned[rm[0]]
        );
    }
}

/// A query must be closer to its answer than to a random document under BOTH
/// backends. Catches the case where the two agree with each other but are both
/// wrong — e.g. if the task prefixes were dropped from both.
#[test]
#[ignore = "needs Apple Silicon + both models cached + --features cross-backend"]
fn should_both_separate_relevant_from_irrelevant() {
    let (llama, mlx) = both_backends();
    let query = "what does a cell use for energy?";
    let relevant = "The mitochondria is the powerhouse of the cell.";
    let irrelevant = "BM25 is a lexical ranking function.";

    for (name, (q, d)) in [
        (
            "llama.cpp",
            (
                llama.embed_texts(&[query], true).expect("q").remove(0),
                llama
                    .embed_texts(&[relevant, irrelevant], false)
                    .expect("d"),
            ),
        ),
        (
            "mlx",
            (
                mlx.embed_texts(&[query], true).expect("q").remove(0),
                mlx.embed_texts(&[relevant, irrelevant], false).expect("d"),
            ),
        ),
    ] {
        let rel = cosine(&q, &d[0]);
        let irr = cosine(&q, &d[1]);
        println!("  {name:<10} relevant={rel:.4}  irrelevant={irr:.4}");
        assert!(
            rel > irr,
            "{name}: relevant doc must outrank irrelevant ({rel:.4} vs {irr:.4})"
        );
    }
}
