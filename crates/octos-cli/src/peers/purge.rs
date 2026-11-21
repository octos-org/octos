//! `peer/purge` records (UPCR-2026-034, #2604): the tombstones and the audit
//! log that outlive a purged host-owned app peer.
//!
//! A purge erases the peer's directory `peers/<slug>/` (and with it the host
//! binding the host token was checked against), so the records that must
//! survive it live beside `peers/`, in the profile data dir:
//!
//! * `peer-purges/tokens/<sha256(host token)>.json`: one per purged peer, so a
//!   retried `peer/purge` with the same credential is answered
//!   `already_purged` instead of `peer_not_found`;
//! * `peer-purges/slugs/<slug>`: while no peer is staged under `<slug>` again,
//!   a `#peer-<slug>` session is refused (a stale client of the erased peer
//!   must not run on as an ordinary profile session with the profile's
//!   memory);
//! * `peer_purge_audit.jsonl`: one row per purge.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    app_binding::{PeerContextBinding, PeerHostBinding, host_token_matches},
    peer_io,
};

/// Directory (under the profile data dir) of the purge tombstones.
pub(crate) const PEER_PURGES_DIR: &str = "peer-purges";

/// Audit log leaf (in the profile data dir) of `peer/purge`.
pub(crate) const PEER_PURGE_AUDIT_LEAF: &str = "peer_purge_audit.jsonl";

/// The record a purge leaves under its host token's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PurgeTombstone {
    pub(crate) slug: String,
    /// The peer's display name, when it had one.
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// The peer's originator session (the host's system agent session).
    pub(crate) originator: String,
    pub(crate) memory_namespace: String,
    /// RFC 3339.
    pub(crate) purged_at: String,
    /// Erase steps that failed, on records left by older kernels: since
    /// #2696 a purge with residue never finalizes, so only earlier
    /// tombstones carry these; the retry path and `residual_errors` keep
    /// surfacing them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) errors: Vec<String>,
}

impl PurgeTombstone {
    /// Whether `ident` (a name or slug, as `peer/purge` takes it) names this
    /// purged peer.
    pub(crate) fn names(&self, ident: &str) -> bool {
        let ident = ident.trim();
        ident == self.slug
            || self
                .name
                .as_deref()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(ident))
    }
}

/// Pre-erase authorization and deletion scope for an unfinished purge (#2712
/// State 1). This is not a tombstone and never authorizes a retry by itself:
/// the caller must still prove the originator, host token and live route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PurgeIntent {
    pub(crate) version: u32,
    pub(crate) slug: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    pub(crate) originator: String,
    pub(crate) binding: PeerHostBinding,
    pub(crate) contexts: Vec<(String, PeerContextBinding)>,
    /// RFC 3339. Retries may resume only before this bounded deadline.
    pub(crate) expires_at: String,
}

impl PurgeIntent {
    pub(crate) fn new(
        slug: String,
        name: Option<String>,
        originator: String,
        binding: PeerHostBinding,
        contexts: Vec<(String, PeerContextBinding)>,
    ) -> Self {
        Self {
            version: 1,
            slug,
            name,
            originator,
            binding,
            contexts,
            expires_at: (chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339(),
        }
    }

    fn names(&self, ident: &str) -> bool {
        let ident = ident.trim();
        ident == self.slug
            || self
                .name
                .as_deref()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(ident))
    }

    fn expired(&self) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .map(|expires| expires <= chrono::Utc::now())
            .unwrap_or(true)
    }
}

const PURGE_INTENT_READ_CAP: usize = peer_io::PEER_FILE_READ_CAP_LARGE;
const PURGE_INTENT_SCAN_CAP: usize = 1024;

fn intents_dir(peers_root: &Path) -> Option<PathBuf> {
    purges_dir(peers_root).map(|dir| dir.join("intents"))
}

fn intent_path(peers_root: &Path, slug: &str) -> Option<PathBuf> {
    if !super::peer_slug_is_safe(slug) {
        return None;
    }
    Some(intents_dir(peers_root)?.join(format!("{slug}.json")))
}

