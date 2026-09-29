//! Standalone deep_crawl skill binary.
//!
//! Reads JSON input from stdin, launches headless Chrome via CDP,
//! performs BFS crawl, extracts rendered text, saves results to disk,
//! and writes JSON output to stdout.

use std::collections::{HashSet, VecDeque};
use std::io::Read as _;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

// ---------------------------------------------------------------------------
// Cancellation (W3.D1)
// ---------------------------------------------------------------------------
//
// Plugin protocol v2 contract: on SIGTERM the host gives us 10s to clean up
// before SIGKILL. We track an in-process atomic that is set by the SIGTERM
// handler, and the BFS loop checks it at every iteration. The Chrome child
// pid is tracked so the handler can kill it directly even if the main task
// is blocked in a CDP call.

/// Set to 1 by the SIGTERM handler; the BFS loop and synchronous
/// CDP-sender code paths poll this and exit early when set.
static CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Pid of the Chrome child we launched. The handler kills this directly
/// so chromium does not linger after the main task winds down.
/// 0 means "no child to kill".
static CHROME_PID: AtomicI32 = AtomicI32::new(0);

fn cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAX_OUTPUT_CHARS: usize = 50_000;
const PAGE_SETTLE_MS: u64 = 3000;
const PAGE_SETTLE_RETRY_MS: u64 = 5000;
const NAV_TIMEOUT_SECS: u64 = 30;
const MAX_PAGE_TEXT_CHARS: usize = 200_000;
const PREVIEW_CHARS: usize = 2000;
const MIN_USEFUL_TEXT_LEN: usize = 200;
const MAX_EMPTY_RETRIES: u32 = 2;
const CDP_CONNECT_TIMEOUT_SECS: u64 = 15;
const DEFAULT_MAX_DEPTH: u32 = 3;
const DEFAULT_MAX_PAGES: u32 = 50;

/// Two-second waits for a self-clearing challenge page (see
/// `octos_research::access::interstitial_text`).
const INTERSTITIAL_WAITS: u32 = 5;

/// Largest rendered HTML returned per page when `include_html` is set.
const MAX_PAGE_HTML_BYTES: usize = 2 * 1024 * 1024;
/// Cap on a robots.txt `Crawl-delay` we will honour between pages.
const MAX_CRAWL_DELAY_SECS: u64 = 10;

/// Product token appended to the browser's own User-Agent so sites can tell
/// this is an automated octos reader (policy: no disguised automation; see
/// SKILL.md "Automation policy").
const UA_SUFFIX: &str = "octos-research/1.0 (+https://github.com/octos-org/octos)";

/// Environment variables to block when launching Chrome.
const BLOCKED_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    "DYLD_VERSIONED_FRAMEWORK_PATH",
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "RUBYOPT",
    "RUBYLIB",
    "PERL5OPT",
    "PERL5LIB",
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
];

// ---------------------------------------------------------------------------
// Input / Output types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Input {
    url: String,
    #[serde(default = "default_max_depth")]
    max_depth: u32,
    #[serde(default = "default_max_pages")]
    max_pages: u32,
    #[serde(default)]
    path_prefix: Option<String>,
    /// Return each page's rendered HTML and final URL in `pages` (used by
    /// `deep-search` to read JS-heavy pages it will cite).
    #[serde(default)]
    include_html: bool,
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_DEPTH
}
fn default_max_pages() -> u32 {
    DEFAULT_MAX_PAGES
}

#[derive(Serialize, Default)]
struct Output {
    output: String,
    success: bool,
    /// Rendered pages, only when `include_html` was requested.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pages: Vec<PageHtml>,
}

#[derive(Serialize)]
struct PageHtml {
    url: String,
    final_url: String,
    /// Main-frame navigations the browser reported (redirect chain).
    navigations: Vec<String>,
    html: String,
    /// Why the page yielded nothing (e.g. "blocked by a bot challenge (not
    /// bypassed)"), so the reader can report a specific reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Default)]
struct CrawledPage {
    url: String,
    depth: u32,
    text: String,
    links: Vec<String>,
    error: Option<String>,
    final_url: String,
    html: String,
    navigations: Vec<String>,
}

// ---------------------------------------------------------------------------
// Chrome process management
// ---------------------------------------------------------------------------

/// Find the Chrome/Chromium binary on this system.
fn find_chrome_binary() -> Option<String> {
    // Check standard binary names via PATH
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium-browser",
        "chromium",
    ];
    for name in &names {
        if which::which(name).is_ok() {
            return Some(name.to_string());
        }
    }

    // macOS application bundle
    let mac_path = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
    if std::path::Path::new(mac_path).exists() {
        return Some(mac_path.to_string());
    }

    // macOS Chromium
    let mac_chromium = "/Applications/Chromium.app/Contents/MacOS/Chromium";
    if std::path::Path::new(mac_chromium).exists() {
        return Some(mac_chromium.to_string());
    }

    // Windows
    let win_paths = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ];
    for path in &win_paths {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }

    None
}

/// Find a free TCP port.
fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(9222)
}

/// Chrome command line. Deliberately no automation-hiding switches
/// (`--disable-blink-features=AutomationControlled`, spoofed `--user-agent`,
/// `--disable-infobars`): see SKILL.md "Automation policy".
fn chrome_args(port: u16, user_data_dir: &std::path::Path) -> Vec<String> {
    vec![
        "--headless=new".to_string(),
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", user_data_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-gpu".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--disable-extensions".to_string(),
        "--disable-background-networking".to_string(),
        "about:blank".to_string(),
    ]
}

