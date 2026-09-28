//! Why a page could not be read: a small, stable taxonomy of read failures
//! ([`ReadFailure`], carried by [`ReadError`]) and the conservative
//! heuristics that recognise walls a reader must not get past.
//!
//! The reader never works around what it finds: a bot challenge, a consent
//! or cookie wall, a paywall or a login wall ends the read, and the failure
//! says which one it was (and the final URL, when known) instead of a bare
//! "no main text".
//!
//! Heuristics are deliberately narrow, so a real article that merely talks
//! about cookies, paywalls or "Just a moment..." screens is never reported
//! as a wall:
//!
//! - URL rules (always): Google's consent hosts, Google's "unusual traffic"
//!   page, and a Google News article link the browser never left.
//! - Markers only challenge pages carry (always): Cloudflare's
//!   `_cf_chl_opt`, DataDome's `captcha-delivery.com`, HUMAN's `px-captcha`;
//!   or a challenge phrase as the page `<title>`.
//! - Everything else (challenge, consent, paywall and login phrases, a
//!   paywall declared in schema.org `isAccessibleForFree`, error pages) only
//!   when the page yielded no main text and has little visible text.
//!
//! No HTML parser is needed, so `deep-crawl` (which builds without
//! `extract`) can share the detector.

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::urls;

/// Why a read failed. Serialized as its [`code`](ReadFailure::code):
/// `redirect_unresolved`, `consent_page`, `paywall`, `login_wall`,
/// `bot_challenge`, `render_failed`, `render_timeout`, `no_main_text`,
/// `blocked`, `http_<status>`, `robots`, `robots_unreachable`,
/// `fetch_error`, `unsupported_content_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadFailure {
    /// An aggregator link (Google News) was not resolved to the publisher:
    /// no browser renderer, or the browser stayed on the aggregator.
    RedirectUnresolved,
    /// A cookie or privacy consent wall (not clicked through).
    ConsentPage,
    /// A subscriber-only page (not bypassed).
    Paywall,
    /// The content is shown only to signed-in users.
    LoginWall,
    /// An anti-bot check (not bypassed).
    BotChallenge,
    /// The browser renderer failed.
    RenderFailed,
    /// The browser renderer did not finish in time.
    RenderTimeout,
    /// The page loaded but had no extractable main text.
    NoMainText,
    /// Refused by octos: SSRF protection (private or internal address) or
    /// the caller's scope.
    Blocked,
    /// The publisher answered with this HTTP status (or an error page that
    /// states it, when the renderer does not report status codes).
    Http(u16),
    /// robots.txt disallows the page (only when robots checks are on).
    Robots,
    /// robots.txt could not be fetched (only when robots checks are on).
    RobotsUnreachable,
    /// Network error before any response.
    FetchError,
    /// Not HTML, XML or text.
    UnsupportedContentType,
}

impl ReadFailure {
    /// The stable reason code.
    pub fn code(&self) -> String {
        match self {
            Self::Http(status) => format!("http_{status}"),
            other => other.fixed_code().to_string(),
        }
    }

    fn fixed_code(&self) -> &'static str {
        match self {
            Self::RedirectUnresolved => "redirect_unresolved",
            Self::ConsentPage => "consent_page",
            Self::Paywall => "paywall",
            Self::LoginWall => "login_wall",
            Self::BotChallenge => "bot_challenge",
            Self::RenderFailed => "render_failed",
            Self::RenderTimeout => "render_timeout",
            Self::NoMainText => "no_main_text",
            Self::Blocked => "blocked",
            Self::Http(_) => "http",
            Self::Robots => "robots",
            Self::RobotsUnreachable => "robots_unreachable",
            Self::FetchError => "fetch_error",
            Self::UnsupportedContentType => "unsupported_content_type",
        }
    }

    /// Parse a reason code (`bot_challenge`, `http_403`, …).
    pub fn from_code(code: &str) -> Option<Self> {
        let code = code.trim();
        if let Some(status) = code.strip_prefix("http_") {
            return status
                .parse::<u16>()
                .ok()
                .filter(|s| (100..=599).contains(s))
                .map(Self::Http);
        }
        [
            Self::RedirectUnresolved,
            Self::ConsentPage,
            Self::Paywall,
            Self::LoginWall,
            Self::BotChallenge,
            Self::RenderFailed,
            Self::RenderTimeout,
            Self::NoMainText,
            Self::Blocked,
            Self::Robots,
            Self::RobotsUnreachable,
            Self::FetchError,
            Self::UnsupportedContentType,
        ]
        .into_iter()
        .find(|f| f.fixed_code() == code)
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code())
    }
}