/// Persist the complete authorized scope before the first erase. Rewriting an
/// intent is safe: the atomic durable write leaves either the old or new full
/// snapshot after a crash, never a partial JSON document.
pub(crate) fn write_purge_intent(peers_root: &Path, intent: &PurgeIntent) -> std::io::Result<()> {
    let Some(dir) = intents_dir(peers_root) else {
        return Err(std::io::Error::other("peers root has no parent"));
    };
    if intent.version != 1 {
        return Err(std::io::Error::other("unsupported purge intent version"));
    }
    let body = serde_json::to_string(intent).map_err(std::io::Error::other)?;
    if body.len() > PURGE_INTENT_READ_CAP {
        return Err(std::io::Error::other("purge intent exceeds its read cap"));
    }
    std::fs::create_dir_all(&dir)?;
    peer_io::write_peer_file_durable(&dir, &format!("{}.json", intent.slug), &body)
}

pub(crate) fn remove_purge_intent(peers_root: &Path, slug: &str) -> std::io::Result<()> {
    let Some(dir) = intents_dir(peers_root) else {
        return Ok(());
    };
    let Some(path) = intent_path(peers_root, slug) else {
        return Ok(());
    };
    remove_tree_within(&dir, &path).map(|_| ())
}

fn read_purge_intent(peers_root: &Path, slug: &str) -> Option<PurgeIntent> {
    let path = intent_path(peers_root, slug)?;
    let dir = path.parent()?;
    let leaf = path.file_name()?.to_str()?;
    let body = peer_io::read_peer_file(dir, leaf, PURGE_INTENT_READ_CAP)?;
    let intent: PurgeIntent = serde_json::from_str(&body).ok()?;
    (intent.version == 1 && intent.slug == slug).then_some(intent)
}

fn find_purge_intent(
    peers_root: &Path,
    ident: &str,
    originator: &str,
    host_token: Option<&str>,
) -> Option<PurgeIntent> {
    let authorized = |intent: &PurgeIntent| {
        intent.originator == originator && host_token_matches(&intent.binding, host_token)
    };
    if super::peer_slug_is_safe(ident)
        && let Some(intent) = read_purge_intent(peers_root, ident)
        && intent.names(ident)
        && authorized(&intent)
    {
        return Some(intent);
    }
    let entries = std::fs::read_dir(intents_dir(peers_root)?).ok()?;
    for entry in entries.flatten().take(PURGE_INTENT_SCAN_CAP) {
        let slug = entry.file_name().to_string_lossy().into_owned();
        let Some(slug) = slug.strip_suffix(".json") else {
            continue;
        };
        if let Some(intent) = read_purge_intent(peers_root, slug)
            && intent.names(ident)
            && authorized(&intent)
        {
            return Some(intent);
        }
    }
    None
}

/// Resolve a State-1 retry only when all of these independent facts hold:
/// originator + host token still match, the intent is unexpired, and the slug
/// is either the same staged generation or carries its unfinalized tombstone.
/// Unknown ids, forged scope and a later finalized generation all fail closed.
pub(crate) fn purge_intent_for_retry(
    peers_root: &Path,
    ident: &str,
    originator: &str,
    host_token: Option<&str>,
) -> Option<PurgeIntent> {
    let intent = find_purge_intent(peers_root, ident, originator, host_token)?;
    if intent.expired() {
        let _ = remove_purge_intent(peers_root, &intent.slug);
        return None;
    }
    if intent.originator != originator || !host_token_matches(&intent.binding, host_token) {
        return None;
    }
    let staged_binding = super::app_binding::read_peer_host_binding(peers_root, &intent.slug);
    if staged_binding
        .as_ref()
        .is_some_and(|binding| binding != &intent.binding)
    {
        // An unfinalized slug alone must never override a newer staged
        // generation. Its own binding is the generation fence.
        return None;
    }
    let same_staged_generation = staged_binding.is_some();
    let unfinished_residue = slug_tombstone_exists(peers_root, &intent.slug)
        && !slug_tombstone_is_finalized(peers_root, &intent.slug);
    (same_staged_generation || unfinished_residue).then_some(intent)
}

