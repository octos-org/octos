"""Retrieval + MRL oracle for the EmbeddingGemma Rust port.

Builds a small labeled retrieval set (24 queries, each with ONE known-relevant
doc, sharing a corpus of 50 docs = 24 relevant + 26 distractors), embeds it with
the SAME 8-bit `mlx_embeddings` model (query/document prompts applied), and
computes NDCG@10 + Recall@1 at MRL dims 768/512/256/128.

Outputs to `<crate>/tests/golden/`:
  retrieval_corpus.json  : {queries:[...], docs:[...], relevant:[doc_idx per query]}
  retrieval_python.json  : per-dim metrics + per-query relevant-doc rank (for the
                           Rust bench to match ranking-for-ranking).

Run: python3 retrieval_bench.py
"""

import json
import math
import os

import mlx.core as mx
import numpy as np
from mlx_embeddings import load

MODEL = "mlx-community/embeddinggemma-300m-8bit"
OUT = os.environ.get(
    "GOLDEN_OUT",
    os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "tests", "golden")),
)
os.makedirs(OUT, exist_ok=True)

QUERY_PROMPT = "task: search result | query: "
DOC_PROMPT = "title: none | text: "

# (query, relevant_doc) pairs — the relevant doc answers/paraphrases the query.
PAIRS = [
    ("How do I stop my vector index from going stale as code changes?",
     "Re-embedding changed files on each commit keeps a vector index fresh but is the main maintenance cost."),
    ("What is the capital of France?",
     "Paris has served as the capital of France since the medieval period."),
    ("How does Rust prevent data races?",
     "Rust's ownership and borrow checker reject aliased mutable access at compile time, preventing data races."),
    ("What does BM25 do?",
     "BM25 ranks documents with a bag-of-words scoring function over an inverted index."),
    ("How do I make bread rise?",
     "Yeast ferments the sugars in dough, producing carbon dioxide that makes bread rise."),
    ("Why is the sky blue?",
     "Rayleigh scattering sends short blue wavelengths of sunlight in every direction, coloring the sky blue."),
    ("What is photosynthesis?",
     "Plants convert sunlight, water, and carbon dioxide into glucose and oxygen through photosynthesis."),
    ("How do transformers use attention?",
     "A transformer computes attention weights so each token can attend to every other token in the sequence."),
    ("What is the boiling point of water at sea level?",
     "At standard sea-level pressure water boils at 100 degrees Celsius."),
    ("How do I center a div in CSS?",
     "Use display flex with justify-content and align-items set to center to center a div."),
    ("What causes tides in the ocean?",
     "The gravitational pull of the Moon and Sun raises and lowers ocean tides."),
    ("How does HTTPS keep traffic private?",
     "HTTPS wraps HTTP in a TLS tunnel that encrypts traffic between browser and server."),
    ("What is a black hole?",
     "A black hole is a region where gravity is so strong that not even light can escape."),
    ("How do bees make honey?",
     "Bees collect flower nectar and evaporate its water in the hive to produce honey."),
    ("What is the Pythagorean theorem?",
     "In a right triangle the square of the hypotenuse equals the sum of the squares of the other two sides."),
    ("How do I reverse a linked list?",
     "Iterate through the list rewiring each node's next pointer to its predecessor to reverse a linked list."),
    ("What is machine learning overfitting?",
     "Overfitting happens when a model memorizes training noise and fails to generalize to new data."),
    ("How does garbage collection work?",
     "A garbage collector reclaims heap memory that is no longer reachable from live references."),
    ("What is the speed of light?",
     "Light travels through a vacuum at about 299,792 kilometers per second."),
    ("How do I treat a minor burn?",
     "Cool a minor burn under running water for several minutes and cover it loosely with a clean dressing."),
    ("What is inflation in economics?",
     "Inflation is a sustained rise in the general price level that erodes the purchasing power of money."),
    ("How does DNS resolve a domain name?",
     "A DNS resolver walks the domain hierarchy to translate a hostname into its IP address."),
    ("What is a hash table?",
     "A hash table maps keys to buckets via a hash function for average constant-time lookups."),
    ("How do vaccines create immunity?",
     "A vaccine trains the immune system with a harmless antigen so it can recognize the real pathogen later."),
]

