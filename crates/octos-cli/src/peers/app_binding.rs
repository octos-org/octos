//! Host-owned app peers and their bound request contexts (UPCR-2026-034).
//!
//! An OctoSense-style host launches apps that need the assistant. For each
//! authorized app it creates (or resumes) ONE peer owned by the host's system
//! agent session — the peer's recorded originator — through `peer/prepare`
//! with a host binding. The binding is durable and host-supplied:
//!
//! * `cwd` — the app's host-owned workspace. Every session of the peer runs
//!   there; a `session/open` naming another workspace is refused.
//! * `memory_namespace` — the app/account memory namespace
//!   ([`crate::runtime::memory_namespace`]). Capture, retrieval and automatic
//!   injection for the peer use that namespace, never the profile's own
//!   memory or another app's.
//!
//! An app can host smaller clients of its own (Rinx's mini apps). They do not
//! get peers: each gets a **request context** of the app peer — a separate
//! transcript in a separate workspace under the peer's, with a child memory
//! namespace and the peer's model lane. Its session key is derived by the
//! kernel (`<originator base>#peerctx-<slug>.<context_id>`), and a session
//! with that topic only runs while its binding is open: an unknown or closed
//! context is refused at bootstrap and at every turn start, so a stale client
//! cannot revive it.
//!
//! The binding files live beside the peer's other staging files and are read
//! through the same fd-anchored, symlink-refusing [`peer_io`] layer.

use std::path::{Path, PathBuf};

use octos_core::SessionKey;
use serde::{Deserialize, Serialize};

use super::{peer_io, peer_slug_is_safe, staged_peer_dir};

/// Peer-dir leaf holding the [`PeerHostBinding`].
pub(crate) const HOST_BINDING_LEAF: &str = "host_binding.json";

/// Topic prefix of a request-context session.
pub(crate) const PEER_CONTEXT_TOPIC_PREFIX: &str = "peerctx-";

/// Peer-dir leaf prefix of a context binding (`context-<id>.json`).
const CONTEXT_LEAF_PREFIX: &str = "context-";

/// Maximum length of a context id.
pub(crate) const CONTEXT_ID_MAX_BYTES: usize = 64;

/// A host-owned app peer's durable binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerHostBinding {
    pub(crate) version: u32,
    /// Canonical workspace every session of the peer runs in.
    pub(crate) cwd: PathBuf,
    /// Validated app/account memory namespace.
    pub(crate) memory_namespace: String,
    /// SHA-256 (hex) of the host token minted when the peer was created.
    /// Every later control call on the peer (resume, request contexts, model
    /// selection) must present that token: the self-reported originator
    /// session alone does not authorize them.
    #[serde(default)]
    pub(crate) token_sha256: String,
}

/// A fresh 256-bit host token (hex) and its SHA-256 (hex).
pub(crate) fn mint_host_token() -> Result<(String, String), String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| format!("no randomness for the host token: {err}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let digest = token_digest(&token);
    Ok((token, digest))
}