/// Launch headless Chrome with remote debugging and return the child process + debug port.
fn launch_chrome(user_data_dir: &std::path::Path) -> Result<(Child, u16), String> {
    let chrome_bin = find_chrome_binary()
        .ok_or_else(|| "Chrome/Chromium not found on this system".to_string())?;

    let port = find_free_port();

    let mut cmd = Command::new(&chrome_bin);
    cmd.args(chrome_args(port, user_data_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // Remove blocked env vars
    for var in BLOCKED_ENV_VARS {
        cmd.env_remove(var);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to launch Chrome: {e}"))?;
    Ok((child, port))
}

// ---------------------------------------------------------------------------
// CDP WebSocket client
// ---------------------------------------------------------------------------

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Global message ID counter for CDP JSON-RPC.
static MSG_ID: AtomicU64 = AtomicU64::new(1);

/// Discover the WebSocket debugger URL from Chrome's /json/version endpoint.
async fn get_ws_url(port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CDP_CONNECT_TIMEOUT_SECS);

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Timed out waiting for Chrome debug endpoint on port {port}"
            ));
        }

        if let Ok(resp) = reqwest::get(&url).await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(ws_url) = body["webSocketDebuggerUrl"].as_str() {
                    return Ok(ws_url.to_string());
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Connect to Chrome's CDP WebSocket.
async fn connect_cdp(ws_url: &str) -> Result<WsStream, String> {
    let (ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .map_err(|e| format!("Failed to connect CDP WebSocket: {e}"))?;
    Ok(ws)
}

/// Send a CDP command and wait for its response.
async fn cdp_send(
    ws: &mut WsStream,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = MSG_ID.fetch_add(1, Ordering::SeqCst);
    let msg = serde_json::json!({
        "id": id,
        "method": method,
        "params": params,
    });

    ws.send(Message::Text(msg.to_string()))
        .await
        .map_err(|e| format!("CDP send error: {e}"))?;

    // Read messages until we get our response
    let deadline = tokio::time::Instant::now() + Duration::from_secs(NAV_TIMEOUT_SECS + 10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("CDP response timeout for {method}"));
        }

        let read_result = timeout(Duration::from_secs(5), ws.next()).await;

        match read_result {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(resp) = serde_json::from_str::<serde_json::Value>(&text) {
                    if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        if let Some(err) = resp.get("error") {
                            return Err(format!("CDP error: {err}"));
                        }
                        return Ok(resp.get("result").cloned().unwrap_or(serde_json::json!({})));
                    }
                    // Not our response: an event (maybe a paused request).
                    handle_event(ws, &resp).await;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                return Err("CDP WebSocket closed".to_string());
            }
            Ok(Some(Err(e))) => {
                return Err(format!("CDP WebSocket error: {e}"));
            }
            Ok(None) => {
                return Err("CDP WebSocket stream ended".to_string());
            }
            Err(_) => {
                // Timeout on individual read, retry until deadline
                continue;
            }
            _ => continue,
        }
    }
}

/// Create a new CDP target (tab) and connect to it.
async fn create_target(ws: &mut WsStream) -> Result<String, String> {
    let result = cdp_send(
        ws,
        "Target.createTarget",
        serde_json::json!({"url": "about:blank"}),
    )
    .await?;

    result["targetId"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No targetId in createTarget response".to_string())
}

/// Attach to a target and get its session WebSocket URL.
async fn attach_to_target(ws: &mut WsStream, target_id: &str) -> Result<String, String> {
    let result = cdp_send(
        ws,
        "Target.attachToTarget",
        serde_json::json!({"targetId": target_id, "flatten": true}),
    )
    .await?;

    result["sessionId"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No sessionId in attachToTarget response".to_string())
}

/// Send a CDP command within a session (flat session mode).
async fn cdp_session_send(
    ws: &mut WsStream,
    session_id: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = MSG_ID.fetch_add(1, Ordering::SeqCst);
    let msg = serde_json::json!({
        "id": id,
        "sessionId": session_id,
        "method": method,
        "params": params,
    });

    ws.send(Message::Text(msg.to_string()))
        .await
        .map_err(|e| format!("CDP send error: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(NAV_TIMEOUT_SECS + 10);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("CDP session response timeout for {method}"));
        }

        let read_result = timeout(Duration::from_secs(5), ws.next()).await;

        match read_result {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(resp) = serde_json::from_str::<serde_json::Value>(&text) {
                    if resp.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        if let Some(err) = resp.get("error") {
                            return Err(format!("CDP error: {err}"));
                        }
                        return Ok(resp.get("result").cloned().unwrap_or(serde_json::json!({})));
                    }
                    handle_event(ws, &resp).await;
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                return Err("CDP WebSocket closed".to_string());
            }
            Ok(Some(Err(e))) => {
                return Err(format!("CDP WebSocket error: {e}"));
            }
            Ok(None) => {
                return Err("CDP WebSocket stream ended".to_string());
            }
            Err(_) => continue,
            _ => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// Page interaction via CDP
// ---------------------------------------------------------------------------

/// JS returning `{href, html}` of the rendered document.
const PAGE_HTML_JS: &str = r#"
    JSON.stringify({
        href: location.href,
        html: document.documentElement ? document.documentElement.outerHTML : ''
    })
"#;

/// JS to extract links from the page.
const EXTRACT_LINKS_JS: &str = r#"
    (() => {
        const urls = new Set();
        document.querySelectorAll('a[href]').forEach(a => {
            if (a.href && !a.href.startsWith('javascript:') && !a.href.startsWith('mailto:'))
                urls.add(a.href);
        });
        document.querySelectorAll('[data-href], [data-url], [data-link]').forEach(el => {
            const href = el.getAttribute('data-href')
                || el.getAttribute('data-url')
                || el.getAttribute('data-link');
            if (href) {
                try { urls.add(new URL(href, location.origin).href); } catch {}
            }
        });
        return JSON.stringify(Array.from(urls));
    })()
"#;

/// Navigate to a URL using CDP.
async fn navigate(ws: &mut WsStream, session_id: &str, url: &str) -> Result<(), String> {
    // Enable Page domain for navigation events
    let _ = cdp_session_send(ws, session_id, "Page.enable", serde_json::json!({})).await;

    cdp_session_send(
        ws,
        session_id,
        "Page.navigate",
        serde_json::json!({"url": url}),
    )
    .await?;

    // Wait for loadEventFired or timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(NAV_TIMEOUT_SECS);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break; // Don't fail on timeout -- page may still have content
        }

        let read_result = timeout(Duration::from_secs(2), ws.next()).await;

        match read_result {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                    handle_event(ws, &msg).await;
                    if msg.get("method").and_then(|m| m.as_str()) == Some("Page.loadEventFired") {
                        break;
                    }
                    // Also break on frameStoppedLoading for SPAs
                    if msg.get("method").and_then(|m| m.as_str())
                        == Some("Page.frameStoppedLoading")
                    {
                        break;
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                return Err("WebSocket closed during navigation".to_string());
            }
            _ => continue,
        }
    }

    Ok(())
}

/// Evaluate a JavaScript expression and return its string result.
async fn evaluate_js(
    ws: &mut WsStream,
    session_id: &str,
    expression: &str,
) -> Result<String, String> {
    let result = cdp_session_send(
        ws,
        session_id,
        "Runtime.evaluate",
        serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": false,
        }),
    )
    .await?;

    if let Some(exception) = result.get("exceptionDetails") {
        return Err(format!("JS exception: {exception}"));
    }

    let value = &result["result"]["value"];
    match value {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Null => Ok(String::new()),
        other => Ok(other.to_string()),
    }
}

