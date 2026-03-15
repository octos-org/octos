//! Plugin SDK for crew-rs: manifest parsing, plugin discovery, and gating.
//!
//! This crate provides the foundational types and logic for the crew-rs plugin
//! system. It handles:
//!
//! - **Manifest parsing** — reading and validating `manifest.json` files
//! - **Plugin discovery** — scanning directories for plugins with precedence
//! - **Gating** — checking binary, environment, and OS requirements
//!
//! # Example
//!
//! ```no_run
//! use std::collections::HashMap;
//! use std::path::PathBuf;
//! use crew_plugin::discovery::{PluginSource, discover_plugins};
//! use crew_plugin::types::PluginOrigin;
//!
//! let sources = vec![
//!     PluginSource {
//!         path: PathBuf::from("/home/user/.crew/plugins"),
//!         origin: PluginOrigin::User,
//!     },
//! ];
//! let plugins = discover_plugins(&sources, &HashMap::new());
//! for p in &plugins {
//!     println!("{}: {:?}", p.manifest.id, p.status);
//! }
//! ```

pub mod discovery;
pub mod gating;
pub mod manifest;
pub mod types;

// Re-export primary types for convenience.
pub use discovery::{PluginSource, discover_plugins};
pub use gating::{GateCheck, GatingResult, check_requirements};
pub use manifest::{InstallSpec, PluginManifest, PluginType, Requirements, ToolDefinition};
pub use types::{DiscoveredPlugin, PluginOrigin, PluginStatus};
