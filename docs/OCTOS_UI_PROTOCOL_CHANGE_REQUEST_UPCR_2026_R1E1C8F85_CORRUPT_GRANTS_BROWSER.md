# UPCR-2026-R1E1C8F85 — corrupt grants file must not strip browser turn capability

Status: proposed (behavior fix, no contract change)
PR: TBD (T-0309)

## Decision record

`UPCR-2026-R1E1C8F85`.

`observe_background_grant` failed closed for EVERY connection when the
work-secret grants file was corrupt/unreadable, so an unrelated corruption
rejected all ordinary browser `turn/start`. The file is now read
fail-closed only for INGRESS binds (a secret was presented); non-ingress
browser turns degrade to a read-only observation of the current
background-work generation (`BackgroundWorkStore::peek`), which neither
retires nor cancels legitimate background work and is not mistaken for a
revocation. Nothing is cached: a repaired file is observed normally on the
next admission. No wire-shape, permission, or error-label change.
