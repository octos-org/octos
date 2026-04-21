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
            Some(tier) => self
                .tier_of(tool_name)
                .is_some_and(|actual| actual <= tier),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_produce_canonical_group_names() {
        assert_eq!(group_name(SafetyTier::Observe), "group:robot:observe");
        assert_eq!(
            group_name(SafetyTier::SafeMotion),
            "group:robot:safe_motion"
        );
        assert_eq!(
            group_name(SafetyTier::FullActuation),
            "group:robot:full_actuation"
        );
        assert_eq!(
            group_name(SafetyTier::EmergencyOverride),
            "group:robot:emergency_override"
        );
    }

    #[test]
    fn should_round_trip_group_names() {
        for tier in SafetyTier::ALL {
            assert_eq!(parse_group_name(&group_name(*tier)), Some(*tier));
        }
        assert!(parse_group_name("group:robot:banana").is_none());
        assert!(parse_group_name("group:fs").is_none());
    }

    #[test]
    fn should_expand_tier_to_include_lower_tiers() {
        let mut reg = RobotToolRegistry::new();
        reg.insert("camera_read", SafetyTier::Observe);
        reg.insert("slow_move", SafetyTier::SafeMotion);
        reg.insert("fast_move", SafetyTier::FullActuation);
        reg.insert("e_stop", SafetyTier::EmergencyOverride);

        let observe = reg.tools_for_tier(SafetyTier::Observe);
        assert_eq!(observe, vec!["camera_read".to_string()]);

        let safe = reg.tools_for_tier(SafetyTier::SafeMotion);
        assert_eq!(safe, vec!["camera_read".to_string(), "slow_move".to_string()]);

        let full = reg.tools_for_tier(SafetyTier::FullActuation);
        assert_eq!(full.len(), 3);
        assert!(full.contains(&"fast_move".to_string()));

        let emergency = reg.tools_for_tier(SafetyTier::EmergencyOverride);
        assert_eq!(emergency.len(), 4);
    }

    #[test]
    fn should_match_group_when_tool_tier_at_or_below_grant() {
        let mut reg = RobotToolRegistry::new();
        reg.insert("camera_read", SafetyTier::Observe);
        reg.insert("fast_move", SafetyTier::FullActuation);

        assert!(reg.matches_group("group:robot:safe_motion", "camera_read"));
        assert!(!reg.matches_group("group:robot:safe_motion", "fast_move"));
        assert!(reg.matches_group("group:robot:full_actuation", "fast_move"));
        assert!(reg.matches_group("group:robot:emergency_override", "fast_move"));
        assert!(!reg.matches_group("group:robot:observe", "fast_move"));
        // Non-robot group never matches here.
        assert!(!reg.matches_group("group:fs", "camera_read"));
    }

    #[test]
    fn should_move_tool_to_new_tier_on_reinsert() {
        let mut reg = RobotToolRegistry::new();
        reg.insert("arm", SafetyTier::Observe);
        assert_eq!(reg.tier_of("arm"), Some(SafetyTier::Observe));
        reg.insert("arm", SafetyTier::FullActuation);
        assert_eq!(reg.tier_of("arm"), Some(SafetyTier::FullActuation));
        assert_eq!(reg.tools_for_tier(SafetyTier::Observe), Vec::<String>::new());
    }
}
