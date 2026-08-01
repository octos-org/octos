//! Workflow execution subsystem extracted from `octos-cli`.
//!
//! `workflow_runtime` (with its `#[path]` `workflow_families` submodule) and the
//! `workflows` delivery modules form a self-contained unit — zero outbound
//! `crate::` dependencies on the rest of octos-cli; they only reference each
//! other. octos-cli re-exports both at its crate root, so every existing
//! `crate::workflow_runtime::…` / `crate::workflows::…` reference (session_actor,
//! project_templates, …) resolves unchanged.

pub mod workflow_runtime;
pub mod workflows;
