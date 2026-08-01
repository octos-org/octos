//! Episodic memory layer for octos.
//!
//! This crate provides persistent memory for agents:
//! - Episode storage (summaries of completed tasks)
//! - Memory store (long-term, daily notes)

pub mod guard;

mod episode;
mod hybrid_search;
mod memory_store;
mod store;

pub use episode::{Episode, EpisodeOutcome, EpisodeSource};
pub use hybrid_search::{HybridIndex, HybridScore, VectorCoverage};
pub use memory_store::{
    DEFAULT_MAX_INJECT_TOKENS, ExtractionItem, MemoryStore, NoteKind, NoteOrigin, StagingNote,
    UsageMap, UsageStat, estimate_tokens, extract_abstract, is_reserved_memory_name,
    is_valid_entry_id,
};
pub use store::{DEFAULT_DIMENSION as EPISODIC_INDEX_DIMENSION, EpisodeStore};