/// Extract innerText from the page, stripping boilerplate elements.
async fn extract_text(ws: &mut WsStream, session_id: &str) -> Result<String, String> {
    // Remove nav, footer, aside, cookie banners, ads before extracting text.
    // This runs in the browser so we get clean content without boilerplate.
    let text = evaluate_js(
        ws,
        session_id,
        "(function(){if(!document.body)return '';var c=document.body.cloneNode(true);c.querySelectorAll('nav,footer,aside,[role=navigation],[role=banner],[role=complementary],[role=contentinfo],[class*=cookie],[class*=consent],[class*=gdpr],[class*=sidebar],[class*=newsletter],[class*=advertisement],[id*=cookie],[id*=consent],[id*=sidebar],[class*=popup],[class*=modal],[class*=overlay],iframe,svg,form,script,style,noscript').forEach(function(e){e.remove()});return c.innerText||'';})()",
    )
    .await?;
    Ok(truncate_string(text, MAX_PAGE_TEXT_CHARS))
}

/// Extract links from the page.
async fn extract_links(ws: &mut WsStream, session_id: &str) -> Vec<String> {
    match evaluate_js(ws, session_id, EXTRACT_LINKS_JS).await {
        Ok(json_str) => serde_json::from_str::<Vec<String>>(&json_str).unwrap_or_default(),
        Err(_) => vec![],
    }
}

// ---------------------------------------------------------------------------
// Bot detection
// ---------------------------------------------------------------------------

fn is_bot_blocked(text: &str) -> bool {
    let lower = text.to_lowercase();
    // The shared list (Chinese sites' WAF pages included), on short text.
    octos_research::access::challenge_text(text)
        || lower.contains("performing security verification")
        || lower.contains("press & hold to confirm you are")
        || lower.contains("please verify you are a human")
        || lower.contains("checking your browser")
        || lower.contains("just a moment...")
        || lower.contains("attention required! | cloudflare")
        || lower.contains("enable javascript and cookies to continue")
}

// ---------------------------------------------------------------------------
// SSRF protection
// ---------------------------------------------------------------------------

/// SSRF check (shared `octos_research::net`): http(s) only, no private,
/// loopback, link-local/metadata or reserved address, DNS fail-closed.
async fn check_ssrf(url_str: &str) -> Option<String> {
    octos_research::net::check_url(url_str).await.err()
}

/// Per-host verdicts for in-browser request interception (one DNS lookup
/// per host per crawl).
static HOST_VERDICTS: std::sync::Mutex<Vec<(String, bool)>> = std::sync::Mutex::new(Vec::new());
/// Main-frame document URLs the browser navigated to for the current page.
static NAVIGATIONS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
/// Document (page/frame) requests the interceptor refused for the current
/// page.
static BLOCKED_REQUESTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Decide whether the browser may fetch `url`. `data:`/`blob:` are local;
/// every network request must pass the SSRF check.
async fn browser_request_allowed(url: &str) -> bool {
    let Ok(u) = Url::parse(url) else {
        return false;
    };
    match u.scheme() {
        "data" | "blob" | "about" => return true,
        "http" | "https" => {}
        _ => return false,
    }
    let key = format!(
        "{}:{}",
        u.host_str().unwrap_or(""),
        u.port_or_known_default().unwrap_or(0)
    );
    let cached = HOST_VERDICTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v);
    if let Some(v) = cached {
        return v;
    }
    let ok = check_ssrf(url).await.is_none();
    HOST_VERDICTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((key, ok));
    ok
}

