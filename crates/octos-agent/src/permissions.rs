//! Supervisory safety tiers for robotic tool authorization.
//!
//! The four-tier model expresses how much physical risk a tool can take.
//! Tiers are enforced through `ToolPolicy` groups (`group:robot:<tier>`),
//! not through a dedicated trait method — see `tools::robot_groups`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Safety tiers ordered from least to most dangerous.
///
/// `Observe < SafeMotion < FullActuation < EmergencyOverride`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyTier {
    /// Read-only: cameras, sensors, status queries. No actuation.
    Observe,
    /// Low-risk motion: slow moves within verified workspace bounds.
    SafeMotion,
    /// Full-speed actuation with force control. Requires operator awareness.
    FullActuation,
    /// Bypass all safety limits. Emergency recovery only.
    EmergencyOverride,
}

impl SafetyTier {
    /// Canonical snake_case label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::SafeMotion => "safe_motion",
            Self::FullActuation => "full_actuation",
            Self::EmergencyOverride => "emergency_override",
        }
    }

    /// Slice of all tiers ordered least to most dangerous.
    pub const ALL: &'static [SafetyTier] = &[
        Self::Observe,
        Self::SafeMotion,
        Self::FullActuation,
        Self::EmergencyOverride,
    ];
}

impl fmt::Display for SafetyTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Error returned by `SafetyTier::from_str` for unknown tier names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSafetyTier {
    pub input: String,
}

impl fmt::Display for InvalidSafetyTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid safety tier '{}' (expected one of: observe, safe_motion, full_actuation, emergency_override)",
            self.input
        )
    }
}

impl std::error::Error for InvalidSafetyTier {}

impl FromStr for SafetyTier {
    type Err = InvalidSafetyTier;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observe" => Ok(Self::Observe),
            "safe_motion" => Ok(Self::SafeMotion),
            "full_actuation" => Ok(Self::FullActuation),
            "emergency_override" => Ok(Self::EmergencyOverride),
            _ => Err(InvalidSafetyTier {
                input: s.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_order_tiers_from_observe_to_emergency() {
        assert!(SafetyTier::Observe < SafetyTier::SafeMotion);
        assert!(SafetyTier::SafeMotion < SafetyTier::FullActuation);
        assert!(SafetyTier::FullActuation < SafetyTier::EmergencyOverride);
    }

    #[test]
    fn should_parse_canonical_names_when_from_str_called() {
        assert_eq!("observe".parse::<SafetyTier>().unwrap(), SafetyTier::Observe);
        assert_eq!(
            "safe_motion".parse::<SafetyTier>().unwrap(),
            SafetyTier::SafeMotion
        );
        assert_eq!(
            "full_actuation".parse::<SafetyTier>().unwrap(),
            SafetyTier::FullActuation
        );
        assert_eq!(
            "emergency_override".parse::<SafetyTier>().unwrap(),
            SafetyTier::EmergencyOverride
        );
    }

    #[test]
    fn should_accept_mixed_case_when_from_str_called() {
        assert_eq!(
            "OBSERVE".parse::<SafetyTier>().unwrap(),
            SafetyTier::Observe
        );
        assert_eq!(
            "Safe_Motion".parse::<SafetyTier>().unwrap(),
            SafetyTier::SafeMotion
        );
        assert_eq!(
            " emergency_override ".parse::<SafetyTier>().unwrap(),
            SafetyTier::EmergencyOverride
        );
    }

    #[test]
    fn should_reject_unknown_names_when_from_str_called() {
        let err = "dangerous".parse::<SafetyTier>().unwrap_err();
        assert_eq!(err.input, "dangerous");
        assert!(
            err.to_string().contains("invalid safety tier"),
            "error message should name the invalid input: {err}"
        );

        assert!("".parse::<SafetyTier>().is_err());
        assert!("safe-motion".parse::<SafetyTier>().is_err());
    }

    #[test]
    fn should_serialize_as_snake_case_json() {
        assert_eq!(
            serde_json::to_string(&SafetyTier::SafeMotion).unwrap(),
            "\"safe_motion\""
        );
        assert_eq!(
            serde_json::from_str::<SafetyTier>("\"full_actuation\"").unwrap(),
            SafetyTier::FullActuation
        );
    }
}
