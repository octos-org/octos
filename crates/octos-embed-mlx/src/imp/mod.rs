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

/// Pin MLX's default compute device to the CPU (for the CPU latency benchmark).
#[doc(hidden)]
pub fn set_default_device_cpu() {
    mlx_rs::Device::set_default(&mlx_rs::Device::cpu());
}

/// Pin MLX's default compute device back to the GPU (Metal).
#[doc(hidden)]
pub fn set_default_device_gpu() {
    mlx_rs::Device::set_default(&mlx_rs::Device::gpu());
}
