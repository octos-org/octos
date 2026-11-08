# UPCR-2026-RCA306A51 — Octos-core protocol test diet

Status: proposed (implemented for review)
Task: T-0453 / #3069

## Decision record

No wire, DTO, method, capability, compatibility, or persistence contract changes. This change deletes `#[cfg(test)]`-only OUP JSON goldens, per-field serde assertions, and method/constant shape tests from `octos-core`; production protocol code and the specification are unchanged.

Cross-system OUP behavior remains covered by the octos-cli API/transport test suites and existing UPCR/spec gates. Octos-core deliberately retains the cross-artifact §10 numeric-error parity checks and §14 executable projection-example checks in `ui_protocol_tests.rs`; these are public-contract drift guards, not per-field serde tests. Other retained octos-core tests cover session routing, JSONL persistence compatibility, schema-version recovery, typed error behavior, redaction, path safety, locking, UTF-8, and byte boundaries. The frozen UPCR README registry is unchanged.
