//! Build-cache pool: allocate, release, and reclaim cargo target dirs.
//!
//! Motivation (outer-loop #1, 2026-09-06 disk incident): every peer cloned
//! its own workspace, each workspace grew its own `target/`, and cargo never
//! reclaims old artifacts — 43 targets totaling 303 GB, with the agent host
//! killed twice by a full disk. This module is the mechanism side of the fix
//! (design: docs/build-cache-pool.md): a bounded pool of reusable compile
//! slots per repository, exclusive access via `flock`, holder metadata for
//! crash recovery, and a free-space gate that refuses new allocations when
//! the pool's filesystem is nearly full.
//!
//! Layout (§1.3):
//!
//! ```text
//! <pool-root>/<repo-key>/
//!   slot-N/            # peer slots, N = 1..peer_slots (default 2)
//!     .lock            # flock lock file (content meaningless, NEVER deleted)
//!     holder.json      # holder metadata (exists only while held)
//!     last_used        # one line, unix seconds
//!     target/          # the CARGO_TARGET_DIR handed to cargo
//!   verify-N/          # outer-loop slots, N = 1..verify_slots (default 1)
//! ```
//!
//! Core invariants (referenced throughout as I1–I4):
//!
//! - I1 slot exclusivity: at most one holder per slot at any time. The
//!   advisory file lock is the truth; holder metadata is advisory only.
//! - I2 reusable, never wrongly deleted: release keeps the cache contents;
//!   only "no holder + last_used past the stale window" may be GC'd, and
//!   directory mtimes are NEVER a signal.
//! - I3 space gate first: measure free space before allocating; refuse with
//!   a readable error instead of filling the disk.
//! - I4 minimal exposure: only the peer's own slot is writable in the
//!   sandbox (wiring lands with #4).
//!
//! peer slots (`slot-N`) and outer-loop slots (`verify-N`) are two separate
//! namespaces: a peer never takes `verify-N` and `octos cache acquire
//! --purpose verify` never takes `slot-N`, so outer-loop re-verification is
//! not crowded out by running peers and vice versa (§1.3).

pub mod pool;
pub mod repo_key;

pub use pool::{
    BuildCacheConfig, BuildCacheError, GcPolicy, ReclaimOutcome, ReclaimReport, Slot, SlotKind,
    SlotOutcome, SlotPurpose, acquire, acquire_detached, reclaim_stale, release, release_detached,
    touch,
};
pub use repo_key::{RepoKeyParseError, repo_key, repo_key_for_path};
