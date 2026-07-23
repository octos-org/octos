//! EmbeddingGemma-300M forward pass on mlx-rs (Apple MLX / Metal).
//!
//! Compiled only under
//! `cfg(all(target_os = "macos", target_arch = "aarch64", feature = "embed-mlx"))`.

mod attention;
mod block;
mod config;
mod mlp;
mod model;
mod norm;
mod provider;
mod tokenizer;
mod weights;

pub use provider::MlxEmbedder;

// Re-exported for the (ignored) parity/bench integration tests.
pub use config::GemmaConfig;
pub use model::GemmaModel;
pub use tokenizer::{DOC_PROMPT, GemmaTokenizer, QUERY_PROMPT};
