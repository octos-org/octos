# UPCR-2026-RE909673C — Telegram transport for `serve.im` (no protocol change)

Status: proposed
PR: #3365

## Decision record

T-0663 (`docs/IM_REMOTE_CONTROL.md` §9, §14) connects each `serve.im` bot
(UPCR-2026-RB5306F64) to Telegram. A bot's direct messages reach the bot's
IM bridge (UPCR-2026-REF9D0516), and the bridge's replies go back.

Nothing on the wire changes:

- No method, params, result, error code, message, `data`, capability,
  feature token or event shape is added, removed or altered. The bridge's
  turns are the same external turns on a ledger-only connection, and only
  a message's text reaches the model and the ledger.
- `api/ui_protocol_im_telegram.rs` polls `getUpdates` through
  `octos_bus::telegram_api`. It confirms an update to Telegram only after
  the bridge has handled it, and it sends replies with `sendMessage`. Both
  are Telegram Bot API calls, not OUP.
- `api/ui_protocol_im_config.rs` gives each bridge its bot's transport
  instead of a placeholder that sent nothing, and stops polling when serve
  drops its `serve.im` state.
- The bridge's module notes now say what feeds it; its behaviour is
  unchanged.

Cite this record elsewhere as `UPCR-2026-RE909673C`.
