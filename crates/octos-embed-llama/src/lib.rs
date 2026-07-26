//! In-process GGUF embedding provider backed by llama.cpp.
//!
//! The cross-platform counterpart to `octos-embed-mlx`. That crate hand-ports a
//! single model (EmbeddingGemma-300M) onto Apple MLX and is therefore
//! macOS+aarch64 only; this one runs any GGUF embedding model on any platform
//! llama.cpp supports, with the CPU backend as a genuinely usable default.
//!
//! # Why both exist
//!
//! Benchmarked head-to-head on an M3 Max, same model, same 8-bit quantization:
//! a single short text costs ~6.5 ms through llama.cpp/Metal against ~6.85 ms
//! through the MLX port — a tie, because at that size both are dominated by
//! fixed per-forward-pass overhead rather than arithmetic. MLX keeps a modest
//! edge once batched. What llama.cpp adds is every platform that is not an
//! Apple laptop, plus any GGUF instead of one hand-written architecture.
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
