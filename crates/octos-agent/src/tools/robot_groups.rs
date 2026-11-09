//! Registry mapping robot tools to supervisory safety tiers.
//!
//! The four groups `group:robot:observe`, `group:robot:safe_motion`,
//! `group:robot:full_actuation`, and `group:robot:emergency_override` are
//! resolved through this registry. Integrators register each robot tool with
//! the minimum tier it requires; `ToolPolicy::evaluate` then expands group
//! names against the current registry, so tools move through the standard
//! allow / deny pipeline without any Tool-trait changes.
//!
//! Subset semantics (strictly nested):
//!   observe ⊂ safe_motion ⊂ full_actuation ⊂ emergency_override
//!
//! A policy granting `group:robot:safe_motion` implicitly allows every
//! `observe` tool. Granting `group:robot:emergency_override` allows all four
//! tiers.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{OnceLock, RwLock};

use crate::permissions::SafetyTier;

/// Group name prefix — every robot-tier group is `group:robot:<tier>`.
pub const ROBOT_GROUP_PREFIX: &str = "group:robot:";

/// Build the canonical group name for a tier.
pub fn group_name(tier: SafetyTier) -> String {
    format!("{ROBOT_GROUP_PREFIX}{}", tier.label())
}

/// Parse a group name and return the embedded tier, if any.
pub fn parse_group_name(group: &str) -> Option<SafetyTier> {
    let tier = group.strip_prefix(ROBOT_GROUP_PREFIX)?;
    tier.parse::<SafetyTier>().ok()
}

/// Registry of robot tools keyed by the minimum tier each one requires.
#[derive(Debug, Default, Clone)]
pub struct RobotToolRegistry {
    /// One entry per tier holding the explicit tools at that tier
    /// (not the expanded subset — subset expansion is done on read).
    by_tier: BTreeMap<SafetyTier, BTreeSet<String>>,
}

impl RobotToolRegistry {
    /// Create an empty registry. The robot integrator populates it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool at the given tier. Returns `true` if newly inserted.
    pub fn insert(&mut self, tool_name: impl Into<String>, tier: SafetyTier) -> bool {
        let name = tool_name.into();
        // Keep a tool in at most one tier — prefer the most-recently-set.
        for set in self.by_tier.values_mut() {
            set.remove(&name);
        }
        self.by_tier.entry(tier).or_default().insert(name)
    }

    /// Remove a tool from every tier. Returns `true` if anything was removed.
    pub fn remove(&mut self, tool_name: &str) -> bool {
        let mut removed = false;
        for set in self.by_tier.values_mut() {
            removed |= set.remove(tool_name);
        }
        removed
    }

    /// Tier a tool is registered at, if any.
    pub fn tier_of(&self, tool_name: &str) -> Option<SafetyTier> {
        for (tier, set) in &self.by_tier {
            if set.contains(tool_name) {
                return Some(*tier);
            }
        }
        None
    }

    /// True if any robot tool is registered.
    pub fn is_empty(&self) -> bool {
        self.by_tier.values().all(|set| set.is_empty())
    }

    /// All tools permitted at `tier` (includes all lower tiers — subset semantics).
    pub fn tools_for_tier(&self, tier: SafetyTier) -> Vec<String> {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for (t, set) in &self.by_tier {
            if *t <= tier {
                out.extend(set.iter().cloned());
            }
        }
        out.into_iter().collect()
    }

    /// True if `tool_name` is at `tier` or lower (i.e. permitted at `tier`).
    pub fn matches_group(&self, group: &str, tool_name: &str) -> bool {
        match parse_group_name(group) {
            Some(tier) => self.tier_of(tool_name).is_some_and(|actual| actual <= tier),
            None => false,
        }
    }
}

/// Global registry instance. Tests and integrators mutate it through
/// `install_registry` or `with_registry_mut`.
static GLOBAL: OnceLock<RwLock<RobotToolRegistry>> = OnceLock::new();

fn cell() -> &'static RwLock<RobotToolRegistry> {
    GLOBAL.get_or_init(|| RwLock::new(RobotToolRegistry::new()))
}

/// Replace the global registry with `next`. Useful for integrators and tests.
pub fn install_registry(next: RobotToolRegistry) {
    let lock = cell();
    *lock.write().unwrap_or_else(|e| e.into_inner()) = next;
}

/// Read-only snapshot of the global registry.
pub fn snapshot() -> RobotToolRegistry {
    cell().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Mutate the global registry in place.
pub fn with_registry_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut RobotToolRegistry) -> R,
{
    let mut guard = cell().write().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Fast path used by `ToolPolicy::evaluate` — true if `tool_name` falls under
/// robot group `group` per the global registry.
pub fn group_covers_tool(group: &str, tool_name: &str) -> bool {
    if !group.starts_with(ROBOT_GROUP_PREFIX) {
        return false;
    }
    cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .matches_group(group, tool_name)
}

/// True if any robot group references `tool_name` (i.e. the tool has a tier).
pub fn tool_has_tier(tool_name: &str) -> bool {
    cell()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .tier_of(tool_name)
        .is_some()
}