impl Serialize for ReadFailure {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.code())
    }
}

impl<'de> Deserialize<'de> for ReadFailure {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let code = String::deserialize(d)?;
        Self::from_code(&code)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown read failure {code:?}")))
    }
}

/// A failed read: the reason, a human-readable detail, and the URL the
/// reader or browser ended on when it is known. Displays as
/// `<code>: <detail> (final URL: <url>)`, so code that groups reasons by the
/// text before the first `:` keeps working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadError {
    pub reason: ReadFailure,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
}

impl ReadError {
    pub fn new(reason: ReadFailure, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
            final_url: None,
        }
    }

    /// Record where the read ended (ignored when empty or `about:`).
    pub fn at(mut self, final_url: impl Into<String>) -> Self {
        let u = final_url.into();
        if !u.is_empty() && !u.starts_with("about:") {
            self.final_url = Some(u);
        }
        self
    }

    /// The reason code (`bot_challenge`, `http_403`, …).
    pub fn code(&self) -> String {
        self.reason.code()
    }

    /// Classify an error message from a browser renderer. A message that
    /// starts with a reason code (`bot_challenge: …`, `render_timeout`,
    /// `http_403: …`) keeps it; otherwise SSRF refusals are `blocked`,
    /// messages naming a bot challenge are `bot_challenge`, time-outs are
    /// `render_timeout`, and anything else is `render_failed`.
    pub fn from_render_error(message: &str) -> Self {
        let lower = message.trim().to_ascii_lowercase();
        // A leading code: the whole message, or the text before its `:`.
        let lead = lower.split(':').next().unwrap_or("").trim();
        let reason = if let Some(f) = ReadFailure::from_code(lead) {
            f
        } else if lead == "ssrf_blocked" || lower.contains("ssrf") {
            ReadFailure::Blocked
        } else if lower.contains("bot challenge") || lower.contains("captcha") {
            ReadFailure::BotChallenge
        } else if lower.contains("timed out") || lower.contains("timeout") {
            ReadFailure::RenderTimeout
        } else {
            ReadFailure::RenderFailed
        };
        Self::new(reason, message.trim())
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code())?;
        if !self.detail.is_empty() {
            write!(f, ": {}", self.detail)?;
        }
        if let Some(u) = &self.final_url {
            write!(f, " (final URL: {u})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ReadError {}

impl From<ReadError> for String {
    fn from(e: ReadError) -> Self {
        e.to_string()
    }
}

/// Challenge phrases count only on pages with at most this much visible
/// text (vendors' challenge pages have a few hundred characters).
const MAX_CHALLENGE_TEXT_CHARS: usize = 1000;

/// Pages with more visible text than this are never classified from
/// phrases alone (an article that quotes "Just a moment..." stays an
/// article, and a failed extraction of it stays `no_main_text`).
const MAX_WALL_TEXT_CHARS: usize = 3000;

/// Whether `url` is a Google News article link, which reaches the
/// publisher only through a script.
pub fn is_google_news_article(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    u.host_str() == Some("news.google.com")
        && ["/rss/articles/", "/articles/", "/read/"]
            .iter()
            .any(|p| u.path().starts_with(p))
}

/// Why a fetched or rendered page is not readable, when it can be told.
///
/// `final_url` is where the page was loaded from, `requested_url` the link
/// the reader was given, `html` the document and `main_text` what
/// extraction found (`None` when it found no article). `None` means nothing
/// specific was recognised (the caller reports `no_main_text` if there is
/// no text).
pub fn diagnose(
    requested_url: &str,
    final_url: &str,
    html: &str,
    main_text: Option<&str>,
) -> Option<ReadError> {
    let host = urls::domain_of(final_url).unwrap_or_default();
    // 1. URL rules.
    if is_consent_host(&host) {
        return Some(
            ReadError::new(ReadFailure::ConsentPage, format!("redirected to {host}")).at(final_url),
        );
    }
    if is_google_sorry(final_url) {
        return Some(
            ReadError::new(
                ReadFailure::BotChallenge,
                "Google's unusual-traffic page (not bypassed)",
            )
            .at(final_url),
        );
    }
    if is_google_news_article(final_url)
        || (is_google_news_article(requested_url) && host == "news.google.com")
    {
        return Some(
            ReadError::new(
                ReadFailure::RedirectUnresolved,
                "the Google News link did not lead to the publisher",
            )
            .at(final_url),
        );
    }

    let head = head_of(html);
    let lower = head.to_ascii_lowercase();
    let title = page_title(head).to_lowercase();

    // 2. Markers only challenge pages carry.
    if let Some(what) = challenge_marker(&lower, &title) {
        return Some(
            ReadError::new(ReadFailure::BotChallenge, format!("{what} (not bypassed)"))
                .at(final_url),
        );
    }
    if let Some(main) = main_text {
        // Extraction can take a consent dialog for the article: only the
        // wall's own wording, on a page that shows the wall's buttons.
        let main = main.to_lowercase();
        let short = main.chars().filter(|c| !c.is_whitespace()).count() <= MAX_CONSENT_TEXT_CHARS;
        let wording = CONSENT_WORDING
            .iter()
            .filter(|w| main.contains(**w))
            .count();
        if short && wording >= 2 && is_consent_wall(&visible_text(head).to_lowercase()) {
            return Some(
                ReadError::new(
                    ReadFailure::ConsentPage,
                    "the page text is a consent dialog (not clicked through)",
                )
                .at(final_url),
            );
        }
        return None;
    }

    // 3. Pages with no main text and little visible text.
    let text = visible_text(head);
    if text.chars().filter(|c| !c.is_whitespace()).count() > MAX_WALL_TEXT_CHARS {
        return None;
    }
    let short = text.chars().filter(|c| !c.is_whitespace()).count() <= MAX_CHALLENGE_TEXT_CHARS;
    let text = text.to_lowercase();
    let wall =
        |reason: ReadFailure, detail: &str| Some(ReadError::new(reason, detail).at(final_url));
    if let Some(p) = CHALLENGE_PHRASES
        .iter()
        .find(|p| short && text.contains(**p))
    {
        return wall(
            ReadFailure::BotChallenge,
            &format!("the page asks to verify a human ({p:?}; not bypassed)"),
        );
    }
    if is_consent_wall(&text) {
        return wall(
            ReadFailure::ConsentPage,
            "a cookie or privacy consent wall (not clicked through)",
        );
    }
    if declares_paywall(&lower) || PAYWALL_PHRASES.iter().any(|p| text.contains(p)) {
        return wall(ReadFailure::Paywall, "subscriber-only (not bypassed)");
    }
    if LOGIN_PHRASES.iter().any(|p| text.contains(p)) {
        return wall(ReadFailure::LoginWall, "the page asks to log in");
    }
    if let Some(status) = error_page_status(&title, &text) {
        return wall(
            ReadFailure::Http(status),
            "stated by the page (the renderer reported no status)",
        );
    }
    None
}

/// Whether `html` is a bot-challenge page, judged from markers only such
/// pages carry, or a challenge phrase in its title or short text.
pub fn is_bot_challenge(html: &str) -> bool {
    let head = head_of(html);
    let lower = head.to_ascii_lowercase();
    let title = page_title(head).to_lowercase();
    if challenge_marker(&lower, &title).is_some() {
        return true;
    }
    let text = visible_text(head);
    text.chars().filter(|c| !c.is_whitespace()).count() <= MAX_CHALLENGE_TEXT_CHARS
        && CHALLENGE_PHRASES
            .iter()
            .any(|p| text.to_lowercase().contains(p))
}

/// Challenge phrases (visible text of the vendors' challenge pages).
const CHALLENGE_PHRASES: &[&str] = &[
    "performing security verification",
    "press & hold to confirm you are",
    "please verify you are a human",
    "verify you are human",
    "checking your browser",
    "just a moment...",
    "attention required! | cloudflare",
    "enable javascript and cookies to continue",
    "unusual traffic from your computer network",
];

/// Titles challenge pages use.
const CHALLENGE_TITLES: &[&str] = &[
    "just a moment...",
    "attention required! | cloudflare",
    "access to this page has been denied",
    "are you a robot?",
];

fn challenge_marker(lower_html: &str, title: &str) -> Option<&'static str> {
    if lower_html.contains("window._cf_chl_opt") || lower_html.contains("/orchestrate/chl_page/") {
        return Some("a Cloudflare challenge");
    }
    if lower_html.contains("captcha-delivery.com") {
        return Some("a DataDome challenge");
    }
    if lower_html.contains("id=\"px-captcha\"") || lower_html.contains("px-captcha") {
        return Some("a HUMAN (PerimeterX) challenge");
    }
    if CHALLENGE_TITLES.iter().any(|t| title.trim() == *t) {
        return Some("a bot-challenge page");
    }
    None
}

fn is_consent_host(host: &str) -> bool {
    [
        "consent.google.com",
        "consent.youtube.com",
        "consent.yahoo.com",
        "guce.yahoo.com",
        "guce.aol.com",
    ]
    .iter()
    .any(|h| host == *h || host.starts_with(&format!("{h}.")))
        || host.starts_with("consent.google.")
}

fn is_google_sorry(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.host_str()
            .is_some_and(|h| h == "www.google.com" || h == "google.com")
            && u.path().starts_with("/sorry/")
    })
}

