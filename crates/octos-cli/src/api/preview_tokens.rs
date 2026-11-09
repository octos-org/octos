//! In-memory signed-preview token cache (codex design for issue
//! #1001 follow-up).
//!
//! Why this module exists
//! ----------------------
//! PR #1001 closed the cross-tenant `/api/preview/{profile_id}/...`
//! data-read by requiring `Authorization: Bearer ...` on every request.
//! The SPA dashboard's `<iframe src=/api/preview/...>` cannot inject
//! that header — `iframe` requests are plain GETs — so the dashboard
//! either had to drop the iframe or rely on cookies (which we don't
//! ship).
//!
//! Codex's design (verbatim in the PR thread): mint an opaque,
//! cryptographically random token via `POST /api/my/preview/sign`,
//! stash a grant in this process-local cache, and serve the preview
//! through a PUBLIC route `GET /api/preview-signed/{token}/{*path}`
//! where the token IS the auth credential. Relative assets under the
//! preview HTML inherit the `/api/preview-signed/{token}/` prefix
//! without any rewriting — that's why the token lives in the URL path
//! rather than a query string.
//!
//! Security properties of the token
//! --------------------------------
//! - **Opacity**: 256 bits of OS randomness encoded as 64 hex chars.
//!   Not a JWT — no claims, no signature, no key rotation surface. The
//!   token is a pure server-side handle: a 32-byte cookie that
//!   indexes into this in-memory map. Brute force is computationally
//!   infeasible (2^256), and a leaked token only grants the exact
//!   `{profile_id, session_id, site_slug}` triple that was approved.
//! - **Bound to issuer**: the grant captures the bearer that minted
//!   it (`issuer_bearer`) plus the `AuthIdentity` snapshot. The
//!   serve handler re-validates the bearer on every request — if the
//!   user logs out, the session is revoked, or the daemon restarts,
//!   the bearer is no longer valid and the preview 403s. That makes
//!   logout / session-delete naturally invalidate every outstanding
//!   preview without explicit token revocation.
//! - **Short TTL**: 10 minutes by default. The SPA renews at
//!   `expires_at - 60s` by re-signing, so a leaked token only buys
//!   the attacker the remainder of the current 10-minute window
//!   AND ownership of the issuer bearer (re-validated server-side).
//! - **Path-bound**: the grant fixes `profile_id`, `session_id`, and
//!   `site_slug`. The token cannot be re-used to read a different
//!   tenant's preview by tweaking the request — those fields come
//!   from the grant, not from the URL, and the URL has only `{token}`
//!   and `{*path}` segments.
//! - **Daemon restart invalidates everything**: tokens live in a
//!   `tokio::sync::RwLock<HashMap<...>>` that doesn't persist. A
//!   restart drops all outstanding grants and the SPA simply re-signs
//!   on the next render. This matches the "10 minute TTL" intent
//!   (nothing should outlive a restart).
//!
//! Not stored on disk
//! ------------------
//! Persisting tokens would (1) defeat the daemon-restart invalidation
//! property, (2) introduce a new on-disk secret store, and (3) make
//! the rotation surface considerably harder to reason about. Re-signing
//! is cheap (one POST + a HashMap insert) and the SPA already has the
//! bearer, so a persistent cache earns nothing.
//!
//! Rate limiting & DoS hardening (codex GAP 8 + #1007 / #1008)
//! -----------------------------------------------------------
//! Without a cap, an authenticated client could mint unbounded tokens
//! (each ~200 bytes, held 10 minutes by default) and OOM the daemon.
//! Three layers of bounds:
//!   1. Per-`issuer_bearer`: at most [`PreviewTokens::MAX_PER_BEARER`]
//!      (32) concurrent grants. Bounds how much one logged-in session
//!      can mint before it has to wait for previews to expire.
//!   2. Per-IDENTITY (#1007): at most
//!      [`PreviewTokens::MAX_PER_IDENTITY`] (64) concurrent grants
//!      across EVERY live bearer that resolves to the same identity
//!      (user id or `admin`). Closes the bypass where logging out and
//!      logging back in would reset the per-bearer counter — without
//!      this cap, an attacker who can rotate sessions could mint
//!      `MAX_PER_BEARER × N_rotations` tokens with no upper bound. The
//!      identity is read from `Grant.identity_snapshot`. Mints over
//!      the cap return `IssueError::PerIdentityLimitReached`.
//!   3. Global: at most [`PreviewTokens::MAX_TOTAL`] (10 000) entries
//!      across all identities. With ~200 bytes each that caps the
//!      whole map at ~2 MB. Bounds the worst case where a many-tenant
//!      fleet each sits at the per-identity cap simultaneously.
//!
//! #1008 / #1012 change: when the global cap is full the issue path
//! **does not** evict live grants. The original #1008 patch tried to
//! evict the earliest-expiring entry, but after the inline
//! `sweep_expired` already drops every expired token the
//! "earliest-expiring" candidate is the closest live grant — evicting
//! it pulls an active iframe out from under a legitimate user before
//! its advertised `expires_at`. The corrected contract (#1012) is:
//! sweep first, then if the map is still at the ceiling return
//! [`IssueError::GlobalLimitReached`] so the caller gets a 429 +
//! `Retry-After` instead of breaking somebody else's preview.
//!
//! All three checks happen AFTER the lazy `sweep_expired` so a
//! long-idle identity doesn't get stuck against its old quota.
//!
//! Background sweep (codex NEEDS-FOLLOWUP 6)
//! -----------------------------------------
//! `issue` / `consume` lazily call `sweep_expired`, but a daemon with
//! NO preview traffic accumulates expired entries until the next
//! request. The serve binary spawns [`PreviewTokens::spawn_background_sweeper`]
//! at startup to call `sweep_expired_all` every 60 s in production,
//! bounding the staleness window even when idle. Test code can pass a
//! shorter interval to assert the contract without sleeping for
//! minutes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use super::router::AuthIdentity;

