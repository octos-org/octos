# Completion and checked output

`pptx_tool` `cycle` is a synchronous native build/render/check operation. A
successful result names real completed output files and their hashes; no
background conversion server or polling helper is needed. A failed or cancelled
call is not a completed render. Preserve any partial output and use a fresh
attempt directory after diagnosing the failure.

For external image services, use their actual returned job IDs and the host's
existing job-completion tools. Do not launch duplicate jobs while a prior job is
pending. Never invent job success or count waiting/polling as a repair cycle.
Validate source/output association before using an external asset.

Runtime command schemas: [Rust runtime](rust-runtime.md). Reviewed image hashes
bind evidence to an artifact; they do not prove a reviewer understood the image.
