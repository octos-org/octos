//! Typed harness errors.
//!
//! Bootstrapped by M6.7 so the synchronous `DelegateTool` can return a typed
//! depth-exhaustion error. M6.1 will consolidate and expand this module with
//! the full harness error hierarchy.
//
// TODO(M6.1): consolidate — merge these variants into the M6.1 harness error
// hierarchy and re-export from `lib.rs` through the unified surface.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Typed error surface for the harness.
///
/// Synchronous callers return `HarnessError` via `eyre::Report`; the concrete
/// variant is recovered by downcasting. Serde-stable so structured logs,
/// metrics, and persistence can round-trip the error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessError {
    /// Delegate tool tried to descend past the configured `max_depth`.
    /// `current` is the attempting parent's depth (the depth of the level
    /// that issued the call that would have exceeded the budget), `max` is
    /// the configured ceiling, and `attempted` is the depth the rejected
    /// child would have run at.
    DelegateDepthExceeded {
        current: u32,
        max: u32,
        attempted: u32,
    },
}

impl HarnessError {
    /// Stable metric label for the error variant.
    pub fn metric_label(&self) -> &'static str {
        match self {
            HarnessError::DelegateDepthExceeded { .. } => "delegate_depth_exceeded",
        }
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HarnessError::DelegateDepthExceeded {
                current,
                max,
                attempted,
            } => write!(
                f,
                "delegate_task rejected: depth budget exhausted (attempted depth {attempted} exceeds max {max}; parent already at depth {current})"
            ),
        }
    }
}

impl std::error::Error for HarnessError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_display_delegate_depth_exceeded_with_context() {
        let error = HarnessError::DelegateDepthExceeded {
            current: 2,
            max: 2,
            attempted: 3,
        };
        let rendered = error.to_string();
        assert!(rendered.contains("attempted depth 3"));
        assert!(rendered.contains("max 2"));
        assert!(rendered.contains("depth 2"));
    }

    #[test]
    fn should_expose_stable_metric_label_for_depth_error() {
        let error = HarnessError::DelegateDepthExceeded {
            current: 2,
            max: 2,
            attempted: 3,
        };
        assert_eq!(error.metric_label(), "delegate_depth_exceeded");
    }

    #[test]
    fn should_serde_round_trip_delegate_depth_exceeded() {
        let error = HarnessError::DelegateDepthExceeded {
            current: 1,
            max: 2,
            attempted: 3,
        };
        let json = serde_json::to_string(&error).unwrap();
        // Tag is the "kind" field and snake_case.
        assert!(json.contains("\"kind\":\"delegate_depth_exceeded\""));
        let parsed: HarnessError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, error);
    }
}