/// Default time-to-live for a signed-preview token. The SPA schedules
/// a renewal at `expires_at - 60s`, so the in-flight window is at most
/// 9 minutes between sign / re-sign cycles.
pub const DEFAULT_PREVIEW_TOKEN_TTL: Duration = Duration::from_secs(600);

/// Default interval at which the background sweeper runs in production.
/// The serve binary passes this to
/// [`PreviewTokens::spawn_background_sweeper`]; tests override with a
/// much shorter interval so the sweep contract is exercisable without
/// real-time waits. Keep this comfortably shorter than the default TTL
/// so an expired token cannot live longer than `TTL + sweep_interval`.
pub const DEFAULT_PREVIEW_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Reasons [`PreviewTokens::issue`] can fail to mint a token. The
/// handler maps each variant to a specific HTTP status so the SPA can
/// distinguish a transient OS issue from quota exhaustion.
#[derive(Debug)]
pub enum IssueError {
    /// `getrandom` failed — usually means the OS entropy source is
    /// unavailable (very rare). Handler maps to HTTP 503.
    Random(std::io::Error),
    /// The requesting bearer is at [`PreviewTokens::MAX_PER_BEARER`]
    /// concurrent grants. Handler maps to HTTP 429 + `Retry-After`.
    PerBearerLimitReached,
    /// The requesting identity (#1007) — user id or admin — is at
    /// [`PreviewTokens::MAX_PER_IDENTITY`] concurrent grants across
    /// every live bearer it owns. Closes the session-rotation bypass:
    /// logging out and back in produces a fresh bearer but the same
    /// identity, so the cap is enforced against the stable identity
    /// snapshot stored on each grant. Handler maps to HTTP 429 +
    /// `Retry-After`.
    PerIdentityLimitReached,
    /// The cache as a whole is at [`PreviewTokens::MAX_TOTAL`] after
    /// the inline expiry sweep ran (#1012). The issue path explicitly
    /// does NOT evict live grants here — doing so would break a
    /// legitimate user's active preview iframe before its advertised
    /// `expires_at`. Handler maps to HTTP 429 + `Retry-After`. Distinct
    /// from the per-bearer / per-identity variants so logs can
    /// distinguish "this user is over quota" from "the daemon is at
    /// its hard global ceiling".
    GlobalLimitReached,
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueError::Random(err) => write!(f, "getrandom failed: {err}"),
            IssueError::PerBearerLimitReached => {
                write!(f, "per-bearer preview-token cap reached")
            }
            IssueError::PerIdentityLimitReached => {
                write!(f, "per-identity preview-token cap reached")
            }
            IssueError::GlobalLimitReached => {
                write!(f, "global preview-token cap reached")
            }
        }
    }
}