/// Handle a CDP event seen while waiting for something else:
/// - `Fetch.requestPaused`: every browser request (documents, redirects,
///   subresources) is held until we allow it; private/internal destinations
///   are failed with `BlockedByClient`, so the browser never reaches them.
/// - `Page.frameNavigated` (main frame): recorded so the whole navigation
///   chain can be re-validated before any content is used.
async fn handle_event(ws: &mut WsStream, msg: &serde_json::Value) {
    match msg.get("method").and_then(|m| m.as_str()).unwrap_or("") {
        "Fetch.requestPaused" => {
            let params = &msg["params"];
            let request_id = params["requestId"].as_str().unwrap_or("").to_string();
            let url = params["request"]["url"].as_str().unwrap_or("").to_string();
            let is_document = params["resourceType"].as_str() == Some("Document");
            let allowed = browser_request_allowed(&url).await;
            let params = if allowed {
                serde_json::json!({ "requestId": request_id })
            } else {
                eprintln!(
                    "[deep_crawl] blocked browser request to a private/invalid destination: {url}"
                );
                if is_document {
                    BLOCKED_REQUESTS
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(url);
                }
                serde_json::json!({ "requestId": request_id, "errorReason": "BlockedByClient" })
            };
            let mut out = serde_json::json!({
                "id": MSG_ID.fetch_add(1, Ordering::SeqCst),
                "method": if allowed { "Fetch.continueRequest" } else { "Fetch.failRequest" },
                "params": params,
            });
            if let Some(sid) = msg.get("sessionId") {
                out["sessionId"] = sid.clone();
            }
            let _ = ws.send(Message::Text(out.to_string())).await;
        }
        "Page.frameNavigated" => {
            let frame = &msg["params"]["frame"];
            if frame.get("parentId").is_none() {
                if let Some(u) = frame["url"].as_str() {
                    NAVIGATIONS
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(u.to_string());
                }
            }
        }
        _ => {}
    }
}

/// Read and handle CDP events for `dur` (instead of a plain sleep, so paused
/// requests keep flowing while the page settles).
async fn pump_events(ws: &mut WsStream, dur: Duration) {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        match timeout(deadline - now, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                    handle_event(ws, &msg).await;
                }
            }
            Ok(Some(Ok(_))) => {}
            _ => return,
        }
    }
}

