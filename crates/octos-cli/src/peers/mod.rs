//! Peer-agent staging, addressing, and parked-prompt plumbing.
//!
//! Lifted VERBATIM out of the `api`-gated `api::ui_protocol` tree (Phase 3 of
//! bringing peer-agent goal into `octos chat`, mirroring the Phase 0 autonomy
//! extraction and the Phase 3 `crate::contracts` extraction). No logic changed:
//! only module placement and item visibility (`pub(crate)`), so the serve/WS
//! path keeps calling the exact same functions through a glob re-import.
//!
//! What stayed behind in `api::ui_protocol`: everything that touches `AppState`
//! or a `WsConnection` — `register_peer_wire_session`, `evict_peer_wire_session`,
//! `wake_master_on_peer_awaiting_input`, `write_peer_result_if_peer_session`,
//! the `peer/prepare` + `peer/gather` RPC handlers, the fleet-synthesis
//! scheduler, the model-lane provider builders (they need `SessionRuntime`),
//! and `build_peer_close_callback`. Those are genuinely serve-shaped.
//!
//! What moved: the pure filesystem/registry layer that a peer HOST needs
//! regardless of transport —
//!
//! * addressing: [`peer_slug_is_safe`], [`name_to_slug`], [`staged_peer_dir`],
//!   [`resolve_peer_name_to_slug`], [`peer_slug_and_profile`]
//! * fd-anchored peer-file I/O: [`peer_io`]
//! * staging: [`stage_peer`] and the `peer_handoff` callback builder
//! * the blackboard reader + `peer_list` renderer
//! * the parked-prompt projection and [`peer_respond_resolve`]
//! * the process-global [`peer_wire_registry`] mapping `"{profile}:peer:{slug}"`
//!   to the peer's live `SessionKey`
//!
//! The wire registry and `crate::contracts::contract_stores()` are BOTH
//! process-global `OnceLock`s. That is the whole reason a single-process
//! `octos chat --peers` can work: the peer's own approval/question requester
//! registers its oneshot in the same registry the master's `peer_respond`
//! resolves it from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::Utc;
use octos_core::SessionKey;
use octos_core::ui_protocol::{
    ApprovalDecidedEvent, ApprovalDecision, ApprovalId, PeerStagedEvent, RpcError,
    UserQuestionRespondParams,
};
use tracing::warn;

use crate::autonomy::agent_orchestrator::default_agent_orchestrator;
use crate::contracts::UiProtocolContractStores;

pub(crate) mod host;

/// Cap a string at `cap` bytes on a char boundary; returns (text, truncated).
///
/// Not peer-specific, but every remaining caller is (`peer_pending_prompt_summary`,
/// `read_peer_blackboard`, `compose_peer_list_text`), so it travelled with them
/// rather than growing a third home. `api::ui_protocol` picks it back up
/// through its `crate::peers::*` glob.
pub(crate) fn capped_utf8(text: String, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut text = text;
    text.truncate(cut);
    (text, true)
}

/// #436 — serve-side registry mapping `"{profile}:peer:{slug}"` → the peer
/// session's wire `SessionKey`, populated on `session/open` for `peer-<slug>`
/// sessions. `peer_send_input` reads this to resolve a slug to the
/// continuation-queue key it enqueues an injected turn under: the serve
/// process has no gateway `ActorRegistry` to populate the inbox registry, so
/// the tool delivers via the master continuation queue instead. A stale entry
/// (a peer that has since closed) is harmless — the enqueued continuation is
/// durable and drains when the peer next reconnects — so entries are not
/// evicted on disconnect; a bounded cap prevents unbounded growth on a
/// long-lived serve that opens many distinct peers.
#[derive(Default)]
pub(crate) struct PeerWireRegistry {
    pub(crate) by_key: std::sync::Mutex<HashMap<String, SessionKey>>,
}

/// Soft cap on the peer-wire registry. A new key past the cap is dropped (that
/// peer is not injectable until re-opened); existing keys still refresh.
pub(crate) const PEER_WIRE_REGISTRY_MAX: usize = 8192;

impl PeerWireRegistry {
    /// Register (or UPDATE) the slug→wire mapping. Latest open wins (#436 P1
    /// #1): a reconnect under a fresh client-chosen wire key overwrites the
    /// prior mapping so resolution always targets the CURRENT session.
    pub(crate) fn register(&self, key: String, session_id: SessionKey) {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.len() >= PEER_WIRE_REGISTRY_MAX && !map.contains_key(&key) {
            tracing::warn!(
                key = %key,
                cap = PEER_WIRE_REGISTRY_MAX,
                "peer wire registry at capacity; skipping new peer registration"
            );
            return;
        }
        map.insert(key, session_id);
    }

    pub(crate) fn resolve(&self, key: &str) -> Option<SessionKey> {
        self.by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .cloned()
    }

    /// Evict the mapping for `key` ONLY when it still points at `session_id`
    /// (#436 P1 #5). The conditional guard is race-safe: if the peer already
    /// reopened under a newer wire key (register overwrote the value), a late
    /// close of the OLD session must not clobber the fresh mapping. Returns
    /// whether an entry was removed. Also frees a slot against the cap.
    pub(crate) fn evict_if_value(&self, key: &str, session_id: &SessionKey) -> bool {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.get(key) == Some(session_id) {
            map.remove(key);
            true
        } else {
            false
        }
    }
}

pub(crate) fn peer_wire_registry() -> &'static PeerWireRegistry {
    static PEER_WIRE_REGISTRY: OnceLock<PeerWireRegistry> = OnceLock::new();
    PEER_WIRE_REGISTRY.get_or_init(PeerWireRegistry::default)
}

/// #1868 Phase 1 — maps a staged peer to the `TaskSupervisor` task enrolled on
/// its MASTER's session, so the peer's retirement can mark that task terminal.
///
/// Peers were never registered with the supervisor at all (`goal_tool.rs` had
/// zero supervisor references), which is why the master's task count never
/// showed a peer, why peers had no cancel token, and why the in-flight liveness
/// rule added in #2014 had to read `state.agents` directly instead of asking
/// the supervisor. Enrolling them puts all three kinds of supervised work —
/// sub-agents, background tasks, peers — behind one source of truth.
///
/// Keyed by the same `"{profile}:peer:{slug}"` string as [`peer_wire_registry`]
/// so both registries are addressed identically; the VALUE is the supervisor's
/// task id. Process-global for the same reason the wire registry is: a peer is
/// staged on one path and retired on another.
#[derive(Default)]
pub(crate) struct PeerTaskRegistry {
    pub(crate) by_key: std::sync::Mutex<HashMap<String, String>>,
}

impl PeerTaskRegistry {
    /// Bind `key` to a supervisor task id. A re-stage under the same key
    /// overwrites, mirroring [`PeerWireRegistry::register`]'s latest-open-wins.
    pub(crate) fn bind(&self, key: String, task_id: String) {
        let mut map = self
            .by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if map.len() >= PEER_WIRE_REGISTRY_MAX && !map.contains_key(&key) {
            tracing::warn!(
                key = %key,
                cap = PEER_WIRE_REGISTRY_MAX,
                "peer task registry at capacity; peer will not be supervised"
            );
            return;
        }
        map.insert(key, task_id);
    }

    /// Take the task id for `key`, removing the binding. Retirement is
    /// exactly-once: a second close finds nothing and must NOT re-mark a task
    /// terminal (the supervisor's terminal guard would reject it anyway, but a
    /// second `mark_completed` would also race a task id later reused).
    pub(crate) fn take(&self, key: &str) -> Option<String> {
        self.by_key
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(key)
    }
}

pub(crate) fn peer_task_registry() -> &'static PeerTaskRegistry {
    static PEER_TASK_REGISTRY: OnceLock<PeerTaskRegistry> = OnceLock::new();
    PEER_TASK_REGISTRY.get_or_init(PeerTaskRegistry::default)
}

/// Registry key for a peer session: `"{profile}:peer:{slug}"` (mirrors the
/// gateway inbox registry's key construction in `session_actor`).
pub(crate) fn peer_wire_key(profile_id: &str, slug: &str) -> String {
    format!("{profile_id}:peer:{slug}")
}

/// Split a `peer-<slug>` session key into `(profile_id, slug)`, or `None` for
/// a non-peer or unprofiled session.
pub(crate) fn peer_slug_and_profile(session_id: &SessionKey) -> Option<(&str, &str)> {
    // NOT a peer session. The overwhelmingly common case, and the only one where
    // `None` is uninteresting — every caller correctly skips peer bookkeeping.
    let slug = session_id
        .topic()
        .and_then(|topic| topic.strip_prefix("peer-"))?;

    // Past this point the topic SAYS `peer-…`, so something intended a peer
    // session. Each rejection below still returns `None` (callers must treat it
    // as a non-peer session — that is the #436 fence), but it is now LOUD.
    //
    // Why: all ~10 callers do `let Some(..) = .. else { return }`, which is
    // right for "not a peer" and silently wrong for "malformed peer key". A peer
    // whose key is rejected here keeps running and looks healthy while its wire
    // registration, result recording, blackboard writes, awaiting-input wake and
    // fleet synthesis ALL no-op. That failure is invisible at every layer — the
    // peer produces work nobody records — so the only place it can be reported
    // is here, where the reason is still known.

    // #436 security — the topic-derived slug feeds `Path::join` (closed marker,
    // peers dir) and the wire-key registry. Reject an unsafe one (e.g. a
    // `peer-/tmp/x` or `peer-../x` topic) HERE so EVERY caller treats it as a
    // NON-peer session rather than a path that escapes `peers/`.
    if slug.is_empty() || !peer_slug_is_safe(slug) {
        warn!(
            session = %session_id,
            "peer session key has an unusable slug; peer bookkeeping (results, \
             blackboard, wake, synthesis) is DISABLED for this session"
        );
        return None;
    }

    // A peer key with no profile component. Nothing downstream can address the
    // peer without one — `peers_root` is per-profile and the wire key is
    // `{profile}:peer:{slug}` — so this silently disables the same bookkeeping.
    let Some(profile_id) = session_id.profile_id() else {
        warn!(
            session = %session_id,
            slug,
            "peer session key has NO profile component; peer bookkeeping \
             (results, blackboard, wake, synthesis) is DISABLED for this \
             session. Peer keys must be `{{profile}}:{{channel}}:{{chat}}#peer-{{slug}}`"
        );
        return None;
    };
    Some((profile_id, slug))
}

/// Upper bound (bytes) on a peer slug — a slug is a short handle, not a
/// payload. Aligns with [`name_to_slug`]'s cap so a derived slug always
/// satisfies [`peer_slug_is_safe`].
pub(crate) const PEER_SLUG_MAX_BYTES: usize = 64;

/// Reject a peer slug that could escape `peers/` or mis-key the wire registry:
/// empty, over-long, a `.`/`..` component, any path separator / NUL, a drive /
/// alternate-data-stream `:`, a control char (`< 0x20`), or a trailing `.`/
/// space (which some filesystems strip → a DIFFERENT real path). A slug is a
/// single path component (a dir name under `peers/`) — real slugs from
/// `reserve_peer_dir` / [`name_to_slug`] are lowercase `[a-z0-9-]` / `%`-escaped
/// `[A-Za-z0-9_%-]`, so any of the above is illegitimate. Called at the TOP of
/// the `peer_close` / `peer_send_input` callbacks (after resolving a name to a
/// slug) before any path join or wire-key op. Mirrors
/// `octos_core::session_scope::is_safe_session_id`, hardened for cross-platform.
pub(crate) fn peer_slug_is_safe(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > PEER_SLUG_MAX_BYTES {
        return false;
    }
    if slug == "." || slug == ".." {
        return false;
    }
    // A trailing dot or space aliases to a different real path on Windows.
    if slug.ends_with('.') || slug.ends_with(' ') {
        return false;
    }
    // Path separators, NUL/control chars (incl. 0x7f DEL), and the drive/ADS colon.
    !slug
        .bytes()
        .any(|b| matches!(b, b'/' | b'\\' | b':') || b < 0x20 || b == 0x7f)
}

