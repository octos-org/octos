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
//! Rate limiting & DoS hardening (codex GAP 8)
//! -------------------------------------------
//! Without a cap, an authenticated client could mint unbounded tokens
//! (each ~200 bytes, held 10 minutes by default) and OOM the daemon.
//! Two layers of bounds:
//!   1. Per-`issuer_bearer`: at most [`PreviewTokens::MAX_PER_BEARER`]
//!      (64) concurrent grants. At ~200 bytes each that's ~12 KB per
//!      user — generous for a SPA that mints one preview per site
//!      iframe, restrictive for an attacker. Mints over the cap return
//!      `IssueError::PerBearerLimitReached` which the handler maps to
//!      HTTP 429.
//!   2. Global: at most [`PreviewTokens::MAX_TOTAL`] (10 000) entries
//!      across all bearers. With ~200 bytes each that caps the whole
//!      map at ~2 MB. Bounds the worst case where a large number of
//!      tenants each sit at the per-bearer cap simultaneously. Mints
//!      over the global cap also return 429.
//!
//! Both checks happen AFTER the lazy `sweep_expired` so a long-idle
//! bearer doesn't get stuck against its old quota.
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
    /// concurrent grants. Handler maps to HTTP 429.
    PerBearerLimitReached,
    /// The cache as a whole is at [`PreviewTokens::MAX_TOTAL`]. Handler
    /// maps to HTTP 429.
    GlobalLimitReached,
}

