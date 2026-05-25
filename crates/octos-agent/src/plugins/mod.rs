//! Plugin system for extending the agent with external tools.
//!
//! A plugin is a directory containing a `manifest.json` and an executable.
//! The executable receives tool arguments on stdin as JSON and returns
//! `{ "output": "...", "success": true/false }` on stdout.

pub mod extras;
pub mod http_discovery;
pub mod loader;
pub mod manifest;
pub mod tool;

pub use extras::{SkillExtras, resolve_extras};
pub use http_discovery::fetch_http_tool_catalog;
pub use loader::{
    PluginLoadOptions, PluginLoadResult, PluginLoader, SkillActivateResult, activate_skill,
    run_shutdown_phase,
};
pub use manifest::{PluginManifest, PluginToolDef};
pub use tool::{PluginTool, SynthesisConfig};
