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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

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

const STEALTH_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

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
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_DEPTH
}
fn default_max_pages() -> u32 {
    DEFAULT_MAX_PAGES
}

#[derive(Serialize)]
struct Output {
    output: String,
    success: bool,
}

struct CrawledPage {
    url: String,
    depth: u32,
    text: String,
    links: Vec<String>,
    error: Option<String>,
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

/// Launch headless Chrome with remote debugging and return the child process + debug port.
fn launch_chrome(user_data_dir: &std::path::Path) -> Result<(Child, u16), String> {
    let chrome_bin = find_chrome_binary()
        .ok_or_else(|| "Chrome/Chromium not found on this system".to_string())?;

    let port = find_free_port();

    let mut cmd = Command::new(&chrome_bin);
    cmd.arg("--headless=new")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-extensions")
        .arg("--disable-background-networking")
        .arg("--disable-blink-features=AutomationControlled")
        .arg(format!("--user-agent={STEALTH_USER_AGENT}"))
        .arg("--disable-features=AutomationControlled")
        .arg("--disable-infobars")
        .arg("about:blank")
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
                    // Not our response, could be an event -- skip it
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

/// JS to remove automation indicators.
const STEALTH_JS: &str = r#"
    Object.defineProperty(navigator, 'webdriver', { get: () => undefined });
    Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] });
    Object.defineProperty(navigator, 'plugins', { get: () => [1, 2, 3, 4, 5] });
    window.chrome = { runtime: {} };
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

