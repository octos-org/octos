//! Agreement against frozen golden embeddings.
//!
//! The vectors in `tests/golden/golden_mlx.json` were produced by the
//! `octos-embed-mlx` port, which was itself verified against a Python oracle:
//! golden per-stage activations, worst end-to-end cosine 0.999997, NDCG@10
//! 1.0000 at every MRL dim. That crate has since been removed — this fixture is
//! what survives of it, and it is the only check here that can catch a
//! *semantic* regression (wrong pooling, a dropped task prefix, an
//! un-normalized vector) rather than a crash. All of those produce perfectly
//! well-formed unit vectors that simply retrieve badly.
//!
//! # Why the thresholds are not 1.0
//!
//! The fixture came from MLX affine 8-bit weights; this provider runs GGUF Q8_0
//! through different kernels. Measured agreement is 0.962–0.991 cosine — high
//! enough to prove identical semantics, and far too low to mix in one index.
//! That drift (0.02–0.04) is the same order as the gap between genuinely
//! related documents, which is why the strong assertion here is on RANKING,
//! which survives it, rather than on vector equality, which does not.
//!
//! The practical consequence: changing `embedding.provider` invalidates a
//! populated index exactly as changing the model would. Re-embed stored
//! episodes.
//!
//! Run:
//! ```bash
//! cargo test -p octos-embed-llama --features embed-llama,metal --test golden_agreement -- --ignored --nocapture
//! ```
#![cfg(feature = "embed-llama")]

use std::path::PathBuf;

use octos_embed_llama::LlamaEmbedder;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/golden_mlx.json");

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

/// GPU when built with an accelerator, CPU otherwise.
const NGL: u32 = if cfg!(any(feature = "metal", feature = "cuda")) {
    99
} else {
    0
};

struct Golden {
    docs: Vec<String>,
    doc_embeddings: Vec<Vec<f32>>,
    queries: Vec<String>,
    query_embeddings: Vec<Vec<f32>>,
}

fn golden() -> Golden {
    let raw = std::fs::read_to_string(GOLDEN).expect("read golden fixture");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse golden fixture");
    let strs = |k: &str| -> Vec<String> {
        v[k].as_array()
            .unwrap_or_else(|| panic!("{k} missing"))
            .iter()
            .map(|s| s.as_str().expect("string").to_owned())
            .collect()
    };
    let mat = |k: &str| -> Vec<Vec<f32>> {
        v[k].as_array()
            .unwrap_or_else(|| panic!("{k} missing"))
            .iter()
            .map(|row| {
                row.as_array()
                    .expect("row")
                    .iter()
                    .map(|x| x.as_f64().expect("float") as f32)
                    .collect()
            })
            .collect()
    };
    Golden {
        docs: strs("docs"),
        doc_embeddings: mat("doc_embeddings"),
        queries: strs("queries"),
        query_embeddings: mat("query_embeddings"),
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dimension mismatch vs golden");
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-9)
}

fn rank(q: &[f32], d: &[Vec<f32>]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..d.len()).collect();
    idx.sort_by(|&x, &y| {
        cosine(q, &d[y])
            .partial_cmp(&cosine(q, &d[x]))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_agree_with_golden_on_direction() {
    let g = golden();
    let e = LlamaEmbedder::from_model_file(gguf(), NGL).expect("load GGUF");
    let texts: Vec<&str> = g.docs.iter().map(String::as_str).collect();
    let got = e.embed_texts(&texts, false).expect("embed");

    println!("\nagreement with golden MLX vectors:");
    let mut worst = 1.0f32;
    for (i, (a, b)) in got.iter().zip(&g.doc_embeddings).enumerate() {
        let cos = cosine(a, b);
        println!("  doc_{i} (len {:>4})  cos={cos:.5}", g.docs[i].len());
        worst = worst.min(cos);
    }
    println!("worst = {worst:.5}");
    assert!(
        worst >= 0.95,
        "diverged from the verified reference (worst cosine {worst:.5}) — suspect \
         pooling, task prefix, or normalization rather than quantization noise"
    );
}

/// The load-bearing check: retrieval order must match the verified reference,
/// even though the vectors themselves drift with quantization.
#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_match_golden_retrieval_ranking() {
    let g = golden();
    let e = LlamaEmbedder::from_model_file(gguf(), NGL).expect("load GGUF");
    let docs: Vec<&str> = g.docs.iter().map(String::as_str).collect();
    let queries: Vec<&str> = g.queries.iter().map(String::as_str).collect();

    let d = e.embed_texts(&docs, false).expect("docs");
    let q = e.embed_texts(&queries, true).expect("queries");

    println!("\nretrieval vs golden:");
    for (i, query) in queries.iter().enumerate() {
        let got = rank(&q[i], &d);
        let want = rank(&g.query_embeddings[i], &g.doc_embeddings);
        println!(
            "  q{i}: got top3={:?}  golden top3={:?}",
            &got[..3],
            &want[..3]
        );
        assert_eq!(
            got[0], want[0],
            "top-1 differs from the verified reference for {query:?}: \
             got {:?}, golden {:?}",
            g.docs[got[0]], g.docs[want[0]]
        );
    }
}

/// Agreement alone is not correctness — a dropped prefix on BOTH sides would
/// still agree. This asserts the provider is independently useful.
#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_separate_relevant_from_irrelevant_on_its_own() {
    let e = LlamaEmbedder::from_model_file(gguf(), NGL).expect("load GGUF");
    let query = "what does a cell use for energy?";
    let relevant = "The mitochondria is the powerhouse of the cell.";
    let irrelevant = "BM25 is a lexical ranking function.";

    let q = e.embed_texts(&[query], true).expect("q").remove(0);
    let d = e.embed_texts(&[relevant, irrelevant], false).expect("d");
    let (rel, irr) = (cosine(&q, &d[0]), cosine(&q, &d[1]));
    println!("\nrelevant={rel:.4}  irrelevant={irr:.4}");
    assert!(
        rel > irr,
        "relevant must outrank irrelevant ({rel:.4} vs {irr:.4})"
    );
}
