# UPCR-2026-R79767D0C — registry/search reports the Core host platform

Status: proposed
PR: T-0563

## Decision record

`profile/skills/install_reviewed` (UPCR-2026-R48CB467B) requires
`params.platform == platform_key()` — the Core host `{os}-{arch}` (macOS maps
to `darwin`). But `profile/skills/registry/search` previously returned only
`{profile_id, packages}`, so a client could not learn that value: source-only
packages carry no `binaries` map to read a key from, and multi-platform
packages do not say which key the host accepts. A web client would have to
guess from `navigator`/UA, which is neither authoritative nor verifiable.

This adds ONE additive read-only field to the search result: top-level
`server_platform`, produced by the SAME `platform_key()` the install gate
validates (no second OS/arch mapping that could drift). It is additive; old
clients ignore it, and old cores simply lack it — under
`profile.skills.reviewed_install.v1` semantics those cores also lack the
reviewed-install method and feature, so the web keeps no install entry point.
The legacy `profile/skills/install` is unchanged. A companion
`OCTOS_SKILLS_REGISTRY_URL` override (argument > env > default, shared by the
search projection and the reviewed-install re-fetch) lets operators point at a
private registry and lets tests run hermetically; it never changes the
validated identity fields. Cite this record elsewhere as
`UPCR-2026-R79767D0C`.
