//! In-process GGUF embedding provider backed by llama.cpp.
//!
//! The only embedding backend. It replaced `octos-embed-mlx`, a hand-written
//! port of one model (EmbeddingGemma-300M) onto Apple MLX, which was
//! macOS+aarch64 only — so `provider = "mlx"` gave a Linux deployment nothing.
//! This runs any GGUF embedding model anywhere llama.cpp builds, with a CPU
//! backend that is a real implementation rather than a fallback.
//!
//! # Why it replaced the MLX port
//!
//! Benchmarked in-process on an M3 Max, same weights, same 8-bit class:
//!
//! | | single | batched |
//! |---|---|---|
//! | llama.cpp | 5.68 ms | 0.98 ms |
//! | MLX port  | 6.85 ms | 1.10 ms |
//!
//! So portability cost nothing — this is at parity or slightly ahead. At these
//! sizes both are dominated by fixed per-forward-pass overhead rather than
//! arithmetic, which is why batching (5.8x) is the only lever that has ever
//! moved the number. It also drops a hand-derived forward pass that needed
//! golden tests against a Python oracle to stay honest, and generalizes to any
//! GGUF instead of one architecture.
//!
//! # Switching backends invalidates a populated index
//!
//! `tests/cross_backend.rs` pins this provider against the MLX port, which is
//! verified against a Python oracle. They agree on semantics — same pooling,
//! prefixes and normalization — and their retrieval rankings match. But the
//! vectors themselves only agree to 0.962–0.991 cosine, because the two run
//! different 8-bit quantizations through different kernels.
//!
//! That drift is the same order as the gap between genuinely related documents,
//! so embeddings from the two backends MUST NOT share an index. Changing
//! `embedding.provider` between `"mlx"` and `"llamacpp"` requires re-embedding
//! stored episodes, exactly as changing the model would.
//!
//! # Gating
//!
//! Compiled only under the `embed-llama` feature, which pulls a CMake build of
//! llama.cpp. `cargo build --workspace` without it stays pure Rust.
//!
//! ```bash
//! cargo build -p octos-embed-llama --features embed-llama,metal   # Apple GPU
//! cargo build -p octos-embed-llama --features embed-llama         # CPU
//! ```

#[cfg(feature = "embed-llama")]
mod imp;

#[cfg(feature = "embed-llama")]
pub use imp::LlamaEmbedder;

// Pure helpers, deliberately NOT feature-gated so a plain `cargo test` covers
// them on every platform — the model-backed tests cannot run without a GGUF.
mod prompt;

pub use prompt::{DOC_PROMPT, QUERY_PROMPT, batch_plan, l2_normalize, mrl_truncate, with_prompt};
