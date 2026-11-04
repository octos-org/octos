# UPCR-2026-R98434402 — Disabled-profile session/open contract pinned (no protocol change)

Status: proposed (implemented for review)
PR: #3028

## Decision record

The 2026-11-04 audit read #2971's launch/resolve comment ("an authenticated
but disabled profile cannot be activated or opened") and suspected
`session/open` still admits an OTP-authenticated connection whose profile is
`enabled: false`. Verified empirically with a real `open_session_result`
integration test: it does — and this record pins that as the intended
contract.

**No wire change.** No RPC method, parameter, notification, capability, or
error shape changes. The changes are a regression test plus a comment
clarification only:

- `enabled` on a profile gates ONLY the standalone gateway auto-start. BYOK
  public profiles are created `enabled: false` permanently and must keep
  serving authenticated AppUI/OTP sessions; `ensure_session_profile_runtime`
  documents this and
  `should_bootstrap_appui_runtime_when_gateway_autostart_is_disabled` pins it.
  OTP login likewise never consults `enabled`.
- `launch/resolve` returning `NoProfile` for an absent/disabled
  authenticated profile is an onboarding/launch decision only — it does not
  and must not block `session/open`.
- There is intentionally no operator user-suspension mechanism today. Adding
  one (e.g. a dedicated suspension flag enforced at the auth layer) is a
  separate product decision and explicitly out of scope here.

Cite this record as `UPCR-2026-R98434402`.