impl std::error::Error for IssueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IssueError::Random(err) => Some(err),
            _ => None,
        }
    }
}

/// In-memory cache of signed-preview grants.
///
/// Thread-safety: every entry-point takes `&self` and locks the
/// internal `RwLock`. Concurrent reads (the GET serve handler) take
/// the write lock briefly to consume + clean expired entries —
/// codex piggybacks the expiry sweep on `consume` so we don't need a
/// background sweeper task. With <1000 outstanding entries per
/// tenant this is fine; the sweep on a 1k-entry HashMap is a few
/// microseconds and runs at most once per preview request.
pub struct PreviewTokens {
    entries: RwLock<HashMap<String, Grant>>,
    ttl: Duration,
}

/// Server-side grant for one minted token. Captures everything the
/// `serve_signed_preview` handler needs to (a) re-validate the issuer
/// and (b) resolve the on-disk preview path WITHOUT consulting the
/// incoming URL for trust-bearing values.
#[derive(Clone)]
pub struct Grant {
    /// The bearer token that was sent on the `/api/my/preview/sign`
    /// request. The serve handler re-resolves this on every request
    /// — if the underlying auth manager session has been revoked /
    /// expired / deleted, the serve handler refuses with 403. This
    /// is what makes logout/session-delete invalidate outstanding
    /// previews "for free" without an explicit revocation API.
    pub issuer_bearer: String,
    /// `AuthIdentity` snapshot taken at sign time. Kept for
    /// authorisation re-checks (`is_authorized_for_profile`) and for
    /// audit/tracing — codex's design specifies storing the identity
    /// alongside the bearer.
    pub identity_snapshot: AuthIdentity,
    /// Profile that the preview targets. The serve handler resolves
    /// this profile's data dir to find the on-disk content.
    pub profile_id: String,
    /// Session that the preview targets.
    pub session_id: String,
    /// Site slug under the session workspace.
    pub site_slug: String,
    /// Wall-clock instant after which this grant is invalid. Stored
    /// as `Instant` (monotonic) so clock jumps cannot extend the
    /// grant's validity. The wire `expires_at` exposed to the SPA is
    /// converted to a `chrono::DateTime<Utc>` from this monotonic
    /// instant at sign time (see `SignedPreviewResponse`).
    pub expires_at: Instant,
}

/// Wire response for `POST /api/my/preview/sign`. The SPA stores
/// `preview_url` in iframe state, and schedules a re-sign timer at
/// `expires_at - 60s`.
#[derive(Debug, Serialize)]
pub struct SignedPreviewResponse {
    /// 64-char hex string. The SPA does not need to know this — it's
    /// already embedded in `preview_url` — but exposing it keeps the
    /// response self-describing for debugging and lets clients build
    /// non-`index.html` deep-links if they want.
    pub token: String,
    /// Fully-formed URL the SPA can stuff into `<iframe src=...>`.
    /// Form: `/api/preview-signed/{token}/index.html`. The SPA does
    /// not concatenate the host — the iframe inherits the same origin
    /// as the dashboard.
    pub preview_url: String,
    /// Wall-clock expiry (`chrono::DateTime<Utc>` serialised to RFC
    /// 3339). The SPA's renewal scheduler subtracts 60 s and re-signs
    /// when that timer fires.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl Default for PreviewTokens {
    fn default() -> Self {
        Self::with_ttl(DEFAULT_PREVIEW_TOKEN_TTL)
    }
}