/// Derive a filesystem/URL-safe ASCII slug from a peer's display NAME:
/// lowercase, each run of non-`[a-z0-9]` collapses to a single `-`, trim
/// leading/trailing `-`, cap at [`PEER_SLUG_MAX_BYTES`] bytes. A name with NO
/// ASCII alphanumerics (a CJK / emoji display name — `爱迪生`, `🔬`) has no
/// readable slug, so it falls back to a stable FNV-1a hash of the trimmed,
/// lowercased name: `peer-<16 hex>`. The DISPLAY name (unicode, stored in
/// `peers/<slug>/name`) is what users see and address; the slug is only the
/// directory handle and resolution is by the name file, so the same name always
/// yields the same slug — a duplicate is rejected, never suffixed. Returns
/// `None` ONLY for a blank / whitespace-only name (which has no peer at all).
pub(crate) fn name_to_slug(name: &str) -> Option<String> {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let mut slug = slug.trim_matches('-').to_owned();
    // All retained bytes are ASCII, so a byte cut is a char boundary; re-trim a
    // dash the cut may have exposed.
    if slug.len() > PEER_SLUG_MAX_BYTES {
        slug.truncate(PEER_SLUG_MAX_BYTES);
        slug = slug.trim_end_matches('-').to_owned();
    }
    if slug.is_empty() {
        // No ASCII handle (pure CJK / emoji / punctuation): hash the normalized
        // unicode name into a stable ASCII slug so the peer is still addressable
        // (by its display name, via the `name` file).
        let key = name.trim().to_lowercase();
        if key.is_empty() {
            return None; // blank / whitespace-only — not a name at all
        }
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        slug = format!("peer-{hash:016x}");
    }
    peer_slug_is_safe(&slug).then_some(slug)
}

/// The REAL, staged peer directory for `slug` under `peers_root`, or `None`
/// when it is not safe to touch. EVERY peer-dir access — reads AND the
/// close-marker write — routes through this so a hostile or stray
/// `peers/<slug>` SYMLINK can never redirect I/O outside `peers_root`. Returns
/// `Some(dir)` ONLY when: [`peer_slug_is_safe`], `peers_root.join(slug)` is a
/// REAL directory that is NOT a symlink (`symlink_metadata` inspects the LINK,
/// not its target), and it carries the `brief.md` staging contract.
pub(crate) fn staged_peer_dir(peers_root: &Path, slug: &str) -> Option<PathBuf> {
    if !peer_slug_is_safe(slug) {
        return None;
    }
    let dir = peers_root.join(slug);
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    if !peer_io::peer_regular_file_exists(&dir, "brief.md") {
        return None;
    }
    Some(dir)
}

/// fd-anchored, symlink/FIFO/DoS-safe I/O for the per-session peer files under
/// `peers/<slug>/` (octos#1824). [`staged_peer_dir`] validates the `<slug>`
/// directory by PATH; a subsequent path-based `std::fs` read/write then races a
/// parent swap — an attacker who can write under `peers/` replaces `<slug>` (or
/// a leaf) with a symlink between the check and the I/O, and the plain read/
/// `atomic_write` follows the swap. It also accepts a FIFO/device leaf, so a
/// hostile `model`/`brief.md` FIFO parks a turn on an unbounded blocking read.
///
/// Every op here re-opens the peer DIR fd `O_NOFOLLOW|O_DIRECTORY` (so a
/// symlinked `<slug>` is refused, not followed) and resolves the LEAF relative
/// to that pinned inode with `openat`/`renameat`/`unlinkat` — no path is ever
/// re-walked after the anchor, closing the parent-swap race. Reads open
/// `O_NOFOLLOW|O_NONBLOCK`, `fstat` the opened handle, require a regular file
/// (`S_ISREG` — a FIFO/device/dir/symlink is refused before any `read`), and
/// bound the read to a cap. This mirrors the anchored pattern already used by
/// `api::memory_panel` for the memory-panel reads.
pub(crate) mod peer_io {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Read cap for the large peer files (`brief.md`, `result.md`). Over-cap
    /// content reads as absent (`None`) rather than a truncated prefix, matching
    /// `memory_panel`'s over-cap posture. 1 MiB is far above any legitimate
    /// brief/result (the write side caps `result.md` at 256 KiB and the gather
    /// display re-caps to tens of KiB).
    pub(crate) const PEER_FILE_READ_CAP_LARGE: usize = 1024 * 1024;

    /// Read cap for the small control files (`model`, `originator`, `name`,
    /// `turns.txt`, `closed`). These hold a lane key, a session id, a display
    /// name, or a compact line index — all KB-scale.
    pub(crate) const PEER_FILE_READ_CAP_SMALL: usize = 64 * 1024;

    /// Raw directory-scan budget for [`peer_dir_count_prefixed`]. A legitimate
    /// peer has a handful of `result-<n>.md` files; this only bounds a hostile
    /// flood so a directory stuffed with entries can't stall a turn.
    pub(crate) const PEER_DIR_SCAN_CAP: usize = 100_000;

    /// Process-unique suffix source for temp filenames, so concurrent
    /// atomic writes to the same leaf never collide on the `O_EXCL` create.
    static TMP_UNIQ: AtomicU64 = AtomicU64::new(0);

    /// A `.<leaf>.tmp-<pid>-<uniq>` sibling name for the atomic temp file. The
    /// leading `.` keeps it out of `result-*` globs (e.g.
    /// `count_peer_result_versions`), which a bare `<leaf>.tmp` would otherwise
    /// pollute for `result-<n>.md`.
    fn tmp_name(leaf: &str) -> String {
        let uniq = TMP_UNIQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        format!(".{leaf}.tmp-{pid}-{uniq}")
    }

    /// Read a peer leaf file, anchored on the peer dir fd. Returns the content
    /// when the leaf is a REGULAR file no larger than `cap` bytes; `None` for a
    /// symlinked dir/leaf, a FIFO/device/dir leaf, an over-cap file, invalid
    /// UTF-8, or any I/O error.
    pub(crate) fn read_peer_file(peer_dir: &Path, leaf: &str, cap: usize) -> Option<String> {
        imp::read_peer_file(peer_dir, leaf, cap)
    }

    /// Atomically replace a peer leaf file (temp + fsync + rename), every step
    /// relative to the peer dir fd. A symlinked dir/leaf is refused (never
    /// followed); on any write/rename error the temp file is best-effort
    /// removed.
    pub(crate) fn write_peer_file_atomic(
        peer_dir: &Path,
        leaf: &str,
        content: &str,
    ) -> std::io::Result<()> {
        imp::write_peer_file_atomic(peer_dir, leaf, content)
    }

    /// Append `line` to a peer leaf file (the `turns.txt` index), anchored on
    /// the peer dir fd. Creates the file if absent; refuses a symlinked dir/
    /// leaf and a non-regular (FIFO/device) leaf. Opened `O_NONBLOCK` so a
    /// planted FIFO fails fast instead of parking the writer on the missing
    /// reader.
    pub(crate) fn append_peer_line(peer_dir: &Path, leaf: &str, line: &str) -> std::io::Result<()> {
        imp::append_peer_line(peer_dir, leaf, line)
    }

    /// `true` when the peer leaf exists as a REGULAR file, resolved under the
    /// peer dir fd with a no-follow stat (`S_ISREG` required). A symlinked/FIFO/
    /// dir/device leaf — or a symlinked peer dir — reads as absent. Replaces the
    /// path-following `dir.join(leaf).is_file()` status probes so an existence
    /// gate (e.g. the `closed` close-marker) can't be redirected by a parent- or
    /// leaf-swap (#1824).
    pub(crate) fn peer_regular_file_exists(peer_dir: &Path, leaf: &str) -> bool {
        imp::peer_file_mtime(peer_dir, leaf).is_some()
    }

    /// The mtime of a peer leaf REGULAR file, resolved under the peer dir fd
    /// with the same no-follow `S_ISREG` gate as [`peer_regular_file_exists`].
    /// `None` for a symlinked/FIFO/dir leaf, a symlinked peer dir, or a stat
    /// error. Used where the mtime AFFECTS behavior (the ready-note freshness
    /// gate), so a swapped leaf can neither park nor mislead it.
    pub(crate) fn peer_file_mtime(peer_dir: &Path, leaf: &str) -> Option<std::time::SystemTime> {
        imp::peer_file_mtime(peer_dir, leaf)
    }

    /// Count REGULAR-file entries whose name starts with `prefix`, enumerating
    /// the peer dir through its own `O_NOFOLLOW|O_DIRECTORY` fd (`fdopendir`) —
    /// never a path `read_dir`, so swapping `<slug>` to a symlink after
    /// [`staged_peer_dir`] cannot redirect the scan into an attacker's tree
    /// (#1824). A symlinked/dir/FIFO entry matching the prefix is NOT counted
    /// (so it can't inflate a version number), and the raw scan stops after
    /// `cap` entries so a hostile flood can't stall the turn. Any failure
    /// (symlinked peer dir, open/read error) → 0.
    pub(crate) fn peer_dir_count_prefixed(peer_dir: &Path, prefix: &str, cap: usize) -> usize {
        imp::peer_dir_count_prefixed(peer_dir, prefix, cap)
    }

    /// `true` when `peer_dir` exists as a REAL (non-symlink) directory, opened
    /// `O_NOFOLLOW|O_DIRECTORY` — a symlinked `<slug>` is refused. Anchored
    /// replacement for a path-following `is_dir()` gate on a per-slug peer dir.
    pub(crate) fn peer_dir_exists(peer_dir: &Path) -> bool {
        imp::peer_dir_exists(peer_dir)
    }

    #[cfg(unix)]
    mod imp {
        use std::ffi::CStr;
        use std::io::{Read, Write};
        use std::os::fd::OwnedFd;
        use std::os::unix::fs::OpenOptionsExt;
        use std::path::Path;

        use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fsync, openat, renameat, unlinkat};