/// Main text this short may be a consent dialog taken for the article.
const MAX_CONSENT_TEXT_CHARS: usize = 1500;

/// Wording consent dialogs use and articles rarely do (IAB TCF and common
/// consent-management texts).
const CONSENT_WORDING: &[&str] = &[
    "we value your privacy",
    "store and/or access information on a device",
    "personalised advertising",
    "personalized advertising",
    "our partners",
    "legitimate interest",
    "we use cookies",
    "your consent choices",
];

/// A consent wall: consent vocabulary together with the wall's own buttons.
fn is_consent_wall(text: &str) -> bool {
    let topic = ["cookie", "consent", "privacy", "personal data"]
        .iter()
        .any(|w| text.contains(w));
    let choice = [
        "accept all",
        "reject all",
        "agree and continue",
        "before you continue",
        "manage options",
        "manage preferences",
        "i agree",
    ]
    .iter()
    .filter(|w| text.contains(**w))
    .count();
    topic && choice >= 2
}

const PAYWALL_PHRASES: &[&str] = &[
    "subscribe to continue reading",
    "subscribe to read the full",
    "this article is for subscribers",
    "this content is for subscribers",
    "only available to subscribers",
    "exclusive to subscribers",
    "subscribe now to continue",
    "to continue reading, subscribe",
    "you have reached your limit of free articles",
    "you've reached your free article limit",
];