impl PreviewTokens {
    /// Maximum concurrent grants per `issuer_bearer`. At ~200 bytes per
    /// entry that's ~6.4 KB of cache per active session. Lower than
    /// [`Self::MAX_PER_IDENTITY`] on purpose: it bounds how much a
    /// single session can mint before the user has to wait, and the
    /// per-identity cap (issue #1007) bounds the total across every
    /// rotated session.
    pub const MAX_PER_BEARER: usize = 32;

    /// Maximum concurrent grants per identity (user id, or `admin`)
    /// summed across every live bearer that resolves to that identity.
    /// Closes the session-rotation bypass in #1007: logging out and
    /// back in produces a fresh bearer, so a per-bearer-only cap would
    /// let one user mint `MAX_PER_BEARER × N_rotations` tokens. The
    /// per-identity cap is enforced against the
    /// [`AuthIdentity`] snapshot recorded at issue time.
    pub const MAX_PER_IDENTITY: usize = 64;

    /// Global cap on the whole cache, summed across every identity.
    /// Bounds the worst case where a many-tenant fleet has many users
    /// each sitting at their per-identity cap. ~200 bytes × 10 000 =
    /// ~2 MB — fits comfortably in the daemon's resident set. #1012:
    /// when the cap is hit `issue` runs the inline expiry sweep and,
    /// if the map is STILL at the cap, refuses with
    /// [`IssueError::GlobalLimitReached`]. The earlier #1008 attempt at
    /// "evict the earliest-expiring entry" was withdrawn because the
    /// candidate after the sweep is always a LIVE grant — evicting it
    /// breaks a real user's iframe before its `expires_at`.
    pub const MAX_TOTAL: usize = 10_000;

