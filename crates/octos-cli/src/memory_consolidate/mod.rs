//! Memory consolidation engine (memory-refresh design, PR-4).
//!
//! Merges staging notes (`memory/staging/notes/`) and extraction files
//! (`memory/staging/extract/`) into `MEMORY.md` through ONE LLM merge call,
//! under machine-enforced authority gates: the model proposes ops, Rust
//! decides what is allowed based exclusively on host-written metadata.
//!
//! The engine is a leaf: everything arrives as parameters, all file IO stays
//! under the profile's memory directory. Wiring into schedulers/CLI lands in
//! a later integration PR.

pub mod entry;
pub mod staging;