/// SHA-256 (hex) of a host token.
pub(crate) fn token_digest(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `token` is the one `binding` was created with (constant-time over
/// the digests).
pub(crate) fn host_token_matches(binding: &PeerHostBinding, token: Option<&str>) -> bool {
    let Some(token) = token else { return false };
    if binding.token_sha256.is_empty() {
        return false;
    }
    let presented = token_digest(token);
    let (a, b) = (presented.as_bytes(), binding.token_sha256.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether two memory namespaces share stores: equal, or one inside the
/// other (a peer's request contexts live under `<peer ns>/ctx-…`).
pub(crate) fn namespaces_overlap(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// Whether two canonical workspaces nest (either contains the other).
pub(crate) fn workspaces_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Every host-bound peer under `peers_root` (closed ones too: their stores
/// still hold data), with its slug.
pub(crate) fn host_bound_peers(peers_root: &Path) -> Vec<(String, PeerHostBinding)> {
    let Ok(entries) = std::fs::read_dir(peers_root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let slug = entry.file_name().to_string_lossy().into_owned();
            let dir = staged_peer_dir(peers_root, &slug)?;
            Some((slug, read_host_binding_in(&dir)?))
        })
        .collect()
}

/// The first existing host-bound peer (other than `except`) whose namespace
/// or workspace would share state with a new binding.
pub(crate) fn binding_conflict(
    peers_root: &Path,
    namespace: &str,
    cwd: &Path,
    except: Option<&str>,
) -> Option<String> {
    host_bound_peers(peers_root)
        .into_iter()
        .filter(|(slug, _)| Some(slug.as_str()) != except)
        .find_map(|(slug, other)| {
            if namespaces_overlap(namespace, &other.memory_namespace) {
                Some(format!(
                    "memory namespace '{namespace}' overlaps peer '{slug}''s '{}'",
                    other.memory_namespace
                ))
            } else if workspaces_overlap(cwd, &other.cwd) {
                Some(format!(
                    "workspace {} overlaps peer '{slug}''s {}",
                    cwd.display(),
                    other.cwd.display()
                ))
            } else {
                None
            }
        })
}

/// One request context's durable binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerContextBinding {
    pub(crate) version: u32,
    /// Canonical workspace, inside the peer's workspace.
    pub(crate) cwd: PathBuf,
    /// Child namespace of the peer's (`<peer ns>/ctx-<id>`).
    pub(crate) memory_namespace: String,
    /// Set by `peer/context/close`; a closed context never runs again.
    #[serde(default)]
    pub(crate) closed: bool,
    /// Set by `peer/context/open` with `share_history`: the person's lane
    /// of the peer, running in parallel with the peer's own session and
    /// shown its recent turns read-only (and the reverse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) share_history: Option<super::shared_history::ShareHistory>,
    /// Set by `peer/context/open` with `read_parent: true` (host only): the
    /// context's turns may READ the peer's folder, never another context's
    /// folder, and still write only their own. Fixed at creation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) read_parent: bool,
}

/// The folder every request context of a peer rooted at `peer_root` lives
/// in; a `read_parent` context's view of `peer_root` excludes it.
pub(crate) fn contexts_folder(peer_root: &Path) -> PathBuf {
    peer_root.join("contexts")
}

/// Validate a context id: `[a-z0-9][a-z0-9-]{0,63}`.
pub(crate) fn validate_context_id(raw: &str) -> Result<String, String> {
    let id = raw.trim();
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > CONTEXT_ID_MAX_BYTES {
        return Err(format!(
            "context_id must be 1..={CONTEXT_ID_MAX_BYTES} bytes"
        ));
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        || !bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("context_id may only contain [a-z0-9-] and must start with [a-z0-9]".into());
    }
    Ok(id.to_owned())
}

/// The memory namespace of context `context_id` of a peer in `peer_namespace`.
pub(crate) fn context_memory_namespace(peer_namespace: &str, context_id: &str) -> String {
    format!("{peer_namespace}/ctx-{context_id}")
}

/// The kernel-derived (not secret) session key of a request context: the originator's base
/// key with topic `peerctx-<slug>.<context_id>`. A slug never contains `.`,
/// and a context id never does either, so the split is unambiguous.
pub(crate) fn context_session_key(
    originator: &SessionKey,
    slug: &str,
    context_id: &str,
) -> SessionKey {
    SessionKey(format!(
        "{}#{PEER_CONTEXT_TOPIC_PREFIX}{slug}.{context_id}",
        originator.base_key()
    ))
}

/// `(slug, context_id)` of a `peerctx-` topic, or `None` for any other topic.
pub(crate) fn parse_context_topic(topic: &str) -> Option<(&str, &str)> {
    let rest = topic.strip_prefix(PEER_CONTEXT_TOPIC_PREFIX)?;
    rest.rsplit_once('.')
}

fn context_leaf(context_id: &str) -> String {
    format!("{CONTEXT_LEAF_PREFIX}{context_id}.json")
}

/// Every recorded request context of staged peer `slug`, open or closed:
/// `(context id, binding)`.
pub(crate) fn context_bindings(peers_root: &Path, slug: &str) -> Vec<(String, PeerContextBinding)> {
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_prefix(CONTEXT_LEAF_PREFIX)?
                .strip_suffix(".json")?
                .to_owned();
            let binding = read_context_binding(peers_root, slug, &id)?;
            Some((id, binding))
        })
        .collect()
}

/// Read the host binding of staged peer `slug`, if it has one.
pub(crate) fn read_peer_host_binding(peers_root: &Path, slug: &str) -> Option<PeerHostBinding> {
    let dir = staged_peer_dir(peers_root, slug)?;
    read_host_binding_in(&dir)
}

/// Whether staged peer `slug` is a host-owned app peer (ADR 0007): its tool
/// approvals are answered only by the person, in the app's own UI, never by
/// the owning system agent through `peer_respond`. Fail-closed — a peer dir
/// that carries the binding leaf counts even when the leaf is unreadable or
/// malformed, so a torn binding can never re-open the originator path.
/// Check that a bound workspace (a host-owned peer's folder, or one of its
/// request contexts' folders) is still exactly the directory the binding
/// recorded. Bindings store the CANONICAL path at `peer/prepare` /
/// `peer/context/open`, so a folder since replaced by a symlink (or with a
/// symlinked ancestor, or moved) no longer canonicalizes to itself — and a
/// session rooted through it would run somewhere else. A missing folder is
/// recreated first (then checked the same way, so a link planted in an
/// ancestor still refuses).
pub(crate) fn verify_bound_dir(cwd: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(cwd).is_err() {
        std::fs::create_dir_all(cwd).map_err(|err| {
            format!(
                "bound workspace {} is no longer usable: {err}",
                cwd.display()
            )
        })?;
    }
    match dunce::canonicalize(cwd) {
        Ok(real) if real == cwd && real.is_dir() => Ok(()),
        Ok(real) => Err(format!(
            "bound workspace {} no longer resolves to itself (it now leads to {}): it was \
             moved or replaced by a link",
            cwd.display(),
            real.display()
        )),
        Err(err) => Err(format!(
            "bound workspace {} is no longer usable: {err}",
            cwd.display()
        )),
    }
}

pub(crate) fn peer_is_host_owned(peers_root: &Path, slug: &str) -> bool {
    staged_peer_dir(peers_root, slug)
        .is_some_and(|dir| dir.join(HOST_BINDING_LEAF).symlink_metadata().is_ok())
}

pub(crate) fn read_host_binding_in(peer_dir: &Path) -> Option<PeerHostBinding> {
    let body = peer_io::read_peer_file(
        peer_dir,
        HOST_BINDING_LEAF,
        peer_io::PEER_FILE_READ_CAP_SMALL,
    )?;
    serde_json::from_str(&body).ok()
}

/// Durably write a host binding into a (reserved, not yet visible) peer dir.
pub(crate) fn write_host_binding_in(
    peer_dir: &Path,
    binding: &PeerHostBinding,
) -> std::io::Result<()> {
    let body = serde_json::to_string(binding).map_err(std::io::Error::other)?;
    peer_io::write_peer_file_durable(peer_dir, HOST_BINDING_LEAF, &body)
}

/// Read context `context_id` of staged peer `slug`.
pub(crate) fn read_context_binding(
    peers_root: &Path,
    slug: &str,
    context_id: &str,
) -> Option<PeerContextBinding> {
    validate_context_id(context_id).ok()?;
    let dir = staged_peer_dir(peers_root, slug)?;
    let body = peer_io::read_peer_file(
        &dir,
        &context_leaf(context_id),
        peer_io::PEER_FILE_READ_CAP_SMALL,
    )?;
    serde_json::from_str(&body).ok()
}

/// Durably write context `context_id` of staged peer `slug`.
pub(crate) fn write_context_binding(
    peers_root: &Path,
    slug: &str,
    context_id: &str,
    binding: &PeerContextBinding,
) -> Result<(), String> {
    validate_context_id(context_id)?;
    let dir = staged_peer_dir(peers_root, slug)
        .ok_or_else(|| format!("peer '{slug}' is not a staged peer"))?;
    let body = serde_json::to_string(binding).map_err(|err| err.to_string())?;
    peer_io::write_peer_file_durable(&dir, &context_leaf(context_id), &body)
        .map_err(|err| format!("failed to record request context: {err}"))
}

/// What the kernel must enforce for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionAppBinding {
    /// An ordinary session: today's behavior.
    Unbound,
    /// Runs only in `cwd`, on the `memory_namespace` stores.
    Bound {
        cwd: PathBuf,
        memory_namespace: String,
        /// A `read_parent` request context: the peer's folder, which its
        /// turns may read (minus [`contexts_folder`]) but not write.
        read_view: Option<PathBuf>,
    },
    /// Must not run (closed peer or context, or a context with no binding).
    Refused(String),
}

impl SessionAppBinding {
    pub(crate) fn is_bound_or_refused(&self) -> bool {
        !matches!(self, Self::Unbound)
    }
}

/// Resolve what `session` is bound to under `peers_root` (the profile's
/// `peers/` directory). Filesystem-only; fail-closed for context topics.
pub(crate) fn resolve_session_app_binding(
    peers_root: &Path,
    session: &SessionKey,
) -> SessionAppBinding {
    let Some(topic) = session.topic() else {
        return SessionAppBinding::Unbound;
    };
    if topic.starts_with(PEER_CONTEXT_TOPIC_PREFIX) {
        let Some((slug, context_id)) = parse_context_topic(topic) else {
            return SessionAppBinding::Refused(format!(
                "'{topic}' is not a valid request-context topic"
            ));
        };
        if !peer_slug_is_safe(slug) || validate_context_id(context_id).is_err() {
            return SessionAppBinding::Refused(format!(
                "'{topic}' is not a valid request-context topic"
            ));
        }
        if read_peer_host_binding(peers_root, slug).is_none() {
            return SessionAppBinding::Refused(format!(
                "request context '{context_id}' has no host-bound peer '{slug}'"
            ));
        }
        if super::peer_is_closed(peers_root, slug) {
            return SessionAppBinding::Refused(format!("peer '{slug}' is closed"));
        }
        return match read_context_binding(peers_root, slug, context_id) {
            Some(binding) if binding.closed => SessionAppBinding::Refused(format!(
                "request context '{context_id}' of peer '{slug}' is closed"
            )),
            Some(binding) => SessionAppBinding::Bound {
                cwd: binding.cwd,
                memory_namespace: binding.memory_namespace,
                read_view: binding.read_parent.then(|| {
                    let peer = read_peer_host_binding(peers_root, slug)
                        .map(|peer| peer.cwd)
                        .unwrap_or_default();
                    dunce::canonicalize(&peer).unwrap_or(peer)
                }),
            },
            None => SessionAppBinding::Refused(format!(
                "request context '{context_id}' of peer '{slug}' was never opened"
            )),
        };
    }
    if let Some(slug) = topic.strip_prefix("peer-") {
        if !peer_slug_is_safe(slug) {
            return SessionAppBinding::Unbound;
        }
        if let Some(binding) = read_peer_host_binding(peers_root, slug) {
            if super::peer_is_closed(peers_root, slug) {
                return SessionAppBinding::Refused(format!("peer '{slug}' is closed"));
            }
            return SessionAppBinding::Bound {
                cwd: binding.cwd,
                memory_namespace: binding.memory_namespace,
                read_view: None,
            };
        }
        // A purged host-owned app peer (`peer/purge`) never runs again, not
        // even as an ordinary profile session, until a new peer is staged
        // under its slug.
        if super::purge::slug_is_purged(peers_root, slug) {
            return SessionAppBinding::Refused(format!("peer '{slug}' was purged"));
        }
        // Fail closed on a torn or tampered peer dir that still carries a
        // host binding (brief missing, symlinked dir): never run it as an
        // ordinary profile session with the profile's memory.
        let dir = peers_root.join(slug);
        if std::fs::symlink_metadata(dir.join(HOST_BINDING_LEAF)).is_ok()
            || std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink())
        {
            return SessionAppBinding::Refused(format!(
                "peer '{slug}' has an incomplete host binding"
            ));
        }
    }
    SessionAppBinding::Unbound
}