/// schema.org: a page marks paywalled content `isAccessibleForFree: false`.
fn declares_paywall(lower_html: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""isaccessibleforfree"\s*:\s*"?false"?"#).expect("regex"))
        .is_match(lower_html)
}

const LOGIN_PHRASES: &[&str] = &[
    "forgot account?",
    "log in to continue",
    "sign in to continue",
    "log in to see",
    "you must log in",
    "you must be logged in",
    "please log in to",
];

/// An error page's status: its title (`403 Forbidden`, `Error 404 Not
/// Found`), or a bare "Access denied".
fn error_page_status(title: &str, text: &str) -> Option<u16> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"^\s*(?:error\s*|http\s*)?([45]\d\d)\b").expect("regex"));
    let from = |s: &str| re.captures(s).and_then(|c| c[1].parse::<u16>().ok());
    from(title).or_else(|| from(text)).or_else(|| {
        let short = text.chars().filter(|c| !c.is_whitespace()).count() < 200;
        (short && (text.contains("access denied") || text.contains("403 forbidden"))).then_some(403)
    })
}

/// The first 512 KiB of a document (walls are small; this bounds the work
/// on huge pages).
fn head_of(html: &str) -> &str {
    let mut end = html.len().min(512 * 1024);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    &html[..end]
}

fn page_title(html: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("regex"))
        .captures(html)
        .map(|c| decode_entities(c[1].trim()))
        .unwrap_or_default()
}