/// Complete only the metadata tail of a purge whose token tombstone proves the
/// destructive erase already finished. This covers a crash before slug
/// finalization or intent cleanup without ever repeating that erase. Expiry is
/// deliberately irrelevant here: no new deletion authority is granted.
pub(crate) fn finish_purge_metadata_after_token_tombstone(
    peers_root: &Path,
    tombstone: &PurgeTombstone,
    host_token: Option<&str>,
) -> std::io::Result<bool> {
    let Some(intent) = find_purge_intent(
        peers_root,
        &tombstone.slug,
        &tombstone.originator,
        host_token,
    ) else {
        return Ok(false);
    };
    if intent.originator != tombstone.originator || !host_token_matches(&intent.binding, host_token)
    {
        return Ok(false);
    }
    // A different staged binding is a later generation, not metadata for this
    // purge. Leave both its slug record and the old intent untouched.
    if super::app_binding::read_peer_host_binding(peers_root, &intent.slug)
        .is_some_and(|binding| binding != intent.binding)
    {
        return Ok(false);
    }
    finalize_slug_tombstone(peers_root, &intent.slug, &tombstone.purged_at)?;
    remove_purge_intent(peers_root, &intent.slug)?;
    Ok(true)
}

/// A real non-symlink peer residue directory, without requiring `brief.md`.
/// Missing paths are handled separately by the idempotent erase.
pub(crate) fn purge_residue_dir(peers_root: &Path, slug: &str) -> Option<PathBuf> {
    if !super::peer_slug_is_safe(slug) {
        return None;
    }
    let dir = peers_root.join(slug);
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    (meta.is_dir() && !meta.file_type().is_symlink()).then_some(dir)
}

fn purges_dir(peers_root: &Path) -> Option<PathBuf> {
    peers_root.parent().map(|data| data.join(PEER_PURGES_DIR))
}

fn digest_is_hex(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Record the slug tombstone of a purged peer: while no peer is staged under
/// `<slug>` again, a `#peer-<slug>` session is refused. `peer/purge` writes
/// it while the peer dir still exists, so the refusal hands over from the
/// close marker to this tombstone without a gap. The record starts
/// UNFINALIZED ([`slug_tombstone_is_finalized`]); [`finalize_slug_tombstone`]
/// completes it once nothing of the peer is left.
pub(crate) fn write_slug_tombstone(
    peers_root: &Path,
    slug: &str,
    purged_at: &str,
) -> std::io::Result<()> {
    write_slug_record(
        peers_root,
        slug,
        &serde_json::json!({ "purged_at": purged_at }).to_string(),
    )
}

/// Mark the slug tombstone of `slug` as complete: the purge erased the peer
/// dir for good, so a staged, unbound dir under this slug from now on can
/// only be a peer staged under the slug again — never debris of this purge
/// (#2712).
pub(crate) fn finalize_slug_tombstone(
    peers_root: &Path,
    slug: &str,
    purged_at: &str,
) -> std::io::Result<()> {
    write_slug_record(
        peers_root,
        slug,
        &serde_json::json!({ "purged_at": purged_at, "finalized": true }).to_string(),
    )
}

fn write_slug_record(peers_root: &Path, slug: &str, body: &str) -> std::io::Result<()> {
    let Some(dir) = purges_dir(peers_root) else {
        return Err(std::io::Error::other("peers root has no parent"));
    };
    if !super::peer_slug_is_safe(slug) {
        return Err(std::io::Error::other("unsafe peer slug"));
    }
    let slugs = dir.join("slugs");
    std::fs::create_dir_all(&slugs)?;
    peer_io::write_peer_file_durable(&slugs, slug, body)
}

/// Record the tombstone under the host token's digest: the record that
/// answers a retried `peer/purge` with the same credential `already_purged`.
/// Written only once nothing of the peer is left, so a purge with residue
/// never finalizes.
pub(crate) fn write_token_tombstone(
    peers_root: &Path,
    token_sha256: &str,
    tombstone: &PurgeTombstone,
) -> std::io::Result<()> {
    let Some(dir) = purges_dir(peers_root) else {
        return Err(std::io::Error::other("peers root has no parent"));
    };
    if digest_is_hex(token_sha256) {
        let tokens = dir.join("tokens");
        std::fs::create_dir_all(&tokens)?;
        let body = serde_json::to_string(tombstone).map_err(std::io::Error::other)?;
        peer_io::write_peer_file_durable(&tokens, &format!("{token_sha256}.json"), &body)?;
    }
    Ok(())
}

/// The tombstone a purge left for host token `token`, if any.
pub(crate) fn tombstone_for_token(peers_root: &Path, token: &str) -> Option<PurgeTombstone> {
    let digest = super::app_binding::token_digest(token);
    let dir = purges_dir(peers_root)?.join("tokens");
    let body = peer_io::read_peer_file(
        &dir,
        &format!("{digest}.json"),
        peer_io::PEER_FILE_READ_CAP_SMALL,
    )?;
    serde_json::from_str(&body).ok()
}

/// Whether slug `slug` was purged and no peer has been staged under it since.
pub(crate) fn slug_is_purged(peers_root: &Path, slug: &str) -> bool {
    if !super::peer_slug_is_safe(slug) || super::staged_peer_dir(peers_root, slug).is_some() {
        return false;
    }
    slug_tombstone_exists(peers_root, slug)
}

/// Whether a purge left its slug tombstone for `slug` — regardless of any
/// later re-staging under the slug ([`slug_is_purged`] additionally requires
/// the slug to be unstaged).
pub(crate) fn slug_tombstone_exists(peers_root: &Path, slug: &str) -> bool {
    if !super::peer_slug_is_safe(slug) {
        return false;
    }
    purges_dir(peers_root).is_some_and(|dir| {
        std::fs::symlink_metadata(dir.join("slugs").join(slug)).is_ok_and(|m| m.is_file())
    })
}

/// Whether the slug tombstone of `slug` records a COMPLETED purge: one whose
/// `peers/<slug>/` removal is done, so a staged, unbound dir under the slug
/// now reads as a peer staged again, not as this purge's debris (#2712).
/// An unfinalized record is the initial state `write_slug_tombstone` leaves;
/// only `finalize_slug_tombstone` completes it. A record written before the
/// unfinalized form existed (a bare timestamp, pre-#2712) reads as
/// finalized: it can only come from a purge that predates the debris check,
/// and reading it as debris would strand peers staged under the slug since.
pub(crate) fn slug_tombstone_is_finalized(peers_root: &Path, slug: &str) -> bool {
    if !slug_tombstone_exists(peers_root, slug) {
        return false;
    }
    let Some(dir) = purges_dir(peers_root) else {
        return false;
    };
    let Some(body) =
        peer_io::read_peer_file(&dir.join("slugs"), slug, peer_io::PEER_FILE_READ_CAP_SMALL)
    else {
        return false;
    };
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(record) => record["finalized"] == serde_json::json!(true),
        // A bare timestamp: an older kernel's completed purge.
        Err(_) => true,
    }
}

