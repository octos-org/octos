# Portable runtime

Octos bundles `octos-pptx` and the version-matched instructions. Call `pptx_tool`
with `action: doctor`; use the [Rust runtime](rust-runtime.md) for all operations.
No Python, Node or LibreOffice is needed in the production path. A missing tool
means the installed release bundle is incomplete; do not silently rebuild an
alternate page generator. Existing accepted projects remain immutable.

Font availability and the image-service account are separate from installation.
Inspect reported font substitution and missing glyphs; preserve the user's font
choice or report a real missing font. Image generation/editing uses the host's
configured image tool. Keep credentials in its existing configuration, never in
page sources, generated prompts or skills. Both peer sessions must receive actual
PNG content using their image-capable reader.

The native renderer writes PNG directly from the saved/reopened PPTX. Label it
DeckCraft-rendered; PowerPoint compatibility is a separate application check.
