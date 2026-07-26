//! Model-backed tests for [`LlamaEmbedder`].
//!
//! `#[ignore]` because they need the `embed-llama` feature and a cached GGUF.
//! Run:
//! ```bash
//! cargo test -p octos-embed-llama --features embed-llama,metal --test embed -- --ignored --nocapture
//! ```
//! The model auto-resolves from the HuggingFace cache, or set
//! `OCTOS_EMBED_GGUF` to a file path.
#![cfg(feature = "embed-llama")]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use octos_embed_llama::LlamaEmbedder;

/// llama.cpp's backend is global; serialize model-using tests so they do not
/// race on it.
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

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

/// GPU when built with an accelerator, CPU otherwise. 99 = offload everything.
const NGL: u32 = if cfg!(feature = "metal") || cfg!(feature = "cuda") {
    99
} else {
    0
};

fn embedder() -> LlamaEmbedder {
    LlamaEmbedder::from_model_file(gguf(), NGL).expect("load GGUF")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dim mismatch");
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-9)
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_produce_unit_vectors_of_the_model_dimension() {
    let _g = serial();
    let e = embedder();
    assert_eq!(e.native_dim(), 768, "EmbeddingGemma is 768-d");
    assert_eq!(e.output_dim(), e.native_dim());

    let out = e.embed_texts(&["hello world"], false).expect("embed");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), 768);
    assert!(
        (norm(&out[0]) - 1.0).abs() < 1e-4,
        "llama.cpp mean-pooling does not normalize; we must — got norm {}",
        norm(&out[0])
    );
}

/// The real quality check: a query must rank its answer above an unrelated
/// document. This is what would break if pooling, prefixes, or normalization
/// were wrong — none of which a shape assertion would catch.
#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_rank_the_relevant_document_above_an_unrelated_one() {
    let _g = serial();
    let e = embedder();

    let query = "how do I keep my search index from going stale?";
    let relevant = "Grep needs no index at all, so it stays fresh even as files change.";
    let unrelated = "The mitochondria is the powerhouse of the cell.";

    let q = e.embed_texts(&[query], true).expect("query").remove(0);
    let docs = e
        .embed_texts(&[relevant, unrelated], false)
        .expect("documents");

    let s_rel = cosine(&q, &docs[0]);
    let s_unrel = cosine(&q, &docs[1]);
    println!("\nrelevant={s_rel:.4}  unrelated={s_unrel:.4}");
    assert!(
        s_rel > s_unrel,
        "relevant doc must outrank the unrelated one ({s_rel:.4} vs {s_unrel:.4})"
    );
}

/// Batching must not change any individual embedding — the same property the
/// MLX backend pins. A batch is padded and decoded together, so a bug here
/// silently corrupts whichever text shares a batch with a longer one.
#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_match_one_at_a_time_when_batched() {
    let _g = serial();
    let e = embedder();

    let long = "The quick brown fox jumps over the lazy dog. ".repeat(30);
    let corpus: Vec<String> = vec![
        "hi".into(),
        long,
        "BM25 is a lexical ranking function.".into(),
        "octos".into(),
        "Apple Silicon runs MLX on unified memory.".into(),
    ];
    let texts: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let batched = e.embed_texts(&texts, false).expect("batched");
    println!("\nbatched vs one-at-a-time:");
    for (i, &t) in texts.iter().enumerate() {
        let solo = e.embed_texts(&[t], false).expect("solo").remove(0);
        let cos = cosine(&batched[i], &solo);
        println!("  text_{i} (len {:>4})  cos={cos:.6}", corpus[i].len());
        assert!(
            cos >= 0.999,
            "text_{i}: batching changed the embedding (cos {cos})"
        );
    }
}

#[test]
#[ignore = "needs the embed-llama feature + a cached GGUF"]
fn should_truncate_and_renormalize_for_mrl() {
    let _g = serial();
    let e = embedder().with_output_dim(256);
    assert_eq!(e.output_dim(), 256);

    let out = e.embed_texts(&["hello world"], false).expect("embed");
    assert_eq!(out[0].len(), 256);
    assert!(
        (norm(&out[0]) - 1.0).abs() < 1e-4,
        "MRL truncation must renormalize, got {}",
        norm(&out[0])
    );
}

#[test]
#[ignore = "latency benchmark — single vs batched"]
fn bench_single_and_batched() {
    let _g = serial();
    let e = embedder();
    let corpus: Vec<String> = (0..16)
        .map(|i| format!("Episode {i}: the agent edited a file and ran the test suite."))
        .collect();
    let texts: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let _ = e.embed_texts(&texts[..1], false).expect("warm");

    let t0 = std::time::Instant::now();
    for &t in &texts {
        let _ = e.embed_texts(&[t], false).expect("single");
    }
    let single_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = std::time::Instant::now();
    let _ = e.embed_texts(&texts, false).expect("batched");
    let batched_ms = t1.elapsed().as_secs_f64() * 1000.0;

    println!(
        "\n{} texts: one-at-a-time {single_ms:.1} ms ({:.2} ms each), batched {batched_ms:.1} ms ({:.2} ms each) — {:.1}x",
        texts.len(),
        single_ms / texts.len() as f64,
        batched_ms / texts.len() as f64,
        single_ms / batched_ms.max(f64::EPSILON)
    );
    assert!(batched_ms < single_ms, "batching must win");
}
