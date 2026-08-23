//! Shared observability data-root resolution (OLP L1, 整改 slice 2/3/4).
//!
//! Contract: task-req-olp-obs-cli.spec.md + 外环整改令 (2026-08-24):
//! the read-only CLI commands must resolve the SAME per-instance data
//! root serve actually uses, via ONE shared helper — never a
//! re-implementation of the cwd-hash / profile / data-root assembly.
//!
//! Layout (produced by the launcher, e.g. octoscode
//! `instance_data_dir_for_launch`, which passes
//! `--instance-data-dir <octos_home>/instances/<cwd-hash>` to serve):
//!
//! ```text
//! <octos_home>/instances/<cwd-hash>/          ← per-instance RUNTIME root
//! <octos_home>/instances/<cwd-hash>/profiles/<profile>/data/
//!     {goal-ledgers, peers, inbox, …}         ← per-profile data
//! ```
//!
//! With no per-instance override the runtime root IS the state home
//! (`~/.octos`) and the profile data lives at `<state_home>/profiles/
//! <profile>/data`. The `cwd_hash` algorithm mirrors the launcher's
//! (`DefaultHasher` over the canonicalized cwd, 16 hex chars) — the SAME
//! SipHash fixed-key function serve's inbox hash uses, so "same Rust
//! toolchain" guarantees stability; the hash is an internal detail and
//! consumers always go through the CLI (operator 拍板: 零迁移).

use std::path::{Path, PathBuf};

/// Stable, filesystem-safe 16-hex hash of a directory — mirrors the
/// launcher's `cwd_hash` (octoscode profiles.rs) byte-for-byte:
/// `DefaultHasher` over the CANONICALIZED path. Deterministic across
/// processes (fixed SipHash keys).
pub(crate) fn cwd_instance_hash(cwd: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The per-instance runtime root for the CURRENT cwd's serve:
/// `<state_home>/instances/<cwd_hash(cwd)>` when that dir exists (the
/// multi-instance stdio layout), else `state_home` itself (single shared
/// instance — byte-identical to serve's `state_home == data_dir` case).
///
/// Detection by EXISTENCE is deliberate: serve picks the instance dir
/// only when the launcher explicitly passes `--instance-data-dir`, and
/// the launcher creates it on boot. A stale instance dir for a DIFFERENT
/// project can never hijack resolution because the hash keys on cwd.
pub(crate) fn resolve_instance_runtime_root(state_home: &Path, cwd: &Path) -> PathBuf {
    let instance_dir = state_home.join("instances").join(cwd_instance_hash(cwd));
    if instance_dir.is_dir() {
        instance_dir
    } else {
        state_home.to_path_buf()
    }
}

/// The PROFILE data root the obs commands read
/// (`{goal-ledgers,peers,inbox}` live here): serve resolves a profile's
/// data dir as `profile.data_dir` if set, else
/// `<runtime_root>/profiles/<profile_id>/data`
/// (`ProfileStore::resolve_data_dir`). We cannot cheaply evaluate the
/// profile's `data_dir` override without the registry; the default
/// profile ("octos") virtually never sets it, and an explicit
/// `--data-dir` flag on the command itself overrides the whole chain —
/// documented residual for custom `data_dir` profiles (a later slice can
/// thread the registry lookup).
pub(crate) fn resolve_profile_data_root(
    state_home: &Path,
    cwd: &Path,
    profile_id: &str,
) -> PathBuf {
    resolve_instance_runtime_root(state_home, cwd)
        .join("profiles")
        .join(profile_id)
        .join("data")
}

/// The default profile id the solo/operator flow runs under.
pub(crate) const DEFAULT_PROFILE_ID: &str = "octos";

#[cfg(test)]
mod tests {
    use super::*;

    /// 整改要求 3: replicate the REAL instance layout in a tempdir and
    /// read back through the resolver: `instances/<hash>/profiles/
    /// <profile>/data/...` must be found for the matching cwd, and a
    /// DIFFERENT cwd (different hash) must fall back to the state home.
    #[test]
    fn resolver_finds_real_instance_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_home = temp.path().join("octos_home");
        let project = temp.path().join("project-a");
        std::fs::create_dir_all(&project).expect("project dir");

        // Real layout: <home>/instances/<hash>/profiles/octos/data/
        let hash = cwd_instance_hash(&project);
        let instance_data = state_home
            .join("instances")
            .join(&hash)
            .join("profiles")
            .join("octos")
            .join("data");
        std::fs::create_dir_all(instance_data.join("goal-ledgers")).expect("layout");
        std::fs::create_dir_all(instance_data.join("peers")).expect("layout");

        let resolved = resolve_profile_data_root(&state_home, &project, "octos");
        assert_eq!(resolved, instance_data);
        assert!(resolved.join("goal-ledgers").is_dir());

        // A different cwd hashes to a different instance; with no such
        // instance dir on disk the resolver falls back to the state home
        // layout (single shared instance).
        let other = temp.path().join("project-b");
        std::fs::create_dir_all(&other).expect("other project");
        let resolved_other = resolve_profile_data_root(&state_home, &other, "octos");
        assert_eq!(
            resolved_other,
            state_home.join("profiles").join("octos").join("data")
        );
    }

    /// The hash is stable (same cwd → same hash across calls) and
    /// 16-hex-shaped like the launcher's.
    #[test]
    fn cwd_hash_is_stable_and_hex16() {
        let temp = tempfile::tempdir().expect("tempdir");
        let h1 = cwd_instance_hash(temp.path());
        let h2 = cwd_instance_hash(temp.path());
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
