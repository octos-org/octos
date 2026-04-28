//! Plugin SDK for octos: manifest parsing, plugin discovery, and gating.
//!
//! This crate provides the foundational types and logic for the octos plugin
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
//! use octos_plugin::discovery::{PluginSource, discover_plugins};
//! use octos_plugin::types::PluginOrigin;
//!
//! let sources = vec![
//!     PluginSource {
//!         path: PathBuf::from("/home/user/.octos/plugins"),
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
pub mod lifecycle;
pub mod manifest;
pub mod protocol_v2;
pub mod types;

// Re-export primary types for convenience.
pub use discovery::{PluginSource, discover_plugins};
pub use gating::{GateCheck, GatingResult, check_requirements};
pub use lifecycle::{
    BLOCKED_ENV_VARS, HardwareLifecycle, LifecycleExecutor, LifecyclePhase, LifecycleStep,
    NoSandbox, PhaseResult, SafePolicyDenial, Sandbox, StepKillReason, StepOutcome, StepResult,
    is_safe_shell_command,
};
pub use manifest::{InstallSpec, PluginManifest, PluginType, Requirements, ToolDefinition};
pub use protocol_v2::{
    ArtifactEvent, CostEvent, LineParse, LogEvent, PhaseEvent, ProgressEvent, ProtocolV2Event,
    ResultCost, ResultSource, ResultSummary, emit_cost, emit_event, emit_progress,
    parse_event_line,
};
pub use types::{DiscoveredPlugin, PluginOrigin, PluginStatus};
