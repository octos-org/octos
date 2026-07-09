//! External agent bridge contracts.
//!
//! The bridge module owns small, transport-agnostic primitives that let
//! `octos serve` issue short-lived session ingress credentials without
//! coupling guest agents to dashboard or OAuth auth flows.

pub mod work_secret;
