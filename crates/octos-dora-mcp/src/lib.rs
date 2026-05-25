//! Compatibility re-export crate.
//!
//! The canonical home for the dora bridge types is now
//! [`octos_agent::tools::dora_bridge`]. This crate is preserved for the
//! older `octos-dora-mcp` rename and forwards everything via re-export.
//!
//! New code should depend on `octos-agent` directly.

pub use octos_agent::tools::dora_bridge::{
    BridgeConfig, DoraToolBridge, DoraToolMapping, default_timeout, load_bridges,
};
