//! The polite page reader shared by the research tools (deep-search skill
//! and the built-in `search`): one implementation of the reading discipline.
//!
//! For each URL: SSRF check, robots.txt only when the operator enabled it
//! ([`ReaderConfig::respect_robots`], default off), per-host spacing (plus
//! `Crawl-delay` when robots is on), SSRF-safe GET with DNS pinning on every
//! hop, one backoff-and-retry on 429/503 honouring `Retry-After`,
//! content-type and size caps, readability extraction. When plain HTTP yields
//! no main text, an optional browser [`Renderer`] renders the page; the
//! rendered result is only accepted after the browser's final URL and every
//! navigation it reports pass the same SSRF check (and robots.txt when the
//! site changed). Anything that fails is discarded, never extracted.
//!
//! Every failure is a [`ReadError`] with a specific reason (see
//! [`crate::access`]): `blocked`, `robots`, `http_<status>`,
//! `bot_challenge`, `consent_page`, `paywall`, `login_wall`,
//! `redirect_unresolved`, `render_failed`, `render_timeout`,
//! `no_main_text`, … and the final URL when it is known. Walls are reported,
//! never worked around: a bot challenge over plain HTTP is not retried in the
//! browser, and nothing is clicked in a rendered page.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::access::{self, ReadError, ReadFailure};
use crate::extract::{self, Extracted, PageMeta};
use crate::{HostThrottle, RobotsCache, net, urls};

/// What a browser renderer returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rendered {
    /// URL of the document whose HTML is returned (after JS redirects).
    pub final_url: String,
    pub html: String,
    /// Every document URL the browser navigated through, when it can report
    /// them (redirect chain, JS/meta redirects). All are SSRF-checked.
    pub navigations: Vec<String>,
    /// HTTP status of the final document, when the renderer reports it.
    /// A status of 400 or more fails the read as `http_<status>`.
    pub status: Option<u16>,
}

pub type RenderFuture = Pin<Box<dyn Future<Output = Result<Rendered, String>> + Send>>;
/// Browser renderer: URL in, rendered document out. Implementations should
/// also block private destinations inside the browser (request
/// interception); the reader re-validates regardless.
///
/// Prefer returning `Ok` with the page the browser ended on, even when it is
/// a bot challenge, consent wall or error page: the reader then classifies
/// it and reports the final URL. An `Err` message is classified by
/// [`ReadError::from_render_error`]; start it with a reason code
/// (`bot_challenge: …`) to be exact.
pub type Renderer = Arc<dyn Fn(String) -> RenderFuture + Send + Sync>;

/// Reader settings.
#[derive(Clone)]
pub struct ReaderConfig {
    pub host_interval: Duration,
    pub timeout: Duration,
    pub max_page_bytes: usize,
    /// Fetch and obey robots.txt (and `Crawl-delay`). Default off: an
    /// operator setting ([`crate::RESPECT_ROBOTS_ENV`]). When off robots.txt
    /// is never requested.
    pub respect_robots: bool,
    /// Keep the page HTML in [`ReadPage::html`] (for link extraction).
    pub keep_html: bool,
    /// Plain-text fallback when readability finds no article (e.g. a
    /// boilerplate-stripping HTML→text converter).
    pub fallback_text: Option<fn(&str) -> String>,
    pub renderer: Option<Renderer>,
    /// Longest a render may take before the read fails as
    /// `render_timeout`.
    pub render_timeout: Duration,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            host_interval: Duration::from_secs(1),
            timeout: Duration::from_secs(15),
            max_page_bytes: 3 * 1024 * 1024,
            respect_robots: false,
            keep_html: false,
            fallback_text: None,
            renderer: None,
            render_timeout: Duration::from_secs(60),
        }
    }
}