    /// Build a token cache with the default 10-minute TTL.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-only override that lets the integration tests rig a tiny
    /// TTL so expiry can be exercised without sleeping for minutes.
    /// Production callers should use [`Self::new`].
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Mint a new token. Generates 256 bits of OS randomness via
    /// `getrandom`, encodes as lowercase hex, and stashes the
    /// associated `Grant` keyed by the hex string. Returns the
    /// wire-shaped response that `POST /api/my/preview/sign` echoes
    /// back to the SPA.
    ///
    /// Determinism: the random number generator is the OS source
    /// (`/dev/urandom` on Linux, `getrandom(2)` on modern Linux,
    /// `getentropy` on macOS, `BCryptGenRandom` on Windows). Falling
    /// back is not allowed — if `getrandom` errors we return the
    /// error to the caller, who maps it to 503.
    ///
    /// Rate limiting (codex GAP 8 + #1007 + #1012):
    ///   1. Lazy sweep clears expired entries so a long-idle bearer
    ///      doesn't get stuck against its old quota.
    ///   2. Count live grants for THIS bearer; refuse with
    ///      `PerBearerLimitReached` if at [`Self::MAX_PER_BEARER`].
    ///   3. Count live grants for THIS identity (across all live
    ///      bearers — #1007 closes session-rotation bypass); refuse
    ///      with `PerIdentityLimitReached` if at
    ///      [`Self::MAX_PER_IDENTITY`].
    ///   4. If the map is STILL at [`Self::MAX_TOTAL`] after the sweep,
    ///      refuse with `GlobalLimitReached` (#1012). The earlier #1008
    ///      patch evicted the earliest-expiring entry here, but since
    ///      the inline sweep already drops expired tokens the candidate
    ///      after the sweep is always a LIVE grant — evicting it pulls
    ///      an active iframe out from under a legitimate user before
    ///      its advertised `expires_at`. Returning 429 instead lets the
    ///      losing client back off via `Retry-After` without disrupting
    ///      anyone else's active preview.
    pub async fn issue(
        &self,
        issuer_bearer: String,
        identity_snapshot: AuthIdentity,
        profile_id: String,
        session_id: String,
        site_slug: String,
    ) -> Result<SignedPreviewResponse, IssueError> {
        let mut raw = [0u8; 32];
        getrandom::getrandom(&mut raw).map_err(|err| {
            IssueError::Random(std::io::Error::other(format!("getrandom: {err}")))
        })?;
        let token = hex_encode(&raw);

        let expires_at = Instant::now() + self.ttl;
        let wire_expires_at = chrono::Utc::now()
            + chrono::Duration::from_std(self.ttl)
                .unwrap_or_else(|_| chrono::Duration::seconds(600));

        let grant = Grant {
            issuer_bearer,
            identity_snapshot,
            profile_id,
            session_id,
            site_slug,
            expires_at,
        };

        let mut map = self.entries.write().await;
        // Piggyback expiry sweep on issue — keeps the map bounded
        // without forcing the caller to wait for the background
        // sweeper. Sweep first, THEN count, so a bearer/identity
        // whose old tokens just expired isn't artificially capped
        // against stale entries.
        sweep_expired(&mut map);

        // Rate limit (per-bearer). Read `grant.issuer_bearer` rather
        // than the consumed `issuer_bearer` argument since `grant`
        // owns it now.
        let owner_bearer = &grant.issuer_bearer;
        let per_bearer = map
            .values()
            .filter(|g| g.issuer_bearer == *owner_bearer)
            .count();
        if per_bearer >= Self::MAX_PER_BEARER {
            return Err(IssueError::PerBearerLimitReached);
        }

        // Rate limit (per-identity — #1007). Count live grants whose
        // `identity_snapshot` matches this issue's identity. Stable
        // across session rotation: logging out clears the bearer but
        // not the identity, so the cap is enforced even when the
        // attacker controls bearer rotation.
        let owner_identity = identity_key(&grant.identity_snapshot);
        let per_identity = map
            .values()
            .filter(|g| identity_key(&g.identity_snapshot) == owner_identity)
            .count();
        if per_identity >= Self::MAX_PER_IDENTITY {
            return Err(IssueError::PerIdentityLimitReached);
        }

        // Global cap (#1012, supersedes #1008). The inline
        // `sweep_expired` above already dropped every expired token.
        // If the map is still at `MAX_TOTAL`, every remaining entry is
        // a LIVE grant — evicting any of them would break a legitimate
        // user's active iframe before its `expires_at`, which is
        // exactly the regression #1008 introduced. Belt-and-suspenders:
        // run one more sweep to cover the race where the background
        // sweeper is mid-tick and an entry expired between the sweep
        // above and now (rare but cheap to handle). After that, the
        // contract is simply "if still full, return 429" — the losing
        // caller can back off via the `Retry-After: 60` header without
        // disrupting anybody else's preview.
        if map.len() >= Self::MAX_TOTAL {
            sweep_expired(&mut map);
            if map.len() >= Self::MAX_TOTAL {
                return Err(IssueError::GlobalLimitReached);
            }
        }

        map.insert(token.clone(), grant);
        drop(map);

        let preview_url = format!("/api/preview-signed/{token}/index.html");
        Ok(SignedPreviewResponse {
            token,
            preview_url,
            expires_at: wire_expires_at,
        })
    }