/// Extract innerText from the page.
async fn extract_text(ws: &mut WsStream, session_id: &str) -> Result<String, String> {
    let text = evaluate_js(
        ws,
        session_id,
        "document.body ? document.body.innerText : ''",
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
    lower.contains("performing security verification")
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

/// Basic SSRF check: block private/loopback/link-local IPs.
async fn check_ssrf(url_str: &str) -> Option<String> {
    let parsed = match Url::parse(url_str) {
        Ok(u) => u,
        Err(_) => return Some("Invalid URL".to_string()),
    };

    let host = match parsed.host_str() {
        Some(h) => h.to_string(),
        None => return Some("URL has no host".to_string()),
    };

    // Check if host is a raw IP
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_private_ip(ip) {
            return Some(format!(
                "Blocked: {host} resolves to a private/loopback address"
            ));
        }
    }

    // DNS resolution check
    let port = parsed.port_or_known_default().unwrap_or(80);
    match tokio::net::lookup_host(format!("{host}:{port}")).await {
        Ok(addrs) => {
            for addr in addrs {
                if is_private_ip(addr.ip()) {
                    return Some(format!(
                        "Blocked: {host} resolves to a private address ({})",
                        addr.ip()
                    ));
                }
            }
        }
        Err(_) => {
            // DNS resolution failed -- allow the request, Chrome will handle the error
        }
    }

    None
}

fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                // 100.64.0.0/10 (Carrier-grade NAT)
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
                // 169.254.0.0/16 (link-local, already covered by is_link_local but explicit)
                || v4.octets()[0] == 169 && v4.octets()[1] == 254
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // ULA fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped ::ffff:0:0/96
                || v6.segments()[..5] == [0, 0, 0, 0, 0] && v6.segments()[5] == 0xffff
                // IPv4-compatible ::/96 (deprecated)
                || v6.segments()[..6] == [0, 0, 0, 0, 0, 0] && v6.segments()[6] != 0
        }
    }
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
    format!("{:03}_{truncated}", index)
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
async fn crawl_single_page(
    ws: &mut WsStream,
    session_id: &str,
    url: &str,
    page_settle_ms: u64,
) -> CrawledPage {
    // Inject stealth JS before navigation
    let _ = evaluate_js(ws, session_id, STEALTH_JS).await;

    // Navigate
    if let Err(e) = navigate(ws, session_id, url).await {
        return CrawledPage {
            url: url.to_string(),
            depth: 0,
            text: String::new(),
            links: vec![],
            error: Some(format!("Navigation failed: {e}")),
        };
    }

    // Wait for JS settle
    tokio::time::sleep(Duration::from_millis(page_settle_ms)).await;

    // Re-inject stealth after navigation
    let _ = evaluate_js(ws, session_id, STEALTH_JS).await;

    // Extract text with retry for near-empty or bot-blocked pages
    let mut text = match extract_text(ws, session_id).await {
        Ok(t) => t,
        Err(e) => {
            return CrawledPage {
                url: url.to_string(),
                depth: 0,
                text: String::new(),
                links: vec![],
                error: Some(e),
            };
        }
    };

    // Retry if page looks empty or bot-blocked
    for _retry in 0..MAX_EMPTY_RETRIES {
        let trimmed_len = text.trim().len();
        if trimmed_len >= MIN_USEFUL_TEXT_LEN && !is_bot_blocked(&text) {
            break;
        }
        eprintln!(
            "[deep_crawl] page looks empty or bot-blocked (len={trimmed_len}), retrying: {url}"
        );
        tokio::time::sleep(Duration::from_millis(PAGE_SETTLE_RETRY_MS)).await;
        text = match extract_text(ws, session_id).await {
            Ok(t) => t,
            Err(_) => break,
        };
    }

    // Extract links
    let links = extract_links(ws, session_id).await;

    CrawledPage {
        url: url.to_string(),
        depth: 0,
        text,
        links,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let result = run().await;
    let output_json = serde_json::to_string(&result).unwrap_or_else(|_| {
        r#"{"output":"Internal serialization error","success":false}"#.to_string()
    });
    println!("{output_json}");
}

async fn run() -> Output {
    // Read input from stdin
    let mut stdin_buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin_buf) {
        return Output {
            output: format!("Failed to read stdin: {e}"),
            success: false,
        };
    }

    let input: Input = match serde_json::from_str(&stdin_buf) {
        Ok(v) => v,
        Err(e) => {
            return Output {
                output: format!("Invalid JSON input: {e}"),
                success: false,
            };
        }
    };

    let max_depth = input.max_depth.clamp(1, 10);
    let max_pages = input.max_pages.clamp(1, 200);

    // Validate seed URL
    let seed_url = match Url::parse(&input.url) {
        Ok(u) => u,
        Err(_) => {
            return Output {
                output: "Invalid URL".to_string(),
                success: false,
            };
        }
    };

    let scheme = seed_url.scheme();
    if scheme != "http" && scheme != "https" {
        return Output {
            output: format!("Only http:// and https:// URLs are allowed, got {scheme}://"),
            success: false,
        };
    }

    // SSRF check on seed URL
    if let Some(msg) = check_ssrf(&input.url).await {
        return Output {
            output: msg,
            success: false,
        };
    }

    let seed_origin = seed_url.origin().ascii_serialization();

    // Prepare output directory
    let crawl_dir = PathBuf::from(format!("crawl-{}", host_slug(&seed_url)));
    if let Err(e) = tokio::fs::create_dir_all(&crawl_dir).await {
        return Output {
            output: format!("Failed to create output directory: {e}"),
            success: false,
        };
    }

    // Create temp dir for Chrome user data
    let temp_dir = match tempfile::Builder::new().prefix("deep-crawl-").tempdir() {
        Ok(d) => d,
        Err(e) => {
            return Output {
                output: format!("Failed to create temp dir: {e}"),
                success: false,
            };
        }
    };

    // Launch Chrome
    let (mut child, port) = match launch_chrome(temp_dir.path()) {
        Ok(c) => c,
        Err(e) => {
            return Output {
                output: e,
                success: false,
            };
        }
    };

    // Connect to Chrome via CDP
    let ws_url = match get_ws_url(port).await {
        Ok(u) => u,
        Err(e) => {
            let _ = child.kill();
            return Output {
                output: format!("Failed to connect to Chrome: {e}"),
                success: false,
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

    while let Some((url, depth)) = queue.pop_front() {
        if results.len() >= max_pages as usize {
            break;
        }

        eprintln!(
            "[deep_crawl] crawling [{}/{}] depth={depth}: {url}",
            results.len() + 1,
            max_pages
        );

        let mut crawled = crawl_single_page(&mut ws, &session_id, &url, PAGE_SETTLE_MS).await;
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

    // Shutdown browser
    let _ = ws.close(None).await;
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
            .unwrap_or_else(|| format!("{:03}_page.md", i));
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

    Output {
        output,
        success: true,
    }
}