/// A page read for citation.
#[derive(Debug, Clone, Default)]
pub struct ReadPage {
    pub final_url: String,
    pub text: String,
    pub meta: PageMeta,
    /// Page HTML when [`ReaderConfig::keep_html`] is set.
    pub html: String,
    pub rendered: bool,
    pub fetched_at: String,
}

impl ReadPage {
    /// Canonical URL: the page's own canonical link (same site), else the
    /// final URL, without tracking parameters.
    pub fn canonical_url(&self) -> String {
        urls::canonicalize(self.meta.canonical.as_deref().unwrap_or(&self.final_url))
    }
}

/// Polite reader. Cheap to share by reference across concurrent reads.
pub struct Reader {
    robots: RobotsCache,
    throttle: HostThrottle,
    cfg: ReaderConfig,
}

impl Reader {
    pub fn new(cfg: ReaderConfig) -> Self {
        Self {
            robots: RobotsCache::new(),
            throttle: HostThrottle::new(cfg.host_interval),
            cfg,
        }
    }

    /// Whether robots.txt is being applied.
    pub fn respects_robots(&self) -> bool {
        self.cfg.respect_robots
    }

    /// Origins whose robots.txt was requested (0 when robots is off).
    pub fn robots_origins_requested(&self) -> usize {
        self.robots.origins_requested()
    }