    /// Look up a grant by token. Returns `None` if the token is
    /// unknown OR expired. The caller maps both cases to HTTP 404 so
    /// the response shape doesn't leak whether the token ever existed.
    ///
    /// Why not "consume-on-read"? Previews fetch many assets
    /// (`index.html`, CSS, JS, images) sequentially — single-use
    /// semantics would break the iframe on the first asset fetch.
    /// The grant remains valid until `expires_at`.
    pub async fn consume(&self, token: &str) -> Option<Grant> {
        let mut map = self.entries.write().await;
        // Lazy sweep on every consume keeps the map bounded while
        // we hold the write lock anyway.
        sweep_expired(&mut map);
        let grant = map.get(token)?.clone();
        if grant.expires_at <= Instant::now() {
            // Belt-and-braces: the sweep above should have removed it,
            // but if a token is exactly at the boundary (sweep ran at
            // `t == expires_at`, request lands one nanosecond later)
            // we still refuse here.
            map.remove(token);
            return None;
        }
        Some(grant)
    }

    /// Number of outstanding entries. Used by the integration test
    /// suite to assert the background sweeper actually evicted expired
    /// grants, and as a debug/metrics aid. The value is best-effort:
    /// concurrent `issue` / `consume` / sweeper calls race with this
    /// read, and a busy daemon may see the count change between reads.
    /// Not load-bearing for any auth decision — the auth surface uses
    /// `consume` which is properly serialised against the write lock.
    #[allow(clippy::len_without_is_empty)]
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Public hook for the background sweeper task. Acquires the
    /// write lock just long enough to drop every expired entry, then
    /// releases it. Codex NEEDS-FOLLOWUP 6 fix.
    ///
    /// Why a separate method (rather than calling `sweep_expired`
    /// directly from the spawned task): keeping the lock-acquisition
    /// inside `&self` lets the sweeper task hold an `Arc<Self>`
    /// without leaking the internal `RwLock` shape. The task is
    /// otherwise a one-liner.
    pub async fn sweep_expired_all(&self) {
        let mut map = self.entries.write().await;
        sweep_expired(&mut map);
    }

    /// Spawn the background sweeper. Codex NEEDS-FOLLOWUP 6 fix.
    ///
    /// Without this, expired grants only get cleaned up on
    /// `issue`/`consume` traffic — an idle daemon can accumulate
    /// expired entries indefinitely. The spawned task runs forever,
    /// sweeping at `interval` cadence; the returned [`JoinHandle`]
    /// should be kept by the serve binary so the task lives as long
    /// as the process. Callers that want the task to be aborted on
    /// drop should wrap it in [`PreviewSweeperHandle`] (issue #1009).
    ///
    /// Tests pass a short interval (e.g. 20 ms) to exercise the
    /// contract; production passes
    /// [`DEFAULT_PREVIEW_SWEEP_INTERVAL`] (60 s).
    pub fn spawn_background_sweeper(cache: Arc<Self>, interval: Duration) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // `Interval::tick` fires immediately on first call; skip
            // the boot tick so we wait `interval` before the first
            // sweep (the cache was just constructed and is empty).
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                cache.sweep_expired_all().await;
            }
        })
    }
}

/// Owning wrapper around the background sweeper's [`JoinHandle`] that
/// aborts the task when the handle is dropped (issue #1009).
///
/// Before #1009 the serve binary stored the handle in a `let
/// _preview_sweeper = ...` local and relied on `process::exit(0)` to
/// terminate the runtime — so a panicking caller, an `Err(_)`-bailing
/// startup path, or any code path that drops `AppState` without going
/// through `exit` would leak the sweeper task. Threading the handle
/// through `AppState` (or any `Arc<...>` that lives as long as the
/// daemon) plus this `Drop` impl makes the lifetime symmetric: the
/// sweeper exits exactly when the cache that owns it goes away.
///
/// The wrapper is also `Clone`-free on purpose — there should be at
/// most one owner of the abort signal at any time. Tests that need to
/// reach the inner handle can call [`Self::into_inner`] before drop.
pub struct PreviewSweeperHandle {
    handle: Option<JoinHandle<()>>,
}

