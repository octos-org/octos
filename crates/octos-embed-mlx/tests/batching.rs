//! Batched-forward equivalence tests.
//!
//! The batched path right-pads ragged sequences and masks the padding out of
//! both attention and mean-pooling. The property that has to hold is:
//! **a text's embedding must not depend on what it was batched with.** These
//! tests pin that against the (already golden-verified) one-at-a-time path.
//!
//! `#[ignore]` for the same reasons as the parity suite: Apple Silicon, the
//! `embed-mlx` feature, and the cached model.
//!
//! Run:
//! ```bash
//! cargo test -p octos-embed-mlx --features embed-mlx --test batching -- --ignored --nocapture
//! ```
#![cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use octos_embed_mlx::MlxEmbedder;

/// MLX/Metal aborts if driven from several test threads at once.
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
        .map(|e| e.path())
        .find(|p| p.join("model.safetensors").exists())
        .expect("no snapshot with model.safetensors")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "dim mismatch");
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb).max(1e-9)
}

fn max_abs_diff(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

/// Deliberately ragged: a 4-token query next to a multi-hundred-token document
/// is the worst case for padding correctness.
fn ragged_corpus() -> Vec<String> {
    let long = "The quick brown fox jumps over the lazy dog. ".repeat(40);
    let medium = "Retrieval augmented generation grounds a model in external documents. ".repeat(4);
    vec![
        "hi".to_string(),
        long,
        "BM25 is a lexical ranking function.".to_string(),
        medium,
        "octos".to_string(),
        "Apple Silicon runs MLX on the unified memory architecture.".to_string(),
    ]
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_match_sequential_when_batched_over_ragged_lengths() {
    let _g = serial();
    let embedder = MlxEmbedder::from_model_dir(model_dir()).expect("load");

    let corpus = ragged_corpus();
    let texts: Vec<&str> = corpus.iter().map(String::as_str).collect();

    // Reference: one sequence per forward pass — never pads, so it is the
    // path the golden parity suite already validated.
    let sequential = MlxEmbedder::from_model_dir(model_dir())
        .expect("load")
        .with_max_batch(1);
    let want = sequential.embed_texts(&texts, false).expect("sequential");

    // Batched: all six in one padded forward pass.
    let got = embedder
        .with_max_batch(64)
        .embed_texts(&texts, false)
        .expect("batched");

    assert_eq!(got.len(), want.len());
    println!("\nbatched vs sequential (ragged, one padded forward pass):");
    let mut worst_cos = 1.0f32;
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        let cos = cosine(g, w);
        let mad = max_abs_diff(g, w);
        println!(
            "  text_{i} (len {:>4})  cos={cos:.7}  maxAbsDiff={mad:.2e}",
            corpus[i].len()
        );
        worst_cos = worst_cos.min(cos);
        assert!(
            cos >= 0.9999,
            "text_{i}: batched embedding diverges from sequential (cos {cos})"
        );
    }
    println!("worst cosine = {worst_cos:.7}");
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_not_depend_on_batch_neighbours() {
    let _g = serial();
    let embedder = MlxEmbedder::from_model_dir(model_dir())
        .expect("load")
        .with_max_batch(64);

    let target = "BM25 is a lexical ranking function.";
    let long = "The quick brown fox jumps over the lazy dog. ".repeat(60);

    // Same text, three different padding contexts.
    let alone = embedder
        .embed_texts(&[target], false)
        .expect("alone")
        .remove(0);
    let with_short = embedder
        .embed_texts(&[target, "hi"], false)
        .expect("with short")
        .remove(0);
    let with_long = embedder
        .embed_texts(&[target, long.as_str()], false)
        .expect("with long")
        .remove(0);

    let c_short = cosine(&alone, &with_short);
    let c_long = cosine(&alone, &with_long);
    println!(
        "\npadding independence: alone-vs-short cos={c_short:.7}, alone-vs-long(+{} tok-ish) cos={c_long:.7}",
        long.len()
    );
    assert!(
        c_short >= 0.9999,
        "neighbour changed the embedding: {c_short}"
    );
    assert!(
        c_long >= 0.9999,
        "long-padding neighbour changed the embedding: {c_long}"
    );
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_preserve_input_order_despite_length_sorting() {
    let _g = serial();
    let embedder = MlxEmbedder::from_model_dir(model_dir())
        .expect("load")
        .with_max_batch(2); // forces several groups + reordering

    // Lengths are intentionally out of order so the internal length sort has to
    // scatter results back.
    let corpus = ragged_corpus();
    let texts: Vec<&str> = corpus.iter().map(String::as_str).collect();
    let batched = embedder.embed_texts(&texts, false).expect("batched");

    // Compare each against its own single-text embedding.
    for (i, &t) in texts.iter().enumerate() {
        let solo = embedder.embed_texts(&[t], false).expect("solo").remove(0);
        let cos = cosine(&batched[i], &solo);
        assert!(
            cos >= 0.9999,
            "position {i} holds the wrong text's embedding (cos {cos} vs solo)"
        );
    }
    println!(
        "\ninput order preserved across {} length-sorted groups",
        texts.len().div_ceil(2)
    );
}

#[test]
#[ignore = "needs Apple Silicon + embed-mlx feature + cached model"]
fn should_beat_sequential_on_throughput() {
    let _g = serial();
    let dir = model_dir();
    let corpus: Vec<String> = (0..16)
        .map(|i| format!("Episode {i}: the agent edited a file and ran the test suite."))
        .collect();
    let texts: Vec<&str> = corpus.iter().map(String::as_str).collect();

    let seq = MlxEmbedder::from_model_dir(&dir)
        .expect("load")
        .with_max_batch(1);
    let bat = MlxEmbedder::from_model_dir(&dir)
        .expect("load")
        .with_max_batch(16);

    // Warm both (first call pays lazy Metal kernel compilation).
    let _ = seq.embed_texts(&texts[..1], false).expect("warm");
    let _ = bat.embed_texts(&texts[..1], false).expect("warm");

    let t0 = std::time::Instant::now();
    let a = seq.embed_texts(&texts, false).expect("seq");
    let seq_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = std::time::Instant::now();
    let b = bat.embed_texts(&texts, false).expect("bat");
    let bat_ms = t1.elapsed().as_secs_f64() * 1000.0;

    // Same answers, faster.
    let worst = a
        .iter()
        .zip(&b)
        .map(|(x, y)| cosine(x, y))
        .fold(1.0f32, f32::min);
    println!(
        "\nthroughput on {} texts: sequential {seq_ms:.1} ms, batched {bat_ms:.1} ms ({:.1}x), worst cos {worst:.7}",
        texts.len(),
        seq_ms / bat_ms.max(f64::EPSILON)
    );
    assert!(worst >= 0.9999, "batched answers differ: {worst}");
    assert!(
        bat_ms < seq_ms,
        "batching was not faster: {bat_ms:.1} ms vs {seq_ms:.1} ms"
    );
}
