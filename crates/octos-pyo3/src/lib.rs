//! octos-pyo3: a **native** Python extension for embedding octos, built with
//! [pyo3](https://pyo3.rs/) over the native core exposed by `octos-ffi`
//! ([`octos_ffi::OctosRuntime`]).
//!
//! This is the *recommended* Python binding. The sibling `octos-uniffi` crate
//! generates a *reference* Python module too, but its real purpose is Swift /
//! Kotlin from one definition; for Python, this hand-written pyo3 surface is the
//! idiomatic, faster path (compiled extension, no ctypes marshalling).
//!
//! Like `octos-uniffi`, this crate is a thin wrapper: it adds NO logic beyond
//! type marshalling. The hardened credential path (single key resolution +
//! pinning + exact secret-scrub of the caller's own key) lives entirely in
//! `octos-ffi`'s [`octos_ffi::OctosRuntime::from_config`], so it exists in
//! exactly one place, shared by the C-ABI, uniffi, and this pyo3 surface.
//!
//! # The `python` feature (default OFF)
//!
//! All pyo3 code lives in the [`bindings`] module behind the `python` feature.
//! With the feature OFF — the default, and what `cargo build --workspace` uses
//! on octos CI (self-hosted linux-x64/arm64 **and** Windows) — this crate is an
//! essentially empty library that links **zero** libpython, so a Python-less CI
//! lane cannot break. `maturin` turns the feature on (via `extension-module`) to
//! build the wheel; `cargo test -p octos-pyo3 --features python` runs the Rust
//! tests.
//!
//! # Safety
//!
//! This crate keeps the workspace-wide `deny(unsafe_code)` (restated explicitly
//! at the crate root) and writes NO hand-unsafe code of its own. pyo3 0.23's
//! proc-macros (`#[pyclass]`, `#[pymethods]`, `#[pymodule]`,
//! `create_exception!`) generate the `unsafe` FFI scaffolding that bridges Rust
//! and the CPython C-API, but they annotate that generated code so it does NOT
//! trip the crate's `unsafe_code` lint. So — unlike `octos-ffi`'s hand-written
//! C-ABI, which needs a crate-scoped `#![allow(unsafe_code)]` — this binding
//! compiles cleanly under `deny`, verified by the workspace build.
#![deny(unsafe_code)]

#[cfg(feature = "python")]
mod bindings;

#[cfg(feature = "python")]
pub use bindings::*;
