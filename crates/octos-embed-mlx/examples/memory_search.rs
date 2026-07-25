//! Live end-to-end: EmbeddingGemma-300M (MLX / Metal) → the REAL octos-memory
//! `HybridIndex` (BM25 + HNSW cosine fusion). This is the actual search engine
//! the agent loop uses for "Relevant Past Experiences" — here driven by the
//! in-process MLX embedder instead of a remote API.
//!
//! Run (Apple Silicon):
//!   cargo run -p octos-embed-mlx --features embed-mlx --example memory_search
//! Model auto-resolves from ~/.cache/huggingface, or set OCTOS_EMBED_MODEL_DIR.

use std::path::PathBuf;

use eyre::Result;
use octos_embed_mlx::MlxEmbedder;
use octos_memory::HybridIndex;

fn resolve_model_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("OCTOS_EMBED_MODEL_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var("HOME").map_err(|_| eyre::eyre!("HOME unset"))?;
    let base = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join("models--mlx-community--embeddinggemma-300m-8bit")
        .join("snapshots");
    std::fs::read_dir(&base)
        .map_err(|e| eyre::eyre!("no HF snapshot dir {}: {e}", base.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("model.safetensors").exists())
        .ok_or_else(|| {
            eyre::eyre!(
                "no snapshot with model.safetensors under {}",
                base.display()
            )
        })
}

fn main() -> Result<()> {
    let dir = resolve_model_dir()?;
    println!(
        "loading EmbeddingGemma-300M (MLX/Metal) from {}\n",
        dir.display()
    );
    let embedder = MlxEmbedder::from_model_dir(&dir)?;
    let dim = embedder.output_dim(); // == EmbeddingProvider::dimension()

    let docs: [(&str, &str); 6] = [
        (
            "d1",
            "Grep needs no index at all, so it stays fresh even as files change every keystroke.",
        ),
        (
            "d2",
            "Re-embedding on every commit is the main maintenance cost of a vector database.",
        ),
        ("d3", "The mitochondria is the powerhouse of the cell."),
        (
            "d4",
            "BM25 builds an inverted index that updates cheaply per document.",
        ),
        (
            "d5",
            "Sourcegraph removed embeddings because vector databases do not scale past 100k repositories.",
        ),
        (
            "d6",
            "EmbeddingGemma runs on Apple Silicon via MLX at roughly five milliseconds per embedding.",
        ),
    ];
    let text_of = |id: &str| {
        docs.iter()
            .find(|(d, _)| *d == id)
            .map(|(_, t)| *t)
            .unwrap_or("")
    };

    // 1) Embed the documents on Metal (EmbeddingGemma DOCUMENT prompt).
    let texts: Vec<&str> = docs.iter().map(|(_, t)| *t).collect();
    let doc_embs = embedder.embed_texts(&texts, false)?;

    // 2) Insert them into the REAL octos-memory hybrid index (HNSW + BM25).
    let mut index = HybridIndex::new(dim); // default weights: 0.7 vector / 0.3 bm25
    for ((id, text), emb) in docs.iter().zip(&doc_embs) {
        index.insert(id, text, Some(emb));
    }
    println!(
        "indexed {} docs into octos_memory::HybridIndex (dim {}, HNSW + BM25, weights 0.7/0.3)\n",
        index.len(),
        dim
    );

    // 3) Hybrid query — the query embedding also comes from MLX (QUERY prompt).
    let query = "how do I keep my search index from going stale when the code changes?";
    let q_emb = embedder.embed_texts(&[query], true)?.remove(0);

    println!("query: {query}\n");
    println!("HYBRID  (MLX vector 0.7 + BM25 0.3):");
    for (rank, (id, s)) in index
        .search_scored(query, Some(&q_emb), docs.len())
        .iter()
        .enumerate()
    {
        println!(
            "  {}. {:<3} combined={:.3}   vec={:.3}  bm25={:.3}   {}",
            rank + 1,
            id,
            s.combined,
            s.vector,
            s.bm25,
            text_of(id)
        );
    }

    // 4) Ablation: the SAME index, BM25 only (no query embedding). Shows the
    //    MLX vector lane actually reorders the results vs. keyword-only.
    println!("\nBM25-ONLY  (no query embedding — the no-embedder fallback path):");
    for (rank, (id, s)) in index
        .search_scored(query, None, docs.len())
        .iter()
        .enumerate()
    {
        println!(
            "  {}. {:<3} combined={:.3}   {}",
            rank + 1,
            id,
            s.combined,
            text_of(id)
        );
    }

    Ok(())
}