        /// Open the peer dir as an `O_NOFOLLOW|O_DIRECTORY` fd: a symlinked
        /// `<slug>` is refused here (belt-and-braces over `staged_peer_dir`'s
        /// path check, and the anchor that makes the leaf ops race-free).
        fn open_peer_dir(peer_dir: &Path) -> std::io::Result<OwnedFd> {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(
                    libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
                .open(peer_dir)
                .map(OwnedFd::from)
        }

        pub(crate) fn read_peer_file(peer_dir: &Path, leaf: &str, cap: usize) -> Option<String> {
            let dir = open_peer_dir(peer_dir).ok()?;
            // NONBLOCK: a FIFO opened plain `O_RDONLY` blocks until a writer
            // appears — a planted FIFO must never park the caller (#1824).
            let fd = openat(
                &dir,
                leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?;
            let file = std::fs::File::from(fd);
            // fstat the OPENED handle (no stat-by-path race). Regular files
            // only: a FIFO/device/dir/socket is refused before any `read`.
            let meta = file.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let cap = cap as u64;
            // Bound the ACTUAL read, not just the fstat snapshot: `take(cap+1)`
            // detects an over-cap file (a full cap+1 bytes) and rejects it
            // rather than serving a truncated prefix. Invalid UTF-8 → `None`
            // (matching the prior `read_to_string`).
            let mut content = String::new();
            let read = Read::take(file, cap + 1)
                .read_to_string(&mut content)
                .ok()?;
            if read as u64 > cap {
                return None;
            }
            Some(content)
        }

        pub(crate) fn write_peer_file_atomic(
            peer_dir: &Path,
            leaf: &str,
            content: &str,
        ) -> std::io::Result<()> {
            let dir = open_peer_dir(peer_dir)?;
            let tmp = super::tmp_name(leaf);
            // O_EXCL|O_NOFOLLOW: create a fresh regular temp beside the leaf,
            // never following/clobbering a pre-existing name.
            let fd = openat(
                &dir,
                tmp.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            let mut file = std::fs::File::from(fd);
            let written = file
                .write_all(content.as_bytes())
                .and_then(|()| file.sync_all());
            if let Err(err) = written {
                let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
                return Err(err);
            }
            // renameat relative to the SAME dir fd — atomic in-dir replace that
            // a parent swap cannot redirect.
            if let Err(err) = renameat(&dir, tmp.as_str(), &dir, leaf) {
                let _ = unlinkat(&dir, tmp.as_str(), AtFlags::empty());
                return Err(err.into());
            }
            // Best-effort dir fsync so the rename entry itself is crash-durable
            // (the tmp file's data was already fsync'd above).
            let _ = fsync(&dir);
            Ok(())
        }

        pub(crate) fn append_peer_line(
            peer_dir: &Path,
            leaf: &str,
            line: &str,
        ) -> std::io::Result<()> {
            let dir = open_peer_dir(peer_dir)?;
            // O_APPEND create; O_NOFOLLOW refuses a symlinked leaf; O_NONBLOCK
            // makes a planted FIFO fail fast (ENXIO, no reader) instead of
            // parking the writer.
            let fd = openat(
                &dir,
                leaf,
                OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::APPEND
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC
                    | OFlags::NONBLOCK,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            let mut file = std::fs::File::from(fd);
            // Regular files only: refuse a device/FIFO leaf that slipped past
            // the open (e.g. a FIFO with a live reader).
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "peer leaf is not a regular file",
                ));
            }
            file.write_all(line.as_bytes())
        }

        pub(crate) fn peer_file_mtime(
            peer_dir: &Path,
            leaf: &str,
        ) -> Option<std::time::SystemTime> {
            let dir = open_peer_dir(peer_dir).ok()?;
            // Anchored no-follow open + fstat (NONBLOCK so a planted FIFO can't
            // park the probe); regular files only, then read the mtime off the
            // opened handle. No content is read.
            let fd = openat(
                &dir,
                leaf,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()?;
            let meta = std::fs::File::from(fd).metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            meta.modified().ok()
        }

        pub(crate) fn peer_dir_exists(peer_dir: &Path) -> bool {
            // O_NOFOLLOW|O_DIRECTORY succeeds only for a REAL non-symlink dir.
            open_peer_dir(peer_dir).is_ok()
        }

        pub(crate) fn peer_dir_count_prefixed(peer_dir: &Path, prefix: &str, cap: usize) -> usize {
            let Ok(dirfd) = open_peer_dir(peer_dir) else {
                return 0;
            };
            // fdopendir on the anchored fd — entries come from THIS inode, never
            // a re-walked path, so a swapped `<slug>` can't redirect the scan.
            let Ok(mut dir) = Dir::read_from(&dirfd) else {
                return 0;
            };
            let prefix = prefix.as_bytes();
            let mut count = 0usize;
            let mut scanned = 0usize;
            // Fetch AT MOST `cap` entries: the budget is checked BEFORE each
            // read, so the cap+1'th entry is never even fetched.
            while scanned < cap {
                let Some(next) = dir.next() else {
                    break;
                };
                // A mid-scan read error is a FAILURE, not a short scan: return 0
                // (the documented contract), never a partial count.
                let Ok(entry) = next else {
                    return 0;
                };
                scanned += 1;
                if !entry.file_name().to_bytes().starts_with(prefix) {
                    continue;
                }
                // Regular files only — a symlinked/dir/FIFO `result-*` entry
                // must not inflate the version count.
                match entry.file_type() {
                    FileType::RegularFile => count += 1,
                    // d_type unavailable on this FS → classify with a no-follow
                    // stat before counting.
                    FileType::Unknown if entry_is_regular(&dirfd, entry.file_name()) => count += 1,
                    _ => {}
                }
            }
            count
        }

        /// No-follow `S_ISREG` check of `name` relative to the peer dir fd, for
        /// the rare filesystem that returns `DT_UNKNOWN` from `readdir`.
        fn entry_is_regular(dirfd: &OwnedFd, name: &CStr) -> bool {
            openat(
                dirfd,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .ok()
            .and_then(|fd| std::fs::File::from(fd).metadata().ok())
            .is_some_and(|meta| meta.is_file())
        }
    }

    #[cfg(not(unix))]
    mod imp {
        use std::io::{Read, Write};
        use std::path::Path;

        /// Refuse a symlinked peer dir; require a real directory. Non-unix
        /// serve is dev-only (matching the `symlink_metadata` fallback
        /// `memory_panel` uses for the same reads), so a path-anchored check
        /// with a documented multi-syscall TOCTOU window is acceptable here.
        fn peer_dir_ok(peer_dir: &Path) -> bool {
            std::fs::symlink_metadata(peer_dir)
                .map(|m| !m.file_type().is_symlink() && m.is_dir())
                .unwrap_or(false)
        }

        pub(crate) fn read_peer_file(peer_dir: &Path, leaf: &str, cap: usize) -> Option<String> {
            if !peer_dir_ok(peer_dir) {
                return None;
            }
            let path = peer_dir.join(leaf);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return None;
            }
            let cap = cap as u64;
            let file = std::fs::File::open(&path).ok()?;
            let mut content = String::new();
            let read = Read::take(file, cap + 1)
                .read_to_string(&mut content)
                .ok()?;
            if read as u64 > cap {
                return None;
            }
            Some(content)
        }

        pub(crate) fn write_peer_file_atomic(
            peer_dir: &Path,
            leaf: &str,
            content: &str,
        ) -> std::io::Result<()> {
            if !peer_dir_ok(peer_dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "peer dir is not a real directory",
                ));
            }
            let path = peer_dir.join(leaf);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "refusing to follow a symlinked peer leaf",
                ));
            }
            let tmp = peer_dir.join(super::tmp_name(leaf));
            {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                if let Err(err) = file
                    .write_all(content.as_bytes())
                    .and_then(|()| file.sync_all())
                {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(err);
                }
            }
            if let Err(err) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(err);
            }
            Ok(())
        }

        pub(crate) fn append_peer_line(
            peer_dir: &Path,
            leaf: &str,
            line: &str,
        ) -> std::io::Result<()> {
            if !peer_dir_ok(peer_dir) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "peer dir is not a real directory",
                ));
            }
            let path = peer_dir.join(leaf);
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if meta.file_type().is_symlink() || !meta.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "peer leaf is not a regular file",
                    ));
                }
            }
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            file.write_all(line.as_bytes())
        }

        pub(crate) fn peer_file_mtime(
            peer_dir: &Path,
            leaf: &str,
        ) -> Option<std::time::SystemTime> {
            if !peer_dir_ok(peer_dir) {
                return None;
            }
            let meta = std::fs::symlink_metadata(peer_dir.join(leaf)).ok()?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return None;
            }
            meta.modified().ok()
        }

        pub(crate) fn peer_dir_exists(peer_dir: &Path) -> bool {
            peer_dir_ok(peer_dir)
        }

        pub(crate) fn peer_dir_count_prefixed(peer_dir: &Path, prefix: &str, cap: usize) -> usize {
            if !peer_dir_ok(peer_dir) {
                return 0;
            }
            let Ok(mut read_dir) = std::fs::read_dir(peer_dir) else {
                return 0;
            };
            let mut count = 0usize;
            let mut scanned = 0usize;
            // Fetch AT MOST `cap` entries (budget checked before each read); a
            // mid-scan read error returns 0, never a partial count.
            while scanned < cap {
                let Some(next) = read_dir.next() else {
                    break;
                };
                let Ok(entry) = next else {
                    return 0;
                };
                scanned += 1;
                if !entry.file_name().to_string_lossy().starts_with(prefix) {
                    continue;
                }
                if std::fs::symlink_metadata(entry.path())
                    .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file())
                {
                    count += 1;
                }
            }
            count
        }
    }
}

#[cfg(all(test, unix))]
mod peer_io_tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::peer_io::{
        PEER_DIR_SCAN_CAP, PEER_FILE_READ_CAP_LARGE, PEER_FILE_READ_CAP_SMALL, append_peer_line,
        peer_dir_count_prefixed, peer_dir_exists, peer_file_mtime, peer_regular_file_exists,
        read_peer_file, write_peer_file_atomic,
    };

    // octos#1824: a symlinked leaf must NOT be followed — the anchored openat
    // is O_NOFOLLOW, so a `model`/`brief.md` symlink pointing at a real file
    // reads as absent instead of leaking the target's content.
    #[test]
    fn symlinked_leaf_is_refused_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        std::fs::write(peer.join("secret"), "SENSITIVE").unwrap();
        for leaf in ["model", "brief.md"] {
            std::os::unix::fs::symlink("secret", peer.join(leaf)).unwrap();
            assert_eq!(
                read_peer_file(peer, leaf, PEER_FILE_READ_CAP_LARGE),
                None,
                "a symlinked `{leaf}` leaf must not be followed"
            );
        }
    }

    // octos#1824: a FIFO leaf must be rejected PROMPTLY (NONBLOCK open +
    // regular-file reject) — never block the caller on the missing writer.
    #[test]
    fn fifo_leaf_reads_none_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path().to_path_buf();
        let status = std::process::Command::new("mkfifo")
            .arg(peer.join("model"))
            .status()
            .expect("mkfifo");
        assert!(status.success());

        // Run the read on a worker thread and require it to return quickly: a
        // blocking open (no NONBLOCK) would never send, tripping the timeout.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let out = read_peer_file(&peer, "model", PEER_FILE_READ_CAP_SMALL);
            let _ = tx.send(out);
        });
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("read must not block on a FIFO leaf");
        assert_eq!(result, None, "FIFO content must not be served");
    }

    // A real regular file round-trips through the atomic writer + anchored
    // reader, landing at the intended leaf.
    #[test]
    fn regular_file_round_trips_to_the_named_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        write_peer_file_atomic(peer, "result.md", "hello peer").unwrap();
        write_peer_file_atomic(peer, "name", "Kestrel").unwrap();
        assert_eq!(
            read_peer_file(peer, "result.md", PEER_FILE_READ_CAP_LARGE).as_deref(),
            Some("hello peer")
        );
        assert_eq!(
            read_peer_file(peer, "name", PEER_FILE_READ_CAP_SMALL).as_deref(),
            Some("Kestrel"),
            "each leaf must read back its OWN content"
        );
        // The atomic temp must not linger under the peer dir.
        let leftover = std::fs::read_dir(peer)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().contains(".tmp-"));
        assert!(
            !leftover,
            "atomic temp file must be renamed away, not left behind"
        );
    }

    // An over-cap file reads as absent, and the read is BOUNDED: a tiny cap on
    // a much larger file must not slurp the whole thing.
    #[test]
    fn oversized_file_is_refused_and_read_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        let cap = 16usize;
        // Exactly at cap → served.
        write_peer_file_atomic(peer, "at_cap", &"a".repeat(cap)).unwrap();
        assert_eq!(
            read_peer_file(peer, "at_cap", cap).map(|s| s.len()),
            Some(cap),
            "a file exactly at the cap must be served whole"
        );
        // One over cap → refused (not a truncated prefix).
        write_peer_file_atomic(peer, "over_cap", &"a".repeat(cap + 1)).unwrap();
        assert_eq!(
            read_peer_file(peer, "over_cap", cap),
            None,
            "an over-cap file must read as absent"
        );
        // Far over cap → still bounded (proves take(cap+1), not a full read).
        std::fs::write(peer.join("huge"), "b".repeat(cap * 4096)).unwrap();
        assert_eq!(read_peer_file(peer, "huge", cap), None);
    }

    // A peer_dir that is itself a symlink fails BOTH ops safely — the
    // O_NOFOLLOW|O_DIRECTORY anchor refuses to open a symlinked `<slug>`.
    #[test]
    fn symlinked_peer_dir_fails_both_ops_safely() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("name"), "present").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            read_peer_file(&link, "name", PEER_FILE_READ_CAP_SMALL),
            None,
            "reads through a symlinked peer dir must be refused"
        );
        assert!(
            write_peer_file_atomic(&link, "name", "evil").is_err(),
            "writes through a symlinked peer dir must be refused"
        );
        // The refused write must not have touched the real file.
        assert_eq!(
            std::fs::read_to_string(real.join("name")).unwrap(),
            "present",
            "a refused write must not reach the symlink target"
        );
    }

    // The `turns.txt` append is anchored too: it round-trips for a real file
    // and refuses a symlinked leaf.
    #[test]
    fn append_round_trips_and_refuses_symlinked_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        append_peer_line(peer, "turns.txt", "1 completed 100\n").unwrap();
        append_peer_line(peer, "turns.txt", "2 completed 200\n").unwrap();
        assert_eq!(
            read_peer_file(peer, "turns.txt", PEER_FILE_READ_CAP_SMALL).as_deref(),
            Some("1 completed 100\n2 completed 200\n")
        );

        let other = dir.path().join("elsewhere");
        std::fs::write(&other, "untouched").unwrap();
        std::os::unix::fs::symlink(&other, peer.join("evil.txt")).unwrap();
        assert!(
            append_peer_line(peer, "evil.txt", "x\n").is_err(),
            "append must refuse a symlinked leaf"
        );
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "untouched");
    }

    // octos#1824 status probes: the anchored existence/mtime gate counts only
    // REGULAR files — a symlinked or FIFO leaf (or absent) is not "present".
    #[test]
    fn peer_regular_file_exists_gates_on_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        // Regular file → present, with a readable mtime.
        std::fs::write(peer.join("closed"), "x").unwrap();
        assert!(peer_regular_file_exists(peer, "closed"));
        assert!(peer_file_mtime(peer, "closed").is_some());
        // Absent → not present.
        assert!(!peer_regular_file_exists(peer, "result.md"));
        assert!(peer_file_mtime(peer, "result.md").is_none());
        // Symlinked leaf → not present (not followed), even to a real file.
        std::fs::write(peer.join("target"), "y").unwrap();
        std::os::unix::fs::symlink("target", peer.join("result.md")).unwrap();
        assert!(!peer_regular_file_exists(peer, "result.md"));
        assert!(peer_file_mtime(peer, "result.md").is_none());
        // FIFO leaf → not present, PROMPTLY (NONBLOCK open + regular-file
        // reject); a blocking probe would trip the timeout.
        let status = std::process::Command::new("mkfifo")
            .arg(peer.join("fifo"))
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let peer_buf = peer.to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(peer_regular_file_exists(&peer_buf, "fifo"));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("existence probe must not block on a FIFO leaf");
        assert!(!got, "a FIFO leaf must not count as a regular file");
    }

    // octos#1824 `.notified` freshness stamp: round-trips through the anchored
    // helpers, and neither read nor write follows a symlinked leaf.
    #[test]
    fn notified_stamp_round_trips_and_refuses_symlinked_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        write_peer_file_atomic(peer, ".notified", "1700000000").unwrap();
        assert_eq!(
            read_peer_file(peer, ".notified", PEER_FILE_READ_CAP_SMALL).as_deref(),
            Some("1700000000")
        );

        // Plant a symlinked `.notified` pointing OUTSIDE the peer dir.
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "original").unwrap();
        std::fs::remove_file(peer.join(".notified")).unwrap();
        std::os::unix::fs::symlink(&outside, peer.join(".notified")).unwrap();

        // Read refuses to follow it.
        assert_eq!(
            read_peer_file(peer, ".notified", PEER_FILE_READ_CAP_SMALL),
            None,
            "a symlinked `.notified` must not be followed on read"
        );
        // Write does not follow it either: renameat REPLACES the symlink with a
        // fresh regular file, so the target outside the peer dir is untouched.
        write_peer_file_atomic(peer, ".notified", "9999").unwrap();
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "original",
            "the stamp write must not reach the symlink target"
        );
        assert_eq!(
            read_peer_file(peer, ".notified", PEER_FILE_READ_CAP_SMALL).as_deref(),
            Some("9999"),
            "after replacing the symlink the stamp reads back its new value"
        );
        assert!(
            !peer
                .join(".notified")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must have been replaced by a regular file"
        );
    }

    // octos#1824: `result-*` version enumeration is fd-anchored and counts only
    // REGULAR prefixed files — a symlinked or non-prefixed entry can't inflate
    // the count, and a symlinked peer dir yields 0 (no follow).
    #[test]
    fn peer_dir_count_prefixed_counts_only_regular_prefixed_files() {
        let dir = tempfile::tempdir().unwrap();
        let peer = dir.path();
        std::fs::write(peer.join("result-1.md"), "a").unwrap();
        std::fs::write(peer.join("result-2.md"), "b").unwrap();
        std::fs::write(peer.join("result.md"), "latest").unwrap(); // no `result-` prefix
        std::fs::write(peer.join("brief.md"), "brief").unwrap(); // other prefix
        std::fs::create_dir(peer.join("result-dir")).unwrap(); // dir, not a file
        // A symlinked `result-*` entry must NOT be counted (not followed).
        std::fs::write(dir.path().join("outside"), "x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside"), peer.join("result-9.md")).unwrap();

        assert_eq!(
            peer_dir_count_prefixed(peer, "result-", PEER_DIR_SCAN_CAP),
            2,
            "only the two REGULAR `result-<n>.md` files may count"
        );

        // A real peer dir exists; a symlinked one is refused and enumerates to 0.
        assert!(peer_dir_exists(peer));
        let link = dir.path().join("peerlink");
        std::os::unix::fs::symlink(peer, &link).unwrap();
        assert!(
            !peer_dir_exists(&link),
            "a symlinked peer dir must be refused"
        );
        assert_eq!(
            peer_dir_count_prefixed(&link, "result-", PEER_DIR_SCAN_CAP),
            0,
            "a symlinked peer dir must not be followed for enumeration"
        );
        // Any open/read failure returns 0 (the documented contract), never a
        // partial count — here an absent dir; a mid-scan readdir error takes the
        // same `return 0` path.
        assert_eq!(
            peer_dir_count_prefixed(&dir.path().join("absent"), "result-", PEER_DIR_SCAN_CAP),
            0,
            "a missing peer dir must count 0"
        );
        // The budget is checked BEFORE each read: cap 0 fetches nothing (→ 0),
        // and cap 1 fetches at most one entry so the count never exceeds the cap.
        assert_eq!(
            peer_dir_count_prefixed(peer, "result-", 0),
            0,
            "cap 0 must fetch no entries"
        );
        assert!(
            peer_dir_count_prefixed(peer, "result-", 1) <= 1,
            "the raw scan must stop at exactly the cap"
        );
    }
}