impl PreviewSweeperHandle {
    /// Wrap a `JoinHandle` so it is aborted on drop.
    pub fn new(handle: JoinHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    /// Spawn the sweeper and return a self-aborting wrapper. Equivalent
    /// to `PreviewSweeperHandle::new(PreviewTokens::spawn_background_sweeper(...))`
    /// but reads more clearly at the call site.
    pub fn spawn(cache: Arc<PreviewTokens>, interval: Duration) -> Self {
        Self::new(PreviewTokens::spawn_background_sweeper(cache, interval))
    }

    /// Take ownership of the inner `JoinHandle`, disarming the abort.
    /// Useful in tests that want to `await` the sweeper directly.
    pub fn into_inner(mut self) -> Option<JoinHandle<()>> {
        self.handle.take()
    }

    /// Abort the sweeper task immediately without waiting for drop.
    /// Idempotent — subsequent calls are no-ops.
    pub fn abort(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Drop for PreviewSweeperHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Drop every entry whose `expires_at` is in the past. Called inline
/// from `issue` and `consume`, which both already hold the write
/// lock. Not called from `len` because that would defeat the test.
fn sweep_expired(map: &mut HashMap<String, Grant>) {
    let now = Instant::now();
    map.retain(|_, grant| grant.expires_at > now);
}

/// Stable string key for an [`AuthIdentity`]. Used by the per-identity
/// rate-limit counter (#1007) to count live grants across rotated
/// bearers. `Admin` is collapsed to a single key — there's one admin
/// surface, so admin previews share a quota.
///
/// We use a `"u:"` / `"a:"` prefix so two unrelated stores (e.g. a
/// future API token store with arbitrary IDs) cannot collide with
/// `Admin` even if they minted an id of `"admin"`.
fn identity_key(identity: &AuthIdentity) -> String {
    match identity {
        AuthIdentity::Admin => "a:admin".to_string(),
        AuthIdentity::User { id, .. } => format!("u:{id}"),
    }
}

impl PreviewTokens {
    /// Test-only: pad the cache with `count` synthetic grants whose
    /// expiry runs from `base_ttl + 0s` upward in 1-second increments,
    /// so the *first* injected entry is the earliest-expiring.
    ///
    /// Exposed `pub` (gated to `#[doc(hidden)]`) so integration tests
    /// in `tests/preview_signed.rs` can prove the #1008 eviction path
    /// end-to-end without minting `MAX_TOTAL` via the HTTP surface.
    /// Each filler uses a unique synthetic bearer + identity so the
    /// per-bearer/per-identity caps cannot trigger while padding —
    /// only the global cap is exercised.
    ///
    /// Returns the token strings in injection order; the caller can
    /// assert `tokens[0]` (the earliest-expiring) is the one evicted.
    #[doc(hidden)]
    pub async fn test_fill_with_synthetic_grants(
        &self,
        count: usize,
        base_ttl: Duration,
    ) -> Vec<String> {
        let mut map = self.entries.write().await;
        let base = Instant::now() + base_ttl;
        let mut tokens = Vec::with_capacity(count);
        for i in 0..count {
            let token = format!("filltoken-{i:06}");
            let identity = AuthIdentity::User {
                id: format!("filler-{i}"),
                role: crate::user_store::UserRole::User,
            };
            let grant = Grant {
                issuer_bearer: format!("BEARER-FILL-{i}"),
                identity_snapshot: identity,
                profile_id: "synthetic".into(),
                session_id: "synthetic".into(),
                site_slug: "synthetic".into(),
                expires_at: base + Duration::from_secs(i as u64),
            };
            map.insert(token.clone(), grant);
            tokens.push(token);
        }
        tokens
    }
}

/// `Arc<PreviewTokens>` is the shape stored in `AppState`. Aliased
/// here so the router/handler wiring doesn't have to repeat the
/// `Arc<...>` everywhere.
pub type SharedPreviewTokens = Arc<PreviewTokens>;

/// Lowercase-hex encode `bytes`. Inlined rather than pulling in the
/// `hex` crate — the only call site is `issue` and the input is
/// 32 bytes.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
