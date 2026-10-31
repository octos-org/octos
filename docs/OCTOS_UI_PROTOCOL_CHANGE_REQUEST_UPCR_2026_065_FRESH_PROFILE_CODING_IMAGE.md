# UPCR-2026-065 — Fresh Onboarding Profile Coding Image

Status: proposed (implemented for review)
Task: T-0083
Capability: no new OUP capability or wire-schema change

## Scope

Document the behavior change in the protocol-gated fresh local profile creation
path. `create_fresh_local_solo_profile` now starts from the existing
`ProfileConfig::default()` and changes only `sandbox.docker.image` to the public
`node:22-bookworm` image, which supplies Node, npm and Git for a minimal coding
workflow.

The global `ubuntu:24.04` default, legacy and existing profile creation paths,
and all previously stored profiles remain unchanged. The stored profile config
can still override the image.

## Compatibility and security

No OUP method, parameter, result, event, capability negotiation or persisted
schema changes. Network policy, workspace permissions, sandbox mode and every
other sandbox field keep their previous defaults. Docker cap-drop and
no-new-privileges execution flags are untouched.

## Image ownership

This first step deliberately uses a public image to validate the onboarding
requirement. Building, publishing and maintaining a dedicated Octos image is a
maintainer-owned release process and is not introduced by this change.
