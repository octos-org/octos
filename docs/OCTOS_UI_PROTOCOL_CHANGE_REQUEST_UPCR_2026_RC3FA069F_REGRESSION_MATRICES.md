# UPCR-2026-RC3FA069F — Table-driven regression matrices without protocol changes

Status: proposed (implemented for review)
Task: T-0274 / T-0201 / T-0208 / T-0221 / T-0227 / T-0229 / T-0231 / T-0233
Capability: none (no protocol change)

## Decision record

Seven independent regression groups collapse duplicated harness control flow
into named table-driven cases. No RPC method, notification, event payload,
schema, capability, permission, lifecycle, or client-visible behavior
changes. WS Origin rejection reasons and every other error shape, command,
message, and filesystem assertion remain intact; 20 scenarios remain while
20 harness functions become 7. Cite this record as `UPCR-2026-RC3FA069F`.

Validation: the focused loader, plugin-tool, prompt, budget, admin, router,
and WS-Origin groups pass on the PR description's commands; agent/CLI
clippy, formatting, typos, and diff checks pass. Each original slice killed
a targeted production mutation before that mutation was reverted.