impl std::fmt::Display for IssueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IssueError::Random(err) => write!(f, "getrandom failed: {err}"),
            IssueError::PerBearerLimitReached => {
                write!(f, "per-bearer preview-token cap reached")
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
    /// entry that's ~12 KB of cache per user — comfortably above the
    /// SPA's normal envelope (one mint per visible iframe) and well
    /// below the threshold where a hostile client could starve the
    /// daemon. Codex GAP 8 fix.
    pub const MAX_PER_BEARER: usize = 64;

    /// Global cap on the whole cache, summed across every bearer.
    /// Bounds the worst case where a many-tenant fleet has many users
    /// each sitting at their per-bearer cap. ~200 bytes × 10 000 =
    /// ~2 MB — fits comfortably in the daemon's resident set.
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
    /// Rate limiting (codex GAP 8): after the lazy sweep we count
    /// entries owned by `issuer_bearer` and refuse if it would exceed
    /// [`Self::MAX_PER_BEARER`]. We also refuse if the global map
    /// would exceed [`Self::MAX_TOTAL`]. Both refusals return
    /// `IssueError::*LimitReached`, which the handler maps to HTTP 429
    /// — explicitly distinct from the 503 we return for OS-level
    /// randomness failures so the SPA can backoff vs surface an error.
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
        // sweeper. Sweep first, THEN count, so a bearer whose old
        // tokens just expired isn't artificially capped against
        // stale entries.
        sweep_expired(&mut map);

        // Rate limit: count this bearer's live grants. We must read
        // `grant.issuer_bearer` (not `&issuer_bearer` directly) to
        // avoid moving `issuer_bearer` before constructing the grant
        // — but we already built `grant` above, so use a captured
        // borrow from the field.
        let owner = &grant.issuer_bearer;
        let per_bearer = map.values().filter(|g| g.issuer_bearer == *owner).count();
        if per_bearer >= Self::MAX_PER_BEARER {
            return Err(IssueError::PerBearerLimitReached);
        }
        if map.len() >= Self::MAX_TOTAL {
            return Err(IssueError::GlobalLimitReached);
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
    /// sweeping at `interval` cadence; the returned `JoinHandle` is
    /// kept by the serve binary so the task lives as long as the
    /// process. There is no graceful shutdown signal: the sweeper is
    /// cheap (one write-lock sweep) and the process exit drops
    /// everything anyway.
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

/// Drop every entry whose `expires_at` is in the past. Called inline
/// from `issue` and `consume`, which both already hold the write
/// lock. Not called from `len` because that would defeat the test.
fn sweep_expired(map: &mut HashMap<String, Grant>) {
    let now = Instant::now();
    map.retain(|_, grant| grant.expires_at > now);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_store::UserRole;

    fn make_identity() -> AuthIdentity {
        AuthIdentity::User {
            id: "tenant-a".into(),
            role: UserRole::User,
        }
    }

    #[tokio::test]
    async fn issue_then_consume_returns_grant() {
        let cache = PreviewTokens::with_ttl(Duration::from_secs(60));
        let signed = cache
            .issue(
                "BEARER-A".into(),
                make_identity(),
                "tenant-a".into(),
                "session-1".into(),
                "site-a".into(),
            )
            .await
            .expect("issue");

        assert_eq!(signed.token.len(), 64, "256 bits -> 64 hex chars");
        assert!(
            signed
                .preview_url
                .starts_with(&format!("/api/preview-signed/{}/", signed.token)),
            "preview_url must embed the token in the URL path"
        );

        let grant = cache.consume(&signed.token).await.expect("grant present");
        assert_eq!(grant.profile_id, "tenant-a");
        assert_eq!(grant.session_id, "session-1");
        assert_eq!(grant.site_slug, "site-a");
        assert_eq!(grant.issuer_bearer, "BEARER-A");
    }

    #[tokio::test]
    async fn consume_unknown_token_returns_none() {
        let cache = PreviewTokens::with_ttl(Duration::from_secs(60));
        assert!(cache.consume("not-a-real-token").await.is_none());
    }

    #[tokio::test]
    async fn expired_grant_is_swept() {
        let cache = PreviewTokens::with_ttl(Duration::from_millis(20));
        let signed = cache
            .issue(
                "BEARER-A".into(),
                make_identity(),
                "tenant-a".into(),
                "session-1".into(),
                "site-a".into(),
            )
            .await
            .expect("issue");
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            cache.consume(&signed.token).await.is_none(),
            "expired grants must not be served"
        );
    }

    #[tokio::test]
    async fn token_uniqueness_across_issues() {
        let cache = PreviewTokens::with_ttl(Duration::from_secs(60));
        let mut seen = std::collections::HashSet::new();
        // `MAX_PER_BEARER` is 64 — the loop walks right up to the cap
        // without crossing it. The next iteration would refuse with
        // `PerBearerLimitReached`; that's exercised in
        // `per_bearer_cap_returns_rate_limited` below.
        for _ in 0..PreviewTokens::MAX_PER_BEARER {
            let signed = cache
                .issue(
                    "BEARER-A".into(),
                    make_identity(),
                    "tenant-a".into(),
                    "session-1".into(),
                    "site-a".into(),
                )
                .await
                .expect("issue");
            assert!(
                seen.insert(signed.token),
                "tokens must be unique across issues"
            );
        }
    }

    /// Codex GAP 8: per-bearer cap. Mint exactly MAX_PER_BEARER
    /// tokens, then assert the next mint fails with
    /// `PerBearerLimitReached`. A second bearer in the SAME cache must
    /// still be able to mint, proving the cap is per-bearer and not a
    /// global side-effect.
    #[tokio::test]
    async fn per_bearer_cap_returns_rate_limited() {
        let cache = PreviewTokens::with_ttl(Duration::from_secs(60));

        // Saturate bearer A up to the cap.
        for _ in 0..PreviewTokens::MAX_PER_BEARER {
            cache
                .issue(
                    "BEARER-A".into(),
                    make_identity(),
                    "tenant-a".into(),
                    "session-1".into(),
                    "site-a".into(),
                )
                .await
                .expect("under-cap issue must succeed");
        }

        // Cap + 1 from bearer A must refuse.
        let err = cache
            .issue(
                "BEARER-A".into(),
                make_identity(),
                "tenant-a".into(),
                "session-1".into(),
                "site-a".into(),
            )
            .await
            .expect_err("over-cap issue must fail");
        assert!(
            matches!(err, IssueError::PerBearerLimitReached),
            "expected PerBearerLimitReached, got: {err:?}"
        );

        // Bearer B must still succeed — the cap is per-bearer.
        cache
            .issue(
                "BEARER-B".into(),
                make_identity(),
                "tenant-a".into(),
                "session-2".into(),
                "site-a".into(),
            )
            .await
            .expect("second bearer must not be capped by the first");
    }

    /// Codex GAP 8: per-bearer expiry-sweep interaction. If the
    /// bearer's old tokens have expired, the lazy sweep inside `issue`
    /// must reset the count so the bearer can mint again — otherwise
    /// the cap would degrade to "lifetime grants per bearer" instead
    /// of "concurrent grants per bearer".
    #[tokio::test]
    async fn per_bearer_cap_clears_after_expiry_sweep() {
        let cache = PreviewTokens::with_ttl(Duration::from_millis(20));

        // Saturate bearer A.
        for _ in 0..PreviewTokens::MAX_PER_BEARER {
            cache
                .issue(
                    "BEARER-A".into(),
                    make_identity(),
                    "tenant-a".into(),
                    "session-1".into(),
                    "site-a".into(),
                )
                .await
                .expect("under-cap issue");
        }

        // Wait past the TTL.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Next mint must succeed — the lazy sweep inside `issue`
        // empties the map first.
        cache
            .issue(
                "BEARER-A".into(),
                make_identity(),
                "tenant-a".into(),
                "session-1".into(),
                "site-a".into(),
            )
            .await
            .expect("post-expiry mint must succeed (sweep cleared the cap)");
    }

    /// Codex NEEDS-FOLLOWUP 6: `sweep_expired_all` is the public hook
    /// the background task uses. Assert it drops expired entries
    /// WITHOUT requiring any `issue`/`consume` call.
    #[tokio::test]
    async fn sweep_expired_all_drops_expired_entries() {
        let cache = PreviewTokens::with_ttl(Duration::from_millis(20));
        cache
            .issue(
                "BEARER-A".into(),
                make_identity(),
                "tenant-a".into(),
                "session-1".into(),
                "site-a".into(),
            )
            .await
            .expect("issue");
        assert_eq!(cache.len().await, 1, "fresh entry present");
        tokio::time::sleep(Duration::from_millis(80)).await;
        cache.sweep_expired_all().await;
        assert_eq!(
            cache.len().await,
            0,
            "sweep_expired_all must drop expired entries"
        );
    }

    /// Codex NEEDS-FOLLOWUP 6: end-to-end exercise of the spawned
    /// background sweeper. With a short interval and a short TTL the
    /// task should empty the cache without any `issue`/`consume`
    /// activity from the test.
    #[tokio::test]
    async fn spawn_background_sweeper_drops_expired_entries() {
        let cache = Arc::new(PreviewTokens::with_ttl(Duration::from_millis(40)));
        cache
            .issue(
                "BEARER-A".into(),
                make_identity(),
                "tenant-a".into(),
                "session-1".into(),
                "site-a".into(),
            )
            .await
            .expect("issue");
        assert_eq!(cache.len().await, 1);

        let _handle =
            PreviewTokens::spawn_background_sweeper(cache.clone(), Duration::from_millis(15));

        // Wait long enough for: first-tick consume (+15ms) + at least
        // one post-TTL sweep (+15ms more after t=40). 200ms is plenty.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            cache.len().await,
            0,
            "background sweeper must have evicted the expired token"
        );
    }
}