/// Append one row to the profile's purge audit log (never inside `peers/`,
/// so it survives every purge).
pub(crate) fn append_audit(peers_root: &Path, row: &Value) {
    let Some(dir) = peers_root.parent() else {
        return;
    };
    let size = std::fs::symlink_metadata(dir.join(PEER_PURGE_AUDIT_LEAF))
        .map(|m| m.len())
        .unwrap_or(0);
    if size >= super::host_tools::AUDIT_MAX_BYTES {
        tracing::warn!("peer purge audit log is full; not recording this purge");
        return;
    }
    if let Err(error) =
        peer_io::append_peer_line_durable(dir, PEER_PURGE_AUDIT_LEAF, &format!("{row}\n"))
    {
        tracing::warn!(%error, "failed to append the peer purge audit row");
    }
}

/// Remove `path` (a directory tree or a file). Missing is fine; a symlink is
/// removed itself, never followed. Returns whether something was removed.
pub(crate) fn remove_tree(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path).map(|()| true),
        Ok(_) => std::fs::remove_file(path).map(|()| true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// [`remove_tree`] of `path`, only when its real parent lies inside the real
/// `root`: a symlink anywhere above `path` that re-roots it outside `root` is
/// refused (an error), never followed. `path` itself, when it is a symlink,
/// is removed as a link. A missing `root` or parent is `Ok(false)`.
pub(crate) fn remove_tree_within(root: &Path, path: &Path) -> std::io::Result<bool> {
    let not_found = |err: &std::io::Error| err.kind() == std::io::ErrorKind::NotFound;
    let real_root = match dunce::canonicalize(root) {
        Ok(real) => real,
        Err(err) if not_found(&err) => return Ok(false),
        Err(err) => return Err(err),
    };
    let (Some(parent), Some(leaf)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::other(format!(
            "refusing to erase {}",
            path.display()
        )));
    };
    let real_parent = match dunce::canonicalize(parent) {
        Ok(real) => real,
        Err(err) if not_found(&err) => return Ok(false),
        Err(err) => return Err(err),
    };
    if !real_parent.starts_with(&real_root) {
        return Err(std::io::Error::other(format!(
            "refusing to erase {}: it resolves outside {}",
            path.display(),
            root.display()
        )));
    }
    let path = real_parent.join(leaf);
    // The turn's terminal and its bounded settle delay do not join detached
    // host-tool audit writers. Wait for any in-flight leaf mutation and keep
    // subsequent writes outside the entire directory enumeration/removal.
    // After removal those writers' no-follow directory open fails; no retry
    // hides a real removal error or allows a token tombstone before success.
    let lock = peer_io::mutation_lock(&path);
    let _mutation = lock.lock().unwrap_or_else(|p| p.into_inner());
    remove_tree(&path)
}