/// Whether `candidate` is `root` or strictly inside it (both canonical).
pub(crate) fn path_is_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(peers_root: &Path, slug: &str, cwd: &Path, ns: &str) -> PathBuf {
        let dir = peers_root.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        write_host_binding_in(
            &dir,
            &PeerHostBinding {
                version: 1,
                cwd: cwd.to_path_buf(),
                memory_namespace: ns.to_owned(),
                token_sha256: token_digest("t"),
            },
        )
        .unwrap();
        std::fs::write(dir.join("brief.md"), "brief").unwrap();
        dir
    }

    fn key(topic: &str) -> SessionKey {
        SessionKey(format!("octos:api:host#{topic}"))
    }

    #[test]
    fn should_leave_ordinary_and_unbound_peer_sessions_unbound() {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        std::fs::create_dir_all(peers.join("plain")).unwrap();
        std::fs::write(peers.join("plain/brief.md"), "b").unwrap();
        assert_eq!(
            resolve_session_app_binding(&peers, &SessionKey("octos:api:x".into())),
            SessionAppBinding::Unbound
        );
        assert_eq!(
            resolve_session_app_binding(&peers, &key("peer-plain")),
            SessionAppBinding::Unbound
        );
    }

    #[test]
    fn should_bind_a_host_peer_session_to_its_workspace_and_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        stage(&peers, "rinx", Path::new("/ws/rinx"), "app/rinx/acct-1");
        assert_eq!(
            resolve_session_app_binding(&peers, &key("peer-rinx")),
            SessionAppBinding::Bound {
                cwd: PathBuf::from("/ws/rinx"),
                memory_namespace: "app/rinx/acct-1".into(),
                read_view: None,
            }
        );
    }

    #[test]
    fn should_bind_a_read_parent_context_with_the_peer_folder_as_its_read_view_when_it_was_opened_so()
     {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        stage(&peers, "rinx", Path::new("/ws/rinx"), "app/rinx/acct-1");
        let binding = |read_parent| PeerContextBinding {
            version: 1,
            cwd: PathBuf::from("/ws/rinx/contexts/app-a"),
            memory_namespace: context_memory_namespace("app/rinx/acct-1", "app-a"),
            closed: false,
            share_history: None,
            read_parent,
        };
        write_context_binding(&peers, "rinx", "app-a", &binding(true)).unwrap();
        write_context_binding(&peers, "rinx", "app-b", &binding(false)).unwrap();
        assert!(matches!(
            resolve_session_app_binding(&peers, &key("peerctx-rinx.app-a")),
            SessionAppBinding::Bound { read_view: Some(root), .. } if root == Path::new("/ws/rinx")
        ));
        assert!(matches!(
            resolve_session_app_binding(&peers, &key("peerctx-rinx.app-b")),
            SessionAppBinding::Bound {
                read_view: None,
                ..
            }
        ));
        // A binding written before the field existed reads as no view.
        let old: PeerContextBinding = serde_json::from_str(
            r#"{"version":1,"cwd":"/ws/rinx/contexts/x","memory_namespace":"n","closed":false}"#,
        )
        .unwrap();
        assert!(!old.read_parent);
    }

    #[test]
    fn should_refuse_a_closed_host_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        let dir = stage(&peers, "rinx", Path::new("/ws/rinx"), "app/rinx/acct-1");
        std::fs::write(dir.join("closed"), "host\n1\n").unwrap();
        assert!(matches!(
            resolve_session_app_binding(&peers, &key("peer-rinx")),
            SessionAppBinding::Refused(_)
        ));
    }

    #[test]
    fn should_refuse_unknown_and_closed_contexts_and_bind_open_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        stage(&peers, "rinx", Path::new("/ws/rinx"), "app/rinx/acct-1");
        assert!(matches!(
            resolve_session_app_binding(&peers, &key("peerctx-rinx.app-a")),
            SessionAppBinding::Refused(_)
        ));
        let binding = PeerContextBinding {
            version: 1,
            cwd: PathBuf::from("/ws/rinx/contexts/app-a"),
            memory_namespace: context_memory_namespace("app/rinx/acct-1", "app-a"),
            closed: false,
            share_history: None,
            read_parent: false,
        };
        write_context_binding(&peers, "rinx", "app-a", &binding).unwrap();
        assert_eq!(
            resolve_session_app_binding(&peers, &key("peerctx-rinx.app-a")),
            SessionAppBinding::Bound {
                cwd: PathBuf::from("/ws/rinx/contexts/app-a"),
                memory_namespace: "app/rinx/acct-1/ctx-app-a".into(),
                read_view: None,
            }
        );
        write_context_binding(
            &peers,
            "rinx",
            "app-a",
            &PeerContextBinding {
                closed: true,
                ..binding
            },
        )
        .unwrap();
        assert!(matches!(
            resolve_session_app_binding(&peers, &key("peerctx-rinx.app-a")),
            SessionAppBinding::Refused(_)
        ));
    }

    #[test]
    fn should_refuse_a_context_of_an_unbound_peer_or_a_malformed_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        std::fs::create_dir_all(peers.join("plain")).unwrap();
        std::fs::write(peers.join("plain/brief.md"), "b").unwrap();
        for topic in [
            "peerctx-plain.a",
            "peerctx-../x.a",
            "peerctx-plain",
            "peerctx-plain.A",
        ] {
            assert!(
                matches!(
                    resolve_session_app_binding(&peers, &key(topic)),
                    SessionAppBinding::Refused(_)
                ),
                "{topic}"
            );
        }
    }

    #[test]
    fn should_mint_context_keys_on_the_originator_base_and_parse_them_back() {
        let originator = SessionKey("octos:api:host#system".into());
        let ctx = context_session_key(&originator, "rinx", "app-a");
        assert_eq!(ctx.0, "octos:api:host#peerctx-rinx.app-a");
        assert_eq!(
            parse_context_topic(ctx.topic().unwrap()),
            Some(("rinx", "app-a"))
        );
        assert_eq!(parse_context_topic("peer-rinx"), None);
    }

    #[test]
    fn should_fail_closed_on_a_torn_host_peer_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        let dir = stage(&peers, "rinx", Path::new("/ws/rinx"), "app/rinx/acct-1");
        std::fs::remove_file(dir.join("brief.md")).unwrap();
        assert!(matches!(
            resolve_session_app_binding(&peers, &key("peer-rinx")),
            SessionAppBinding::Refused(_)
        ));
    }

    #[test]
    fn should_detect_namespace_and_workspace_overlaps() {
        assert!(namespaces_overlap("app/rinx/a", "app/rinx/a"));
        assert!(namespaces_overlap("app/rinx/a/ctx-x", "app/rinx/a"));
        assert!(namespaces_overlap("app/rinx", "app/rinx/a"));
        assert!(!namespaces_overlap("app/rinx/a", "app/rinx/ab"));
        let tmp = tempfile::tempdir().unwrap();
        let peers = tmp.path().join("peers");
        stage(&peers, "rinx", Path::new("/ws/rinx"), "app/rinx/acct-1");
        assert!(
            binding_conflict(
                &peers,
                "app/rinx/acct-1/ctx-a",
                Path::new("/ws/other"),
                None
            )
            .is_some()
        );
        assert!(
            binding_conflict(&peers, "app/notes/acct-1", Path::new("/ws/rinx/sub"), None).is_some()
        );
        assert!(binding_conflict(&peers, "app/notes/acct-1", Path::new("/ws"), None).is_some());
        assert!(
            binding_conflict(&peers, "app/notes/acct-1", Path::new("/ws/notes"), None).is_none()
        );
        assert!(
            binding_conflict(
                &peers,
                "app/rinx/acct-1",
                Path::new("/ws/rinx"),
                Some("rinx")
            )
            .is_none()
        );
    }

    #[test]
    fn should_match_only_the_minted_host_token() {
        let (token, digest) = mint_host_token().unwrap();
        assert_eq!(token.len(), 64);
        let binding = PeerHostBinding {
            version: 1,
            cwd: PathBuf::from("/ws"),
            memory_namespace: "app/x".into(),
            token_sha256: digest,
        };
        assert!(host_token_matches(&binding, Some(&token)));
        assert!(!host_token_matches(&binding, Some("guess")));
        assert!(!host_token_matches(&binding, None));
        let legacy = PeerHostBinding {
            token_sha256: String::new(),
            ..binding
        };
        assert!(!host_token_matches(&legacy, Some(&token)));
    }

    #[test]
    fn should_validate_context_ids() {
        assert!(validate_context_id("app-a-3").is_ok());
        for bad in ["", "-a", "A", "a.b", "a/b", "a_b", &"a".repeat(65)] {
            assert!(validate_context_id(bad).is_err(), "{bad:?}");
        }
    }
}
