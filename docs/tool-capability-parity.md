# Four-tool visual capability parity

A four-tool roster is an interface count, not a capability count. Pi's native
`read` attaches images; a roster containing Octos `read_file` previously exposed
only text reads even though the separate `view_image` could return pixels.

`read_file` now returns PNG/JPEG/WEBP images up to 5 MiB as `model_media`, through
exactly the same bounded image-header, scope and symlink checks as `view_image`.
It does not call another model. Image reads bypass the unchanged-text cache.
Unsupported or oversized images explain why no pixels were attached.

For an OpenAI-compatible provider such as DeepSeek, enable retained image
history for visual work with the standalone CLI runtime configuration below. For `serve`/Octoscode profiles,
put the same hints under `config.llm.primary.model_hints`; a top-level profile
`config.model_hints` is ignored when the active model selection is projected:

```json
{
  "tool_policy": {
    "allow": ["read_file", "write_file", "edit_file", "bash"],
    "deny": []
  },
  "model_hints": {
    "retain_tool_images": true,
    "lacks_vision": false,
    "reasoning_style": "effort_and_thinking_toggle"
  }
}
```

The reasoning style above is specific to the DeepSeek test route. When changing
an existing configuration, preserve its other model hints. The new retention
hint defaults to false for compatibility. When enabled, old image attachments
remain in their original request positions until their transcript rows are
compacted away; modality fallback still applies. It does not retain old videos.

New tool images are now copied into persistent, integrity-checked snapshots
before entering history; later changes to the original filename cannot change
historical pixels. See [tool-io-parity.md](tool-io-parity.md) for retention and
MCP behavior. This is not a guarantee of provider-side cache hits. Retained
images consume image/context quota. Pi's Files API adapter and Octos inline
transport are still different transports.

## Controlled comparison contract

- Verify a real native image read in the producer's main request, followed by
  another tool call whose next request still includes that image. Metadata,
  `shown_to_model`, OCR and a separate visual report are insufficient evidence.
- Use the same image bytes and dimensions within both tools' limits. Pi can
  auto-resize; Octos currently reports oversized images. Prepare identical
  inspection views or disable Pi resizing for a controlled comparison.
- Keep the same skills, outline, assets, service helpers and model route.
  Native tool names need not match, but available operations must be recorded.
- Both edit tools support multiple replacements. Pi matches original regions;
  Octos applies replacements in sequence. Use independent, nonoverlapping
  changes for a matched capability probe. Do not claim identical semantics.
- Both write tools create parent directories. Both Bash tools run scripts;
  timeout defaults, output limits and text-cache behavior remain different.
- Count model requests made inside service helpers as auxiliary requests.
  Four native entry points do not mean only four possible operations.

## Regression checks

`read_image_wire` runs the real agent, four native tools and streaming provider
serializer against a local HTTP endpoint. It verifies actual image bytes and
an unchanged request prefix after a later text read. It requires no external
model and establishes transport correctness, not visual reasoning quality.

The read-file tests cover cached rereads, session-scoped images, traversal and
symlink refusals, invalid files and oversized images. Existing `view_image`
tests protect its shared implementation. The LLM tests cover image retention,
legacy defaults and modality fallback.