/// Whether `path` is its own canonical form (no symlink re-roots it).
pub(crate) fn is_real_path(path: &Path) -> bool {
    dunce::canonicalize(path).is_ok_and(|real| real == path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peers::app_binding::token_digest;

    fn fixture_intent() -> PurgeIntent {
        PurgeIntent::new(
            "mail".into(),
            Some("Mail".into()),
            "api:owner".into(),
            PeerHostBinding {
                version: 1,
                cwd: PathBuf::from("/tmp/mail-workspace"),
                memory_namespace: "app/mail/acct-1".into(),
                token_sha256: token_digest("owner-token"),
            },
            Vec::new(),
        )
    }

    #[test]
    fn state1_intent_requires_owner_token_and_the_same_unfinished_generation() {
        let dir = tempfile::tempdir().unwrap();
        let peers_root = dir.path().join("peers");
        std::fs::create_dir_all(peers_root.join("mail")).unwrap();
        let intent = fixture_intent();
        write_purge_intent(&peers_root, &intent).unwrap();
        write_slug_tombstone(&peers_root, "mail", "2026-11-20T00:00:00Z").unwrap();

        assert!(
            purge_intent_for_retry(&peers_root, "mail", "api:other", Some("owner-token")).is_none()
        );
        assert!(
            purge_intent_for_retry(&peers_root, "mail", "api:owner", Some("forged-token"))
                .is_none()
        );
        assert!(
            purge_intent_for_retry(&peers_root, "unknown", "api:owner", Some("owner-token"))
                .is_none()
        );
        // A process-local reader has no in-memory authority: this is the same
        // durable re-read a restarted owner performs.
        assert_eq!(
            purge_intent_for_retry(&peers_root, "Mail", "api:owner", Some("owner-token")),
            Some(intent)
        );

        finalize_slug_tombstone(&peers_root, "mail", "2026-11-20T00:01:00Z").unwrap();
        assert!(
            purge_intent_for_retry(&peers_root, "mail", "api:owner", Some("owner-token")).is_none(),
            "an old intent must never erase a later finalized generation"
        );
    }

    #[test]
    fn display_name_retry_skips_another_owners_intent() {
        let dir = tempfile::tempdir().unwrap();
        let peers_root = dir.path().join("peers");
        std::fs::create_dir_all(peers_root.join("mail-a")).unwrap();
        std::fs::create_dir_all(peers_root.join("mail-b")).unwrap();
        let mut shadow = fixture_intent();
        shadow.slug = "mail-a".into();
        shadow.originator = "api:other".into();
        shadow.binding.token_sha256 = token_digest("other-token");
        let mut authorized = fixture_intent();
        authorized.slug = "mail-b".into();
        write_purge_intent(&peers_root, &shadow).unwrap();
        write_purge_intent(&peers_root, &authorized).unwrap();
        write_slug_tombstone(&peers_root, "mail-a", "2026-11-20T00:00:00Z").unwrap();
        write_slug_tombstone(&peers_root, "mail-b", "2026-11-20T00:00:00Z").unwrap();

        assert_eq!(
            purge_intent_for_retry(&peers_root, "Mail", "api:owner", Some("owner-token")),
            Some(authorized)
        );
    }

    #[test]
    fn expired_state1_intent_is_removed_and_cannot_authorize_retry() {
        let dir = tempfile::tempdir().unwrap();
        let peers_root = dir.path().join("peers");
        std::fs::create_dir_all(peers_root.join("mail")).unwrap();
        let mut intent = fixture_intent();
        intent.expires_at = "2000-01-01T00:00:00Z".into();
        write_purge_intent(&peers_root, &intent).unwrap();
        write_slug_tombstone(&peers_root, "mail", "2026-11-20T00:00:00Z").unwrap();

        assert!(
            purge_intent_for_retry(&peers_root, "mail", "api:owner", Some("owner-token")).is_none()
        );
        assert!(read_purge_intent(&peers_root, "mail").is_none());
    }
}