/// `true` when `peers/<slug>/closed` exists — the durable marker written by
/// `peer_close` retiring a peer. Shared by the continuation-drain freshness
/// gates and the reconnect-retarget skip so a closed peer is never a live
/// injection target, even for an injection queued just before the close.
/// Routes through [`staged_peer_dir`] so a symlinked / unsafe slug is never
/// followed.
pub(crate) fn peer_is_closed(peers_root: &Path, slug: &str) -> bool {
    staged_peer_dir(peers_root, slug)
        .is_some_and(|dir| peer_io::peer_regular_file_exists(&dir, "closed"))
}

/// Resolve a peer IDENTIFIER (its display NAME or its slug) to the slug. A name
/// match is case-insensitive against each REAL staged `peers/<slug>/name`; if
/// none matches, an `ident` that is itself a safe, staged dir is returned as-is
/// (slug addressing, and legacy peers that have no `name` file). Both branches
/// route through [`staged_peer_dir`], so a SYMLINKED entry is skipped and never
/// resolved. Returns `None` when nothing matches. Names are the primary
/// address, so callbacks resolve through this BEFORE any auth / path / wire op.
pub(crate) fn resolve_peer_name_to_slug(peers_root: &Path, ident: &str) -> Option<String> {
    let target = ident.trim();
    if target.is_empty() {
        return None;
    }
    let lowered = target.to_lowercase();
    if let Ok(read_dir) = std::fs::read_dir(peers_root) {
        for entry in read_dir.flatten() {
            let slug = entry.file_name().to_string_lossy().into_owned();
            // Only a REAL, staged (non-symlink) peer dir may claim a name.
            let Some(dir) = staged_peer_dir(peers_root, &slug) else {
                continue;
            };
            if let Some(name) =
                peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
            {
                if name.trim().to_lowercase() == lowered {
                    return Some(slug);
                }
            }
        }
    }
    // Fall back to slug addressing: an ident that is a safe, staged, non-symlink
    // dir name.
    staged_peer_dir(peers_root, target).map(|_| target.to_owned())
}

/// #436 P1 #6 — authorize a `peer_send_input` injection: ONLY the peer's
/// recorded ORIGINATOR — the session that staged it via `peer_handoff` /
/// `peer_prepare`, written to `peers/<slug>/originator` — may inject into it.
/// Previously any non-peer session in the same profile could inject into any
/// open staged peer.
///
/// Authorizing by the STABLE originator identity (not the ephemeral wire key)
/// composes with the reconnect wire-resolution fix: the caller is the master
/// session, and the target peer's wire may change across reconnects without
/// affecting this check. Fail-closed: a missing originator record (e.g. a
/// profile-scoped `peer_prepare` that recorded no originator) is unauthorized.
///
/// # Security model (#436 #5 — single-user-per-profile / Option C)
///
/// Authorization is SESSION-scoped WITHIN a single user's own trust domain. In
/// serve, the authenticated identity IS the profile: `authenticated_profile_id`
/// returns the `AuthIdentity::User { id }` id *as* the profile, so a profile is
/// exactly one user's own trust domain.
///
/// - **Cross-USER injection is blocked by profile scoping.** A connection can
///   only open / run turns in sessions under its own profile
///   (`validate_authenticated_session_scope`), so a different user cannot reach
///   another user's peer at all — the strong isolation boundary.
/// - **An LLM cannot cross-session-inject.** `caller_session` is the
///   SERVER-CAPTURED session of the running turn (never a client-supplied
///   argument), and the LLM cannot call `session/open` — so an LLM in a
///   non-owner session sees its own session key ≠ the recorded originator and
///   is rejected. This check blocks LLM-level cross-session injection, the
///   meaningful in-band threat.
/// - **The residual same-user, cross-session "spoof" is by design.** A CLIENT
///   that deliberately `session/open`s the owner session and drives a turn
///   there satisfies the originator check — but that is the USER exercising
///   their own authority within their own profile, not a cross-trust breach.
///   Making it non-spoofable would require a capability / session-access-control
///   model (a per-peer owner token held outside any session-replayable channel;
///   Option A), which is OUT OF SCOPE for the single-user serve model. If
///   serve ever gains sub-user identities or multi-user profiles, revisit here.
pub(crate) fn peer_send_input_authorized(
    peers_root: &Path,
    slug: &str,
    caller_session: &str,
) -> Result<(), String> {
    // Route through `staged_peer_dir`: the originator read must target a REAL,
    // non-symlink staged peer under `peers/`, never a symlinked/unsafe slug that
    // could redirect the read outside the root. Defense-in-depth — callers
    // already resolve the slug, but auth is the boundary and stays self-safe.
    let Some(dir) = staged_peer_dir(peers_root, slug) else {
        return Err(format!(
            "peer session '{slug}' is not a staged peer; cannot authorize input"
        ));
    };
    match peer_io::read_peer_file(&dir, "originator", peer_io::PEER_FILE_READ_CAP_SMALL) {
        Some(recorded) if recorded.trim() == caller_session => Ok(()),
        Some(_) => Err(format!(
            "not the owner of peer session '{slug}' — only the session that \
             staged this peer may send it input"
        )),
        None => Err(format!(
            "peer session '{slug}' has no recorded owner; cannot authorize input"
        )),
    }
}

/// Upper bound on a peer brief. Briefs are task contracts, not payloads — a
/// cap keeps a runaway client from turning the profile dir into blob storage.
pub(crate) const PEER_BRIEF_MAX_BYTES: usize = 64 * 1024;

/// Derive a unique directory slug for a peer under `peers/`: sanitized from
/// the title (else the brief's leading words), numeric `-N` suffix on
/// collision. Returns the reserved (created) directory alongside the slug so
/// two concurrent prepares can never race into the same dir —
/// `create_dir` is the atomic claim.
pub(crate) fn reserve_peer_dir(
    peers_root: &Path,
    seed: &str,
) -> Result<(String, PathBuf), RpcError> {
    // Dashed-alnum normalization (NOT bare `safe_filename`, which
    // percent-encodes spaces — `%20` in a slug leaks into branch names and
    // paths). Unicode alphanumerics survive so CJK titles keep their words;
    // `safe_filename` stays as the filesystem-safety belt on the result.
    let mut dashed = String::new();
    for ch in seed.chars() {
        if ch.is_alphanumeric() {
            dashed.extend(ch.to_lowercase());
        } else if !dashed.ends_with('-') && !dashed.is_empty() {
            dashed.push('-');
        }
    }
    let base = octos_core::safe_filename(dashed.trim_matches('-'));
    let mut base = base.chars().take(40).collect::<String>();
    if base.is_empty() {
        base = "peer".to_owned();
    }
    std::fs::create_dir_all(peers_root)
        .map_err(|err| RpcError::internal_error(format!("failed to create peers dir: {err}")))?;
    for attempt in 0..100u32 {
        let slug = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{}", attempt + 1)
        };
        let dir = peers_root.join(&slug);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((slug, dir)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(RpcError::internal_error(format!(
                    "failed to reserve peer dir: {err}"
                )));
            }
        }
    }
    Err(RpcError::invalid_params(
        "too many peers with this title — pick a distinct title",
    ))
}

/// True when a staged peer already claims this NAME (case-insensitive, reading
/// each `peers/<slug>/name`) or already occupies the derived SLUG. Guards the
/// NAMED staging path: names are the primary address, so they must be unique.
pub(crate) fn existing_peer_name_conflict(peers_root: &Path, name: &str, slug: &str) -> bool {
    let target = name.trim().to_lowercase();
    let Ok(read_dir) = std::fs::read_dir(peers_root) else {
        return false;
    };
    for entry in read_dir.flatten() {
        let entry_slug = entry.file_name().to_string_lossy().into_owned();
        // Any entry occupying the derived slug path — even a symlink or an
        // unstaged dir — blocks the reservation (`create_dir` fails on it too).
        if entry_slug == slug {
            return true;
        }
        // A NAME collision only counts against a REAL staged peer: route the
        // `name` read through `staged_peer_dir` so a symlinked entry is never
        // followed and never falsely registers as a conflict.
        if let Some(dir) = staged_peer_dir(peers_root, &entry_slug) {
            if let Some(existing) =
                peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
            {
                if existing.trim().to_lowercase() == target {
                    return true;
                }
            }
        }
    }
    false
}

