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
//! - Extracted main text that is not an article: a consent dialog (mostly
//!   consent wording, with three of the dialog's own phrases unquoted), an
//!   embedded player's consent prompt with no article text left
//!   (`consent_page`), or boilerplate, a playlist or a video caption with
//!   less than a few sentences of its own (`stub_page`).
//!
//! No HTML parser is needed, so `deep-crawl` (which builds without
//! `extract`) can share the detector.

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::urls;

/// Why a read failed. Serialized as its [`code`](ReadFailure::code):
/// `redirect_unresolved`, `consent_page`, `stub_page`, `paywall`, `login_wall`,
/// `bot_challenge`, `render_failed`, `render_timeout`, `no_main_text`,
/// `blocked`, `http_<status>`, `robots`, `robots_unreachable`,
/// `fetch_error`, `unsupported_content_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadFailure {
    /// An aggregator link (Google News) was not resolved to the publisher:
    /// no browser renderer, or the browser stayed on the aggregator.
    RedirectUnresolved,
    /// A cookie or privacy consent wall (not clicked through), including a
    /// consent dialog or an embedded player's consent prompt that extraction
    /// took for the article.
    ConsentPage,
    /// The page loaded, but its text is only a stub: a video page's player
    /// and caption, a playlist, or boilerplate with no article text.
    StubPage,
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
            Self::StubPage => "stub_page",
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
            Self::StubPage,
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
        return judge_main_text(&main, final_url, head).map(|e| e.at(final_url));
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
    // Results pages (#2607): DuckDuckGo's HTML-endpoint check and Bing's.
    "unfortunately, bots use duckduckgo too",
    "please solve the challenge below to continue",
    // Chinese sites' WAF pages (Volcano Engine, Alibaba Cloud, Tencent
    // Cloud and similar): "running a security check", "checking the current
    // network environment", "human verification", "complete the security
    // check", "slide to verify".
    "正在进行安全检测",
    "正在检测当前网络环境",
    "人机验证",
    "请完成安全验证",
    "滑动验证",
    "拖动滑块",
];

/// Phrases of challenge pages that clear themselves after a few seconds in
/// a real browser (a JavaScript check, no person needed). A renderer that
/// meets one waits and reads the page again ([`is_interstitial`]).
const INTERSTITIAL_PHRASES: &[&str] = &[
    "just a moment...",
    "checking your browser",
    "performing security verification",
    "enable javascript and cookies to continue",
    "正在进行安全检测",
    "正在检测当前网络环境",
];

/// Whether `html` is a challenge page that usually clears itself in a real
/// browser within seconds ("Just a moment…", "正在进行安全检测…"). Renderers
/// wait and read again instead of reporting it at once; one that asks a
/// person (a CAPTCHA, a slider) is not an interstitial.
pub fn is_interstitial(html: &str) -> bool {
    if !is_bot_challenge(html) {
        return false;
    }
    let head = head_of(html);
    let text = visible_text(head);
    let lower = text.to_lowercase();
    let title = page_title(head).to_lowercase();
    !asks_person(&lower)
        && INTERSTITIAL_PHRASES
            .iter()
            .any(|p| title.contains(p) || lower.contains(p))
}

/// Whether a page's visible text (short, as challenge pages are) reads as a
/// bot challenge. For renderers that see text rather than HTML.
pub fn challenge_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    text.chars().filter(|c| !c.is_whitespace()).count() <= MAX_CHALLENGE_TEXT_CHARS
        && CHALLENGE_PHRASES.iter().any(|p| lower.contains(p))
}

/// Whether a page's visible text reads as a challenge that clears itself in
/// a real browser (see [`is_interstitial`]).
pub fn interstitial_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    challenge_text(text)
        && !asks_person(&lower)
        && INTERSTITIAL_PHRASES.iter().any(|p| lower.contains(p))
}