/// Validate what the browser actually loaded: no document request may have
/// been blocked, the page must not be a browser error page, and the final
/// document URL and every main-frame navigation must pass the SSRF check.
/// Otherwise the page's content is discarded.
async fn validate_navigation(
    final_url: &str,
    navigations: &[String],
    blocked_documents: &[String],
) -> Result<(), String> {
    if let Some(b) = blocked_documents.first() {
        return Err(format!(
            "blocked: page tried to navigate to a private/internal address ({b})"
        ));
    }
    if final_url.starts_with("chrome-error:") {
        return Err("blocked: browser error page (navigation failed or was refused)".to_string());
    }
    for u in navigations
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(final_url))
    {
        if u.is_empty() || u.starts_with("about:") || u.starts_with("chrome-error:") {
            // Error pages are rejected above via the final URL.
            continue;
        }
        if let Some(e) = check_ssrf(u).await {
            return Err(format!("blocked: browser navigated to {u}: {e}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// URL utilities
// ---------------------------------------------------------------------------

/// Normalize a URL: remove fragment, trailing slash, lowercase scheme+host.
fn normalize_url(url: &str) -> Option<String> {
    let mut parsed = Url::parse(url).ok()?;
    parsed.set_fragment(None);
    let mut s = parsed.to_string();
    if s.ends_with('/') && s.len() > parsed.origin().ascii_serialization().len() + 1 {
        s.pop();
    }
    Some(s)
}

/// Generate a filesystem-safe slug from a hostname.
fn host_slug(url: &Url) -> String {
    let host = url.host_str().unwrap_or("unknown");
    host.replace('.', "-")
}

/// Generate a filesystem-safe filename from a URL path.
fn page_slug(url: &Url, index: usize) -> String {
    let path = url.path().trim_matches('/');
    let slug = if path.is_empty() {
        "index".to_string()
    } else {
        path.replace('/', "_")
            .replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_")
    };
    let truncated = if slug.len() > 80 {
        let mut end = 80;
        while !slug.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &slug[..end]
    } else {
        &slug
    };
    format!("{index:03}_{truncated}")
}

/// Truncate a string to max_len at a UTF-8 safe boundary.
fn truncate_string(mut s: String, max_len: usize) -> String {
    if s.len() <= max_len {
        return s;
    }
    // Find a char boundary at or before max_len
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n\n... (truncated)");
    s
}

// ---------------------------------------------------------------------------
// Crawl logic
// ---------------------------------------------------------------------------

/// Crawl a single page: navigate, wait for JS render, extract text and links.
///
/// No automation hiding: the browser keeps its default automation signals
/// (`navigator.webdriver`, HeadlessChrome UA plus our product token). A page
/// that answers with a bot challenge is recorded as blocked, not bypassed.
async fn crawl_single_page(
    ws: &mut WsStream,
    session_id: &str,
    url: &str,
    page_settle_ms: u64,
    include_html: bool,
) -> CrawledPage {
    NAVIGATIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    BLOCKED_REQUESTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    // Navigate
    if let Err(e) = navigate(ws, session_id, url).await {
        return CrawledPage {
            url: url.to_string(),
            error: Some(format!("Navigation failed: {e}")),
            ..Default::default()
        };
    }

    // Wait for JS settle (keep serving intercepted requests meanwhile)
    pump_events(ws, Duration::from_millis(page_settle_ms)).await;

    // Extract text, retrying while a slow page is still near-empty.
    let mut text = match extract_text(ws, session_id).await {
        Ok(t) => t,
        Err(e) => {
            return CrawledPage {
                url: url.to_string(),
                error: Some(e),
                ..Default::default()
            };
        }
    };

    // A check that clears itself in a real browser ("Just a moment…",
    // "正在进行安全检测…"): wait for it, up to ~10 s, instead of giving up.
    let mut waited = 0;
    while waited < INTERSTITIAL_WAITS && octos_research::access::interstitial_text(&text) {
        pump_events(ws, Duration::from_secs(2)).await;
        waited += 1;
        if let Ok(t) = extract_text(ws, session_id).await {
            text = t;
        }
    }
    if is_bot_blocked(&text) {
        eprintln!("[deep_crawl] bot challenge, not bypassing: {url}");
        return CrawledPage {
            url: url.to_string(),
            error: Some("blocked by a bot challenge (not bypassed)".to_string()),
            ..Default::default()
        };
    }

    for _retry in 0..MAX_EMPTY_RETRIES {
        let trimmed_len = text.trim().len();
        if trimmed_len >= MIN_USEFUL_TEXT_LEN {
            break;
        }
        eprintln!("[deep_crawl] page looks empty (len={trimmed_len}), waiting longer: {url}");
        pump_events(ws, Duration::from_millis(PAGE_SETTLE_RETRY_MS)).await;
        text = match extract_text(ws, session_id).await {
            Ok(t) => t,
            Err(_) => break,
        };
    }

    // Extract links
    let links = extract_links(ws, session_id).await;

    let (final_url, mut html) = page_html(ws, session_id).await.unwrap_or_default();
    if !include_html {
        html.clear();
    }
    let navigations = NAVIGATIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();

    // SSRF: whatever the page did (JS/meta redirects), the document we
    // extracted must come from a public address. Otherwise discard it.
    let blocked_documents = BLOCKED_REQUESTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if let Err(e) = validate_navigation(&final_url, &navigations, &blocked_documents).await {
        eprintln!("[deep_crawl] {e}");
        return CrawledPage {
            url: url.to_string(),
            error: Some(e),
            final_url,
            navigations,
            ..Default::default()
        };
    }

    CrawledPage {
        url: url.to_string(),
        depth: 0,
        text,
        links,
        error: None,
        final_url,
        html,
        navigations,
    }
}

/// Rendered `(final_url, outerHTML)`, HTML capped at [`MAX_PAGE_HTML_BYTES`].
async fn page_html(ws: &mut WsStream, session_id: &str) -> Option<(String, String)> {
    let raw = evaluate_js(ws, session_id, PAGE_HTML_JS).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let href = v["href"].as_str().unwrap_or("").to_string();
    let mut html = v["html"].as_str().unwrap_or("").to_string();
    if html.len() > MAX_PAGE_HTML_BYTES {
        let mut end = MAX_PAGE_HTML_BYTES;
        while end > 0 && !html.is_char_boundary(end) {
            end -= 1;
        }
        html.truncate(end);
    }
    Some((href, html))
}

/// Append our product token to the browser's own User-Agent for this tab.
async fn set_identifiable_user_agent(ws: &mut WsStream, session_id: &str) {
    let base = cdp_send(ws, "Browser.getVersion", serde_json::json!({}))
        .await
        .ok()
        .and_then(|v| v["userAgent"].as_str().map(str::to_string))
        .unwrap_or_default();
    let ua = if base.is_empty() {
        UA_SUFFIX.to_string()
    } else {
        format!("{base} {UA_SUFFIX}")
    };
    let _ = cdp_session_send(
        ws,
        session_id,
        "Network.setUserAgentOverride",
        serde_json::json!({ "userAgent": ua }),
    )
    .await;
}

/// Whether robots.txt is applied: operator setting `OCTOS_RESPECT_ROBOTS`,
/// default off (env lookup injected for tests).
fn robots_enabled(lookup: impl Fn(&str) -> Option<String>) -> bool {
    octos_research::respect_robots(lookup)
}

/// robots.txt check for one URL (RFC 9309 via `octos-research`), fetched
/// once per origin with an identifiable User-Agent.
async fn robots_check(
    cache: &octos_research::RobotsCache,
    url: &str,
) -> octos_research::robots::RobotsDecision {
    cache
        .check(url, octos_research::AGENT_TOKEN, |robots_url| async move {
            match octos_research::net::safe_get(&robots_url, Duration::from_secs(10)).await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    // Cap: 500 KiB is the RFC 9309 minimum a parser must handle.
                    let body = octos_research::net::read_capped(resp, 512 * 1024)
                        .await
                        .unwrap_or_default();
                    (Some(status), body)
                }
                Err(_) => (None, String::new()),
            }
        })
        .await
}

// ---------------------------------------------------------------------------
// Plugin protocol v2 stderr emitters
// ---------------------------------------------------------------------------

/// Emit a v2 `progress` event on stderr.
fn emit_v2_progress(stage: &str, message: &str, progress_fraction: Option<f64>) {
    let event = serde_json::json!({
        "type": "progress",
        "stage": stage,
        "message": message,
        "progress": progress_fraction,
    });
    match serde_json::to_string(&event) {
        Ok(line) => eprintln!("{line}"),
        Err(_) => eprintln!("[{stage}] {message}"),
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    install_sigterm_handler();
    let result = run().await;
    // Best-effort: kill chrome before we exit. Required when the timeout
    // path didn't run (success exit) AND when the host SIGKILLs us before
    // our drop runs.
    cleanup_chrome();
    let output_json = serde_json::to_string(&result).unwrap_or_else(|_| {
        r#"{"output":"Internal serialization error","success":false}"#.to_string()
    });
    println!("{output_json}");
}

/// Install a SIGTERM handler that flips the [`CANCELLED`] atomic, kills
/// the Chrome child, and exits within the host's 10s grace window.
#[cfg(unix)]
fn install_sigterm_handler() {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[deep_crawl] failed to install SIGTERM handler: {e}");
                return;
            }
        };
        if term.recv().await.is_some() {
            CANCELLED.store(true, Ordering::Relaxed);
            emit_v2_progress(
                "cleanup",
                "SIGTERM received, killing chrome and exiting",
                None,
            );
            cleanup_chrome();
            // 130 = 128 + SIGTERM(2). Stay well within the 10s budget.
            std::process::exit(130);
        }
    });
}

#[cfg(not(unix))]
fn install_sigterm_handler() {
    // No SIGTERM on Windows; deep_crawl exits via host's TerminateProcess.
}

