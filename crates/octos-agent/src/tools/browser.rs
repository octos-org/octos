//! Browser automation tool using chromiumoxide (Chrome DevTools Protocol).
//!
//! Launches headless Chrome on first use via `chromiumoxide::Browser`.
//! Feature-gated behind `browser`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::ScreenshotParams;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{Tool, ToolResult};
use crate::sandbox::BLOCKED_ENV_VARS;

const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_OUTPUT_CHARS: usize = 50_000;

use super::ssrf::check_ssrf;

// --- Browser session ---

struct BrowserSession {
    browser: Browser,
    page: Page,
    last_used: Instant,
    _handler: tokio::task::JoinHandle<()>,
    _temp_dir: tempfile::TempDir,
}

impl BrowserSession {
    async fn launch() -> Result<Self> {
        let temp_dir = tempfile::Builder::new()
            .prefix("octos-browser-")
            .tempdir()
            .wrap_err("failed to create temp dir for Chrome")?;

        let mut builder = BrowserConfig::builder()
            .user_data_dir(temp_dir.path())
            .arg("--headless=new")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-extensions")
            .arg("--disable-background-networking");

        // Sanitize environment: set blocked vars to empty string
        for var in BLOCKED_ENV_VARS {
            builder = builder.env(*var, "");
        }

        let config = builder
            .build()
            .map_err(|e| eyre::eyre!("failed to build browser config: {e}"))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| eyre::eyre!("failed to launch Chrome: {e}"))?;

        let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| eyre::eyre!("failed to create page: {e}"))?;

        Ok(Self {
            browser,
            page,
            last_used: Instant::now(),
            _handler: handle,
            _temp_dir: temp_dir,
        })
    }

    fn is_idle(&self) -> bool {
        self.last_used.elapsed() > IDLE_TIMEOUT
    }

    fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    async fn shutdown(mut self) {
        let _ = self.browser.close().await;
        self._handler.abort();
    }
}

/// Defense-in-depth: if the session is dropped without calling `shutdown()`
/// (e.g., the owning future was cancelled by a timeout), abort the handler
/// task so Chrome is not left orphaned. `Browser::drop` sends a kill signal
/// to the child process, and aborting the handler ensures the event loop
/// task is cleaned up.
impl Drop for BrowserSession {
    fn drop(&mut self) {
        self._handler.abort();
    }
}

// --- Input ---

#[derive(Debug, Deserialize)]
struct Input {
    action: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    expression: Option<String>,
}

// --- Tool ---

