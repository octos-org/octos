//! Plugin system for extending the agent with external tools.
//!
//! A plugin is a directory containing a `manifest.json` and an executable.
//! The executable receives tool arguments on stdin as JSON and returns
//! `{ "output": "...", "success": true/false }` on stdout.

pub mod extras;
pub mod http_discovery;
pub mod install;
pub mod loader;
pub mod manifest;
pub mod tool;

pub use extras::{SkillExtras, resolve_extras};
pub use http_discovery::fetch_http_tool_catalog;
pub use install::{
    SkillActivateResult, activate_skill, register_http_skills_on_startup, run_shutdown_phase,
};
pub use loader::{
    LoadedSkillAction, PluginLoadError, PluginLoadOptions, PluginLoadResult, PluginLoader,
};
pub use manifest::{
    PluginManifest, PluginToolDef, SkillActionBinding, SkillActionDef, SkillActionExecution,
    SkillActionFileMaterialization, SkillActionInputMode,
};
pub use tool::{PluginTool, SynthesisConfig};