/// Kill the tracked Chrome child if any. Idempotent: safe to call from
/// both the SIGTERM handler and the normal exit path.
fn cleanup_chrome() {
    let pid = CHROME_PID.swap(0, Ordering::Relaxed);
    if pid <= 0 {
        return;
    }
    #[cfg(unix)]
    {
        // SIGKILL the chrome process. Best-effort — Chrome reaps on its
        // own once the WS connection drops, but explicit kill is faster
        // and matches the "no browser zombies" assertion in the test
        // matrix.
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .status();
    }
}

async fn run() -> Output {
    // Read input from stdin
    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        return Output {
            output: format!("Failed to read stdin: {e}"),
            success: false,
            ..Default::default()
        };
    }

    let input: Input = match serde_json::from_str(&stdin_buf) {
        Ok(v) => v,
        Err(e) => {
            return Output {
                output: format!("Invalid JSON input: {e}"),
                success: false,
                ..Default::default()
            };
        }
    };

    let max_depth = input.max_depth.min(10);
    let max_pages = input.max_pages.clamp(1, 200);

    // Validate seed URL
    let seed_url = match Url::parse(&input.url) {
        Ok(u) => u,
        Err(_) => {
            return Output {
                output: "Invalid URL".to_string(),
                success: false,
                ..Default::default()
            };
        }
    };

    let scheme = seed_url.scheme();
    if scheme != "http" && scheme != "https" {
        return Output {
            output: format!("Only http:// and https:// URLs are allowed, got {scheme}://"),
            success: false,
            ..Default::default()
        };
    }

    // SSRF check on seed URL
    if let Some(msg) = check_ssrf(&input.url).await {
        return Output {
            output: msg,
            success: false,
            ..Default::default()
        };
    }

    let seed_origin = seed_url.origin().ascii_serialization();

    // Prepare output directory
    let crawl_dir = PathBuf::from(format!("crawl-{}", host_slug(&seed_url)));
    if let Err(e) = tokio::fs::create_dir_all(&crawl_dir).await {
        return Output {
            output: format!("Failed to create output directory: {e}"),
            success: false,
            ..Default::default()
        };
    }

    // Create temp dir for Chrome user data
    let temp_dir = match tempfile::Builder::new().prefix("deep-crawl-").tempdir() {
        Ok(d) => d,
        Err(e) => {
            return Output {
                output: format!("Failed to create temp dir: {e}"),
                success: false,
                ..Default::default()
            };
        }
    };

    emit_v2_progress("init", "launching headless chrome", Some(0.05));

    // Launch Chrome
    let (mut child, port) = match launch_chrome(temp_dir.path()) {
        Ok(c) => c,
        Err(e) => {
            return Output {
                output: e,
                success: false,
                ..Default::default()
            };
        }
    };

    // Record pid so the SIGTERM handler can kill it directly.
    if let Ok(pid) = i32::try_from(child.id()) {
        CHROME_PID.store(pid, Ordering::Relaxed);
    }

    // Connect to Chrome via CDP
    let ws_url = match get_ws_url(port).await {
        Ok(u) => u,
        Err(e) => {
            let _ = child.kill();
            return Output {
                output: format!("Failed to connect to Chrome: {e}"),
                success: false,
                ..Default::default()
            };
        }
    };

    let mut ws = match connect_cdp(&ws_url).await {
        Ok(w) => w,
        Err(e) => {
            let _ = child.kill();
            return Output {
                output: e,
                success: false,
                ..Default::default()
            };
        }
    };

    // Create a new target and attach to it
    let target_id = match create_target(&mut ws).await {
        Ok(id) => id,
        Err(e) => {
            let _ = child.kill();
            return Output {
                output: format!("Failed to create browser tab: {e}"),
                success: false,
                ..Default::default()
            };
        }
    };

    let session_id = match attach_to_target(&mut ws, &target_id).await {
        Ok(id) => id,
        Err(e) => {
            let _ = child.kill();
            return Output {
                output: format!("Failed to attach to browser tab: {e}"),
                success: false,
                ..Default::default()
            };
        }
    };

    // Enable Runtime domain
    let _ = cdp_session_send(
        &mut ws,
        &session_id,
        "Runtime.enable",
        serde_json::json!({}),
    )
    .await;
    set_identifiable_user_agent(&mut ws, &session_id).await;
    // Hold every browser request (documents, redirects, subresources) until
    // `handle_event` has SSRF-checked its destination.
    if let Err(e) = cdp_session_send(
        &mut ws,
        &session_id,
        "Fetch.enable",
        serde_json::json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
    )
    .await
    {
        let _ = child.kill();
        return Output {
            output: format!("Failed to enable request interception: {e}"),
            success: false,
            ..Default::default()
        };
    }
    let robots = octos_research::RobotsCache::new();
    let respect_robots = robots_enabled(|k| std::env::var(k).ok());

    // BFS crawl
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    let mut results: Vec<CrawledPage> = Vec::new();

    let seed_normalized = normalize_url(&input.url).unwrap_or_else(|| input.url.clone());
    visited.insert(seed_normalized.clone());
    queue.push_back((input.url.clone(), 0));

    eprintln!(
        "[deep_crawl] starting crawl: url={}, max_depth={max_depth}, max_pages={max_pages}, path_prefix={:?}",
        input.url, input.path_prefix
    );
    emit_v2_progress(
        "crawling",
        &format!("starting BFS, max_pages={max_pages}, max_depth={max_depth}"),
        Some(0.10),
    );

    while let Some((url, depth)) = queue.pop_front() {
        if cancelled() {
            eprintln!(
                "[deep_crawl] cancelled, stopping BFS at {} pages",
                results.len()
            );
            emit_v2_progress("cleanup", "cancellation requested", None);
            break;
        }
        if results.len() >= max_pages as usize {
            break;
        }

        eprintln!(
            "[deep_crawl] crawling [{}/{}] depth={depth}: {url}",
            results.len() + 1,
            max_pages
        );
        let progress_fraction = if max_pages > 0 {
            // 0.10 reserved for init; cap progress under 0.95.
            Some(0.10 + (results.len() as f64 / max_pages as f64) * 0.85)
        } else {
            None
        };
        emit_v2_progress(
            "crawling",
            &format!("page {}/{} depth={depth}", results.len() + 1, max_pages),
            progress_fraction,
        );

        // robots.txt (only when the operator enabled it with
        // OCTOS_RESPECT_ROBOTS=1; default off, never fetched): a disallowed
        // (or unreachable-robots) URL is recorded, never navigated, and
        // Crawl-delay is honoured between pages. Otherwise pages are spaced
        // by the settle time alone (sequential, one tab).
        let decision = if respect_robots {
            robots_check(&robots, &url).await
        } else {
            octos_research::robots::RobotsDecision {
                allowed: true,
                crawl_delay: None,
                reason: "robots_off",
            }
        };
        if !decision.allowed {
            eprintln!(
                "[deep_crawl] skipped by robots.txt ({}): {url}",
                decision.reason
            );
            results.push(CrawledPage {
                url: url.clone(),
                depth,
                error: Some(format!("skipped: {} (robots.txt)", decision.reason)),
                ..Default::default()
            });
            continue;
        }
        if let Some(delay) = decision.crawl_delay {
            if !results.is_empty() {
                let delay = delay.min(Duration::from_secs(MAX_CRAWL_DELAY_SECS));
                tokio::time::sleep(delay.saturating_sub(Duration::from_millis(PAGE_SETTLE_MS)))
                    .await;
            }
        }

        let mut crawled = crawl_single_page(
            &mut ws,
            &session_id,
            &url,
            PAGE_SETTLE_MS,
            input.include_html,
        )
        .await;
        crawled.depth = depth;

        // Enqueue discovered links
        if depth < max_depth {
            for link in &crawled.links {
                if results.len() + queue.len() >= max_pages as usize {
                    break;
                }

                let normalized = match normalize_url(link) {
                    Some(n) => n,
                    None => continue,
                };

                if visited.contains(&normalized) {
                    continue;
                }

                // Same-origin check
                let link_url = match Url::parse(&normalized) {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                if link_url.origin().ascii_serialization() != seed_origin {
                    continue;
                }

                // Path prefix filter
                if let Some(ref prefix) = input.path_prefix {
                    if !link_url.path().starts_with(prefix) {
                        continue;
                    }
                }

                // Sign-in, sign-up and account pages hold no content.
                if octos_research::urls::is_account_link(&normalized) {
                    continue;
                }

                // SSRF check on discovered links
                if check_ssrf(&normalized).await.is_some() {
                    continue;
                }

                visited.insert(normalized.clone());
                queue.push_back((normalized, depth + 1));
            }
        }

        results.push(crawled);
    }

    // Shutdown browser. The pid is also tracked in CHROME_PID so the
    // SIGTERM handler can race us; clear our slot before kill so the
    // double-kill from cleanup_chrome() is a noop in the success path.
    emit_v2_progress("cleanup", "closing chrome", Some(0.97));
    let _ = ws.close(None).await;
    CHROME_PID.store(0, Ordering::Relaxed);
    let _ = child.kill();
    let _ = child.wait();

    // Save results to disk and build output
    let mut output = format!(
        "# Deep Crawl: {}\nCrawled {} pages (max_depth: {}, max_pages: {})\n\n## Sitemap\n",
        input.url,
        results.len(),
        max_depth,
        max_pages
    );

    for (i, page) in results.iter().enumerate() {
        let status = if page.error.is_some() { "ERR" } else { "OK" };
        output.push_str(&format!(
            "{}. [depth={}] {} ({})\n",
            i + 1,
            page.depth,
            page.url,
            status
        ));
    }
    output.push('\n');

    for (i, crawled) in results.iter().enumerate() {
        // Save full content to disk
        let file_url = Url::parse(&crawled.url).ok();
        let filename = file_url
            .as_ref()
            .map(|u| format!("{}.md", page_slug(u, i)))
            .unwrap_or_else(|| format!("{i:03}_page.md"));
        let file_path = crawl_dir.join(&filename);

        let file_content = if let Some(ref err) = crawled.error {
            format!("# {}\n\nError: {}\n", crawled.url, err)
        } else {
            format!("# {}\n\n{}\n", crawled.url, crawled.text)
        };

        if let Err(e) = tokio::fs::write(&file_path, &file_content).await {
            eprintln!(
                "[deep_crawl] warning: failed to write {}: {e}",
                file_path.display()
            );
        }

        // Add preview to output
        output.push_str(&format!(
            "## Page {} [depth={}]: {}\n",
            i + 1,
            crawled.depth,
            crawled.url
        ));
        output.push_str(&format!("_Full content: {}_\n\n", file_path.display()));

        if let Some(ref err) = crawled.error {
            output.push_str(&format!("Error: {err}\n\n"));
        } else {
            let preview = if crawled.text.len() > PREVIEW_CHARS {
                let mut p = crawled.text.clone();
                p = truncate_string(p, PREVIEW_CHARS);
                p
            } else {
                crawled.text.clone()
            };
            output.push_str(&preview);
            output.push_str("\n\n");
        }
    }

    output.push_str(&format!(
        "{} pages saved to: {}\nUse read_file to examine specific pages.",
        results.len(),
        crawl_dir.display()
    ));

    // Truncate final output if needed
    output = truncate_string(output, MAX_OUTPUT_CHARS);

    eprintln!(
        "[deep_crawl] complete: {} pages saved to {}",
        results.len(),
        crawl_dir.display()
    );
    emit_v2_progress(
        "complete",
        &format!("crawled {} pages", results.len()),
        Some(1.0),
    );

    let pages = if input.include_html {
        results
            .into_iter()
            .filter(|p| p.error.is_some() || !p.html.is_empty())
            .map(|p| PageHtml {
                final_url: if p.final_url.is_empty() {
                    p.url.clone()
                } else {
                    p.final_url
                },
                url: p.url,
                navigations: p.navigations,
                // A failed page carries its reason, never its HTML.
                html: if p.error.is_some() {
                    String::new()
                } else {
                    p.html
                },
                error: p.error,
            })
            .collect()
    } else {
        Vec::new()
    };

    Output {
        output,
        success: true,
        pages,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_atomic_starts_unset() {
        // Other tests in this module may set the atomic; reset before
        // checking to keep this test deterministic regardless of
        // execution order.
        CANCELLED.store(false, Ordering::Relaxed);
        assert!(!cancelled());
        CANCELLED.store(true, Ordering::Relaxed);
        assert!(cancelled());
        CANCELLED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn cleanup_chrome_is_idempotent_with_zero_pid() {
        // No pid recorded → noop, must not panic.
        CHROME_PID.store(0, Ordering::Relaxed);
        cleanup_chrome();
        cleanup_chrome();
    }

    #[test]
    fn cleanup_chrome_clears_pid_after_call() {
        // Use an obviously-invalid pid so the kill command no-ops on
        // any sane host. The test asserts the slot is cleared so a
        // second cleanup is a noop.
        CHROME_PID.store(1, Ordering::Relaxed);
        cleanup_chrome();
        assert_eq!(CHROME_PID.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn normalize_url_strips_fragment_and_trailing_slash() {
        assert_eq!(
            normalize_url("https://example.com/page#frag"),
            Some("https://example.com/page".to_string())
        );
        assert_eq!(
            normalize_url("https://example.com/page/"),
            Some("https://example.com/page".to_string())
        );
    }

    #[test]
    fn host_slug_replaces_dots_with_dashes() {
        let url = Url::parse("https://www.example.com/").unwrap();
        assert_eq!(host_slug(&url), "www-example-com");
    }

    #[test]
    fn truncate_string_preserves_utf8_boundary() {
        let s = "hello 你好 world".to_string();
        let truncated = truncate_string(s, 7);
        // No panic and result fits within budget.
        assert!(truncated.len() <= 7 + "\n\n... (truncated)".len());
        assert!(truncated.is_char_boundary(0));
    }

    #[test]
    fn truncate_string_passes_through_short_strings() {
        let s = "short".to_string();
        assert_eq!(truncate_string(s, 1000), "short");
    }

    #[test]
    fn is_bot_blocked_detects_known_strings() {
        assert!(is_bot_blocked("Just a moment..."));
        assert!(is_bot_blocked("Performing security verification"));
        assert!(is_bot_blocked("Attention Required! | Cloudflare"));
        assert!(!is_bot_blocked("Welcome to our site"));
    }

    #[tokio::test]
    async fn should_block_browser_requests_and_navigations_to_private_addresses() {
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:9222/json/version",
            "http://[::ffff:10.0.0.1]/",
            "http://localhost/",
            "file:///etc/passwd",
        ] {
            assert!(!browser_request_allowed(u).await, "{u}");
        }
        assert!(browser_request_allowed("data:text/plain,hi").await);
        assert!(browser_request_allowed("http://93.184.216.34/").await);

        // Fixture: a page whose JS redirect lands on the metadata endpoint.
        let err = validate_navigation(
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            &["http://93.184.216.34/start".to_string()],
            &[],
        )
        .await
        .unwrap_err();
        assert!(err.contains("169.254.169.254"), "{err}");
        // A private hop earlier in the chain is caught even if the final
        // URL is public.
        let hops = ["http://10.0.0.7/admin".to_string()];
        assert!(validate_navigation("http://93.184.216.34/end", &hops, &[])
            .await
            .is_err());
        assert!(validate_navigation("http://93.184.216.34/end", &[], &[])
            .await
            .is_ok());
        // An HTTP/JS redirect the interceptor refused leaves Chrome on its
        // error page: rejected, content discarded.
        let blocked = ["http://169.254.169.254/latest/meta-data/".to_string()];
        let err = validate_navigation("chrome-error://chromewebdata/", &[], &blocked)
            .await
            .unwrap_err();
        assert!(err.contains("169.254.169.254"), "{err}");
        assert!(
            validate_navigation("chrome-error://chromewebdata/", &[], &[])
                .await
                .is_err()
        );
    }

    #[test]
    fn emit_v2_progress_does_not_panic_on_unicode() {
        // Catch any future serialization regression that would crash on
        // non-ASCII content.
        emit_v2_progress("crawling", "page 你好/世界", Some(0.5));
    }

    #[test]
    fn should_not_hide_automation_in_chrome_args_or_scripts() {
        let args = chrome_args(9222, std::path::Path::new("/tmp/x")).join(" ");
        assert!(!args.contains("AutomationControlled"), "{args}");
        assert!(!args.contains("--user-agent"), "{args}");
        assert!(!args.contains("--disable-infobars"), "{args}");
        let src = include_str!("main.rs");
        // Split so this test does not match itself.
        let needle = ["'web", "driver'"].concat();
        assert!(!src.contains(&needle), "no webdriver-hiding script");
        assert!(UA_SUFFIX.contains(octos_research::AGENT_TOKEN));
    }

    #[test]
    fn should_skip_robots_txt_unless_the_operator_enables_it() {
        assert!(!robots_enabled(|_| None));
        assert!(robots_enabled(|k| {
            (k == octos_research::RESPECT_ROBOTS_ENV).then(|| "1".to_string())
        }));
    }

    #[test]
    fn page_slug_produces_deterministic_filename_for_same_url() {
        let url = Url::parse("https://example.com/path/to/page").unwrap();
        let slug = page_slug(&url, 5);
        assert!(slug.starts_with("005_"));
        assert!(slug.contains("path"));
    }
}