/// Reserve the EXACT slug derived from a peer NAME (no numeric suffix): a named
/// peer must be addressable by its name, so a collision is an ERROR, not an
/// auto-rename. Rejects a name with no usable slug, a duplicate name
/// (case-insensitive), or a slug already taken. `create_dir` is the atomic
/// claim that also closes the check→reserve race.
pub(crate) fn reserve_named_peer_dir(
    peers_root: &Path,
    name: &str,
) -> Result<(String, PathBuf), RpcError> {
    let Some(slug) = name_to_slug(name) else {
        return Err(RpcError::invalid_params(
            "peer name cannot be blank".to_string(),
        ));
    };
    std::fs::create_dir_all(peers_root)
        .map_err(|err| RpcError::internal_error(format!("failed to create peers dir: {err}")))?;
    if existing_peer_name_conflict(peers_root, name, &slug) {
        return Err(RpcError::invalid_params(format!(
            "a peer named '{name}' already exists"
        )));
    }
    let dir = peers_root.join(&slug);
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok((slug, dir)),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(
            RpcError::invalid_params(format!("a peer named '{name}' already exists")),
        ),
        Err(err) => Err(RpcError::internal_error(format!(
            "failed to reserve peer dir: {err}"
        ))),
    }
}

/// One staged peer, as produced by [`stage_peer`]: the durable facts a
/// `peer/prepare` result entry (and the `peer/staged` notification) carry.
#[derive(Debug)]
pub(crate) struct StagedPeer {
    pub(crate) slug: String,
    /// Session topic the client opens (`peer-<slug>`).
    pub(crate) topic: String,
    /// `peers/<slug>/brief.md` under the profile data dir.
    pub(crate) brief_path: PathBuf,
    /// Worktree checkout when fenced, else the workspace root.
    pub(crate) cwd: PathBuf,
    /// `peer/<slug>` when a worktree fence was created.
    pub(crate) worktree_branch: Option<String>,
}

/// #1801 v3: single-peer staging core shared by the `peer/prepare` fleet
/// loop and the `peer_handoff` tool callback. Reserves the slug dir
/// (`reserve_peer_dir` — `create_dir` is the atomic claim), optionally
/// fences a worktree on branch `peer/<slug>`, and writes the brief
/// atomically. Any failure AFTER the reserve rolls back this member's own
/// dir plus (best-effort) its git-side leavings so the slug is never
/// burned. Synchronous by design (git via `std::process`): the RPC fleet
/// loop keeps it off the reactor via `spawn_blocking`, while the tool
/// callback — a sync `Fn` — runs it directly on the tool's worker.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_peer(
    peers_root: &Path,
    workspace_root: &Path,
    seed: &str,
    name: Option<&str>,
    // codex #6 — the ORIGINATING (master) session that owns this peer, recorded
    // atomically BEFORE `brief.md`. `staged_peer_dir` gates peer visibility on
    // `brief.md`, so writing the owner first guarantees any fleet-ownership scan
    // that can see this peer can also read its owner — no window where a member
    // is visible-but-ownerless (which would let a sibling's completion fire
    // synthesis while this peer is silently omitted). `None` for a
    // profile-scoped `peer/prepare` with no originating session.
    originator: Option<&str>,
    brief: &str,
    worktree: bool,
    // Goal context for this peer (peer-agent-based goal feature): when the
    // master hands off under an active goal, it passes `goal_id` (required for
    // goal-scoped work) and an optional `task_id` (sub-task within the goal).
    // Persisted atomically to `peers/<slug>/goal` as two LF-separated lines
    // (`goal_id\ntask_id-or-empty`) so the peer session can rehydrate them on
    // boot and `goal_*` tools can scope their reads/writes to the goal. A peer
    // without a goal file behaves exactly as today.
    goal_id: Option<&str>,
    task_id: Option<&str>,
) -> Result<StagedPeer, RpcError> {
    // A NAMED peer reserves its EXACT (name-derived) slug and rejects
    // collisions — a name is the primary address, so it must be unique and
    // stable. An unnamed (legacy `peer/prepare`) peer keeps the auto-suffix
    // seed path.
    let (slug, peer_dir) = match name {
        Some(name) => reserve_named_peer_dir(peers_root, name)?,
        None => reserve_peer_dir(peers_root, seed)?,
    };
    // The fence: a worktree on branch `peer/<slug>` under the peer dir.
    let cwd = if worktree {
        let worktree_path = peer_dir.join("wt");
        let branch = format!("peer/{slug}");
        // Best-effort re-validation immediately before handing the path to git:
        // narrows (to near-zero) the window in which `<slug>` could be swapped
        // to a symlink after reservation, which would redirect git's worktree
        // creation outside `peers_root`. NOTE: this does NOT fully close it —
        // git re-resolves `peers/<slug>/wt` by path itself, so a residual
        // path-resolution TOCTOU is inherent to handing a path to a subprocess
        // (tracked as a follow-up; #1824). Accurate scope: all peer-FILE
        // read/write/enumeration I/O is fd-anchored; only this git-worktree
        // creation path is best-effort re-validated.
        if !peer_io::peer_dir_exists(&peer_dir) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::invalid_params(format!(
                "peer '{slug}' staging directory is no longer a real directory"
            )));
        }
        // A CLONE, not `git worktree add`. A worktree's `.git` is a FILE
        // pointing at `<repo>/.git/worktrees/<name>`, which lives OUTSIDE the
        // peer's sandboxed workspace — so every git command inside a worktree
        // peer failed with `fatal: not a git repository ... exit 128`, and the
        // model "recovered" by running `git init`, destroying the fence. The
        // branch then stayed at the seed commit and no deliverable ever landed.
        // A clone puts the whole `.git` INSIDE `peers/<slug>/wt`, so git works
        // with no sandbox widening, and one peer cannot reach another's refs.
        //
        // `--no-hardlinks` because isolation is the entire point here: the
        // default local-clone optimisation shares object-file inodes with the
        // parent, so a peer writing into its own `.git` could corrupt the
        // source repo's objects. Costs a real object copy per peer; revisit
        // with evidence if staging latency becomes the problem.
        let run_git = |args: &[&std::ffi::OsStr]| -> Result<(), String> {
            match std::process::Command::new("git").args(args).output() {
                Err(err) => Err(format!("failed to run git: {err}")),
                Ok(out) if out.status.success() => Ok(()),
                Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            }
        };
        let as_os = |s: &str| std::ffi::OsString::from(s);
        let clone_args: Vec<std::ffi::OsString> = vec![
            as_os("clone"),
            as_os("--quiet"),
            as_os("--no-hardlinks"),
            workspace_root.as_os_str().to_os_string(),
            worktree_path.as_os_str().to_os_string(),
        ];
        let clone_ref: Vec<&std::ffi::OsStr> = clone_args.iter().map(AsRef::as_ref).collect();
        if let Err(detail) = run_git(&clone_ref) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::invalid_params(format!(
                "git clone failed (is {} a git repo?): {}",
                workspace_root.display(),
                detail
            )));
        }
        // The fence branch now lives in the peer's OWN clone.
        let branch_args: Vec<std::ffi::OsString> = vec![
            as_os("-C"),
            worktree_path.as_os_str().to_os_string(),
            as_os("checkout"),
            as_os("-q"),
            as_os("-b"),
            as_os(&branch),
        ];
        let branch_ref: Vec<&std::ffi::OsStr> = branch_args.iter().map(AsRef::as_ref).collect();
        if let Err(detail) = run_git(&branch_ref) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::invalid_params(format!(
                "git checkout -b {branch} failed in the peer clone: {detail}"
            )));
        }
        // A clone does NOT inherit the source's LOCAL config, so a peer would
        // have no commit identity and every `git commit` would fail. Carry the
        // parent's over when it has one; otherwise git falls back to global.
        for key in ["user.name", "user.email"] {
            let read = std::process::Command::new("git")
                .arg("-C")
                .arg(workspace_root)
                .args(["config", "--get", key])
                .output();
            let Ok(out) = read else { continue };
            if !out.status.success() {
                continue;
            }
            let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if value.is_empty() {
                continue;
            }
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&worktree_path)
                .args(["config", key, &value])
                .output();
        }
        worktree_path
    } else {
        workspace_root.to_path_buf()
    };

    // codex #6 — record the owner BEFORE brief.md (the visibility gate), atomic
    // + surfaced. A failed owner-write rolls the whole staging back instead of
    // leaving a silently unowned member that a sibling's completion could omit
    // from the fleet and fire synthesis prematurely.
    if let Some(originator) = originator {
        if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "originator", originator) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to record peer originator: {err}"
            )));
        }
    }

    // Peer-agent-based goal: persist the (goal_id, task_id) pair the master
    // handed off with BEFORE `brief.md`. This ordering is the load-bearing
    // publication invariant: `brief.md` is the visibility gate
    // (`staged_peer_dir` refuses to surface a peer without it), so any peer
    // that becomes visible to the blackboard scan or a peer-boot watcher is
    // GUARANTEED to already carry its goal file. Writing it after brief.md
    // would open a window where a peer boots on the brief and runs its first
    // turn goal-less — a rollback cannot undo an already-started peer.
    //
    // Layout: line 1 = goal_id, line 2 = task_id (may be empty). The goal_id
    // is MANDATORY for the file to exist (no point writing a task_id with no
    // enclosing goal); when the master passes `goal_id = None` the file is
    // omitted entirely and the peer runs goal-less.
    //
    // Failure rolls back the whole staging so the master gets a truthful
    // "handoff failed" rather than a silently goal-less peer.
    if let Some(goal_id_str) = goal_id.map(str::trim).filter(|s| !s.is_empty()) {
        let task_id_str = task_id.map(str::trim).unwrap_or("");
        let body = format!("{goal_id_str}\n{task_id_str}");
        if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "goal", &body) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to write peer goal context: {err}"
            )));
        }
    }

    let brief_path = peer_dir.join("brief.md");
    if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "brief.md", brief) {
        cleanup_staged_peer(workspace_root, &slug, &peer_dir);
        return Err(RpcError::internal_error(format!(
            "failed to write brief: {err}"
        )));
    }

    // Store the display NAME so the peer is addressable by it and readers
    // (`read_peer_blackboard` / `resolve_peer_name_to_slug`) can surface it.
    if let Some(name) = name {
        if let Err(err) = peer_io::write_peer_file_atomic(&peer_dir, "name", name) {
            cleanup_staged_peer(workspace_root, &slug, &peer_dir);
            return Err(RpcError::internal_error(format!(
                "failed to write peer name: {err}"
            )));
        }
    }

    Ok(StagedPeer {
        topic: format!("peer-{slug}"),
        worktree_branch: worktree.then(|| format!("peer/{slug}")),
        slug,
        brief_path,
        cwd,
    })
}

/// The `.git` COMMON dir of `workspace_root` — where per-worktree admin dirs
/// live. Resolved via git (not `join(".git")`) because `.git` is a FILE when the
/// workspace is itself a worktree, and the admin dirs then live in the parent
/// repo. `None` when `workspace_root` is not a repo.
pub(crate) fn git_common_dir(workspace_root: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    // `--git-common-dir` may answer relatively (`.git`) — anchor it.
    Some(if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    })
}

