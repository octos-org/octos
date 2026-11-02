//! Self-contained support services extracted from `octos-cli`.
//!
//! These modules are consumed by the CLI, API server, and embedding facades.
//! Pulling them into their own crate shrinks `octos-cli` and keeps lower-level
//! consumers from depending on the binary crate. `octos-cli` re-exports them
//! at their original paths so existing call sites keep resolving unchanged.

pub mod auth;
pub mod cli_agent_adapter;
pub mod compaction;
pub mod config;
pub mod config_context;
pub mod embed_model;
pub mod persona_service;
pub mod provider_factory;
pub mod soul_service;
pub mod tenant;
pub mod updater;
