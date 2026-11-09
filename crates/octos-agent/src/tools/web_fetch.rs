//! Web fetch tool for retrieving URL content.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{NodeData, RcDom};
use serde::Deserialize;

use super::{Tool, ToolResult};

pub struct WebFetchTool {
    config: Option<Arc<super::tool_config::ToolConfigStore>>,
    /// PR A — a per-host network allowlist (fleet worker grant). The
    /// None/Some distinction is SECURITY-LOAD-BEARING: `None` = unrestricted
    /// (the default for every non-fleet caller); `Some(list)` = RESTRICTED to
    /// `list` (and their subdomains), enforced on the initial URL AND every
    /// redirect hop, on top of the private-IP block. `Some([])` denies
    /// everything (fail closed) — it is never read as "unrestricted".
    host_allowlist: Option<Vec<String>>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            config: None,
            host_allowlist: None,
        }
    }

    pub fn with_config(mut self, config: Arc<super::tool_config::ToolConfigStore>) -> Self {
        self.config = Some(config);
        self
    }

    /// PR A — restrict this tool to a fleet-worker grant's host allowlist. A
    /// fetch (or redirect hop) to any host not in `hosts` (nor a subdomain of
    /// one) is refused before a socket is opened. Passing an empty `hosts`
    /// denies EVERYTHING (fail closed), never "unrestricted" — to leave the
    /// tool unrestricted, simply do not call this.
    pub fn with_host_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.host_allowlist = Some(hosts);
        self
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Input {
    url: String,
    #[serde(default)]
    extract_mode: Option<String>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        static DESCRIPTION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            format!(
                "Fetch a URL and extract its content as markdown or plain text. Output beyond \
                 {} bytes is truncated with a '[N bytes omitted]' middle marker, regardless of \
                 max_chars.",
                octos_core::tool_output_limit("web_fetch")
            )
        });
        &DESCRIPTION
    }

    fn tags(&self) -> &[&str] {
        &["web"]
    }

    fn input_schema(&self) -> serde_json::Value {
        super::batch_read::schema(
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "extract_mode": {
                        "type": "string",
                        "enum": ["markdown", "text"],
                        "description": "Output format: 'markdown' (default) or 'text'"
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default: 50000)"
                    }
                },
                "required": ["url"]
            }),
            "requests",
        )
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        if args.get("requests").is_some() {
            return super::batch_read::execute(self, &super::ToolContext::zero(), args, "requests")
                .await;
        }
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid web_fetch input")?;

        let (cfg_extract_mode, cfg_max_chars) = match &self.config {
            Some(c) => (
                c.get_str("web_fetch", "extract_mode").await,
                c.get_usize("web_fetch", "max_chars").await,
            ),
            None => (None, None),
        };
        let extract_mode = input
            .extract_mode
            .or(cfg_extract_mode)
            .unwrap_or_else(|| "markdown".to_string());
        let max_chars = input.max_chars.or(cfg_max_chars).unwrap_or(50_000);

        if !input.url.starts_with("http://") && !input.url.starts_with("https://") {
            return Ok(ToolResult {
                output: "URL must start with http:// or https://".to_string(),
                success: false,
                ..Default::default()
            });
        }

        // SSRF-safe fetch: validate initial URL, disable auto-redirects,
        // and re-validate each redirect hop against SSRF rules PLUS the fleet
        // grant's host allowlist (PR A).
        let response = match ssrf_safe_fetch(&input.url, self.host_allowlist.as_deref()).await {
            Ok(r) => r,
            Err(msg) => {
                return Ok(ToolResult {
                    output: msg,
                    success: false,
                    ..Default::default()
                });
            }
        };

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let final_url = response.url().to_string();

        if !status.is_success() {
            return Ok(ToolResult {
                output: format!("HTTP {status} for {}", input.url),
                success: false,
                ..Default::default()
            });
        }

        // Cap response body to prevent OOM on huge responses.
        // Reject early if Content-Length exceeds limit, then stream-read
        // up to MAX_BODY_BYTES to avoid buffering unbounded data.
        const MAX_BODY_BYTES: usize = 5_000_000;
        if let Some(len) = response.content_length() {
            if len > MAX_BODY_BYTES as u64 {
                return Ok(ToolResult {
                    output: format!("Response too large ({len} bytes, max {MAX_BODY_BYTES})"),
                    success: false,
                    ..Default::default()
                });
            }
        }
        let body = {
            let mut buf = Vec::with_capacity(MAX_BODY_BYTES.min(256_000));
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.wrap_err("error reading response stream")?;
                buf.extend_from_slice(&chunk);
                if buf.len() > MAX_BODY_BYTES {
                    buf.truncate(MAX_BODY_BYTES);
                    break;
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        };

        let is_html = content_type.contains("text/html");
        let mut content = if is_html {
            match extract_mode.as_str() {
                "text" => extract_text(&body),
                _ => extract_markdown(&body),
            }
        } else {
            body
        };

        octos_core::truncate_utf8(&mut content, max_chars, "\n\n... (content truncated)");

        let mut output = format!("URL: {final_url}\n");
        if final_url != input.url {
            output.push_str(&format!("Redirected from: {}\n", input.url));
        }
        output.push_str(&format!("Content-Type: {content_type}\n"));
        output.push_str(&format!("Length: {} chars\n\n", content.len()));
        output.push_str(&content);

        Ok(ToolResult {
            output,
            success: true,
            ..Default::default()
        })
    }
}