/// Challenge wording that asks a person to act (never done for them).
fn asks_person(lower: &str) -> bool {
    [
        "captcha",
        "人机验证",
        "滑动验证",
        "拖动滑块",
        "请完成安全验证",
        "press & hold",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

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
    if lower_html.contains("anomaly-modal") || lower_html.contains("duckduckgo.com/anomaly.js") {
        return Some("a DuckDuckGo bot check");
    }
    if lower_html.contains("bing.com/turing/captcha") || lower_html.contains("\"/turing/captcha") {
        return Some("a Bing challenge");
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

/// Main text with less article text than this (in [`text_weight`] units:
/// characters, CJK counted three times) is not an article.
const MIN_PROSE_WEIGHT: usize = 150;

/// A video page's text must carry at least this much article text to be
/// more than its caption.
const MIN_VIDEO_PAGE_PROSE_WEIGHT: usize = 400;

/// Share (in tenths) of the main text in consent lines at which, with
/// [`MIN_DIALOG_MARKERS`] of the dialog's own phrases, the text is a
/// consent dialog.
const DIALOG_SHARE_TENTHS: usize = 6;
const MIN_DIALOG_MARKERS: usize = 3;

/// Words that put a line in a consent dialog, in the languages the reader
/// meets most. Lowercase.
const CONSENT_VOCABULARY: &[&str] = &[
    "cookie",
    "consent",
    "privacy",
    "personal data",
    "personal information",
    "tracking",
    "opt out",
    "opt-out",
    "advertising partners",
    "einwilligung",
    "datenschutz",
    "werbepartner",
    "consentement",
    "données personnelles",
    "vie privée",
    "suivi publicitaire",
    "consentimiento",
    "privacidad",
    "datos personales",
    "consenso",
    "dati personali",
    "toestemming",
    "consentimento",
    "隐私",
    "隱私",
    "同意",
    "个人信息",
    "個人資料",
    "クッキー",
    "쿠키",
    "개인정보",
];

/// Phrases consent dialogs say about themselves, or their buttons.
/// Lowercase. Counted only when not quoted, so an article that reports on
/// "Accept all" buttons is not a dialog.
const DIALOG_MARKERS: &[&str] = &[
    "accept all",
    "reject all",
    "allow all",
    "deny all",
    "decline all",
    "accept cookies",
    "cookie settings",
    "cookie preferences",
    "manage preferences",
    "manage options",
    "manage my choices",
    "manage consent",
    "privacy preference center",
    "strictly necessary",
    "always active",
    "performance cookies",
    "targeting cookies",
    "functional cookies",
    "analytics cookies",
    "advertising cookies",
    "these cookies",
    "we use cookies",
    "we store cookies",
    "this site uses cookies",
    "this website uses cookies",
    "store and/or access information on a device",
    "your consent choices",
    "we value your privacy",
    "before you continue",
    "sale of personal data",
    "more options",
    // de
    "alle akzeptieren",
    "alle ablehnen",
    "einstellungen verwalten",
    "wir verwenden cookies",
    "diese cookies",
    "notwendige cookies",
    // fr
    "tout accepter",
    "tout refuser",
    "nous utilisons des cookies",
    "gérer mes choix",
    "paramétrer les cookies",
    // es, pt
    "aceptar todo",
    "rechazar todo",
    "utilizamos cookies",
    "usamos cookies",
    "aceitar tudo",
    "rejeitar tudo",
    // it, nl
    "accetta tutto",
    "rifiuta tutto",
    "utilizziamo i cookie",
    "alles accepteren",
    "alles weigeren",
    "wij gebruiken cookies",
    // zh, ja, ko
    "接受全部",
    "全部接受",
    "接受所有",
    "拒绝全部",
    "全部拒绝",
    "拒绝所有",
    "拒絕全部",
    "我们使用cookie",
    "我們使用cookie",
    "管理偏好",
    "这些cookie",
    "這些cookie",
    "すべて同意",
    "すべて拒否",
    "모두 동의",
    "모두 거부",
];

/// An embedded player's consent prompt ("to display this content from
/// YouTube, you must enable advertisement tracking"). Lowercase.
const EMBED_CONSENT_PHRASES: &[&str] = &[
    "to display this content from",
    "you must enable advertisement tracking",
    "before you continue to youtube",
    "accept cookies to view",
    "accept cookies to watch",
    "pour afficher ce contenu",
    "vous devez activer le suivi publicitaire",
    "bevor sie zu youtube weitergehen",
    "um diesen inhalt anzuzeigen",
    "para mostrar este contenido",
    "per visualizzare questo contenuto",
];

/// Player chrome that extraction keeps as text. Lowercase.
const PLAYER_PHRASES: &[&str] = &[
    "video player",
    "to watch this content",
    "play video",
    "your browser does not support the video",
    "ie 11 is not supported",
    "for an optimal experience visit",
    "lecteur vidéo",
    "regarder ce contenu",
    "视频播放器",
];

/// Short lines that are page furniture, by how they start. Lowercase.
const FURNITURE_STARTS: &[&str] = &[
    "share",
    "read more",
    "read less",
    "advertisement",
    "up next",
    "now playing",
    "subscribe",
    "sign up",
    "follow us",
    "issued on",
    "published",
    "posted",
    "updated",
    "last updated",
    "cover image",
    "image:",
    "photo:",
    "credit:",
    "©",
    "copyright",
    "partager",
    "publié le",
    "image de couverture",
    "分享",
    "发表时间",
    "發表時間",
    "来源：",
    "來源：",
    "责任编辑",
    "責任編輯",
    "广告",
    "廣告",
];

/// What the sentences of an extracted text are.
#[derive(Debug, Default)]
struct LineMix {
    /// Weight of every sentence.
    total: usize,
    /// Weight of sentences in consent vocabulary.
    consent: usize,
    /// Weight of sentences that are neither consent, player, running times
    /// nor furniture.
    prose: usize,
    /// Sentences, and those that are only a running time ("01:34").
    lines: usize,
    running_times: usize,
    /// Lines long enough to be a paragraph that hold article text.
    paragraphs: usize,
}

/// Characters that are not whitespace, with CJK characters counted three
/// times (a CJK character carries about as much as a short word).
fn text_weight(s: &str) -> usize {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if crate::text::is_cjk(c) { 3 } else { 1 })
        .sum()
}

fn is_running_time(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,2}(?::\d{2}){1,2}$").expect("regex"))
        .is_match(line)
}

fn is_furniture(line: &str) -> bool {
    let words = line.split_whitespace().count();
    words <= 8
        && line.chars().count() <= 60
        && (FURNITURE_STARTS.iter().any(|s| line.starts_with(s)) || is_date_line(line))
}

/// A short line that is mostly a date ("28/09/2026 - 10:55").
fn is_date_line(line: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d{1,4}[/.\-年]\d{1,2}[/.\-月]\d{1,4}").expect("regex"))
        .is_match(line)
}

/// Classify the sentences of lowercase `text` (each line split after
/// `.`, `!`, `?` or their CJK forms). Consent sentences count as prose
/// unless `dialog` (the text shows a dialog's own phrases): a short article
/// about privacy stays an article.
fn line_mix(text: &str, dialog: bool) -> LineMix {
    let mut m = LineMix::default();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let cjk = line.chars().filter(|c| crate::text::is_cjk(*c)).count();
        let paragraph = line.split_whitespace().count() >= 25 || cjk >= 60;
        let mut prose_line = false;
        for seg in sentences(line) {
            let w = text_weight(seg);
            m.total += w;
            m.lines += 1;
            if is_running_time(seg) {
                m.running_times += 1;
                continue;
            }
            if CONSENT_VOCABULARY.iter().any(|v| seg.contains(v)) {
                m.consent += w;
                if dialog {
                    continue;
                }
            }
            if PLAYER_PHRASES.iter().any(|p| seg.contains(p)) || is_furniture(seg) {
                continue;
            }
            m.prose += w;
            prose_line = true;
        }
        if paragraph && prose_line {
            m.paragraphs += 1;
        }
    }
    m
}

