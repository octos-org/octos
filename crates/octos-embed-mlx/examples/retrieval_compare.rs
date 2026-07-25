//! Benchmark: **plain-text (BM25) vs embedding (vector) vs hybrid** retrieval,
//! all through octos-memory's real `HybridIndex`, scored on standard NanoBEIR
//! datasets (relevance-labeled) with NDCG@10 / Recall@10 / MRR@10.
//!
//! Data prepared by scratchpad/bench_download.py. Run (Apple Silicon):
//!   OCTOS_BENCH_DATA=<dir> \
//!   cargo run -p octos-embed-mlx --features embed-mlx --release --example retrieval_compare
//! Optional: OCTOS_BENCH_SETS=NanoSciFact  (comma-sep subset)

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use eyre::Result;
use octos_embed_mlx::MlxEmbedder;
use octos_memory::HybridIndex;
use serde::Deserialize;

#[derive(Deserialize)]
struct Item {
    id: String,
    text: String,
}

#[derive(Default, Clone, Copy)]
struct Metrics {
    ndcg: f64,
    recall: f64,
    mrr: f64,
}

fn load_items(p: &Path) -> Result<Vec<Item>> {
    Ok(serde_json::from_reader(BufReader::new(
        std::fs::File::open(p)?,
    ))?)
}

fn load_qrels(p: &Path) -> Result<HashMap<String, Vec<String>>> {
    Ok(serde_json::from_reader(BufReader::new(
        std::fs::File::open(p)?,
    ))?)
}

fn ids(v: Vec<(String, f32)>) -> Vec<String> {
    v.into_iter().map(|(id, _)| id).collect()
}

fn ndcg_at_k(ranked: &[String], rel: &HashSet<String>, k: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, id)| rel.contains(*id))
        .map(|(i, _)| 1.0 / ((i as f64 + 2.0).log2()))
        .sum();
    let ideal = rel.len().min(k);
    let idcg: f64 = (0..ideal).map(|i| 1.0 / ((i as f64 + 2.0).log2())).sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn recall_at_k(ranked: &[String], rel: &HashSet<String>, k: usize) -> f64 {
    if rel.is_empty() {
        return 0.0;
    }
    let hit = ranked.iter().take(k).filter(|id| rel.contains(*id)).count();
    hit as f64 / rel.len() as f64
}

fn mrr_at_k(ranked: &[String], rel: &HashSet<String>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .position(|id| rel.contains(id))
        .map(|p| 1.0 / (p as f64 + 1.0))
        .unwrap_or(0.0)
}

fn resolve_model_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("OCTOS_EMBED_MODEL_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var("HOME").map_err(|_| eyre::eyre!("HOME unset"))?;
    let base = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join("models--mlx-community--embeddinggemma-300m-8bit")
        .join("snapshots");
    std::fs::read_dir(&base)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("model.safetensors").exists())
        .ok_or_else(|| eyre::eyre!("no snapshot under {}", base.display()))
}

fn main() -> Result<()> {
    let data = PathBuf::from(
        std::env::var("OCTOS_BENCH_DATA").map_err(|_| eyre::eyre!("set OCTOS_BENCH_DATA"))?,
    );
    let sets: Vec<String> = std::env::var("OCTOS_BENCH_SETS")
        .unwrap_or_else(|_| "NanoSciFact,NanoNFCorpus,NanoFiQA2018".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let embedder = MlxEmbedder::from_model_dir(resolve_model_dir()?)?;
    let dim = embedder.output_dim();
    let k = 10usize;
    let modes = [
        "plain-text (BM25)",
        "embedding (vector)",
        "hybrid (0.7/0.3)",
    ];

    let mut overall = [Metrics::default(); 3];
    let mut overall_n = 0usize;

    for set in &sets {
        let dir = data.join(set);
        let corpus = load_items(&dir.join("corpus.json"))?;
        let queries = load_items(&dir.join("queries.json"))?;
        let qrels = load_qrels(&dir.join("qrels.json"))?;

        eprintln!("[{set}] embedding {} docs on Metal...", corpus.len());
        let ctexts: Vec<&str> = corpus.iter().map(|d| d.text.as_str()).collect();
        let cembs = embedder.embed_texts(&ctexts, false)?;

        // The SAME docs, in octos's real hybrid index. `hybrid` (default
        // 0.7/0.3) serves both hybrid (with query embedding) and plain-text
        // (query embedding = None). `vec_only` isolates the vector lane.
        let mut hybrid = HybridIndex::new(dim);
        let mut vec_only = HybridIndex::new(dim).with_weights(1.0, 0.0);
        for (doc, emb) in corpus.iter().zip(&cembs) {
            hybrid.insert(&doc.id, &doc.text, Some(emb));
            vec_only.insert(&doc.id, &doc.text, Some(emb));
        }

        let mut agg = [Metrics::default(); 3];
        let mut n = 0usize;
        for q in &queries {
            let rel: HashSet<String> = match qrels.get(&q.id) {
                Some(v) if !v.is_empty() => v.iter().cloned().collect(),
                _ => continue,
            };
            let qe = embedder.embed_texts(&[q.text.as_str()], true)?.remove(0);
            let ranked = [
                ids(hybrid.search(&q.text, None, k)),        // plain-text
                ids(vec_only.search(&q.text, Some(&qe), k)), // embedding
                ids(hybrid.search(&q.text, Some(&qe), k)),   // hybrid
            ];
            for i in 0..3 {
                agg[i].ndcg += ndcg_at_k(&ranked[i], &rel, k);
                agg[i].recall += recall_at_k(&ranked[i], &rel, k);
                agg[i].mrr += mrr_at_k(&ranked[i], &rel, k);
            }
            n += 1;
        }

        println!(
            "\n=== {set}  ({} docs, {n} scored queries) ===",
            corpus.len()
        );
        println!(
            "  {:<20} {:>8} {:>10} {:>8}",
            "mode", "NDCG@10", "Recall@10", "MRR@10"
        );
        for i in 0..3 {
            println!(
                "  {:<20} {:>8.4} {:>10.4} {:>8.4}",
                modes[i],
                agg[i].ndcg / n as f64,
                agg[i].recall / n as f64,
                agg[i].mrr / n as f64
            );
            overall[i].ndcg += agg[i].ndcg;
            overall[i].recall += agg[i].recall;
            overall[i].mrr += agg[i].mrr;
        }
        overall_n += n;
    }

    if sets.len() > 1 {
        println!("\n=== OVERALL  (micro-avg over {overall_n} queries) ===");
        println!(
            "  {:<20} {:>8} {:>10} {:>8}",
            "mode", "NDCG@10", "Recall@10", "MRR@10"
        );
        for i in 0..3 {
            println!(
                "  {:<20} {:>8.4} {:>10.4} {:>8.4}",
                modes[i],
                overall[i].ndcg / overall_n as f64,
                overall[i].recall / overall_n as f64,
                overall[i].mrr / overall_n as f64
            );
        }
    }
    Ok(())
}
