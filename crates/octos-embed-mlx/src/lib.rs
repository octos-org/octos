//! In-process EmbeddingGemma-300M embedding provider on Apple MLX.
//!
//! This crate ports `mlx-community/embeddinggemma-300m-8bit` (a Gemma3-text
//! encoder + SentenceTransformers head) to Rust on `mlx-rs` (Apple MLX / Metal)
//! and exposes it as an [`octos_llm::EmbeddingProvider`].
//!
//! # Gating
//!
//! The real implementation depends on `mlx-rs`/`mlx-sys` (Apple-only C++/Metal
//! FFI). It is therefore compiled ONLY under
//! `cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))`.
//! Without the `embed-mlx` feature the crate is an empty, pure-Rust shell so
//! `cargo build --workspace` is unaffected on every platform.
//!
//! Enable with:
//! ```bash
//! cargo build -p octos-embed-mlx --features embed-mlx   # Apple Silicon only
//! ```

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))]
mod imp;

#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))]
pub use imp::MlxEmbedder;

// Lower-level pieces, exposed (doc-hidden) so the ignored parity/bench
// integration tests can tap per-stage activations. Not part of the stable API.
#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))]
#[doc(hidden)]
pub use imp::{DOC_PROMPT, GemmaConfig, GemmaModel, GemmaTokenizer, QUERY_PROMPT};
