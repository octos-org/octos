# UPCR-2026-R48CB467B — Reviewed skill install binds commit and artifact digest end to end

Status: proposed (implemented for review)
Task: T-0556
Capability: `profile.skills.reviewed_install.v1` (strict opt-in) gating the new
method `profile/skills/install_reviewed`.

## Decision record

The legacy `profile/skills/install` takes `repo/branch/force`: the registry
projection dropped `source_commit`/`reviewed_at`/`binaries`, the install
resolved the branch tip at install time, and the receipt carried only the
skill name — a TOCTOU chain where the object the reviewer approved and the
object Core installed could silently diverge (web #358, T-0554). This change
adds a SEPARATE method rather than optional fields on the legacy method: an
old core has neither the method nor the feature, so a web client has no
reviewed-install entry point there, and nothing depends on a server silently
ignoring unknown expectation fields.

Binding contract:

- `profile/skills/registry/search` now projects `source_commit` (full
  40-hex), `reviewed_at`, and `binaries` (`{platform: {url, sha256,
  signature}}`) per package. These are informational for old clients and the
  review record for new ones.
- `profile/skills/install_reviewed` params `{profile_id?, package, repo,
  source_commit, reviewed_at, platform, artifact?: {sha256, signature},
  force?}` are the EXPECTED values from the review. The server re-fetches the
  registry (fail closed on transport failure) and requires the current entry
  to match every expected field before any clone, download, or disk write:
  repo, source_commit, reviewed_at, and — for binary-bearing entries — the
  platform artifact's sha256 and signature. `platform` must be the server's
  own platform. Branch names are never resolved: git fetches the pinned
  commit object directly and `rev-parse HEAD` must equal it.
- The artifact is verified with `octos_plugin::integrity::verify_remote_binary`
  (Ed25519 signature over package/platform/digest/commit/review epoch against
  the operator trust root, then SHA-256 over the downloaded bytes) while it is
  still in staging. Download and archive-member byte caps are unchanged;
  archive member names are never used as paths; skill trees containing
  symlinks or special files are refused.
- Publish is staging → atomic rename; a replaced install is preserved under a
  rollback name until the post-publish runtime rebuild succeeds. Any failure
  — verification, staging, publish, rebuild — leaves the old install in place
  or restores it, and the RPC reports a typed error instead of success.
- The result and the post-install `profile/skills/list` report the ACTUAL
  `source_commit`, `reviewed_at`, and `artifact_sha256` from the
  `.reviewed-install.json` receipt written into the skill directory. Reviewed
  installs carry no `.source` branch pointer, so `octos skills update` refuses
  to follow a branch tip; updates require a fresh review. The call is
  synchronous and one-shot: a timed-out or unknown result is never replayed
  server-side, and remove stays the explicit inverse.

v1 scope limits (fail closed, documented): single-skill packages only; no
npm/cargo build steps and no lifecycle hooks run — build products are not the
reviewed object; `requires`/shared-dep auto-install is not performed.

Validation: `commands::skills::reviewed_install_tests` cover
review→install→re-read, branch drift, registry entry swap, served-bytes swap,
wrong-key signature, stale review, platform mismatch, missing artifact
expectation, symlink trees, force/rollback/delete semantics; the transport
tests cover the strict feature gate, `deny_unknown_fields`, and capability
announcement. Cite this record as `UPCR-2026-R48CB467B`.
