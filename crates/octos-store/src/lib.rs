//! Self-contained persistence / state stores extracted from `octos-cli`.
//!
//! These modules were leaf modules in `octos-cli` (zero intra-crate
//! dependencies — they reference only external crates + `octos-core`), yet the
//! API server leaned on them heavily. Pulling them into their own crate shrinks
//! `octos-cli`, lets these rarely-changing stores compile once (faster
//! incremental builds), and is the low-risk first slice toward un-godding the
//! `octos-cli` crate. `octos-cli` re-exports these at its crate root, so every
//! existing `crate::<store>::…` reference keeps resolving unchanged.

pub mod admin_audit_store;
pub mod admin_token_store;
pub mod approvals_audit;
pub mod login_allowlist;
pub mod setup_state_store;
pub mod smtp_secret_store;
pub mod usage_ledger;
pub mod user_store;
