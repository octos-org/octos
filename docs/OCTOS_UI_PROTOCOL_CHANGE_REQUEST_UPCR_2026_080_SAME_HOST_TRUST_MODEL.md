# UPCR-2026-080 — Same-Host Trust Model Clarification

Status: proposed (documentation implemented for review)
Task: T-0266

## Decision record

Users and profiles on the same machine are mutually trusted. Solo and same-host
gateway deployments do not provide filesystem content isolation between them.
Protocol identity, profile, and session scope checks are correctness and state
ownership controls, not confidentiality boundaries.

This is a documentation-only clarification. No permission check, wire method,
result type, capability, or transport behavior changes. In particular,
`session/open` cwd handling and local file/shell access are not narrowed by this
record. A hydrate refusal such as `1008` must not be documented as proof that a
same-host user cannot read transcript files through another local path.

External attackers and untrusted inputs remain in scope: revoked or forged
credentials, browser Origin checks, prompt-driven tool use, SSRF, secret
leakage, and host compromise. Deployments that require mutually untrusted
content boundaries must use an OS or container boundary outside this protocol.

T-0216's catalog confinement wording is aligned separately in its pending
branch: it prevents foreign metadata from being mislabeled as the current
profile, but does not claim transcript-body confidentiality on a shared host.
