# Coding Agent Long Workspace Fixture

This fixture is intentionally broken. It models a small multi-crate Rust
workspace that should be realistic enough to exercise a long-running coding
agent UX without using network dependencies.

Suggested prompt:

> Fix this workspace so `cargo test --workspace` passes. Keep the public API
> small, preserve deterministic ordering, and update production code rather than
> weakening tests.

Expected starting point:

- `worklog-core` parses and schedules a tiny task format.
- `worklog-report` renders a status report from the core crate.
- Tests fail in both crates and require changes across multiple source files.