pub struct BrowserTool {
    session: Arc<Mutex<Option<BrowserSession>>>,
    /// Per-action timeout. If any single `execute()` call exceeds this, it returns
    /// a timeout error and kills the browser session to avoid blocking the agent.
    action_timeout: Duration,
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            session: Arc::new(Mutex::new(None)),
            action_timeout: DEFAULT_ACTION_TIMEOUT,
            config: None,
        }
    }

    /// Create a browser tool with a custom per-action timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            session: Arc::new(Mutex::new(None)),
            action_timeout: timeout,
            config: None,
        }
    }

    pub fn with_config(mut self, config: Arc<super::tool_config::ToolConfigStore>) -> Self {
        self.config = Some(config);
        self
    }

    async fn ensure_session(guard: &mut Option<BrowserSession>) -> Result<()> {
        if let Some(session) = guard.as_ref() {
            if !session.is_idle() {
                return Ok(());
            }
            // Session is idle, recycle it
            if let Some(s) = guard.take() {
                s.shutdown().await;
            }
        }
        *guard = Some(BrowserSession::launch().await?);
        Ok(())
    }
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Interact with web pages using a headless browser. Supports navigation, \
         text/HTML extraction, clicking, typing, screenshots, JS evaluation, \
         element discovery, and link extraction."
    }

    fn tags(&self) -> &[&str] {
        &["web", "interactive"]
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["navigate", "get_text", "get_html", "click", "type", "screenshot", "evaluate", "find_elements", "get_links", "close"],
                    "description": "Action to perform"
                },
                "url": {
                    "type": "string",
                    "description": "URL to navigate to (for 'navigate' action)"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector (for 'click', 'type', and 'find_elements' actions)"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (for 'type' action)"
                },
                "expression": {
                    "type": "string",
                    "description": "JavaScript expression (for 'evaluate' action)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid browser tool input")?;

        // Validate action before launching Chrome
        const VALID_ACTIONS: &[&str] = &[
            "navigate",
            "get_text",
            "get_html",
            "click",
            "type",
            "screenshot",
            "evaluate",
            "find_elements",
            "get_links",
            "close",
        ];
        if !VALID_ACTIONS.contains(&input.action.as_str()) {
            return Ok(ToolResult {
                output: format!(
                    "Unknown action: {}. Valid: {}",
                    input.action,
                    VALID_ACTIONS.join(", ")
                ),
                success: false,
                ..Default::default()
            });
        }

        // Close action doesn't need a session
        if input.action == "close" {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.take() {
                session.shutdown().await;
            }
            return Ok(ToolResult {
                output: "Browser session closed".to_string(),
                success: true,
                ..Default::default()
            });
        }

        // Validate navigate parameters BEFORE launching Chrome (avoid expensive startup for bad URLs)
        if input.action == "navigate" {
            let url = match &input.url {
                Some(u) => u,
                None => {
                    return Ok(ToolResult {
                        output: "'url' parameter is required for navigate".to_string(),
                        success: false,
                        ..Default::default()
                    });
                }
            };
            let parsed_url = match reqwest::Url::parse(url) {
                Ok(u) => u,
                Err(_) => {
                    return Ok(ToolResult {
                        output: "Invalid URL".to_string(),
                        success: false,
                        ..Default::default()
                    });
                }
            };
            let scheme = parsed_url.scheme();
            if scheme != "http" && scheme != "https" {
                return Ok(ToolResult {
                    output: format!("Only http:// and https:// URLs are allowed, got {scheme}://"),
                    success: false,
                    ..Default::default()
                });
            }
            if let Some(msg) = check_ssrf(url).await {
                return Ok(ToolResult {
                    output: msg,
                    success: false,
                    ..Default::default()
                });
            }
        }

        // Resolve timeout from config, falling back to constructor value
        let timeout = match &self.config {
            Some(c) => {
                let secs = c.get_u64("browser", "action_timeout_secs").await;
                secs.map(Duration::from_secs).unwrap_or(self.action_timeout)
            }
            None => self.action_timeout,
        };
        // Wrap the entire session-holding section with a timeout to prevent
        // a stuck Chrome from blocking the agent indefinitely.
        let session_arc = self.session.clone();
        match tokio::time::timeout(timeout, self.execute_action(input)).await {
            Ok(result) => result,
            Err(_) => {
                // Timeout: kill the browser session so the next call starts fresh
                let mut guard = session_arc.lock().await;
                if let Some(session) = guard.take() {
                    session.shutdown().await;
                }
                Ok(ToolResult {
                    output: format!(
                        "Browser action timed out after {}s. Session killed. \
                         Try a simpler approach or use web_fetch/web_search instead.",
                        timeout.as_secs()
                    ),
                    success: false,
                    ..Default::default()
                })
            }
        }
    }
}