/// Drop ONLY the worktree belonging to peer dir `dir`.
///
/// Deliberately NOT `git worktree prune`, which is what this used to do: prune
/// is repo-GLOBAL and removes the admin entry of EVERY worktree whose checkout
/// is currently missing. `git worktree add` registers the admin entry BEFORE the
/// checkout is fully in place, so a sibling peer staged concurrently sits in
/// exactly that window — one peer's rollback silently destroyed another peer's
/// fence, leaving its branch checked out nowhere. Every peer checkout is named
/// `wt` (`peers/<slug>/wt`), so git disambiguates the admin dirs as `wt`, `wt1`,
/// … — which is why this showed up as "the SECOND peer lost its worktree".
///
/// Both steps here are scoped STRICTLY to paths under `dir`, so a sibling is
/// never touched no matter what state it is in.
pub(crate) fn remove_peer_worktree(workspace_root: &Path, dir: &Path) {
    // Normal path: the checkout exists, so git can unregister it by PATH.
    let checkout = dir.join("wt");
    if checkout.is_dir() {
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["worktree", "remove", "--force"])
            .arg(&checkout)
            .output();
    }
    // Fallback: a checkout that never finished (or a `remove` git refused)
    // leaves an admin entry behind, and a lingering entry keeps `branch -D`
    // from succeeding — which would burn the slug for any retry. Sweep it by
    // hand, matching ONLY entries whose `gitdir` points inside `dir`.
    let Some(common) = git_common_dir(workspace_root) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return;
    };
    // Compare canonicalized too: on macOS a temp/staging path reaches git as
    // `/private/var/…` while `dir` is the `/var/…` symlink (or vice versa), and
    // a purely lexical match would silently sweep nothing.
    let canonical = std::fs::canonicalize(dir).ok();
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        let target = Path::new(target.trim());
        let mine = target.starts_with(dir)
            || canonical
                .as_deref()
                .is_some_and(|canonical| target.starts_with(canonical));
        if mine {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Fetch a peer's fence branch out of its own clone and into the workspace repo.
///
/// Peers are staged as CLONES (see `stage_peer`), so `peer/<slug>` exists only
/// inside `peers/<slug>/wt`. Without this the work is invisible from the
/// workspace — `git branch` would not list it and the deliverable would look
/// like it never happened. Run on close, once the peer is done writing.
///
/// Best-effort by design: a peer that never committed, was staged without a
/// worktree, or whose clone is gone simply has nothing to collect, and none of
/// those should fail the close.
pub(crate) fn collect_peer_branch(peer_dir: &Path, slug: &str) {
    let clone = peer_dir.join("wt");
    if !clone.join(".git").exists() {
        return;
    }
    // The clone's `origin` IS the workspace repo it was cloned from, so the
    // destination is self-describing — no need to thread a workspace root
    // through the close callback, and it stays correct even if the session's
    // workspace moved after staging.
    let origin = match std::process::Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["config", "--get", "remote.origin.url"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => return,
    };
    if origin.is_empty() {
        return;
    }
    let workspace_root = PathBuf::from(origin);
    let branch = format!("peer/{slug}");
    // `+` forces the update: a re-opened peer that committed again must not be
    // refused for a non-fast-forward.
    let refspec = format!("+{branch}:{branch}");
    match std::process::Command::new("git")
        .arg("-C")
        .arg(&workspace_root)
        .args(["fetch", "--no-tags", "--quiet"])
        .arg(&clone)
        .arg(&refspec)
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(slug, branch = %branch, "collected peer branch from its clone");
        }
        Ok(out) => tracing::warn!(
            slug,
            branch = %branch,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "could not collect the peer branch (it may have committed nothing)"
        ),
        Err(error) => tracing::warn!(slug, %error, "failed to run git to collect the peer branch"),
    }
}

/// Roll back ONE half-staged peer: unregister its OWN worktree, remove its
/// reserved dir, then best-effort `branch -D peer/<slug>` (all no-ops for a
/// member that never got a worktree). The single-member synchronous sibling of
/// [`cleanup_staged_peers`], used inside [`stage_peer`].
pub(crate) fn cleanup_staged_peer(workspace_root: &Path, slug: &str, dir: &Path) {
    // BEFORE `remove_dir_all`: `git worktree remove` needs the checkout on disk
    // to unregister it by path.
    remove_peer_worktree(workspace_root, dir);
    let _ = std::fs::remove_dir_all(dir);
    let branch = format!("peer/{slug}");
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["branch", "-D", &branch])
        .output();
}

/// #1801 v3: per-turn ceiling on `peer_handoff` staging calls. A runaway
/// model cannot fan out an unbounded peer fleet in one turn; the 5th call
/// is rejected with a model-visible error.
pub(crate) const PEER_HANDOFFS_PER_TURN_MAX: u32 = 4;

/// #1801 v3 depth-1 guard: a peer session (topic `peer-<slug>`) never gets
/// the `peer_handoff` tool registered at all — peers cannot hand off
/// recursively, and the tool is not even visible to the model there.
pub(crate) fn peer_handoff_allowed_for_session(session_id: &SessionKey) -> bool {
    !session_id
        .topic()
        .is_some_and(|topic| topic.starts_with("peer-"))
}

/// #1801 v3: build the `peer_handoff` staging callback for ONE turn of the
/// serve/WS path. Turn-scoped state is baked in at wiring time: the
/// profile's `peers/` root, the session's workspace root, the ORIGINATING
/// session key stamped onto the emitted `peer/staged` event, and the
/// per-turn handoff counter enforcing [`PEER_HANDOFFS_PER_TURN_MAX`].
/// `emit_staged` abstracts the durable notification send so tests can
/// observe the event without a live WS connection.
pub(crate) fn build_peer_handoff_callback(
    peers_root: PathBuf,
    workspace_root: PathBuf,
    originating_session: SessionKey,
    profile_id: String,
    // #peer-model — the KEYS of the profile's configured `sub_providers`
    // (model lanes). A `peer_handoff` naming a matching lane records it beside
    // the brief so the peer runs its turns on that provider; an unknown lane
    // is surfaced as a warning note (the peer falls back to the primary
    // model), never a failure.
    available_lanes: Vec<String>,
    handoffs_this_turn: Arc<AtomicU32>,
    emit_staged: Arc<dyn Fn(PeerStagedEvent) + Send + Sync>,
) -> octos_agent::PeerHandoffCallback {
    Arc::new(move |request: octos_agent::PeerHandoffRequest| {
        if handoffs_this_turn.fetch_add(1, Ordering::SeqCst) >= PEER_HANDOFFS_PER_TURN_MAX {
            return Err(format!(
                "peer handoff limit reached for this turn ({PEER_HANDOFFS_PER_TURN_MAX})"
            ));
        }
        // Peers are named: the slug is derived from the (required, validated)
        // name and must be unique — `stage_peer` rejects a duplicate rather
        // than auto-suffixing. `seed` is unused on the named path.
        //
        // codex #6 — record WHO handed off (the originating master session) so
        // the fleet-ownership scan + `peer_results_ready_note` are reliable.
        // `stage_peer` writes it atomically BEFORE brief.md and rolls the
        // staging back on failure, so a peer is never visible-but-ownerless.
        let originator = originating_session.to_string();
        // Peer-agent-based goal AUTO-BIND (#1953): if the master handed off
        // WITHOUT an explicit goal_id but its session has an ACTIVE goal, bind
        // the peer to that goal. The model (esp. k3) does not reliably thread
        // goal_id — it parallelizes goal_create+peer_handoff (the id isn't
        // available yet) or simply omits it — so relying on the LLM leaves
        // every peer goal-less and the whole loop inert. The active goal is
        // the correct default; an explicit goal_id still wins.
        let resolved_goal_id: Option<String> = request.goal_id.clone().or_else(|| {
            default_agent_orchestrator().active_goal_id(&originating_session, &profile_id)
        });
        let staged = stage_peer(
            &peers_root,
            &workspace_root,
            &request.name,
            Some(&request.name),
            Some(originator.as_str()),
            &request.brief,
            request.worktree,
            // Explicit goal_id wins; else the master's active goal (auto-bind).
            resolved_goal_id.as_deref(),
            request.task_id.as_deref(),
        )
        .map_err(|err| err.message)?;
        // #peer-model — optional model lane. Record a VALID lane symlink-safely
        // under the re-validated staged dir; an unknown lane (or a failed
        // record) is a truthful warning (the peer runs on the primary model),
        // never a staging failure.
        let model_note = record_peer_model_lane(
            &peers_root,
            &staged.slug,
            request.model.as_deref(),
            &available_lanes,
        );
        // Durable so reconnect replay still delivers the open request; the
        // client dedups by an already-open session for the topic.
        emit_staged(PeerStagedEvent {
            session_id: originating_session.clone(),
            topic: staged.topic.clone(),
            slug: staged.slug.clone(),
            brief: request.brief.clone(),
            brief_path: staged.brief_path.to_string_lossy().into_owned(),
            cwd: staged.cwd.to_string_lossy().into_owned(),
            worktree_branch: staged.worktree_branch.clone(),
            profile_id: profile_id.clone(),
        });
        Ok(octos_agent::PeerHandoffStaged {
            slug: staged.slug,
            topic: staged.topic,
            brief_path: staged.brief_path.to_string_lossy().into_owned(),
            cwd: staged.cwd.to_string_lossy().into_owned(),
            worktree_branch: staged.worktree_branch,
            model_note,
        })
    })
}

/// #peer-model — read a small text file through an `O_NOFOLLOW` open (Unix) so
/// a symlink leaf swapped into a validated peer dir cannot redirect the read to
/// an off-tenant target (mirrors `read_file_no_follow` for the sync peer-file
/// layer). On non-Unix, re-checks `symlink_metadata` first. `None` on any error
/// (missing, symlink, unreadable).
/// #peer-model — read a peer's optional model LANE key from
/// `peers/<slug>/model` (written by the `peer_handoff` staging callback when
/// the master named a VALID `sub_provider` lane). Routed through
/// [`staged_peer_dir`] (real, non-symlink dir with `brief.md`) AND the
/// fd-anchored [`peer_io::read_peer_file`] (openat `O_NOFOLLOW` under the pinned
/// dir fd, regular-file only) so neither a symlinked dir nor a symlinked/FIFO
/// `model` leaf is ever followed. Returns the trimmed lane key, or `None` when
/// the dir is not a real staged peer, the file is absent/symlinked, or empty.
pub(crate) fn read_peer_model_lane(peers_root: &Path, slug: &str) -> Option<String> {
    let dir = staged_peer_dir(peers_root, slug)?;
    let lane = peer_io::read_peer_file(&dir, "model", peer_io::PEER_FILE_READ_CAP_SMALL)?;
    let lane = lane.trim();
    (!lane.is_empty()).then(|| lane.to_owned())
}

/// #peer-model — record a requested model lane for a freshly-staged peer,
/// returning the tool-visible note (`None` = recorded cleanly, or no lane was
/// requested). Validates the (trimmed) lane against the CURRENT
/// `available_lanes`; a match is written symlink-safely under the RE-VALIDATED
/// [`staged_peer_dir`] (never `brief_path.parent()`, which races a parent
/// swap) via the fd-anchored atomic writer ([`peer_io::write_peer_file_atomic`],
/// no-follow openat + renameat under the pinned dir fd). Both an unknown lane
/// and a failed record are TRUTHFUL: they say the peer will run on the primary
/// model, matching what the turn actually does.
pub(crate) fn record_peer_model_lane(
    peers_root: &Path,
    slug: &str,
    requested: Option<&str>,
    available_lanes: &[String],
) -> Option<String> {
    let lane = requested.map(str::trim).filter(|lane| !lane.is_empty())?;
    if !available_lanes.iter().any(|key| key == lane) {
        let available = if available_lanes.is_empty() {
            "none configured".to_owned()
        } else {
            available_lanes.join(", ")
        };
        return Some(format!(
            "model lane '{lane}' not found (available: {available}) — \
             this peer will use the primary model."
        ));
    }
    let recorded = match staged_peer_dir(peers_root, slug) {
        Some(dir) => peer_io::write_peer_file_atomic(&dir, "model", lane)
            .map_err(|err| eyre::eyre!("failed to write peer model lane: {err}")),
        None => Err(eyre::eyre!("staged peer dir not found for slug {slug}")),
    };
    if let Err(err) = recorded {
        tracing::warn!(
            ?err,
            slug,
            lane,
            "failed to record peer model lane; peer will use the primary model"
        );
        return Some(format!(
            "could not record model lane '{lane}' — this peer will use the primary model."
        ));
    }
    None
}

/// Cap on the human-readable prompt summary shown for a parked prompt.
pub(crate) const PEER_PENDING_PROMPT_CAP: usize = 2048;

/// Kind of interactive prompt a peer session is parked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerPendingKind {
    Approval,
    Question,
}

impl PeerPendingKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Question => "question",
        }
    }
}

