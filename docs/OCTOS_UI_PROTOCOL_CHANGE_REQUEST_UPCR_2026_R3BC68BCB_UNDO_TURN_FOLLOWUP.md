# UPCR-2026-R3BC68BCB — snapshot undo follow-up: conflict filenames, per-turn undo points, guard scope

Status: proposed (behavior fix, no contract change)
PR: TBD (T-0434)

## Decision record

`UPCR-2026-R3BC68BCB`.

Three snapshot/restore behavior fixes, no wire-shape or permission change:

1. `snapshot/restore` conflict errors now keep the full error chain
   (`{err:#}`), so the client-visible message names the conflicting files.
2. `take_snapshot` dedup now requires the same label for turn-scoped
   (`pre-turn <turn_id>`) snapshots, so every WS turn keeps its own undo
   point in `snapshot/list` even when its tree is unchanged; non-turn
   labels keep the old tree-only dedup. A turn and its identical-tree
   successor therefore appear as two commits with different ids.
3. A new `pre-turn` snapshot resets `refs/octos/last-restore`: agent
   writes in the new turn are no longer misclassified as foreign user
   edits by the restore conflict guard. A redo straight after an undo with
   no new turn still refuses to clobber genuine user edits, and the error
   names the files.
