# Shared page method

Use one common page-data schema, style table and Rust builder for the whole deck.
The model determines authoritative copy, local owner regions and placement from
the actual design. Each independent label has its own text occurrence and box.
For tilted labels inspect four local corners and the baseline, not the containing
scene panel. Read [label geometry](label-geometry.md).

Separate decisions from numeric operations: choose each local text area, rotation
and same-role group from the artwork first; then ask `fit` to place visible glyphs
in those targets. Use grouped fitting for shared sizes. Review the whole card,
list or diagram as a unit, so individually plausible labels do not acquire
inconsistent sizes or obstruct neighboring arrows. OCR can locate a phrase but
cannot establish the usable area around it.

Use `cycle` for build/reopen/render/inspection; inspect the full page, then dense
crops from `crop`, whether flagged or not. `fit` proposes typography changes,
`patch` applies accepted data changes, and `compare` makes before/after panels.
See [Rust runtime](rust-runtime.md). Differences show change, not improvement.

A worker edits only its own page data. It returns source and exact checked cycle;
the coordinator assembles those page sources with the same builder and reviews
the actual final deck. Neither a successful build nor a worker summary is acceptance.
