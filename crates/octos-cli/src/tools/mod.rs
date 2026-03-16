//! CLI-specific tools that live in octos-cli (not octos-agent) because they
//! depend on CLI-layer types (Config, profiles, provider creation).

pub mod switch_model;

pub use switch_model::SwitchModelTool;