# Extra distractor documents (relevant to nothing in the query set).
DISTRACTORS = [
    "The mitochondria is the powerhouse of the cell.",
    "Basalt is a common volcanic rock formed from rapidly cooling lava.",
    "The violin is a bowed string instrument with four strings tuned in fifths.",
    "Mount Everest is the highest mountain above sea level on Earth.",
    "A haiku is a three-line Japanese poem with a 5-7-5 syllable pattern.",
    "Espresso is brewed by forcing hot water through finely ground coffee.",
    "The Great Barrier Reef is the world's largest coral reef system.",
    "Chess is a two-player strategy game played on a 64-square board.",
    "Aluminium is a lightweight, corrosion-resistant metal used in aircraft.",
    "The Nile is one of the longest rivers in the world.",
    "Penguins are flightless seabirds found mostly in the Southern Hemisphere.",
    "A sonnet is a fourteen-line poem with a fixed rhyme scheme.",
    "Graphite and diamond are both allotropes of carbon.",
    "The tango is a partner dance that originated in Argentina.",
    "Saturn is famous for its extensive and bright ring system.",
    "Sourdough uses a wild-yeast starter instead of commercial yeast.",
    "The cheetah is the fastest land animal over short distances.",
    "Origami is the Japanese art of folding paper into shapes.",
    "Copper is an excellent conductor widely used in electrical wiring.",
    "The Amazon rainforest produces a large share of the world's oxygen.",
    "A metronome keeps steady musical tempo with an audible click.",
    "Jupiter is the largest planet in the solar system.",
    "Maple syrup is boiled down from the sap of maple trees.",
    "The kangaroo is a marsupial native to Australia.",
    "Quartz is one of the most abundant minerals in Earth's crust.",
    "A lighthouse warns ships away from hazardous coastlines at night.",
]


def l2(x):
    return x / mx.maximum(mx.linalg.norm(x, axis=-1, keepdims=True), 1e-9)


def main():
    model, tokenizer = load(MODEL)

    queries = [q for (q, _) in PAIRS]
    rel_docs = [d for (_, d) in PAIRS]
    docs = rel_docs + DISTRACTORS
    relevant = list(range(len(rel_docs)))  # query i -> doc i

    def embed(texts, prompt):
        vecs = []
        for t in texts:
            enc = tokenizer([prompt + t], return_tensors="mlx", padding=True,
                            truncation=True, max_length=512)
            out = model(enc["input_ids"], enc["attention_mask"])
            v = np.array(out.text_embeds, dtype=np.float32).reshape(-1)
            vecs.append(v)
        return np.stack(vecs)

    q_emb = embed(queries, QUERY_PROMPT)   # [Q, 768]
    d_emb = embed(docs, DOC_PROMPT)        # [D, 768]

    dims = [768, 512, 256, 128]
    metrics = {}
    ranks_by_dim = {}
    for dim in dims:
        qd = renorm(q_emb[:, :dim])
        dd = renorm(d_emb[:, :dim])
        sims = qd @ dd.T  # [Q, D]
        ndcgs, recalls, ranks = [], [], []
        for i in range(len(queries)):
            order = np.argsort(-sims[i])  # doc indices, best first
            rank = int(np.where(order == relevant[i])[0][0]) + 1  # 1-indexed
            ranks.append(rank)
            ndcgs.append(1.0 / math.log2(rank + 1) if rank <= 10 else 0.0)
            recalls.append(1.0 if rank == 1 else 0.0)
        metrics[str(dim)] = {
            "ndcg@10": float(np.mean(ndcgs)),
            "recall@1": float(np.mean(recalls)),
        }
        ranks_by_dim[str(dim)] = ranks

    with open(os.path.join(OUT, "retrieval_corpus.json"), "w") as f:
        json.dump({"queries": queries, "docs": docs, "relevant": relevant}, f, indent=2)
    with open(os.path.join(OUT, "retrieval_python.json"), "w") as f:
        json.dump({"dims": dims, "metrics": metrics, "ranks": ranks_by_dim}, f, indent=2)

    print(f"corpus: {len(queries)} queries, {len(docs)} docs")
    for dim in dims:
        m = metrics[str(dim)]
        print(f"  @{dim:>3}d  NDCG@10={m['ndcg@10']:.4f}  Recall@1={m['recall@1']:.4f}")
    print(f"wrote retrieval oracle to {OUT}")


def renorm(x):
    n = np.linalg.norm(x, axis=-1, keepdims=True)
    return x / np.maximum(n, 1e-9)


if __name__ == "__main__":
    main()