/// A line's sentences, split after sentence-ending punctuation.
fn sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = i + c.len_utf8();
        let split = match c {
            '。' | '！' | '？' => true,
            '.' | '!' | '?' => chars.peek().is_some_and(|(_, n)| n.is_whitespace()),
            _ => false,
        };
        if split {
            let seg = line[start..end].trim();
            if !seg.is_empty() {
                out.push(seg);
            }
            start = end;
        }
    }
    let rest = line[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// Distinct dialog phrases in lowercase `text` that are not quoted.
fn dialog_markers(text: &str) -> usize {
    const QUOTES: &[char] = &['"', '\'', '“', '‘', '«', '„', '「', '『'];
    DIALOG_MARKERS
        .iter()
        .filter(|m| {
            text.match_indices(**m).any(|(i, _)| {
                !text[..i]
                    .chars()
                    .next_back()
                    .is_some_and(|c| QUOTES.contains(&c))
            })
        })
        .count()
}

/// A video page: `og:type` video, or a `video`/`videos`/`watch` path.
fn is_video_page(url: &str, html: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let og = RE.get_or_init(|| {
        Regex::new(
            r#"(?i)<meta[^>]+(?:property=["']og:type["'][^>]*content=["']video|content=["']video[^"']*["'][^>]*property=["']og:type["'])"#,
        )
        .expect("regex")
    });
    let path = url::Url::parse(url).ok().is_some_and(|u| {
        u.path_segments()
            .is_some_and(|mut s| s.any(|p| matches!(p, "video" | "videos" | "watch")))
    });
    path || og.is_match(html)
}

/// Whether extracted main text (lowercase) is a page's stub rather than an
/// article: a consent dialog or an embedded player's consent prompt
/// (`consent_page`), or a player, playlist or boilerplate with no article
/// text (`stub_page`). Conservative: a text with a few sentences of its own
/// is an article, whatever else the page shows.
fn judge_main_text(text: &str, url: &str, html: &str) -> Option<ReadError> {
    let markers = dialog_markers(text);
    let embed = EMBED_CONSENT_PHRASES.iter().any(|p| text.contains(p));
    let mix = line_mix(text, markers >= MIN_DIALOG_MARKERS || embed);
    if mix.total == 0 {
        return None;
    }
    if markers >= MIN_DIALOG_MARKERS && mix.consent * 10 >= mix.total * DIALOG_SHARE_TENTHS {
        return Some(ReadError::new(
            ReadFailure::ConsentPage,
            "the page text is a cookie or privacy dialog (not clicked through)",
        ));
    }
    if embed && mix.prose < MIN_PROSE_WEIGHT {
        return Some(ReadError::new(
            ReadFailure::ConsentPage,
            "an embedded player behind a consent prompt, with no article text (not clicked through)",
        ));
    }
    if mix.prose < MIN_PROSE_WEIGHT {
        return Some(ReadError::new(
            ReadFailure::StubPage,
            "the page text is boilerplate, with no article text",
        ));
    }
    if mix.running_times >= 3 && mix.running_times * 4 >= mix.lines && mix.paragraphs == 0 {
        return Some(ReadError::new(
            ReadFailure::StubPage,
            "a video playlist (titles and running times), with no article text",
        ));
    }
    if mix.prose < MIN_VIDEO_PAGE_PROSE_WEIGHT && is_video_page(url, html) {
        return Some(ReadError::new(
            ReadFailure::StubPage,
            "a video page with only its player and caption",
        ));
    }
    None
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

    fn serp_fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/serp/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn should_report_bot_challenge_when_a_results_page_answers_with_one() {
        // #2607: results-page search treats these as a miss, never solves them.
        assert!(is_bot_challenge(&serp_fixture("ddg_anomaly.html")));
        assert!(is_bot_challenge(&serp_fixture("bing_challenge.html")));
        // The rendered-text form deep-search's Bing path reads.
        assert!(is_bot_challenge(&serp_fixture("bing_challenge.txt")));
    }

    #[test]
    fn should_not_take_a_results_page_about_bot_checks_for_a_challenge() {
        // Results for "anomaly detection" or about DuckDuckGo's bot check:
        // the words appear, the challenge markup does not, and the page has
        // far more text than a challenge.
        let row = |i: usize| {
            format!(
                "<div class=\"result\"><a class=\"result__a\" href=\"https://example.org/{i}\">Anomaly detection {i}</a>\
                 <a class=\"result__snippet\">Unfortunately, bots use DuckDuckGo too, one post says; \
                 others explain isolation forests and time-series anomaly scores in depth.</a></div>"
            )
        };
        let page = format!(
            "<html><head><title>anomaly detection at DuckDuckGo</title></head><body>{}</body></html>",
            (0..10).map(row).collect::<String>()
        );
        assert!(!is_bot_challenge(&page));
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

    /// What `diagnose` says about extracted `text` (no HTML).
    fn text_reason(url: &str, text: &str) -> Option<ReadFailure> {
        diagnose(url, url, "", Some(text)).map(|e| e.reason)
    }

    const F24_VIDEO: &str = "https://www.france24.com/en/video/20260928-the-last-thing-you-expect-locals-near-uk-airbase-react-to-terrorist-act-arrests";
    const WRAL_VIDEO: &str = "https://www.wral.com/video/trump-rejects-iran-proposal-reopen-strait-hormuz-september-2026/";

    #[test]
    fn should_report_consent_page_when_the_text_is_a_video_consent_prompt() {
        // France 24, read in both EU AI Act runs of validation 3: 320
        // characters, the YouTube consent prompt and player boilerplate.
        let text = fixture("france24_video_consent.txt");
        let e = diagnose(F24_VIDEO, F24_VIDEO, "", Some(&text)).unwrap();
        assert_eq!(e.reason, ReadFailure::ConsentPage, "{e}");
        assert!(e.detail.contains("not clicked through"), "{e}");
        assert_eq!(e.final_url.as_deref(), Some(F24_VIDEO));
        // Not because of the URL: the same text anywhere is a prompt.
        assert_eq!(text_reason(ARTICLE, &text), Some(ReadFailure::ConsentPage));
    }

    #[test]
    fn should_report_consent_page_when_the_text_is_a_cookie_preference_center() {
        // WRAL, validation 3 (Strait of Hormuz): 3,395 characters of a
        // OneTrust preference centre taken for the article.
        let text = fixture("wral_video_cookie_dialog.txt");
        assert!(text.chars().count() > MAX_CONSENT_TEXT_CHARS);
        assert_eq!(
            text_reason(WRAL_VIDEO, &text),
            Some(ReadFailure::ConsentPage)
        );
        assert_eq!(text_reason(ARTICLE, &text), Some(ReadFailure::ConsentPage));
    }

    #[test]
    fn should_report_consent_page_when_the_dialog_is_in_another_language() {
        let de = "Datenschutzeinstellungen\n\nWir verwenden Cookies und ähnliche \
                  Technologien, um Inhalte zu personalisieren und Zugriffe zu analysieren. \
                  Mit „Alle akzeptieren“ stimmen Sie der Verwendung zu.\n\nAlle akzeptieren\n\n\
                  Alle ablehnen\n\nEinstellungen verwalten\n\nNotwendige Cookies\n\nDiese \
                  Cookies sind für den Betrieb der Website erforderlich und können nicht \
                  deaktiviert werden.\n\nMarketing-Cookies\n\nDiese Cookies werden von \
                  Werbepartnern gesetzt, um ein Profil Ihrer Interessen zu erstellen.";
        assert_eq!(text_reason(ARTICLE, de), Some(ReadFailure::ConsentPage));
        let fr = "Pour afficher ce contenu YouTube, vous devez activer le suivi \
                  publicitaire et la mesure d'audience.\n\nUne de vos extensions de \
                  navigateur semble bloquer le chargement du lecteur vidéo. Pour pouvoir \
                  regarder ce contenu, vous devez la désactiver ou la désinstaller.\n\n\
                  Image de couverture : © France 24\n02:13\n\nPublié le : 28/09/2026 - 10:55\n\n\
                  Partager";
        assert_eq!(text_reason(ARTICLE, fr), Some(ReadFailure::ConsentPage));
        let zh = "隐私设置\n\n我们使用Cookie来改善您的浏览体验、提供个性化内容并分析网站流量。\
                  点击“接受全部”即表示您同意我们使用Cookie。\n\n接受全部\n\n拒绝全部\n\n\
                  管理偏好设置\n\n必要Cookie\n\n这些Cookie是网站正常运行所必需的，无法关闭。\n\n\
                  广告Cookie\n\n这些Cookie由我们的广告合作伙伴设置，用于建立您的兴趣档案。";
        assert_eq!(text_reason(ARTICLE, zh), Some(ReadFailure::ConsentPage));
        let yt = "Before you continue to YouTube\n\nWe use cookies and data to deliver and \
                  maintain Google services, track outages and protect against spam, fraud \
                  and abuse.\n\nIf you choose to 'Accept all', we will also use cookies and \
                  data to develop and improve new services.\n\nReject all\n\nAccept all\n\n\
                  More options";
        assert_eq!(text_reason(ARTICLE, yt), Some(ReadFailure::ConsentPage));
    }

    #[test]
    fn should_report_stub_page_when_the_text_is_a_video_playlist() {
        // NBC News, validation 3: a video page whose "article" is the
        // playlist (titles and running times).
        let text = fixture("nbc_video_playlist.txt");
        let e = diagnose(ARTICLE, ARTICLE, "", Some(&text)).unwrap();
        assert_eq!(e.reason, ReadFailure::StubPage, "{e}");
        assert_eq!(e.code(), "stub_page");
    }

    #[test]
    fn should_report_stub_page_when_a_video_page_has_only_its_caption() {
        let caption = "Video Player is loading.\n\nPresident Trump rejects Iran proposal to \
                       reopen Strait of Hormuz\n\nU.S. President Donald Trump has rejected \
                       Iran’s proposal to reopen the Strait of Hormuz.\n\nPosted 9/28/2026, \
                       9:52:11 AM\n\n© WRAL\n\nAdvertisement";
        assert_eq!(
            text_reason(WRAL_VIDEO, caption),
            Some(ReadFailure::StubPage)
        );
        // Declared a video page by og:type, on any path.
        let html = r#"<html><head><meta property="og:type" content="video.other"></head><body></body></html>"#;
        assert_eq!(
            diagnose(ARTICLE, ARTICLE, html, Some(caption)).map(|e| e.reason),
            Some(ReadFailure::StubPage)
        );
        // Boilerplate with no article text is a stub on any page.
        let boiler = "Share\n\nRead more\n\nAdvertisement\n\n01:34\n\nIssued on: 28/09/2026 - \
                      10:55\n\nCover image: © Example\n\nUp next\n\nNow playing\n\n02:10\n\n\
                      Updated 28 September 2026\n\nSubscribe\n\nSign up for newsletters";
        assert_eq!(text_reason(ARTICLE, boiler), Some(ReadFailure::StubPage));
    }

    #[test]
    fn should_not_flag_short_real_articles_when_their_text_is_brief() {
        // Real short reads from validation 3 that were articles.
        for name in [
            "short_article_zh.txt",       // chinanews, 283 characters
            "short_lines_article_zh.txt", // 东南网, one short line per phrase
            "short_article_en.txt",       // Anadolu, 1,443 characters
        ] {
            let text = fixture(name);
            assert_eq!(text_reason(ARTICLE, &text), None, "{name}");
        }
        let kyodo = "【共同社9月28日电】今年第26号超强台风“舒力基”28日预计将在维持现有强度的同时接近冲绳和\
                     奄美地区。日本气象厅提醒警惕伴有涌浪的大浪和强风。因台风路径，风力恐将进一步增强。";
        assert_eq!(text_reason(ARTICLE, kyodo), None);
        let brief = "Officials said the bridge would reopen on Tuesday after repairs to a \
                     damaged expansion joint were finished ahead of schedule.\n\nTraffic had \
                     been diverted through the city centre for nine days, adding up to 40 \
                     minutes to some journeys.";
        assert_eq!(text_reason(ARTICLE, brief), None);
        // A short article on a video page keeps its text when it has one.
        let story = format!("{brief}\n\n{brief}\n\n{brief}");
        assert_eq!(text_reason(WRAL_VIDEO, &story), None);
    }

    #[test]
    fn should_not_flag_article_when_it_reports_on_cookie_banners() {
        let text = "Regulators in Brussels said on Monday that websites must make it as easy \
                    to refuse cookies as to accept them, after a two-year review of consent \
                    banners.\n\nThe review found that many sites showed an \"Accept all\" button \
                    on the first screen but hid \"Reject all\" behind a second page of settings. \
                    Officials said that design nudged people into sharing personal data.\n\n\
                    \"People should not have to hunt for the privacy option,\" the commissioner \
                    told reporters. Sites have six months to comply.\n\nIndustry groups said the \
                    rules on strictly necessary cookies were still unclear and asked for \
                    guidance on analytics.";
        assert_eq!(text_reason(ARTICLE, text), None);
        // And a live blog's timestamps are not a playlist.
        let live = "10:32\n\nThe prime minister has arrived at the summit venue, where leaders \
                    are expected to discuss energy prices and the reopening of the strait over \
                    the next two days.\n\n10:05\n\nOil prices rose 2% in early trading as \
                    markets waited for news from the talks, with analysts warning of further \
                    volatility.\n\n09:41\n\nSecurity has been tightened around the city centre, \
                    police said.";
        assert_eq!(text_reason(ARTICLE, live), None);
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
            ReadFailure::StubPage,
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

    #[test]
    fn should_recognise_chinese_challenge_pages_and_which_clear_themselves() {
        let volc = serp_or_read_fixture("read/volcengine_security_check.html");
        assert!(
            is_bot_challenge(&volc),
            "Volcano Engine WAF check is a challenge"
        );
        assert!(is_interstitial(&volc), "and it clears itself in a browser");
        let slider = serp_or_read_fixture("read/slider_captcha_zh.html");
        assert!(is_bot_challenge(&slider));
        assert!(!is_interstitial(&slider), "a slider asks a person");
        let cf = serp_or_read_fixture("read/cloudflare_challenge.html");
        assert!(is_bot_challenge(&cf));
    }

    fn serp_or_read_fixture(rel: &str) -> String {
        let path = format!("{}/tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }
}
