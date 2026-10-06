//! Client profiles for results pages that only answer a browser-like
//! client, as SearXNG does (the maintainer's decision, 2026-09-28: search
//! the way SearXNG does, no person in the loop).
//!
//! `legacy_mobile` is the client Google's page for simple phones
//! (`www.google.com/wml/search`) answers: the TLS and HTTP/2 fingerprint of
//! Chrome 100 on Android, a feature-phone User-Agent and the header set and
//! order below. The request shape was observed black-box from SearXNG's
//! network traffic (its requests through a logging proxy, and its installed
//! HTTP client, curl_cffi); no SearXNG code was read. Plainer clients (an
//! honest User-Agent, curl, rustls, a current Chrome fingerprint) get
//! Google's "unusual traffic" page for the same request.
//!
//! Only engines whose manifest names the profile (`client`) are fetched
//! with it; everything else keeps the identifiable octos client. The
//! private-address protection is the same as `ReqwestFetch`'s: the URL is
//! checked and DNS resolved (fail closed) before the request, and the
//! connection is pinned to the checked addresses. Proxies too: like the
//! plain client, it follows the proxy environment (`https_proxy`,
//! `all_proxy`, `no_proxy`), so every engine leaves through the same egress. Redirects are not
//! followed: a redirect is reported as the page the request ended on
//! (`x-octos-final-url`), which is how an engine recognises Google's
//! `/sorry/` page. Challenges are never solved; the engine is suspended.

use std::time::Duration;

use wreq_util::{Emulation, Platform, Profile};

use super::http::{Fetch, FetchFuture, HandOverFuture, HttpRequest, HttpResponse};

/// The `legacy_mobile` client profile (manifest `client`).
pub const LEGACY_MOBILE: &str = "legacy_mobile";

/// Headers `legacy_mobile` sends, in this order (with the fingerprint, the
/// order is part of what the page checks).
const LEGACY_MOBILE_HEADERS: &[(&str, &str)] = &[
    (
        "sec-ch-ua",
        "\" Not A;Brand\";v=\"99\", \"Chromium\";v=\"99\", \"Google Chrome\";v=\"99\"",
    ),
    ("sec-ch-ua-mobile", "?1"),
    ("sec-ch-ua-platform", "\"Android\""),
    ("upgrade-insecure-requests", "1"),
    (
        "user-agent",
        "Nokia6230/2.0 (05.50) Profile/MIDP-2.0 Configuration/CLDC-1.1",
    ),
    (
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,\
         image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.9",
    ),
    ("sec-fetch-site", "none"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-user", "?1"),
    ("sec-fetch-dest", "document"),
    ("accept-encoding", "gzip, deflate, br"),
    ("accept-language", "en-US,en;q=0.9"),
];

/// Largest body kept: the engine sandbox parses at most 3 MiB.
const MAX_BODY_BYTES: usize = 3 * 1024 * 1024;

/// A fetcher that presents the `legacy_mobile` client for requests that ask
/// for it and passes everything else to `inner`.
pub struct ImpersonatingFetch<F> {
    inner: F,
}

impl<F: Fetch> ImpersonatingFetch<F> {
    pub fn new(inner: F) -> Self {
        Self { inner }
    }
}

impl<F: Fetch> Fetch for ImpersonatingFetch<F> {
    fn fetch(&self, req: HttpRequest) -> FetchFuture<'_> {
        if req.client.as_deref() == Some(LEGACY_MOBILE) {
            return Box::pin(legacy_mobile(req));
        }
        self.inner.fetch(req)
    }

    fn render(&self, req: HttpRequest) -> FetchFuture<'_> {
        self.inner.render(req)
    }

    fn hand_over(&self, url: String) -> HandOverFuture<'_> {
        self.inner.hand_over(url)
    }

    fn can_render(&self) -> bool {
        self.inner.can_render()
    }

    fn supports_client(&self, client: &str) -> bool {
        client == LEGACY_MOBILE || self.inner.supports_client(client)
    }
}

