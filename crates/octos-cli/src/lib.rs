//! octos-cli library surface.
//!
//! The crate primarily exposes a binary (`octos`) but a few modules are also
//! surfaced here so integration tests can drive real code paths (for example
//! the MCP server dispatch in [`commands::mcp_serve`]). Keep the public API
//! narrow — only items that integration tests or sibling crates consume.

#[cfg(feature = "api")]
pub mod api;
pub mod auth;
pub mod commands;
pub mod compaction;
pub mod config;
pub mod config_watcher;
#[cfg(feature = "api")]
pub mod content_catalog;
pub mod cron_tool;
pub mod gateway_dispatcher;
#[cfg(feature = "api")]
pub mod login_allowlist;
#[cfg(feature = "api")]
pub mod monitor;
#[cfg(feature = "api")]
pub mod otp;
pub mod persona_service;
#[cfg(feature = "api")]
pub mod process_manager;
pub mod profiles;
pub mod project_templates;
mod qos_catalog;
pub mod session_actor;
pub mod skills_scope;
pub mod soul_service;
pub mod status_indicator;
pub mod status_layers;
pub mod stream_reporter;
pub mod tenant;
pub mod tools;
#[cfg(feature = "api")]
pub mod updater;
#[cfg(feature = "api")]
pub mod user_store;
pub mod workflow_runtime;
pub mod workflows;
