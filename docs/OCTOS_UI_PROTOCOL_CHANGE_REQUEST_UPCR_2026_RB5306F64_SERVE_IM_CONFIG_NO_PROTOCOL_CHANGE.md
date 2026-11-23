# UPCR-2026-RB5306F64 — `serve.im` config and one owner per bot (no protocol change)

Status: proposed
PR: #3361

## Decision record

T-0654 (`docs/IM_REMOTE_CONTROL.md` §7, §11, §14) makes the IM bridges
(UPCR-2026-REF9D0516) from a read-only `serve.im` section of serve's config.
It also gives each bot account one consumer: serve, or one gateway.

Nothing on the wire changes:

- No method, params, result, error code, message, `data`, capability,
  feature token or event shape is added, removed or altered. Bindings are
  read from the config file only: there is no method to list, add or revoke
  one.
- `api/ui_protocol_im_config.rs` makes the bridges at serve start, before
  any gateway starts or any request is taken. Every binding must resolve
  through the existing `BoundProfile::verify` and `BoundChat::verify`.
  `ServeImBridge::new` must then accept each entry. If anything is wrong,
  serve does not start.
- The bridge's test helpers become visible to the sibling config tests.
  The bridge itself is unchanged.
- Bot locks (`~/.octos/bot-owners/<platform>-<bot id>.lock`) are local
  process state, not protocol.
- Without `serve.im`, serve and every gateway behave as before. A Telegram
  gateway channel is refused only when another process already consumes its
  bot.

Cite this record elsewhere as `UPCR-2026-RB5306F64`.
