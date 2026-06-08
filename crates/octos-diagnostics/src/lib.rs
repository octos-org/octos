//! `octos-diagnostics` — shared, **product-agnostic** diagnostics + update
//! *planning* for the octos binaries (`octos doctor`, and later `octos update
//! --check`).
//!
//! This crate is the dependency-light seam factored out of octos-tui's
//! `doctor`/`install_method` modules (octos-tui#182, ADR
//! `docs/adr/octos-diagnostics-extraction-scope.md`). Stage 1 ships:
//!
//! - the report model ([`CheckStatus`] / [`Check`] / [`Report`]) with the
//!   `[✓]/[!]/[✗]` glyphs, JSON support-bundle output, and exit-code policy;
//! - a [`ProductSpec`] seam so the same logic serves both octos-tui and the
//!   octos server — **the current version is always passed IN** by the caller,
//!   never read from this crate's `CARGO_PKG_VERSION`;
//! - [`InstallMethod`] classification ([`classify_path`] pure + [`detect`]
//!   path/env heuristics), PATH/shadow detection ([`locate`], [`on_path_check`],
//!   [`shadow_check`]) including the #189 npm-shim handling;
//! - semver parse/compare helpers + a pure [`plan`] producing an [`UpdatePlan`]
//!   (NO network, NO mutation);
//! - generic local checks (terminal, config/data-dir writability) and a
//!   [`protocol_skew_check`] adapter over `octos_core::ui_protocol`'s pure
//!   comparator.
//!
//! Stage 1 deliberately carries **no** network/update deps (no `reqwest`, no
//! `axoupdater`); the default build (`default = []`) stays dep-light.
//!
//! **Stage 2** adds an OPTIONAL `github` feature gating a blocking GitHub
//! Releases client ([`reachability`], [`latest_release`], [`parse_release`]) and
//! [`update_check`] — fetch-latest + the pure [`plan`], **planning only, no
//! mutation**. `reqwest` is pulled in ONLY under `github`; the default build is
//! unchanged. Self-update (axoupdater) remains Stage 3 and is NOT introduced.

#![allow(clippy::result_large_err)]

mod checks;
#[cfg(feature = "github")]
mod github;
mod install_method;
mod locate;
mod report;
mod spec;
mod update;

pub use checks::{
    TerminfoProbe, config_writability_check, data_writability_check, protocol_skew_check,
    terminal_checks, writability_check,
};
#[cfg(feature = "github")]
pub use github::{
    Reachability, ReleaseInfo, host_target_triple, latest_release, parse_release, reachability,
    update_check,
};
pub use install_method::{InstallMethod, PathClassifierInput, classify_path, detect};
pub use locate::{LocatedBinaries, locate, on_path_check, shadow_check};
pub use report::{Check, CheckStatus, Report};
pub use spec::{AssetSelector, ProductSpec};
pub use update::{SemVer, UpdatePlan, is_newer, parse_version, plan};
