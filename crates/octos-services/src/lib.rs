//! Self-contained support services extracted from `octos-cli`.
//!
//! Like `octos-store`, these were leaf modules in `octos-cli` (zero
//! intra-crate dependencies) that the CLI and API server consume. Pulling
//! them into their own crate shrinks `octos-cli` and speeds incremental
//! builds. `octos-cli` re-exports each at its crate root, so every existing
//! `crate::<module>::…` reference keeps resolving unchanged.

pub mod cli_agent_adapter;
pub mod compaction;
pub mod persona_service;
pub mod soul_service;
pub mod tenant;
pub mod updater;
