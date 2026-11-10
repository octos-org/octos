# UPCR-2026-RD6BF8098 — wire credential pool + content classifier into AdaptiveRouter

Status: proposed
PR: T-0549

## Decision record

`attach_credential_pool` / `set_content_classifier` were never called in CLI
production code, so a configured credential pool / content classifier had no
effect. This wires them through the single config→builder→router point
(`qos_catalog::build_adaptive_provider_chain`, shared with the gateway
profile-factory path): the same credential-pool `Arc` is attached to every
provider slot and the classifier is installed on the router, returned on
`AdaptiveProviderBundle` so serve/session runtime use the SAME object (serve no
longer builds them separately). Default unchanged (absent config → legacy
single-credential / strong-only). No protocol-visible wire change; the
`ui_protocol_tests.rs` edit only updates a test ProfileRuntime construction for
the two new bundle fields. Cite this record elsewhere as
`UPCR-2026-RD6BF8098`.
