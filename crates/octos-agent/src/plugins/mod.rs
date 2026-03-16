//! Plugin system for extending the agent with external tools.
//!
//! A plugin is a directory containing a `manifest.json` and an executable.
//! The executable receives tool arguments on stdin as JSON and returns
//! `{ "output": "...", "success": true/false }` on stdout.

pub mod extras;
pub mod loader;
pub mod manifest;
pub mod tool;

pub use extras::{SkillExtras, resolve_extras};
pub use loader::{PluginLoadResult, PluginLoader};
pub use manifest::PluginManifest;
pub use tool::PluginTool;