    /// robots.txt decision for `url`: `Ok(crawl_delay)` or `Err(reason)`
    /// (`robots`, `robots_unreachable`, `invalid_url`). Always `Ok(None)`
    /// without any request when robots is off.
    pub async fn robots_allows(&self, url: &str) -> Result<Option<Duration>, String> {
        if !self.cfg.respect_robots {
            return Ok(None);
        }
        let timeout = self.cfg.timeout;
        let d = self
            .robots
            .check(url, crate::AGENT_TOKEN, |robots_url| async move {
                match net::safe_get(&robots_url, timeout).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = net::read_capped(resp, 512 * 1024).await.unwrap_or_default();
                        (Some(status), body)
                    }
                    Err(_) => (None, String::new()),
                }
            })
            .await;
        if d.allowed {
            Ok(d.crawl_delay)
        } else {
            Err(d.reason.to_string())
        }
    }

    /// Read one page. A failure is meant to be recorded as skipped, with
    /// its reason and final URL.
    pub async fn read(&self, url: &str) -> Result<ReadPage, ReadError> {
        net::check_url(url)
            .await
            .map_err(|e| ReadError::new(ReadFailure::Blocked, format!("ssrf: {e}")))?;
        let crawl_delay = self.robots_allows(url).await.map_err(robots_error)?;
        let host = urls::domain_of(url).unwrap_or_default();
        self.throttle.wait(&host, crawl_delay).await;

        let fetch_error = |e: String| ReadError::new(ReadFailure::FetchError, e);
        let mut resp = net::safe_get(url, self.cfg.timeout)
            .await
            .map_err(fetch_error)?;
        // Busy/rate-limited: back off (Retry-After, capped) and retry once;
        // the host's next slot is pushed out for concurrent readers too.
        if matches!(resp.status().as_u16(), 429 | 503) {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| net::parse_retry_after(v, chrono::Utc::now()))
                .unwrap_or(self.cfg.host_interval * 5)
                .min(net::MAX_RETRY_AFTER);
            self.throttle.defer(&host, wait);
            self.throttle.wait(&host, None).await;
            resp = net::safe_get(url, self.cfg.timeout)
                .await
                .map_err(fetch_error)?;
        }
        let status = resp.status();
        let final_url = resp.url().to_string();
        if !status.is_success() {
            // A challenge usually answers 403/503: say so rather than the
            // bare status.
            let body = net::read_capped(resp, 256 * 1024).await.unwrap_or_default();
            if access::is_bot_challenge(&body) {
                return Err(ReadError::new(
                    ReadFailure::BotChallenge,
                    format!(
                        "HTTP {} with a bot challenge (not bypassed)",
                        status.as_u16()
                    ),
                )
                .at(final_url));
            }
            return Err(ReadError::new(ReadFailure::Http(status.as_u16()), "").at(final_url));
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !ctype.is_empty()
            && !ctype.contains("html")
            && !ctype.contains("xml")
            && !ctype.starts_with("text/")
        {
            return Err(ReadError::new(ReadFailure::UnsupportedContentType, ctype).at(final_url));
        }
        let body = net::read_capped(resp, self.cfg.max_page_bytes)
            .await
            .map_err(|e| fetch_error(e).at(final_url.clone()))?;
        let fetched_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        if ctype.starts_with("text/plain") {
            return Ok(ReadPage {
                final_url,
                text: body.trim().to_string(),
                fetched_at,
                ..Default::default()
            });
        }

        let ex = self.extract_with_fallback(&body, &final_url);
        let wall = access::diagnose(url, &final_url, &body, main_text(&ex));
        let mut page = ReadPage {
            final_url,
            html: body,
            fetched_at,
            ..Default::default()
        };
        // Plain HTTP found the article: done.
        let (ex, rendered) = match wall {
            None if !ex.is_empty_text() => (ex, None),
            // A challenge is never retried in a browser (it could pass it),
            // and a stated error is final.
            Some(e) if matches!(e.reason, ReadFailure::BotChallenge | ReadFailure::Http(_)) => {
                return Err(e);
            }
            // Script-built pages, Google News links, and walls a browser
            // may not show: render when a renderer is configured.
            wall => {
                let Some(render) = self.cfg.renderer.as_ref() else {
                    return Err(match wall {
                        Some(e) if e.reason == ReadFailure::RedirectUnresolved => ReadError {
                            detail: "Google News links reach the publisher only through a \
                                     script, and no browser renderer is configured"
                                .into(),
                            ..e
                        },
                        Some(e) => e,
                        None => ReadError::new(ReadFailure::NoMainText, "").at(&page.final_url),
                    });
                };
                let (rex, r) = self.render_and_accept(render, url, &page.final_url).await?;
                (rex, Some(r))
            }
        };
        if let Some(r) = rendered {
            page.final_url = r.final_url;
            page.html = r.html;
            page.rendered = true;
        }
        page.text = ex.text;
        page.meta = ex.meta;
        if !self.cfg.keep_html {
            page.html.clear();
        }
        Ok(page)
    }

    /// Render `page_url` (bounded by [`ReaderConfig::render_timeout`]) and
    /// accept the result ([`Self::accept_rendered`]). `url` is the link the
    /// reader was given.
    async fn render_and_accept(
        &self,
        render: &Renderer,
        url: &str,
        page_url: &str,
    ) -> Result<(Extracted, Rendered), ReadError> {
        let rendered =
            match tokio::time::timeout(self.cfg.render_timeout, render(page_url.to_string())).await
            {
                Err(_) => {
                    return Err(ReadError::new(
                        ReadFailure::RenderTimeout,
                        format!("no page after {}s", self.cfg.render_timeout.as_secs()),
                    ));
                }
                Ok(Err(message)) => return Err(ReadError::from_render_error(&message)),
                Ok(Ok(r)) => r,
            };
        self.accept_rendered(url, rendered).await
    }

    fn extract_with_fallback(&self, html: &str, url: &str) -> Extracted {
        let mut ex = extract::extract(html, url);
        if ex.is_empty_text() {
            if let Some(f) = self.cfg.fallback_text {
                let plain = f(html);
                if plain.chars().filter(|c| !c.is_whitespace()).count()
                    >= extract::MIN_MAIN_TEXT_CHARS
                {
                    ex.text = plain;
                }
            }
        }
        ex
    }

    /// Accept a browser rendering of `page_url` only if the browser's final
    /// URL and every reported navigation pass the SSRF check (private,
    /// loopback, link-local/metadata, fail-closed DNS) and, when the site
    /// changed, robots.txt. Rejected renders are discarded before any
    /// extraction. Then the page must not be an error page or a wall
    /// ([`access::diagnose`]) and must have main text. Failures: `blocked`,
    /// robots reasons, `http_<status>`, `bot_challenge`, `consent_page`,
    /// `paywall`, `login_wall`, `redirect_unresolved`, `no_main_text`, each
    /// with the final URL.
    pub async fn accept_rendered(
        &self,
        page_url: &str,
        rendered: Rendered,
    ) -> Result<(Extracted, Rendered), ReadError> {
        let final_url = if rendered.final_url.is_empty() {
            page_url.to_string()
        } else {
            rendered.final_url.clone()
        };
        for u in rendered
            .navigations
            .iter()
            .chain(std::iter::once(&final_url))
        {
            if u.starts_with("about:") {
                continue;
            }
            net::check_url(u).await.map_err(|e| {
                ReadError::new(
                    ReadFailure::Blocked,
                    format!("ssrf: rendered page went to {u}: {e}"),
                )
            })?;
        }
        if self.cfg.respect_robots && urls::domain_of(&final_url) != urls::domain_of(page_url) {
            self.robots_allows(&final_url)
                .await
                .map_err(|r| robots_error(r).at(&final_url))?;
        }
        if let Some(status) = rendered.status.filter(|s| *s >= 400) {
            if access::is_bot_challenge(&rendered.html) {
                return Err(ReadError::new(
                    ReadFailure::BotChallenge,
                    format!("HTTP {status} with a bot challenge (not bypassed)"),
                )
                .at(&final_url));
            }
            return Err(ReadError::new(ReadFailure::Http(status), "").at(&final_url));
        }
        let ex = self.extract_with_fallback(&rendered.html, &final_url);
        if let Some(e) = access::diagnose(page_url, &final_url, &rendered.html, main_text(&ex)) {
            return Err(e);
        }
        if ex.is_empty_text() {
            return Err(ReadError::new(ReadFailure::NoMainText, "rendered").at(&final_url));
        }
        Ok((
            ex,
            Rendered {
                final_url,
                ..rendered
            },
        ))
    }
}