/// One interactive prompt a PEER session is currently parked on, PROJECTED from
/// the process-global pending store (`contract_stores()`), which is the single
/// AUTHORITY for "awaiting input" — not the filesystem. Because that store is
/// in-memory, internally consistent, and shared by peer_list / peer_respond /
/// peer_close (all in the serve process), there is no torn on-disk index, no
/// stale-delete that could hide a still-parked entry, and no marker a peer could
/// park without: the store entry exists the instant `request_runtime` registers
/// the oneshot, so a parked peer is visible and answerable for as long as it
/// remains OPEN. A CLOSED peer never parks at all: `PeerParkGate` (in the
/// serve/WS tree, `api::ui_protocol`) refuses the registration and the close
/// aborts the peer's turn (#1842), so this projection can never hide a live
/// park.
#[derive(Debug, Clone)]
pub(crate) struct PeerPendingSummary {
    pub(crate) kind: PeerPendingKind,
    /// The `ApprovalId`/`QuestionId` as a string — the master targets a specific
    /// prompt by this (`peer_list` lists each id).
    pub(crate) id: String,
    /// Compact prompt summary for display.
    pub(crate) prompt: String,
    /// For a QUESTION: the offered option labels across its questions (a display
    /// hint; real answers are validated by the store against the stored request).
    /// Empty for an approval.
    pub(crate) options: Vec<String>,
}

/// Compact, capped summary of an interactive prompt (title/body) for display.
pub(crate) fn peer_pending_prompt_summary(title: &str, body: &str) -> String {
    let title = title.trim();
    let body = body.trim();
    let combined = if title.is_empty() {
        body.to_owned()
    } else if body.is_empty() || body == title {
        title.to_owned()
    } else {
        format!("{title} — {body}")
    };
    capped_utf8(combined, PEER_PENDING_PROMPT_CAP).0
}

/// Every prompt `session` is currently parked on, read from the AUTHORITATIVE
/// process-global stores. Approvals first, then questions, each group ordered by
/// id — a deterministic order for display and single-default selection.
pub(crate) fn peer_pending_summaries(
    contracts: &UiProtocolContractStores,
    session: &SessionKey,
) -> Vec<PeerPendingSummary> {
    let mut approvals: Vec<PeerPendingSummary> = contracts
        .approvals
        .pending_for_session(session)
        .into_iter()
        .map(|event| PeerPendingSummary {
            kind: PeerPendingKind::Approval,
            id: event.approval_id.0.to_string(),
            prompt: peer_pending_prompt_summary(&event.title, &event.body),
            options: Vec::new(),
        })
        .collect();
    approvals.sort_by(|a, b| a.id.cmp(&b.id));
    let mut questions: Vec<PeerPendingSummary> = contracts
        .user_questions
        .pending_for_session(session)
        .into_iter()
        .map(|event| PeerPendingSummary {
            kind: PeerPendingKind::Question,
            id: event.question_id.0.to_string(),
            prompt: peer_pending_prompt_summary(&event.title, &event.body),
            options: event
                .questions
                .iter()
                .flat_map(|question| question.options.iter().map(|option| option.label.clone()))
                .collect(),
        })
        .collect();
    questions.sort_by(|a, b| a.id.cmp(&b.id));
    approvals.append(&mut questions);
    approvals
}

/// The peer's TRUSTED session key (#P1-1): the wire it runs its turns under,
/// recorded server-side at `session/open`. `None` when the peer is not currently
/// open — it then has no live oneshot to answer or cancel. This is the ONLY
/// slug→session mapping any peer-control path trusts; it never comes from a
/// client argument or an on-disk file.
pub(crate) fn peer_trusted_session(profile_id: &str, slug: &str) -> Option<SessionKey> {
    peer_wire_registry().resolve(&peer_wire_key(profile_id, slug))
}

/// Map the tool's answer entries onto the store's `UserQuestionAnswer[]`,
/// matching each entry to ITS question's options (#new-P2-#2): a bare string
/// answer to a CHOICE question becomes a real label selection for THAT question
/// (so a 2–4-question choice prompt is answerable), while free text passes
/// through where the question allows it. One answer per stored question, in
/// order; a mismatched count/label surfaces the store's typed error rather than
/// resolving incorrectly.
pub(crate) fn peer_respond_build_answers(
    req_answers: &[octos_agent::PeerRespondAnswer],
    questions: &[octos_core::ui_protocol::UserQuestion],
) -> Vec<octos_core::ui_protocol::UserQuestionAnswer> {
    req_answers
        .iter()
        .enumerate()
        .map(|(index, answer)| {
            if answer.selected_labels.is_empty() {
                if let Some(text) = &answer.free_text {
                    if let Some(option) = questions.get(index).and_then(|question| {
                        question
                            .options
                            .iter()
                            .find(|option| option.label.eq_ignore_ascii_case(text))
                    }) {
                        return octos_core::ui_protocol::UserQuestionAnswer {
                            selected_labels: vec![option.label.clone()],
                            free_text: None,
                        };
                    }
                }
            }
            octos_core::ui_protocol::UserQuestionAnswer {
                selected_labels: answer.selected_labels.clone(),
                free_text: answer.free_text.clone(),
            }
        })
        .collect()
}

/// The cross-session resolution `peer_respond`'s host callback performs — a
/// named fn (rather than an inline closure) so tests exercise the EXACT
/// production path. Authorizes the caller as the peer's recorded originator,
/// derives the peer's TRUSTED session key from the wire registry (#P1-1),
/// selects the targeted parked prompt from the AUTHORITATIVE store (by `id`, or
/// the sole one), and resolves that oneshot via the SAME store the client
/// `approval/respond` / `user_question/respond` RPCs use. For an approval it
/// also emits `approval/decided` + audit via `on_approval_decided` (#P1-5),
/// attributing the MASTER. No filesystem marker is read or written — the store
/// is the source of truth. Every error is a model-visible string.
pub(crate) fn peer_respond_resolve(
    peers_root: &Path,
    origin_session: &str,
    profile_id: &str,
    contracts: &UiProtocolContractStores,
    on_approval_decided: &dyn Fn(&ApprovalDecidedEvent, Option<&str>),
    req: octos_agent::PeerRespondRequest,
) -> Result<(), String> {
    // Resolve NAME/slug → real slug (names are the primary address).
    let slug = resolve_peer_name_to_slug(peers_root, &req.slug).ok_or_else(|| {
        format!(
            "no peer named '{ident}' — check the name (or slug) with peer_list",
            ident = req.slug
        )
    })?;
    if !peer_slug_is_safe(&slug) {
        return Err(format!("invalid peer slug '{slug}'"));
    }
    // Only the peer's recorded originator may respond (the same fail-closed
    // check peer_send_input uses; the originator lives in `peers/<slug>/`).
    peer_send_input_authorized(peers_root, &slug, origin_session)?;
    // A retired peer is not awaiting input.
    if peer_is_closed(peers_root, &slug) {
        return Err(format!("peer '{slug}' is closed"));
    }

    // #P1-1 SECURITY — the peer's TRUSTED session key comes ONLY from the wire
    // registry (server-captured at `session/open`), so a resolution can reach
    // exactly THIS peer's oneshots and no other's.
    let Some(peer_session) = peer_trusted_session(profile_id, &slug) else {
        return Err(format!(
            "peer '{slug}' is not open — the user must open the staged peer session before it can be answered"
        ));
    };

    // The AUTHORITATIVE parked set for this peer, straight from the store.
    let pendings = peer_pending_summaries(contracts, &peer_session);
    if pendings.is_empty() {
        return Err(format!(
            "peer '{slug}' is not awaiting input — nothing to respond to \
             (peer_list shows a peer as `awaiting_input` when it is)"
        ));
    }
    let target = match req.id.as_deref() {
        Some(id) => pendings.iter().find(|p| p.id == id).ok_or_else(|| {
            format!(
                "peer '{slug}' has no pending prompt with id '{id}' — check the ids with peer_list"
            )
        })?,
        None if pendings.len() == 1 => &pendings[0],
        None => {
            let ids = pendings
                .iter()
                .map(|p| format!("{} ({})", p.id, p.kind.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "peer '{slug}' has {n} pending prompts — pass the specific id (one of: {ids})",
                n = pendings.len()
            ));
        }
    };

    // #1961 — the human-readable resolution to stamp on the goal-ledger
    // escalation once the answer/decision is delivered below.
    let escalation_resolution: String;
    match target.kind {
        PeerPendingKind::Approval => {
            let Some(decision) = req.decision.as_deref() else {
                return Err(format!(
                    "peer '{slug}' pending '{id}' is an APPROVAL — pass \
                     decision=\"approve\"/\"deny\" (not answer)",
                    id = target.id
                ));
            };
            escalation_resolution = format!("[approval] {decision}");
            let approval_id: ApprovalId =
                serde_json::from_value(serde_json::Value::String(target.id.clone()))
                    .map_err(|_| format!("peer '{slug}' pending id is malformed"))?;
            let params = octos_core::ui_protocol::ApprovalRespondParams {
                session_id: peer_session,
                approval_id,
                decision: ApprovalDecision::from(decision.to_owned()),
                approval_scope: None,
                client_note: Some(format!(
                    "answered by master via peer_respond ({origin_session})"
                )),
            };
            let outcome = contracts
                .approvals
                .respond_with_context(params.clone())
                .map_err(|err| {
                    format!("could not resolve peer '{slug}' approval: {}", err.message)
                })?;
            // #P1-5 — publish the canonical `approval/decided` + audit,
            // attributing the master, via the shared sink (same builder the RPC
            // handler uses).
            let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
            let event = crate::contracts::approvals::build_decided_event(
                &params,
                &outcome,
                origin_session,
                Utc::now(),
            );
            on_approval_decided(&event, tool_name.as_deref());
        }
        PeerPendingKind::Question => {
            let Some(req_answers) = req.answers.as_deref() else {
                return Err(format!(
                    "peer '{slug}' pending '{id}' is a QUESTION — pass answer/answers (not decision)",
                    id = target.id
                ));
            };
            escalation_resolution = format!(
                "[answer] {}",
                req_answers
                    .iter()
                    .map(|a| if a.selected_labels.is_empty() {
                        a.free_text.clone().unwrap_or_default()
                    } else {
                        a.selected_labels.join("/")
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            let question_id: octos_core::ui_protocol::QuestionId =
                serde_json::from_value(serde_json::Value::String(target.id.clone()))
                    .map_err(|_| format!("peer '{slug}' pending id is malformed"))?;
            // #new-P2-#2 — map each answer against the STORED request's questions
            // (per-question options), re-read from the authoritative store.
            let questions = contracts
                .user_questions
                .pending_for_session(&peer_session)
                .into_iter()
                .find(|event| event.question_id == question_id)
                .map(|event| event.questions)
                .unwrap_or_default();
            let answers = peer_respond_build_answers(req_answers, &questions);
            let params = UserQuestionRespondParams {
                session_id: peer_session,
                question_id,
                answers,
                client_note: Some(format!(
                    "answered by master via peer_respond ({origin_session})"
                )),
            };
            contracts
                .user_questions
                .respond_with_context(&params)
                .map_err(|err| {
                    format!("could not resolve peer '{slug}' question: {}", err.message)
                })?;
        }
    }
    // #1961 — the answer/decision was delivered above; mark this peer's OPEN
    // escalation resolved in the goal ledger so its durable escalation history
    // stops showing it as open. Best-effort: a goal-less peer, a missing
    // ledger, or no open escalation is a benign no-op, and a ledger write
    // failure must NOT fail the resume the caller already committed to.
    //
    // #1967 codex round — ordering: because delivery happens BEFORE this
    // resolve, a timeout sweep (`sweep_escalation_timeouts`) firing in the
    // gap can flip the row to `[timeout] …` first, making this bulk resolve
    // a no-op — the ledger then shows a timeout for an escalation that was
    // actually answered (the delivered answer itself is unaffected). Dormant
    // while producers write `default_after_secs = None`; the hazard and the
    // deliberate no-amend-API decision are documented on the sweep.
    if let Some(peer_dir) = staged_peer_dir(peers_root, &slug) {
        let goal_id = peer_io::read_peer_file(&peer_dir, "goal", peer_io::PEER_FILE_READ_CAP_SMALL)
            .and_then(|body| body.lines().next().map(|l| l.trim().to_owned()))
            .filter(|s| !s.is_empty());
        if let (Some(goal_id), Some(data_dir)) = (goal_id, peers_root.parent()) {
            if let Err(err) = default_agent_orchestrator().model_goal_resolve_peer_escalation(
                data_dir,
                &goal_id,
                &slug,
                &escalation_resolution,
                origin_session,
            ) {
                tracing::warn!(
                    slug = %slug,
                    goal_id = %goal_id,
                    error = %err,
                    "peer-goal: failed to resolve escalation in goal ledger (answer already delivered)"
                );
            }
        }
    }
    Ok(())
}

/// Count how many `result-<n>.md` version files exist in the peer directory,
/// via the fd-anchored, regular-file-only, scan-capped enumerator so swapping
/// `<slug>` to a symlink can neither redirect the scan into another tree nor
/// inflate the derived version number (#1824).
pub(crate) fn count_peer_result_versions(peer_dir: &std::path::Path) -> u32 {
    peer_io::peer_dir_count_prefixed(peer_dir, "result-", peer_io::PEER_DIR_SCAN_CAP) as u32
}

/// Parse `turns.txt` into `[(turn_count, outcome, updated_unix)]`.
/// Returns `None` when the file doesn't exist.
pub(crate) fn parse_peer_turns_index(
    peer_dir: &std::path::Path,
) -> Option<Vec<(u32, String, u64)>> {
    let text = peer_io::read_peer_file(peer_dir, "turns.txt", peer_io::PEER_FILE_READ_CAP_SMALL)?;
    if text.trim().is_empty() {
        return Some(Vec::new());
    }
    let entries: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let count: u32 = parts.next()?.parse().ok()?;
            let outcome = parts.next()?.to_string();
            let ts: u64 = parts.next()?.parse().ok()?;
            Some((count, outcome, ts))
        })
        .collect();
    Some(entries)
}

pub(crate) const PEER_GATHER_BRIEF_CAP: usize = 16 * 1024;

pub(crate) const PEER_GATHER_RESULT_CAP: usize = 48 * 1024;

/// One peer blackboard row as read off disk (`peers/<slug>/`), per-field
/// caps already applied. The shared currency of the `peer/gather` RPC and
/// the `peer_gather` tool callback — both are views over the SAME read.
pub(crate) struct PeerBlackboardRow {
    pub(crate) slug: String,
    /// Display NAME from `peers/<slug>/name` (the peer's primary address),
    /// falling back to the slug for legacy peers that have no `name` file.
    pub(crate) name: String,
    /// `brief.md`, capped at [`PEER_GATHER_BRIEF_CAP`].
    pub(crate) brief: String,
    pub(crate) brief_truncated: bool,
    /// `result.md` when any peer turn has terminated, capped at
    /// [`PEER_GATHER_RESULT_CAP`]; `None` = still running.
    pub(crate) result: Option<String>,
    pub(crate) result_truncated: bool,
    pub(crate) result_updated_unix: Option<u64>,
    pub(crate) has_worktree: bool,
    /// `true` when `peers/<slug>/closed` exists — the durable marker written
    /// by `peer_close` retiring the peer. A closed peer receives no further
    /// input; its result files stay readable.
    pub(crate) closed: bool,
    /// #435: parsed `turns.txt` entries: `[(turn_count, outcome, updated_unix)]`.
    /// `None` when the file doesn't exist (single-turn-or-less peer).
    pub(crate) turn_history: Option<Vec<(u32, String, u64)>>,
    /// #peer-model — the model LANE key from `peers/<slug>/model` (a configured
    /// `sub_provider` this peer runs its turns on), trimmed; `None` for a peer
    /// on the profile's primary model.
    pub(crate) model_lane: Option<String>,
}

/// #1801: row-reading core of the peer blackboard — every staged peer dir
/// under `peers_root` (a `brief.md` is the staging contract; stray dirs are
/// skipped), optionally narrowed to `slugs`, sorted by slug, with the
/// per-field caps applied. Extracted from `raw_peer_gather` verbatim so the
/// RPC's behavior is unchanged and the `peer_gather` tool reads the exact
/// same rows.
pub(crate) fn read_peer_blackboard(
    peers_root: &Path,
    slugs: Option<&[String]>,
) -> Vec<PeerBlackboardRow> {
    let mut rows: Vec<PeerBlackboardRow> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(peers_root) {
        // No `is_dir()` pre-filter — it follows symlinks. `staged_peer_dir`
        // below is the sole gate (safe slug + real non-symlink dir + brief.md).
        let mut dirs: Vec<_> = read_dir.flatten().collect();
        dirs.sort_by_key(|entry| entry.file_name());
        for entry in dirs {
            let slug = entry.file_name().to_string_lossy().into_owned();
            if let Some(filter) = slugs {
                if !filter.iter().any(|wanted| wanted == &slug) {
                    continue;
                }
            }
            // Only REAL, staged (non-symlink) peer dirs — a symlinked entry
            // could redirect reads outside `peers/`; `staged_peer_dir` also
            // enforces the `brief.md` staging contract.
            let Some(dir) = staged_peer_dir(peers_root, &slug) else {
                continue;
            };
            let Some(brief) =
                peer_io::read_peer_file(&dir, "brief.md", peer_io::PEER_FILE_READ_CAP_LARGE)
            else {
                continue;
            };
            let result =
                peer_io::read_peer_file(&dir, "result.md", peer_io::PEER_FILE_READ_CAP_LARGE);
            let result_updated_unix = peer_io::peer_file_mtime(&dir, "result.md")
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs());
            let (brief, brief_truncated) = capped_utf8(brief, PEER_GATHER_BRIEF_CAP);
            let (result, result_truncated) = match result {
                Some(result) => {
                    let (capped, truncated) = capped_utf8(result, PEER_GATHER_RESULT_CAP);
                    (Some(capped), truncated)
                }
                None => (None, false),
            };
            // Display name: `peers/<slug>/name`, trimmed; legacy peers with no
            // `name` file fall back to the slug so the row always has an address.
            let name = peer_io::read_peer_file(&dir, "name", peer_io::PEER_FILE_READ_CAP_SMALL)
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| slug.clone());
            rows.push(PeerBlackboardRow {
                slug,
                name,
                brief,
                brief_truncated,
                result,
                result_truncated,
                result_updated_unix,
                has_worktree: dir.join("wt").is_dir(),
                closed: peer_io::peer_regular_file_exists(&dir, "closed"),
                turn_history: parse_peer_turns_index(&dir),
                // #peer-model — the recorded model lane, if any (fd-anchored
                // no-follow read so a symlinked/FIFO `model` leaf is refused;
                // trimmed, empty treated as absent).
                model_lane: peer_io::read_peer_file(
                    &dir,
                    "model",
                    peer_io::PEER_FILE_READ_CAP_SMALL,
                )
                .map(|lane| lane.trim().to_owned())
                .filter(|lane| !lane.is_empty()),
            });
        }
    }
    rows
}

