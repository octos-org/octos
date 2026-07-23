"""Golden oracle for the EmbeddingGemma-300M Rust port.

Loads `mlx-community/embeddinggemma-300m-8bit` via mlx_embeddings (the SAME
8-bit weights the Rust port loads, so quantization matches bit-for-bit) and
dumps, to safetensors + JSON:

  eval_embeds.safetensors : ids_{i} (int32) + emb_{i} (768-d L2-normed) for the
                            whole eval set — end-to-end parity + retrieval bench.
  intermediates.safetensors: per-stage taps for ONE probe input, to localise any
                            discrepancy: embed_scaled, block0, blocks_all,
                            final_norm, pooled, dense0, dense1, normalized.
  meta.json               : eval texts, query/doc roles, probe text, embed_scale,
                            dim, token-id lists.

Safetensors is chosen because mlx-rs can load it directly (Array::load_safetensors).

Run:
    python3 golden_dump.py
"""

import json
import os

import mlx.core as mx
import numpy as np
from mlx_embeddings import load

MODEL = "mlx-community/embeddinggemma-300m-8bit"
# Write golden tensors into the crate's test tree so the (ignored) Rust parity
# tests can load them via CARGO_MANIFEST_DIR without re-running Python. Defaults
# to `<crate>/tests/golden` relative to this script; override with GOLDEN_OUT.
OUT = os.environ.get(
    "GOLDEN_OUT",
    os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "tests", "golden")),
)
os.makedirs(OUT, exist_ok=True)

QUERY_PROMPT = "task: search result | query: "
DOC_PROMPT = "title: none | text: "


def as_query(t):
    return QUERY_PROMPT + t


def as_doc(t):
    return DOC_PROMPT + t


# Fixed eval set: a mix of query- and doc-prompted sentences. The first is a
# query; the rest are documents (some relevant to the query, some distractors).
EVAL = [
    ("query", "How do I keep my search index from going stale when code changes?"),
    ("doc", "Grep needs no index at all, so it stays fresh even as files change every keystroke."),
    ("doc", "Re-embedding on every commit is the main maintenance cost of a vector database."),
    ("doc", "The mitochondria is the powerhouse of the cell."),
    ("doc", "BM25 builds an inverted index that updates cheaply per document."),
    ("doc", "Sourcegraph removed embeddings because vector DBs do not scale past 100k repos."),
    ("query", "What is the capital of France?"),
    ("doc", "Paris has been the capital of France since the Middle Ages."),
    ("doc", "Rust's borrow checker enforces memory safety at compile time."),
    ("doc", "A hybrid retriever fuses BM25 and dense vector scores for ranking."),
]


def build_text(role, t):
    return as_query(t) if role == "query" else as_doc(t)


def main():
    model, tokenizer = load(MODEL)

    # ---- embed scale (empirically 27.0 due to uint32-dtype truncation) --------
    et = model.model.embed_tokens
    scale_const = mx.array(model.model.config.hidden_size**0.5, et.weight.dtype)
    embed_scale = float(scale_const.astype(mx.float32))

    texts = [build_text(r, t) for (r, t) in EVAL]

    # ---- full forward for the whole eval set (ground-truth text_embeds) -------
    tensors = {}
    id_lists = []
    for i, txt in enumerate(texts):
        enc = tokenizer([txt], return_tensors="mlx", padding=True, truncation=True, max_length=512)
        ids = enc["input_ids"]
        out = model(ids, enc["attention_mask"])
        emb = out.text_embeds  # [1, 768], already L2-normalized
        mx.eval(emb)
        tensors[f"ids_{i}"] = np.array(ids, dtype=np.int32).reshape(-1)
        tensors[f"emb_{i}"] = np.array(emb, dtype=np.float32).reshape(-1)
        id_lists.append(np.array(ids).reshape(-1).tolist())

    _save_st(os.path.join(OUT, "eval_embeds.safetensors"), tensors)

    # ---- per-stage intermediates for ONE probe input -------------------------
    probe_idx = 1  # a document sentence
    probe_text = texts[probe_idx]
    enc = tokenizer([probe_text], return_tensors="mlx", padding=True, truncation=True, max_length=512)
    ids = enc["input_ids"]
    attn = enc["attention_mask"]

    inter = {}
    inter["ids"] = np.array(ids, dtype=np.int32).reshape(-1)

    m = model.model  # Gemma3Model

    # embed + scale (exactly as Gemma3Model.__call__)
    h = m.embed_tokens(ids)
    h = h * mx.array(m.config.hidden_size**0.5, m.embed_tokens.weight.dtype).astype(h.dtype)
    mx.eval(h)
    inter["embed_scaled"] = np.array(h, dtype=np.float32)[0]  # [L, 768]

    # For batch=1 with no padding the extended mask is all-zeros → equivalent to
    # no mask. Pass None; we VERIFY below that the replayed `normalized` matches
    # the real text_embeds, proving the tap is faithful.
    for i, layer in enumerate(m.layers):
        h = layer(h, None, None)
        mx.eval(h)
        inter[f"block_{i}"] = np.array(h, dtype=np.float32)[0]
        if i == 0:
            inter["block0"] = np.array(h, dtype=np.float32)[0]

    mx.eval(h)
    inter["blocks_all"] = np.array(h, dtype=np.float32)[0]

    h = m.norm(h)
    mx.eval(h)
    inter["final_norm"] = np.array(h, dtype=np.float32)[0]

    # mean pooling (attn all-ones → plain mean over tokens)
    from mlx_embeddings.models.base import mean_pooling, normalize_embeddings

    pooled = mean_pooling(h, attn)
    mx.eval(pooled)
    inter["pooled"] = np.array(pooled, dtype=np.float32)[0]

    d0 = model.dense[0](pooled)
    mx.eval(d0)
    inter["dense0"] = np.array(d0, dtype=np.float32)[0]

    d1 = model.dense[1](d0)
    mx.eval(d1)
    inter["dense1"] = np.array(d1, dtype=np.float32)[0]

    normed = normalize_embeddings(d1)
    mx.eval(normed)
    inter["normalized"] = np.array(normed, dtype=np.float32)[0]

    _save_st(os.path.join(OUT, "intermediates.safetensors"), inter)

    # sanity: replayed normalized vs real text_embeds for the probe
    real = tensors[f"emb_{probe_idx}"]
    cos = float(np.dot(normed_flat := inter["normalized"], real) /
                (np.linalg.norm(normed_flat) * np.linalg.norm(real)))
    print(f"[sanity] replayed-vs-real cosine for probe idx {probe_idx}: {cos:.6f}")

    meta = {
        "model": MODEL,
        "embed_scale": embed_scale,
        "hidden_size": int(m.config.hidden_size),
        "dim": int(tensors["emb_0"].shape[0]),
        "query_prompt": QUERY_PROMPT,
        "doc_prompt": DOC_PROMPT,
        "eval": [{"role": r, "text": t} for (r, t) in EVAL],
        "eval_full_texts": texts,
        "probe_idx": probe_idx,
        "probe_text": probe_text,
        "token_ids": id_lists,
    }
    with open(os.path.join(OUT, "meta.json"), "w") as f:
        json.dump(meta, f, indent=2)

    print(f"embed_scale = {embed_scale}")
    print(f"dim = {meta['dim']}")
    print(f"probe token count = {len(inter['ids'])}")
    print(f"wrote golden files to {OUT}")


def _save_st(path, np_dict):
    mx_dict = {k: mx.array(v) for k, v in np_dict.items()}
    mx.save_safetensors(path, mx_dict)


if __name__ == "__main__":
    main()