/// The extracted text, or `None` when extraction found no article.
fn main_text(ex: &Extracted) -> Option<&str> {
    (!ex.is_empty_text()).then_some(ex.text.as_str())
}

/// A robots.txt refusal as a read failure.
fn robots_error(reason: String) -> ReadError {
    match reason.as_str() {
        "robots" => ReadError::new(ReadFailure::Robots, ""),
        "robots_unreachable" => ReadError::new(ReadFailure::RobotsUnreachable, ""),
        other => ReadError::new(ReadFailure::Blocked, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REDIRECT_TO_METADATA: &str = include_str!("../tests/fixtures/js_redirect_private.html");

    fn rendered_metadata_page() -> Rendered {
        // What a browser returns after the fixture's JS redirect lands on
        // the cloud metadata endpoint: attacker-chosen "content" there.
        Rendered {
            final_url: "http://169.254.169.254/latest/meta-data/iam/security-credentials/".into(),
            html: format!(
                "<html><body><article><p>{}</p></article></body></html>",
                "AccessKeyId SecretAccessKey Token ".repeat(20)
            ),
            navigations: vec!["http://93.184.216.34/start".into()],
            status: None,
        }
    }

    #[tokio::test]
    async fn should_discard_render_that_redirects_to_a_private_ip() {
        assert!(REDIRECT_TO_METADATA.contains("169.254.169.254"));
        let reader = Reader::new(ReaderConfig::default());
        let err = reader
            .accept_rendered("http://93.184.216.34/start", rendered_metadata_page())
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::Blocked, "{err}");
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
    }

    #[tokio::test]
    async fn should_check_every_reported_navigation_not_just_the_final_url() {
        let reader = Reader::new(ReaderConfig::default());
        let r = Rendered {
            final_url: "http://93.184.216.34/landing".into(),
            html: "<p>x</p>".into(),
            navigations: vec!["http://127.0.0.1:8080/internal".into()],
            status: None,
        };
        let err = reader
            .accept_rendered("http://93.184.216.34/", r)
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::Blocked);
        assert!(err.to_string().contains("127.0.0.1"), "{err}");
    }

    /// Default runs never request /robots.txt; with the operator setting on,
    /// the origin's robots.txt is requested before the page. Uses TEST-NET-1
    /// (192.0.2.0/24: public per the SSRF rules, never routed), so no real
    /// site is contacted and requests just time out.
    #[tokio::test]
    async fn should_not_request_robots_txt_unless_enabled() {
        let quick = |respect_robots| ReaderConfig {
            timeout: Duration::from_millis(300),
            host_interval: Duration::from_millis(1),
            respect_robots,
            ..Default::default()
        };
        let off = Reader::new(quick(false));
        assert!(!off.respects_robots());
        let _ = off.read("http://192.0.2.1/article").await;
        assert_eq!(
            off.robots_origins_requested(),
            0,
            "robots.txt must not be requested by default"
        );
        assert_eq!(off.robots_allows("http://192.0.2.1/x").await, Ok(None));

        let on = Reader::new(quick(true));
        let err = on.read("http://192.0.2.1/article").await.unwrap_err();
        assert_eq!(on.robots_origins_requested(), 1);
        assert_eq!(
            err.reason,
            ReadFailure::RobotsUnreachable,
            "RFC 9309: unreachable robots.txt disallows"
        );
    }

    #[tokio::test]
    async fn should_refuse_private_urls_before_fetching() {
        let reader = Reader::new(ReaderConfig::default());
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:3000/",
            "http://[::ffff:10.0.0.1]/",
        ] {
            let err = reader.read(u).await.unwrap_err();
            assert_eq!(err.reason, ReadFailure::Blocked, "{u}: {err}");
            assert_eq!(err.code(), "blocked");
        }
    }

    #[tokio::test]
    async fn should_run_renderer_hook_and_discard_private_result() {
        // A renderer that "follows" the fixture's JS redirect.
        let renderer: Renderer = Arc::new(|_url: String| {
            Box::pin(async { Ok(rendered_metadata_page()) }) as RenderFuture
        });
        let reader = Reader::new(ReaderConfig {
            renderer: Some(renderer.clone()),
            ..Default::default()
        });
        let r = renderer("http://93.184.216.34/start".into()).await.unwrap();
        assert!(
            reader
                .accept_rendered("http://93.184.216.34/start", r)
                .await
                .is_err()
        );
    }

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/read/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// A public address literal, so the SSRF check needs no DNS.
    const PUBLISHER: &str = "http://93.184.216.34/2026/09/27/story.html";

    fn page(final_url: &str, html: String, status: Option<u16>) -> Rendered {
        Rendered {
            final_url: final_url.into(),
            html,
            navigations: vec![final_url.into()],
            status,
        }
    }

    fn renderer(result: Result<Rendered, String>) -> Renderer {
        Arc::new(move |_url: String| {
            let r = result.clone();
            Box::pin(async move { r }) as RenderFuture
        })
    }

    fn slow_renderer() -> Renderer {
        Arc::new(|_url: String| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Err("never".to_string())
            }) as RenderFuture
        })
    }

    #[tokio::test]
    async fn should_report_bot_challenge_with_final_url_when_render_lands_on_datadome() {
        let reader = Reader::new(ReaderConfig::default());
        let err = reader
            .accept_rendered(
                PUBLISHER,
                page(PUBLISHER, fixture("datadome_challenge.html"), None),
            )
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::BotChallenge, "{err}");
        assert_eq!(err.final_url.as_deref(), Some(PUBLISHER));
    }

    #[tokio::test]
    async fn should_report_http_status_when_renderer_reports_an_error_status() {
        let reader = Reader::new(ReaderConfig::default());
        let err = reader
            .accept_rendered(
                PUBLISHER,
                page(PUBLISHER, fixture("article_mentions_walls.html"), Some(404)),
            )
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::Http(404));
        assert_eq!(err.code(), "http_404");
        let err = reader
            .accept_rendered(
                PUBLISHER,
                page(PUBLISHER, fixture("cloudflare_challenge.html"), Some(403)),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.reason,
            ReadFailure::BotChallenge,
            "a 403 challenge says so"
        );
    }

    #[tokio::test]
    async fn should_report_paywall_consent_and_error_pages_when_rendered() {
        let reader = Reader::new(ReaderConfig::default());
        for (name, want) in [
            ("paywall.html", ReadFailure::Paywall),
            ("cookie_wall.html", ReadFailure::ConsentPage),
            ("login_wall.html", ReadFailure::LoginWall),
            ("error_403_openresty.html", ReadFailure::Http(403)),
            ("cloudflare_challenge.html", ReadFailure::BotChallenge),
        ] {
            let err = reader
                .accept_rendered(PUBLISHER, page(PUBLISHER, fixture(name), None))
                .await
                .unwrap_err();
            assert_eq!(err.reason, want, "{name}: {err}");
            assert_eq!(err.final_url.as_deref(), Some(PUBLISHER), "{name}");
        }
    }

    #[tokio::test]
    async fn should_report_no_main_text_with_final_url_when_page_is_really_empty() {
        let reader = Reader::new(ReaderConfig::default());
        let err = reader
            .accept_rendered(
                PUBLISHER,
                page(
                    PUBLISHER,
                    "<html><body><div id=app></div></body></html>".into(),
                    None,
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::NoMainText);
        assert_eq!(err.final_url.as_deref(), Some(PUBLISHER));
    }

    #[tokio::test]
    async fn should_accept_article_when_it_mentions_walls() {
        let reader = Reader::new(ReaderConfig::default());
        let (ex, r) = reader
            .accept_rendered(
                PUBLISHER,
                page(PUBLISHER, fixture("article_mentions_walls.html"), Some(200)),
            )
            .await
            .unwrap();
        assert!(ex.text.contains("interstitial"), "{}", ex.text);
        assert_eq!(r.final_url, PUBLISHER);
    }

    #[tokio::test]
    async fn should_classify_renderer_errors_when_render_fails() {
        let reader = Reader::new(ReaderConfig::default());
        let err = reader
            .render_and_accept(
                &renderer(Err("blocked by a bot challenge (not bypassed)".into())),
                PUBLISHER,
                PUBLISHER,
            )
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::BotChallenge);
        let err = reader
            .render_and_accept(
                &renderer(Err("DevTools socket closed".into())),
                PUBLISHER,
                PUBLISHER,
            )
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::RenderFailed);
        assert!(err.detail.contains("DevTools"));
    }

    #[tokio::test(start_paused = true)]
    async fn should_report_render_timeout_when_renderer_does_not_finish() {
        let reader = Reader::new(ReaderConfig {
            render_timeout: Duration::from_secs(5),
            ..Default::default()
        });
        let err = reader
            .render_and_accept(&slow_renderer(), PUBLISHER, PUBLISHER)
            .await
            .unwrap_err();
        assert_eq!(err.reason, ReadFailure::RenderTimeout, "{err}");
    }

    #[tokio::test]
    async fn should_accept_rendered_article_when_renderer_succeeds() {
        let reader = Reader::new(ReaderConfig::default());
        let (ex, _) = reader
            .render_and_accept(
                &renderer(Ok(page(
                    PUBLISHER,
                    fixture("article_mentions_walls.html"),
                    None,
                ))),
                PUBLISHER,
                PUBLISHER,
            )
            .await
            .unwrap();
        assert!(!ex.is_empty_text());
    }
}