/// Compose the compact one-line-per-peer INDEX the `peer_list` tool returns:
/// slug, status (`closed` if retired via `peer_close`, else `done` when a
/// result file exists, else `running`), last-updated unix (or "—"), turn
/// count, and `worktree` when the peer has its own fence. Deliberate contrast
/// with [`compose_peer_gather_text`], which reads each peer's full brief +
/// result — this is the index, that is the payload.
/// Cap on peer rows the `peer_list` index emits inline — a runaway fleet
/// cannot flood the model's context; the overflow folds into a trailing
/// "… and N more" line (read specific peers with peer_gather slugs).
pub(crate) const PEER_LIST_MAX_ROWS: usize = 200;

/// `awaiting_by_slug`: the AUTHORITATIVE parked-prompt set per peer slug,
/// projected from the process-global store by the caller (`build_peer_list_callback`).
/// A slug absent from the map (or closed) is not awaiting input.
pub(crate) fn compose_peer_list_text(
    rows: &[PeerBlackboardRow],
    available_lanes: &[String],
    awaiting_by_slug: &std::collections::HashMap<String, Vec<PeerPendingSummary>>,
) -> String {
    if rows.is_empty() {
        return "(no peers staged)".to_owned();
    }
    let mut lines: Vec<String> = Vec::with_capacity(rows.len().min(PEER_LIST_MAX_ROWS) + 2);
    lines.push(format!("peers ({}):", rows.len()));
    for row in rows.iter().take(PEER_LIST_MAX_ROWS) {
        // Precedence: a retired peer is `closed`; else a peer PARKED on one or
        // more interactive prompts is `awaiting_input` (the master can answer
        // each via peer_respond) — this beats `done` so a persistent peer that
        // finished an earlier turn and is now blocked mid-turn still surfaces as
        // blocked; else `done` when a result exists; else `running`.
        let awaiting: &[PeerPendingSummary] = if row.closed {
            &[]
        } else {
            awaiting_by_slug
                .get(&row.slug)
                .map_or(&[][..], Vec::as_slice)
        };
        let status = if row.closed {
            "closed"
        } else if !awaiting.is_empty() {
            "awaiting_input"
        } else if row.result.is_some() {
            "done"
        } else {
            "running"
        };
        // List each parked prompt (id + kind + short prompt + any offered
        // options) so the master sees WHAT the peer waits on and which `id` to
        // pass to peer_respond — without a peer_gather. Capped so a peer with a
        // flood of prompts can't dominate the index.
        const PEER_LIST_MAX_PENDING: usize = 8;
        let awaiting_note = if awaiting.is_empty() {
            String::new()
        } else {
            let mut items = awaiting
                .iter()
                .take(PEER_LIST_MAX_PENDING)
                .map(|pending| {
                    let (prompt, truncated) = capped_utf8(pending.prompt.trim().to_owned(), 80);
                    let ellipsis = if truncated { "…" } else { "" };
                    let opts = if pending.options.is_empty() {
                        String::new()
                    } else {
                        format!(" options=[{}]", pending.options.join(", "))
                    };
                    format!(
                        "[id={id} {kind}: {prompt}{ellipsis}{opts}]",
                        id = pending.id,
                        kind = pending.kind.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            if awaiting.len() > PEER_LIST_MAX_PENDING {
                items.push_str(&format!(
                    " (+{} more)",
                    awaiting.len() - PEER_LIST_MAX_PENDING
                ));
            }
            format!("  · awaiting: {items}")
        };
        let updated = row
            .result_updated_unix
            .map_or_else(|| "—".to_owned(), |ts| ts.to_string());
        let turns = row.turn_history.as_ref().map_or(0, Vec::len);
        let worktree = if row.has_worktree { "  worktree" } else { "" };
        // #peer-model — annotate the peer's model lane, resolved against the
        // CURRENT `sub_providers` so the index matches what the turn actually
        // does: a lane whose key no longer exists is flagged as falling back to
        // the primary model, not printed as if it were live.
        let model = match row.model_lane.as_deref() {
            Some(lane) if available_lanes.iter().any(|key| key == lane) => {
                format!("  · model={lane}")
            }
            Some(lane) => format!("  · model={lane} (unavailable→primary)"),
            None => String::new(),
        };
        // Address by NAME; show the slug in parens when it differs.
        let addr = if row.name == row.slug {
            row.slug.clone()
        } else {
            format!("{} ({})", row.name, row.slug)
        };
        lines.push(format!(
            "- {addr}  {status}  updated {updated}  turns {turns}{worktree}{model}{awaiting_note}"
        ));
    }
    if rows.len() > PEER_LIST_MAX_ROWS {
        lines.push(format!("… and {} more", rows.len() - PEER_LIST_MAX_ROWS));
    }
    lines.join("\n")
}

/// Build the `peer_list` read callback for ONE turn of the serve/WS path.
/// Mirrors [`build_peer_gather_callback`] but composes the compact status
/// index ([`compose_peer_list_text`]) over the SAME row reader
/// ([`read_peer_blackboard`]); it takes no slugs — it always lists every peer.
/// `available_lanes` (the profile's CURRENT `sub_provider` keys) lets the index
/// flag a peer whose recorded model lane no longer resolves (#peer-model).
pub(crate) fn build_peer_list_callback(
    peers_root: PathBuf,
    available_lanes: Vec<String>,
    contracts: Arc<UiProtocolContractStores>,
    profile_id: String,
) -> octos_agent::PeerListCallback {
    Arc::new(move || {
        let rows = read_peer_blackboard(&peers_root, None);
        // #peer-respond — the AUTHORITATIVE awaiting-input set comes from the
        // process-global store, joined to each open peer by its TRUSTED wire
        // session (never a filesystem marker). A peer with no wire (not open) or
        // no store entries simply isn't awaiting.
        let awaiting_by_slug: std::collections::HashMap<String, Vec<PeerPendingSummary>> = rows
            .iter()
            .filter(|row| !row.closed)
            .filter_map(|row| {
                let session = peer_trusted_session(&profile_id, &row.slug)?;
                let pending = peer_pending_summaries(&contracts, &session);
                (!pending.is_empty()).then(|| (row.slug.clone(), pending))
            })
            .collect();
        Ok(compose_peer_list_text(
            &rows,
            &available_lanes,
            &awaiting_by_slug,
        ))
    })
}
