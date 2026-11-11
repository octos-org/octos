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
