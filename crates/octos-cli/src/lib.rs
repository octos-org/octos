//! octos-cli library surface.
//!
//! The crate primarily exposes a binary (`octos`) but a few modules are also
//! surfaced here so integration tests can drive real code paths (for example
//! the MCP server dispatch in [`commands::mcp_serve`]). Keep the public API
//! narrow — only items that integration tests or sibling crates consume.

#[cfg(feature = "api")]
pub use octos_store::admin_audit_store;
// Extracted to the `octos-store` crate; re-exported so `crate::admin_token_store::…`
// keeps resolving unchanged.
pub use octos_store::admin_token_store;
#[cfg(feature = "api")]
pub mod api;
pub use octos_store::approvals_audit;
pub mod auth;
pub mod cli_agent_adapter;
pub mod commands;
pub mod compaction;
pub mod config;
pub mod config_context;
pub mod config_layer;
pub mod config_watcher;
#[cfg(feature = "api")]
pub mod content_catalog;
#[path = "api/context_manager.rs"]
pub(crate) mod context_manager;
pub mod cron_tool;
pub mod gateway_dispatcher;
#[cfg(feature = "api")]
pub mod goal_tool;
#[cfg(feature = "api")]
pub use octos_store::login_allowlist;
pub mod memory_consolidate;
pub mod memory_refresh;
#[cfg(feature = "api")]
pub mod monitor;
#[cfg(feature = "api")]
pub mod otp;
pub mod persona_service;
#[cfg(feature = "api")]
pub mod process_manager;
pub mod profile_qr;
pub mod profiles;
pub mod project_templates;
mod qos_catalog;
pub mod runtime;
pub mod session_actor;
pub use octos_store::setup_state_store;
pub mod skills_scope;
pub use octos_store::smtp_secret_store;
pub mod soul_service;
pub mod status_indicator;
pub mod status_layers;
pub mod stream_reporter;
pub mod tenant;
pub mod tools;
#[cfg(feature = "api")]
pub mod updater;
pub use octos_store::usage_ledger;
#[cfg(feature = "api")]
pub use octos_store::user_store;
pub mod workflow_runtime;
pub mod workflows;