/// Validate a URL against SSRF rules, build a pinned client, and fetch.
/// Redirects are followed manually with SSRF validation on each hop.
/// DNS failures are treated as blocked (fail-closed).
///
/// The hop loop itself is `octos_research::net::pinned_get` — the one
/// pinned-fetch loop in the workspace, shared with the research readers.
/// This wrapper adds the PR A fleet grant's host allowlist, checked BEFORE
/// any DNS or socket on every hop, and the tool's own User-Agent.
async fn ssrf_safe_fetch(
    initial_url: &str,
    host_allowlist: Option<&[String]>,
) -> Result<reqwest::Response, String> {
    let check_allowlist = move |host: &str| super::ssrf::check_host_allowlist(host, host_allowlist);
    octos_research::net::pinned_get(
        initial_url,
        octos_research::net::PinnedFetch {
            timeout: Duration::from_secs(30),
            user_agent: "octos/0.1 (web-fetch-tool)",
            pre_check: Some(&check_allowlist),
        },
    )
    .await
}

fn extract_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "template", "noscript"])
        .build()
        .convert(html)
        .unwrap_or_else(|_| extract_text(html))
}

fn extract_text(html: &str) -> String {
    // Use the same HTML parser as Markdown extraction. Stripping brackets
    // leaves script/style bodies in the result and mistakes quoted `>` for
    // the end of a tag, consuming the output budget before page evidence.
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let mut result = String::with_capacity(html.len());
    // Iterative traversal avoids an extra recursive walk for deeply nested
    // remote documents. Exit markers separate blocks without splitting words
    // around inline markup such as inter<b>national</b>.
    let mut pending = vec![(dom.document.clone(), false)];
    while let Some((node, exiting_block)) = pending.pop() {
        if exiting_block {
            result.push(' ');
            continue;
        }
        match &node.data {
            NodeData::Text { contents } => result.push_str(&contents.borrow()),
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.as_ref();
                if matches!(tag, "head" | "script" | "style" | "template" | "noscript")
                    || attrs
                        .borrow()
                        .iter()
                        .any(|attr| attr.name.local.as_ref() == "hidden")
                {
                    continue;
                }
                if matches!(
                    tag,
                    "address"
                        | "article"
                        | "aside"
                        | "blockquote"
                        | "br"
                        | "caption"
                        | "dd"
                        | "details"
                        | "dialog"
                        | "div"
                        | "dl"
                        | "dt"
                        | "fieldset"
                        | "figcaption"
                        | "figure"
                        | "footer"
                        | "form"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "header"
                        | "hr"
                        | "li"
                        | "main"
                        | "nav"
                        | "ol"
                        | "p"
                        | "pre"
                        | "section"
                        | "summary"
                        | "table"
                        | "td"
                        | "th"
                        | "tr"
                        | "ul"
                ) {
                    result.push(' ');
                    pending.push((node.clone(), true));
                }
            }
            NodeData::Document => {}
            _ => continue,
        }
        pending.extend(
            node.children
                .borrow()
                .iter()
                .rev()
                .map(|child| (child.clone(), false)),
        );
    }

    result.split_whitespace().collect::<Vec<_>>().join(" ")
}
