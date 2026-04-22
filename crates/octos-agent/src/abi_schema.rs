//! Harness ABI schema versioning.
//!
//! The harness exposes four durable types to external app skills and
//! integrations:
//!
//! - [`WorkspacePolicy`](crate::workspace_policy::WorkspacePolicy)
//! - [`HookPayload`](crate::hooks::HookPayload)
//! - [`ProgressEvent`](crate::progress::ProgressEvent) (emitted shape)
//! - [`TaskResult`](octos_core::TaskResult)
//!
//! Each serialized instance carries a numeric `schema_version` (v1 is the
//! current shape). Missing versions default to v1 for backward compatibility
//! with policies and payloads that pre-date this module.
//!
//! See `docs/OCTOS_HARNESS_ABI_VERSIONING.md` for the stable vs experimental
//! field list and the deprecation rules.
//!
//! This module centralizes the per-type constants and the typed
//! "unsupported schema version" error. Callers MUST validate the version on
//! deserialization via [`check_supported`] before using fields that are
//! specific to the current shape.

use std::fmt;

/// Current schema version for `WorkspacePolicy`.
pub const WORKSPACE_POLICY_SCHEMA_VERSION: u32 = 1;

/// Current schema version for `CompactionPolicy` (harness M6.3).
///
/// Carries the declarative compaction contract: token budget, preserved
/// artifacts/invariants, preflight threshold, tool-result pruning policy, and
/// the summarizer flavour. Persisted instances include this field so durable
/// policy files replay across harness upgrades.
pub const COMPACTION_POLICY_SCHEMA_VERSION: u32 = 1;

/// Current schema version for `HookPayload`.
pub const HOOK_PAYLOAD_SCHEMA_VERSION: u32 = 1;

/// Current schema version for `ProgressEvent` and its legacy serialized
/// envelope at `octos.agent.progress.event.v1`.
pub const PROGRESS_EVENT_SCHEMA_VERSION: u32 = 1;

/// Current schema version for `TaskResult`.
pub const TASK_RESULT_SCHEMA_VERSION: u32 = 1;

/// Current schema version for `HarnessError` events (M6.1, issue #488).
/// Emitted as part of `octos.harness.event.v1` with `kind: "error"`.
pub const HARNESS_ERROR_SCHEMA_VERSION: u32 = 1;

/// Current schema version for `CredentialPoolConfig` persisted in profile
/// files (M6.5). Bumped when the persisted state shape or the `Config`
/// patch contract evolves in a non-backward-compatible way.
pub const CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION: u32 = 1;

/// Typed error returned when a deserialized value advertises a schema version
/// the running harness does not know how to handle.
///
/// This is NOT a panic — callers can log it, surface an actionable error to
/// operators, and optionally fall back to safe defaults. Unknown versions are
/// always rejected upstream rather than silently truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSchemaVersionError {
    /// Human-readable name of the ABI type that failed validation.
    pub kind: &'static str,
    /// The version that the file or payload advertised.
    pub found: u32,
    /// The highest version this harness supports for `kind`.
    pub supported: u32,
}

impl fmt::Display for UnsupportedSchemaVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unsupported {} schema_version {} (max supported: {}); upgrade octos to a newer release",
            self.kind, self.found, self.supported
        )
    }
}

impl std::error::Error for UnsupportedSchemaVersionError {}

/// Validate that `found` is at most `supported`. Returns `Ok(())` if the
/// version is within range (including older versions, which stay readable
/// via their defaulted fields), and a typed error otherwise.
pub fn check_supported(
    kind: &'static str,
    found: u32,
    supported: u32,
) -> Result<(), UnsupportedSchemaVersionError> {
    if found > supported {
        Err(UnsupportedSchemaVersionError {
            kind,
            found,
            supported,
        })
    } else {
        Ok(())
    }
}

/// Default schema version for `WorkspacePolicy` deserialization. Applied when
/// an older policy file omits the field entirely.
pub(crate) fn default_workspace_policy_schema_version() -> u32 {
    WORKSPACE_POLICY_SCHEMA_VERSION
}

/// Default schema version for `HookPayload` deserialization.
pub(crate) fn default_hook_payload_schema_version() -> u32 {
    HOOK_PAYLOAD_SCHEMA_VERSION
}

/// Default schema version for `CompactionPolicy` deserialization. Applied when
/// an older workspace-policy file omits the nested `schema_version` line.
pub(crate) fn default_compaction_policy_schema_version() -> u32 {
    COMPACTION_POLICY_SCHEMA_VERSION
}

/// Default schema version for `CredentialPoolConfig` deserialization (M6.5).
/// Applied when an older profile file omits the field entirely.
pub fn default_credential_pool_config_schema_version() -> u32 {
    CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_supported_accepts_current_version() {
        assert!(
            check_supported(
                "WorkspacePolicy",
                WORKSPACE_POLICY_SCHEMA_VERSION,
                WORKSPACE_POLICY_SCHEMA_VERSION
            )
            .is_ok()
        );
    }

    #[test]
    fn check_supported_accepts_older_versions() {
        // Older files (still v1 today) remain readable by design.
        assert!(check_supported("HookPayload", 1, HOOK_PAYLOAD_SCHEMA_VERSION).is_ok());
    }

    #[test]
    fn check_supported_rejects_future_versions_with_typed_error() {
        let err = check_supported("WorkspacePolicy", 99, WORKSPACE_POLICY_SCHEMA_VERSION)
            .expect_err("future version should be rejected");
        assert_eq!(err.kind, "WorkspacePolicy");
        assert_eq!(err.found, 99);
        assert_eq!(err.supported, WORKSPACE_POLICY_SCHEMA_VERSION);
        let rendered = err.to_string();
        assert!(rendered.contains("schema_version 99"));
        assert!(rendered.contains("WorkspacePolicy"));
    }

    #[test]
    fn defaults_match_current_versions() {
        assert_eq!(
            default_workspace_policy_schema_version(),
            WORKSPACE_POLICY_SCHEMA_VERSION
        );
        assert_eq!(
            default_hook_payload_schema_version(),
            HOOK_PAYLOAD_SCHEMA_VERSION
        );
        assert_eq!(
            default_credential_pool_config_schema_version(),
            CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION
        );
    }

    #[test]
    fn credential_pool_config_schema_version_is_pinned() {
        // Pin the version so later bumps are forced through a code review.
        assert_eq!(CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION, 1);
    }

    #[test]
    fn credential_pool_check_supported_accepts_current_version() {
        assert!(
            check_supported(
                "CredentialPoolConfig",
                CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION,
                CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION
            )
            .is_ok()
        );
    }

    #[test]
    fn credential_pool_check_supported_rejects_future_versions() {
        let err = check_supported(
            "CredentialPoolConfig",
            99,
            CREDENTIAL_POOL_CONFIG_SCHEMA_VERSION,
        )
        .expect_err("future version should be rejected");
        assert_eq!(err.kind, "CredentialPoolConfig");
        assert_eq!(err.found, 99);
    }
}
