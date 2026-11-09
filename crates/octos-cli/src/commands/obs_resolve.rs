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
/// (`{goal-ledgers,peers,inbox}` live here).
///
/// 整改令核心: this does NOT re-assemble the path itself — it opens the
/// profile registry the same way serve does and delegates to
/// [`crate::profiles::ProfileStore::resolve_data_dir`], the ONE function
/// serve uses (honouring a profile's explicit `data_dir` override and
/// the registry/data split). The registry root is the SHARED state home
/// (serve's `state_home`: profile `<id>.json` files are config-like and
/// shared across instances); the data root is the per-instance runtime
/// root. When the registry has no such profile (e.g. a fresh instance
/// where the profile file has not been written yet), we fall back to
/// the same default layout `resolve_data_dir` would produce for a
/// default profile: `<runtime_root>/profiles/<id>/data`.
pub(crate) fn resolve_profile_data_root(
    state_home: &Path,
    cwd: &Path,
    profile_id: &str,
) -> PathBuf {
    let runtime_root = resolve_instance_runtime_root(state_home, cwd);
    let fallback = || runtime_root.join("profiles").join(profile_id).join("data");
    let Ok(store) = crate::profiles::ProfileStore::open(state_home, &runtime_root) else {
        return fallback();
    };
    match store.get(profile_id) {
        Ok(Some(profile)) => store.resolve_data_dir(&profile),
        _ => fallback(),
    }
}

/// The default profile id the solo/operator flow runs under.
pub(crate) const DEFAULT_PROFILE_ID: &str = "octos";
