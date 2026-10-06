//! Episodic memory layer for octos.
//!
//! This crate provides persistent memory for agents:
//! - Episode storage (summaries of completed tasks)
//! - Memory store (long-term, daily notes)

pub mod guard;

mod episode;
mod hybrid_search;
mod memory_store;
pub mod quant;
mod recall;
mod record;
mod store;

pub use episode::{Episode, EpisodeOutcome, EpisodeSource};
pub use hybrid_search::{HybridIndex, HybridScore, VectorCoverage};
pub use memory_store::{
    DEFAULT_MAX_INJECT_TOKENS, ExtractionItem, MemoryStore, NoteKind, NoteOrigin, StagingNote,
    UsageMap, UsageStat, estimate_tokens, extract_abstract, is_reserved_memory_name,
    is_valid_entry_id,
};
pub use recall::{
    AgeReport, DEFAULT_RECALL_DIMENSION, Hit, RecallConfig, RecallStats, RecallStore, SearchFilter,
    UpsertReport, record_from_bank_page, record_from_episode,
};
pub use record::{MAX_ABSTRACT_BYTES, MAX_BODY_BYTES, MAX_TITLE_BYTES, Record, RecordKind, Trust};
pub use store::{
    DEFAULT_DIMENSION as EPISODIC_INDEX_DIMENSION, EpisodeStore, EpisodeStoreLocked,
    is_episode_store_locked,
};