impl BrowserTool {
    async fn execute_action(&self, input: Input) -> Result<ToolResult> {
        let mut guard = self.session.lock().await;
        if let Err(e) = Self::ensure_session(&mut guard).await {
            return Ok(ToolResult {
                output: format!("Failed to launch browser: {e}"),
                success: false,
                ..Default::default()
            });
        }
        let session = guard.as_mut().unwrap();
        session.touch();

        match input.action.as_str() {
            "navigate" => {
                // URL already validated above
                let url = input.url.as_ref().unwrap();
                match session.page.goto(url).await {
                    Ok(_) => {
                        // Best-effort wait for navigation
                        let _ = tokio::time::timeout(
                            Duration::from_secs(30),
                            session.page.wait_for_navigation(),
                        )
                        .await;
                        Ok(ToolResult {
                            output: format!("Navigated to {url}"),
                            success: true,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("Navigation failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "get_text" => match session.page.evaluate("document.body.innerText").await {
                Ok(result) => {
                    let mut text = result.into_value::<String>().unwrap_or_default();
                    octos_core::truncate_utf8(&mut text, MAX_OUTPUT_CHARS, "\n\n... (truncated)");
                    Ok(ToolResult {
                        output: text,
                        success: true,
                        ..Default::default()
                    })
                }
                Err(e) => Ok(ToolResult {
                    output: format!("Failed to get text: {e}"),
                    success: false,
                    ..Default::default()
                }),
            },
            "get_html" => match session.page.content().await {
                Ok(mut html) => {
                    octos_core::truncate_utf8(&mut html, MAX_OUTPUT_CHARS, "\n\n... (truncated)");
                    Ok(ToolResult {
                        output: html,
                        success: true,
                        ..Default::default()
                    })
                }
                Err(e) => Ok(ToolResult {
                    output: format!("Failed to get HTML: {e}"),
                    success: false,
                    ..Default::default()
                }),
            },
            "click" => {
                let selector = match &input.selector {
                    Some(s) => s,
                    None => {
                        return Ok(ToolResult {
                            output: "'selector' parameter is required for click".to_string(),
                            success: false,
                            ..Default::default()
                        });
                    }
                };
                match session.page.find_element(selector).await {
                    Ok(element) => match element.click().await {
                        Ok(_) => Ok(ToolResult {
                            output: "clicked".to_string(),
                            success: true,
                            ..Default::default()
                        }),
                        Err(e) => Ok(ToolResult {
                            output: format!("Click failed: {e}"),
                            success: false,
                            ..Default::default()
                        }),
                    },
                    Err(_) => Ok(ToolResult {
                        output: format!("ERROR: element not found for selector: {selector}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "type" => {
                let selector = match &input.selector {
                    Some(s) => s,
                    None => {
                        return Ok(ToolResult {
                            output: "'selector' parameter is required for type".to_string(),
                            success: false,
                            ..Default::default()
                        });
                    }
                };
                let text = match &input.text {
                    Some(t) => t,
                    None => {
                        return Ok(ToolResult {
                            output: "'text' parameter is required for type".to_string(),
                            success: false,
                            ..Default::default()
                        });
                    }
                };
                match session.page.find_element(selector).await {
                    Ok(element) => {
                        // Click to focus, then type
                        if let Err(e) = element.click().await {
                            return Ok(ToolResult {
                                output: format!("Focus failed: {e}"),
                                success: false,
                                ..Default::default()
                            });
                        }
                        match element.type_str(text).await {
                            Ok(_) => Ok(ToolResult {
                                output: "typed".to_string(),
                                success: true,
                                ..Default::default()
                            }),
                            Err(e) => Ok(ToolResult {
                                output: format!("Type failed: {e}"),
                                success: false,
                                ..Default::default()
                            }),
                        }
                    }
                    Err(_) => Ok(ToolResult {
                        output: format!("ERROR: element not found for selector: {selector}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "screenshot" => {
                let params = ScreenshotParams::builder().full_page(true).build();
                match session.page.screenshot(params).await {
                    Ok(bytes) => {
                        let tmp = tempfile::Builder::new()
                            .prefix("octos-screenshot-")
                            .suffix(".png")
                            .tempfile()
                            .wrap_err("failed to create screenshot temp file")?;
                        let path = tmp.path().to_path_buf();
                        tokio::fs::write(&path, &bytes)
                            .await
                            .wrap_err("failed to write screenshot")?;
                        tmp.keep().map_err(|e| {
                            eyre::eyre!("failed to persist screenshot: {}", e.error)
                        })?;
                        Ok(ToolResult {
                            output: format!("Screenshot saved to {}", path.display()),
                            success: true,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("Screenshot failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "evaluate" => {
                let expr = match &input.expression {
                    Some(e) => e,
                    None => {
                        return Ok(ToolResult {
                            output: "'expression' parameter is required for evaluate".to_string(),
                            success: false,
                            ..Default::default()
                        });
                    }
                };
                match session.page.evaluate(expr.as_str()).await {
                    Ok(result) => {
                        let mut output = match result.value() {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => "undefined".to_string(),
                        };
                        octos_core::truncate_utf8(
                            &mut output,
                            MAX_OUTPUT_CHARS,
                            "\n\n... (truncated)",
                        );
                        Ok(ToolResult {
                            output,
                            success: true,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("JavaScript error: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "find_elements" => {
                let selector = match &input.selector {
                    Some(s) => s,
                    None => {
                        return Ok(ToolResult {
                            output: "'selector' parameter is required for find_elements"
                                .to_string(),
                            success: false,
                            ..Default::default()
                        });
                    }
                };
                match session.page.find_elements(selector).await {
                    Ok(elements) => {
                        let count = elements.len();
                        let mut summaries = Vec::new();
                        for (i, el) in elements.iter().take(50).enumerate() {
                            let text = el.inner_text().await.ok().flatten().unwrap_or_default();
                            let truncated = if text.len() > 200 {
                                format!("{}...", &text[..200])
                            } else {
                                text
                            };
                            summaries.push(format!("[{i}] {truncated}"));
                        }
                        let mut output = format!("Found {count} elements matching '{selector}':\n");
                        output.push_str(&summaries.join("\n"));
                        if count > 50 {
                            output.push_str(&format!("\n... and {} more", count - 50));
                        }
                        octos_core::truncate_utf8(
                            &mut output,
                            MAX_OUTPUT_CHARS,
                            "\n\n... (truncated)",
                        );
                        Ok(ToolResult {
                            output,
                            success: true,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("Find elements failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            "get_links" => {
                // Extract all <a> href + text from the page via JS
                let js = r#"
                    Array.from(document.querySelectorAll('a[href]')).map(a => ({
                        href: a.href,
                        text: (a.innerText || '').trim().substring(0, 200)
                    })).filter(l => l.href && !l.href.startsWith('javascript:'))
                "#;
                match session.page.evaluate(js).await {
                    Ok(result) => {
                        let mut output = match result.value() {
                            Some(Value::Array(links)) => {
                                let mut lines = Vec::new();
                                lines.push(format!("Found {} links:", links.len()));
                                for link in links.iter().take(200) {
                                    let href =
                                        link.get("href").and_then(|v| v.as_str()).unwrap_or("");
                                    let text =
                                        link.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                    if text.is_empty() {
                                        lines.push(format!("  - {href}"));
                                    } else {
                                        lines.push(format!("  - [{text}]({href})"));
                                    }
                                }
                                if links.len() > 200 {
                                    lines.push(format!("  ... and {} more", links.len() - 200));
                                }
                                lines.join("\n")
                            }
                            _ => "No links found".to_string(),
                        };
                        octos_core::truncate_utf8(
                            &mut output,
                            MAX_OUTPUT_CHARS,
                            "\n\n... (truncated)",
                        );
                        Ok(ToolResult {
                            output,
                            success: true,
                            ..Default::default()
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        output: format!("Get links failed: {e}"),
                        success: false,
                        ..Default::default()
                    }),
                }
            }
            // VALID_ACTIONS check above ensures we never reach here
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ssrf::is_private_host;
    use super::*;

    #[test]
    fn test_ssrf_private_hosts() {
        assert!(is_private_host("localhost"));
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.0.0.1"));
        assert!(is_private_host("192.168.1.1"));
        assert!(is_private_host("::1"));
        assert!(!is_private_host("example.com"));
        assert!(!is_private_host("8.8.8.8"));
    }

    #[test]
    fn test_input_deserialization() {
        let v = json!({ "action": "navigate", "url": "https://example.com" });
        let input: Input = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "navigate");
        assert_eq!(input.url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn test_input_minimal() {
        let v = json!({ "action": "get_text" });
        let input: Input = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "get_text");
        assert!(input.url.is_none());
        assert!(input.selector.is_none());
    }

    #[tokio::test]
    async fn test_close_without_session() {
        let tool = BrowserTool::new();
        let result = tool.execute(&json!({ "action": "close" })).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_navigate_ssrf_blocked() {
        let tool = BrowserTool::new();
        let result = tool
            .execute(&json!({ "action": "navigate", "url": "http://127.0.0.1:9222" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("private"));
    }

    #[tokio::test]
    async fn test_navigate_missing_url() {
        let tool = BrowserTool::new();
        let result = tool
            .execute(&json!({ "action": "navigate" }))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_navigate_rejects_file_scheme() {
        let tool = BrowserTool::new();
        let result = tool
            .execute(&json!({ "action": "navigate", "url": "file:///etc/passwd" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Only http://"));
    }

    #[tokio::test]
    async fn test_navigate_rejects_javascript_scheme() {
        let tool = BrowserTool::new();
        let result = tool
            .execute(&json!({ "action": "navigate", "url": "javascript:alert(1)" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Only http://"));
    }

    #[tokio::test]
    async fn test_unknown_action_rejected_early() {
        let tool = BrowserTool::new();
        let result = tool
            .execute(&json!({ "action": "invalid_action" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Unknown action"));
    }

    #[test]
    fn test_unknown_action_deserializes() {
        let v = json!({ "action": "invalid_action" });
        let input: Input = serde_json::from_value(v).unwrap();
        assert_eq!(input.action, "invalid_action");
    }

    #[test]
    fn test_new_actions_in_schema() {
        let tool = BrowserTool::new();
        let schema = tool.input_schema();
        let actions = schema["properties"]["action"]["enum"].as_array().unwrap();
        let action_strs: Vec<&str> = actions.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(action_strs.contains(&"find_elements"));
        assert!(action_strs.contains(&"get_links"));
    }
}
