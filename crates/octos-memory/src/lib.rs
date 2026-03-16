//! Episodic memory layer for octos.
//!
//! This crate provides persistent memory for agents:
//! - Episode storage (summaries of completed tasks)
//! - Memory store (long-term, daily notes)

mod episode;
mod hybrid_search;
mod memory_store;
mod store;

pub use episode::{Episode, EpisodeOutcome};
pub use hybrid_search::HybridIndex;
pub use memory_store::MemoryStore;
pub use store::EpisodeStore;