async fn legacy_mobile(req: HttpRequest) -> Result<HttpResponse, String> {
    if req.method != "GET" {
        return Err(format!("{LEGACY_MOBILE}: only GET"));
    }
    let (host, addrs) = crate::net::check_url(&req.url).await?;
    let emulation = Emulation::builder()
        .profile(Profile::Chrome100)
        .platform(Platform::Android)
        .headers(false)
        .build();
    let client = wreq::Client::builder()
        .emulation(emulation)
        .redirect(wreq::redirect::Policy::none())
        .timeout(req.timeout)
        .connect_timeout(Duration::from_secs(10).min(req.timeout))
        .resolve_to_addrs(host.clone(), addrs.iter().copied())
        .build()
        .map_err(|e| format!("{LEGACY_MOBILE} client: {e}"))?;
    let mut headers = wreq::header::HeaderMap::new();
    let mut order = wreq::header::OrigHeaderMap::new();
    for (k, v) in LEGACY_MOBILE_HEADERS {
        let value = v
            .parse()
            .map_err(|e| format!("{LEGACY_MOBILE} header {k}: {e}"))?;
        headers.insert(*k, value);
        order.insert(*k);
    }
    let resp = client
        .get(&req.url)
        .headers(headers)
        .orig_headers(order)
        .send()
        .await
        .map_err(|e| format!("{LEGACY_MOBILE} request: {e}"))?;
    let status = resp.status().as_u16();
    let response_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            Some((
                k.as_str().to_ascii_lowercase(),
                v.to_str().ok()?.to_string(),
            ))
        })
        .collect();
    if (300..400).contains(&status) {
        // Not followed: the page the request ended on, for the engine.
        let location = response_headers
            .iter()
            .find(|(k, _)| k == "location")
            .map(|(_, v)| absolute(&req.url, v))
            .unwrap_or_default();
        return Ok(HttpResponse {
            status: 200,
            headers: vec![("x-octos-final-url".into(), location)],
            body: String::new(),
        });
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("{LEGACY_MOBILE} body: {e}"))?;
    let bytes = &bytes[..bytes.len().min(MAX_BODY_BYTES)];
    let mut headers = response_headers;
    headers.push(("x-octos-final-url".into(), req.url.clone()));
    Ok(HttpResponse {
        status,
        headers,
        body: String::from_utf8_lossy(bytes).into_owned(),
    })
}

/// `location` resolved against the request URL.
fn absolute(base: &str, location: &str) -> String {
    url::Url::parse(base)
        .and_then(|b| b.join(location))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| location.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;

    impl Fetch for Plain {
        fn fetch(&self, _req: HttpRequest) -> FetchFuture<'_> {
            Box::pin(async {
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: "plain".into(),
                })
            })
        }
    }

    fn request(url: &str, client: Option<&str>) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout: Duration::from_secs(2),
            client: client.map(String::from),
        }
    }

    #[test]
    fn should_offer_only_the_profiles_it_has() {
        let f = ImpersonatingFetch::new(Plain);
        assert!(f.supports_client(LEGACY_MOBILE));
        assert!(!f.supports_client("something_else"));
        assert!(!Plain.supports_client(LEGACY_MOBILE));
    }

    #[tokio::test]
    async fn should_leave_other_requests_to_the_plain_client() {
        let f = ImpersonatingFetch::new(Plain);
        let r = f
            .fetch(request("https://example.org/", None))
            .await
            .unwrap();
        assert_eq!(r.body, "plain");
    }

    #[tokio::test]
    async fn should_keep_the_private_address_protection() {
        let f = ImpersonatingFetch::new(Plain);
        for url in [
            "http://127.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
        ] {
            let err = f
                .fetch(request(url, Some(LEGACY_MOBILE)))
                .await
                .unwrap_err();
            assert!(err.contains("blocked"), "{url}: {err}");
        }
    }

    #[test]
    fn should_send_the_observed_header_set_in_order() {
        let names: Vec<&str> = LEGACY_MOBILE_HEADERS.iter().map(|(k, _)| *k).collect();
        assert_eq!(names.first(), Some(&"sec-ch-ua"));
        assert_eq!(names.last(), Some(&"accept-language"));
        assert!(names.contains(&"user-agent") && names.contains(&"accept-encoding"));
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicates");
    }

    #[test]
    fn should_resolve_a_relative_redirect() {
        assert_eq!(
            absolute(
                "https://www.google.com/wml/search?q=x",
                "/sorry/index?continue=y"
            ),
            "https://www.google.com/sorry/index?continue=y"
        );
    }
}