/// Visible text, roughly: scripts, styles and the head dropped, tags
/// removed, whitespace collapsed.
pub fn visible_text(html: &str) -> String {
    static DROP: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    let drop = DROP.get_or_init(|| {
        Regex::new(
            r"(?is)<(script|style|template|svg|head)\b.*?</(script|style|template|svg|head)\s*>",
        )
        .expect("regex")
    });
    let tag = TAG.get_or_init(|| Regex::new(r"(?s)<[^>]*>").expect("regex"));
    let without = drop.replace_all(html, " ");
    let text = tag.replace_all(&without, " ");
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/read/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    const ARTICLE: &str = "https://example-news.com/2026/09/27/story.html";
    const GN: &str = "https://news.google.com/rss/articles/CBMiW0FVX3lxTE9j?oc=5";

    fn reason(name: &str, final_url: &str) -> Option<ReadFailure> {
        diagnose(ARTICLE, final_url, &fixture(name), None).map(|e| e.reason)
    }

    #[test]
    fn should_report_bot_challenge_when_cloudflare_or_datadome_answers() {
        assert_eq!(
            reason("cloudflare_challenge.html", ARTICLE),
            Some(ReadFailure::BotChallenge)
        );
        // DataDome's text is inside an iframe: the rendered body is empty,
        // so only the markup tells.
        let e = diagnose(ARTICLE, ARTICLE, &fixture("datadome_challenge.html"), None).unwrap();
        assert_eq!(e.reason, ReadFailure::BotChallenge);
        assert!(e.detail.contains("DataDome") && e.detail.contains("not bypassed"));
        assert_eq!(e.final_url.as_deref(), Some(ARTICLE));
    }

    #[test]
    fn should_report_bot_challenge_when_marker_present_even_if_text_was_extracted() {
        // A fallback text extractor can turn a challenge page into 200+
        // characters; the markers still win.
        assert_eq!(
            diagnose(
                ARTICLE,
                ARTICLE,
                &fixture("cloudflare_challenge.html"),
                Some("Performing security verification")
            )
            .map(|e| e.reason),
            Some(ReadFailure::BotChallenge)
        );
        assert!(is_bot_challenge(&fixture("datadome_challenge.html")));
    }

    #[test]
    fn should_report_consent_page_when_redirected_to_a_consent_host() {
        let e = diagnose(
            GN,
            "https://consent.google.com/ml?continue=https://news.google.com/rss/articles/x",
            &fixture("google_consent.html"),
            Some("We use cookies and data to deliver and maintain Google services"),
        )
        .unwrap();
        assert_eq!(e.reason, ReadFailure::ConsentPage);
        assert!(
            e.final_url
                .unwrap()
                .starts_with("https://consent.google.com/")
        );
        assert_eq!(
            reason("cookie_wall.html", "https://guce.yahoo.com/consent?x=1"),
            Some(ReadFailure::ConsentPage)
        );
    }

    #[test]
    fn should_report_consent_page_when_only_a_cookie_wall_is_shown() {
        assert_eq!(
            reason("cookie_wall.html", ARTICLE),
            Some(ReadFailure::ConsentPage)
        );
    }

    #[test]
    fn should_report_consent_page_when_extraction_took_the_dialog_for_the_article() {
        let html = fixture("cookie_wall.html");
        let dialog = "We value your privacy. We and our 842 partners store and/or access \
                      information on a device, such as cookies, and process personal data \
                      for personalised advertising and content.";
        assert_eq!(
            diagnose(ARTICLE, ARTICLE, &html, Some(dialog)).map(|e| e.reason),
            Some(ReadFailure::ConsentPage)
        );
    }

    #[test]
    fn should_report_paywall_when_page_declares_subscriber_only_content() {
        let e = diagnose(ARTICLE, ARTICLE, &fixture("paywall.html"), None).unwrap();
        assert_eq!(e.reason, ReadFailure::Paywall);
        assert!(e.detail.contains("not bypassed"));
    }

    #[test]
    fn should_report_login_wall_when_page_only_offers_log_in() {
        assert_eq!(
            reason(
                "login_wall.html",
                "https://www.example-social.com/civil/posts/1"
            ),
            Some(ReadFailure::LoginWall)
        );
    }

    #[test]
    fn should_report_http_status_when_page_is_an_error_page() {
        assert_eq!(
            reason("error_403_openresty.html", ARTICLE),
            Some(ReadFailure::Http(403))
        );
        assert_eq!(
            reason("access_denied_json.html", ARTICLE),
            Some(ReadFailure::Http(403))
        );
    }

    #[test]
    fn should_report_redirect_unresolved_when_browser_stays_on_google_news() {
        let final_url = "https://news.google.com/rss/articles/CBMiW0FVX3lxTE9j?oc=5&hl=en-US";
        let e = diagnose(
            GN,
            final_url,
            &fixture("google_news_interstitial.html"),
            None,
        )
        .unwrap();
        assert_eq!(e.reason, ReadFailure::RedirectUnresolved);
        assert_eq!(e.final_url.as_deref(), Some(final_url));
        // Even when the interstitial yields "text", it is not the article.
        assert_eq!(
            diagnose(
                GN,
                "https://news.google.com/",
                "<p>Top stories</p>",
                Some("Top stories")
            )
            .map(|e| e.reason),
            Some(ReadFailure::RedirectUnresolved)
        );
    }

    #[test]
    fn should_report_bot_challenge_when_google_shows_its_unusual_traffic_page() {
        assert_eq!(
            diagnose(
                GN,
                "https://www.google.com/sorry/index?continue=x",
                "",
                None
            )
            .map(|e| e.reason),
            Some(ReadFailure::BotChallenge)
        );
    }

    #[test]
    fn should_not_flag_article_when_it_mentions_walls_and_has_a_cookie_banner() {
        let html = fixture("article_mentions_walls.html");
        let text = crate::access::visible_text(&html);
        assert_eq!(diagnose(ARTICLE, ARTICLE, &html, Some(&text)), None);
        assert!(!is_bot_challenge(&html));
        // Even if extraction had failed, a page this long is not called a
        // wall from phrases alone.
        let more = "<p>More reporting follows here.</p>".repeat(120);
        let long = html.replace("</article>", &format!("{more}</article>"));
        assert_eq!(diagnose(ARTICLE, ARTICLE, &long, None), None);
    }

    #[test]
    fn should_report_nothing_specific_when_page_is_merely_empty() {
        assert_eq!(
            diagnose(
                ARTICLE,
                ARTICLE,
                "<html><body><div id=app></div></body></html>",
                None
            ),
            None
        );
    }

    #[test]
    fn should_display_code_detail_and_final_url() {
        let e = ReadError::new(
            ReadFailure::BotChallenge,
            "a DataDome challenge (not bypassed)",
        )
        .at("https://www.reuters.com/world/x");
        assert_eq!(
            e.to_string(),
            "bot_challenge: a DataDome challenge (not bypassed) (final URL: https://www.reuters.com/world/x)"
        );
        assert_eq!(
            ReadError::new(ReadFailure::Http(404), "").to_string(),
            "http_404"
        );
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["reason"], "bot_challenge");
        assert_eq!(json["final_url"], "https://www.reuters.com/world/x");
        let back: ReadError = serde_json::from_value(json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn should_round_trip_every_reason_code() {
        for f in [
            ReadFailure::RedirectUnresolved,
            ReadFailure::ConsentPage,
            ReadFailure::Paywall,
            ReadFailure::LoginWall,
            ReadFailure::BotChallenge,
            ReadFailure::RenderFailed,
            ReadFailure::RenderTimeout,
            ReadFailure::NoMainText,
            ReadFailure::Blocked,
            ReadFailure::Http(451),
            ReadFailure::Robots,
            ReadFailure::RobotsUnreachable,
            ReadFailure::FetchError,
            ReadFailure::UnsupportedContentType,
        ] {
            assert_eq!(ReadFailure::from_code(&f.code()), Some(f), "{f}");
        }
        assert_eq!(ReadFailure::from_code("http_9999"), None);
        assert_eq!(ReadFailure::from_code("nonsense"), None);
    }

    #[test]
    fn should_classify_render_errors_when_renderer_reports_a_string() {
        let r = |m: &str| ReadError::from_render_error(m).reason;
        assert_eq!(
            r("blocked by a bot challenge (not bypassed)"),
            ReadFailure::BotChallenge
        );
        assert_eq!(
            r("ssrf_blocked: the browser went to http://10.0.0.1/"),
            ReadFailure::Blocked
        );
        assert_eq!(r("render timed out after 30s"), ReadFailure::RenderTimeout);
        assert_eq!(
            r("consent_page: consent.google.com"),
            ReadFailure::ConsentPage
        );
        assert_eq!(r("http_451"), ReadFailure::Http(451));
        assert_eq!(
            r("navigation failed: net::ERR_NAME_NOT_RESOLVED"),
            ReadFailure::RenderFailed
        );
        assert_eq!(r("deep_crawl binary not found"), ReadFailure::RenderFailed);
    }

    #[test]
    fn should_recognise_google_news_article_links_only() {
        assert!(is_google_news_article(GN));
        assert!(is_google_news_article(
            "https://news.google.com/articles/CBMi?hl=en"
        ));
        assert!(!is_google_news_article("https://news.google.com/"));
        assert!(!is_google_news_article(
            "https://news.google.com.example.org/rss/articles/x"
        ));
    }
}
