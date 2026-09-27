//! The polite page reader shared by the research tools (deep-search skill
//! and the built-in `search`): one implementation of the reading discipline.
//!
//! For each URL: SSRF check, robots.txt (per origin, cached), per-host
//! spacing (plus `Crawl-delay`), SSRF-safe GET with DNS pinning on every hop,
//! content-type and size caps, readability extraction. When plain HTTP yields
//! no main text, an optional browser [`Renderer`] renders the page; the
//! rendered result is only accepted after the browser's final URL and every
//! navigation it reports pass the same SSRF check (and robots.txt when the
//! site changed). Anything that fails is discarded, never extracted.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

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
}

pub type RenderFuture = Pin<Box<dyn Future<Output = Result<Rendered, String>> + Send>>;
/// Browser renderer: URL in, rendered document out. Implementations should
/// also block private destinations inside the browser (request
/// interception); the reader re-validates regardless.
pub type Renderer = Arc<dyn Fn(String) -> RenderFuture + Send + Sync>;

/// Reader settings.
#[derive(Clone)]
pub struct ReaderConfig {
    pub host_interval: Duration,
    pub timeout: Duration,
    pub max_page_bytes: usize,
    /// Keep the page HTML in [`ReadPage::html`] (for link extraction).
    pub keep_html: bool,
    /// Plain-text fallback when readability finds no article (e.g. a
    /// boilerplate-stripping HTML→text converter).
    pub fallback_text: Option<fn(&str) -> String>,
    pub renderer: Option<Renderer>,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            host_interval: Duration::from_secs(1),
            timeout: Duration::from_secs(15),
            max_page_bytes: 3 * 1024 * 1024,
            keep_html: false,
            fallback_text: None,
            renderer: None,
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

    /// robots.txt decision for `url`: `Ok(crawl_delay)` or `Err(reason)`
    /// (`robots`, `robots_unreachable`, `invalid_url`).
    pub async fn robots_allows(&self, url: &str) -> Result<Option<Duration>, String> {
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

    /// Read one page. `Err(reason)` is meant to be recorded as skipped.
    pub async fn read(&self, url: &str) -> Result<ReadPage, String> {
        net::check_url(url)
            .await
            .map_err(|e| format!("ssrf_blocked: {e}"))?;
        let crawl_delay = self.robots_allows(url).await?;
        let host = urls::domain_of(url).unwrap_or_default();
        self.throttle.wait(&host, crawl_delay).await;

        let resp = net::safe_get(url, self.cfg.timeout)
            .await
            .map_err(|e| format!("fetch_error: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("fetch_error: HTTP {}", status.as_u16()));
        }
        let final_url = resp.url().to_string();
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
            return Err(format!("unsupported_content_type: {ctype}"));
        }
        let body = net::read_capped(resp, self.cfg.max_page_bytes)
            .await
            .map_err(|e| format!("fetch_error: {e}"))?;
        let fetched_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        if ctype.starts_with("text/plain") {
            return Ok(ReadPage {
                final_url,
                text: body.trim().to_string(),
                fetched_at,
                ..Default::default()
            });
        }

        let mut ex = self.extract_with_fallback(&body, &final_url);
        let mut page = ReadPage {
            final_url,
            html: body,
            fetched_at,
            ..Default::default()
        };
        if ex.is_empty_text() {
            if let Some(render) = self.cfg.renderer.clone() {
                match render(page.final_url.clone()).await {
                    Ok(r) => match self.accept_rendered(&page.final_url, r).await {
                        Ok((rex, r)) => {
                            ex = rex;
                            page.final_url = r.final_url;
                            page.html = r.html;
                            page.rendered = true;
                        }
                        Err(reason) if reason.starts_with("ssrf_blocked") => return Err(reason),
                        Err(_) => {}
                    },
                    Err(e) => eprintln!("[octos-research] render skipped for {url}: {e}"),
                }
            }
        }
        if ex.is_empty_text() {
            return Err("no_main_text".to_string());
        }
        page.text = ex.text;
        page.meta = ex.meta;
        if !self.cfg.keep_html {
            page.html.clear();
        }
        Ok(page)
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
    /// extraction. `Err` reasons: `ssrf_blocked: …`, robots reasons,
    /// `no_main_text`.
    pub async fn accept_rendered(
        &self,
        page_url: &str,
        rendered: Rendered,
    ) -> Result<(Extracted, Rendered), String> {
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
            net::check_url(u)
                .await
                .map_err(|e| format!("ssrf_blocked: rendered page went to {u}: {e}"))?;
        }
        if urls::domain_of(&final_url) != urls::domain_of(page_url) {
            self.robots_allows(&final_url).await?;
        }
        let ex = self.extract_with_fallback(&rendered.html, &final_url);
        if ex.is_empty_text() {
            return Err("no_main_text".to_string());
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
        assert!(err.starts_with("ssrf_blocked"), "{err}");
        assert!(err.contains("169.254.169.254"), "{err}");
    }

    #[tokio::test]
    async fn should_check_every_reported_navigation_not_just_the_final_url() {
        let reader = Reader::new(ReaderConfig::default());
        let r = Rendered {
            final_url: "http://93.184.216.34/landing".into(),
            html: "<p>x</p>".into(),
            navigations: vec!["http://127.0.0.1:8080/internal".into()],
        };
        let err = reader
            .accept_rendered("http://93.184.216.34/", r)
            .await
            .unwrap_err();
        assert!(err.contains("127.0.0.1"), "{err}");
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
            assert!(err.starts_with("ssrf_blocked"), "{u}: {err}");
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
}
