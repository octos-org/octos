# UPCR-2026-R3028FBE0 — Telegram transport hardening (no protocol change)

Status: proposed
PR: TBD

## Decision record

T-0665 (`docs/IM_REMOTE_CONTROL.md` §14) hardens the `serve.im` Telegram
transport (UPCR-2026-RE909673C) on the four follow-ups from the #3365
review.

Nothing on the wire changes:

- No method, params, result, error code, message, `data`, capability,
  feature token or event shape is added, removed or altered.
- `api/ui_protocol_im_telegram.rs`:
  - A piece Telegram refused with a 429 and a short `retry_after` is sent
    once more after that wait. A poll told to slow down waits as asked.
  - A message whose handling panics is logged and skipped after a backoff,
    within the same polling task.
- `octos_bus::telegram_api`:
  - It reads updates one by one, so one it cannot read is confirmed and
    skipped instead of failing the batch.
  - `ApiError::Refused` carries `retry_after`.
- `api/ui_protocol_im_bridge.rs`: a chat that is neither bound nor direct
  is told once that it is not linked, then not answered
  (`Handled::Ignored`). Direct chats are answered as before.

These are Telegram Bot API calls and bridge-internal outcomes, not OUP.

Cite this record elsewhere as `UPCR-2026-R3028FBE0`.
