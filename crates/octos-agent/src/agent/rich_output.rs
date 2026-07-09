//! Rich output: produce a self-contained HTML document from a short brief.
//!
//! This is a **focused, tool-less LLM call** — the same class as
//! [`super::verifier`] / the compaction `Summarizer`: system-initiated, single
//! shot, outside the agent loop, and offered no tools. On a voice turn the fast
//! model signals "a visual would help" with an in-band `[[VISUAL:html|brief]]`
//! marker after its spoken reply; the backend then calls this module to produce
//! the HTML and delivers it over the existing `files_attached` channel. The
//! model emits no tool call, so the Gemini-3 thought_signature 400 never arises.
//!
//! # Security — delivery contract (LOAD-BEARING)
//!
//! The produced document is **untrusted, model-authored HTML and may contain
//! author-controlled `<script>`** (interactivity is the whole point). It is
//! delivered as a `.html` file artifact. Any client that renders it **MUST**
//! isolate it in a sandboxed iframe with scripts but **no same-origin access**
//! — i.e. `srcdoc` + `sandbox="allow-scripts"` (NOT `allow-same-origin`), as the
//! octos-web `VisualPanel` does. Never inject it into the host DOM or render it
//! same-origin: that would expose the host origin's cookies/storage/DOM to the
//! model output (XSS / credential theft). The host `/api/files` endpoint does
//! not serve `.html` as `text/html`, so direct navigation can't execute it
//! either.

use base64::{Engine, engine::general_purpose::STANDARD};
use octos_core::Message;
use octos_llm::{ChatConfig, LlmProvider, ReasoningEffort, ToolChoice};

/// Fixed `src` token the model is told to use for the embedded illustration.
/// The backend swaps it for an inlined `data:` URI before delivery, so the
/// produced HTML stays a single self-contained file (no external asset).
/// A sentinel is more reliable than hoping the model echoes a real filename.
pub const ILLUSTRATION_PLACEHOLDER: &str = "__ILLUSTRATION__";

/// Input context for the rich HTML authoring call.
pub struct RichHtmlContext {
    /// The user's raw speech transcript.
    pub transcript: String,
    /// What the fast turn already spoke to the user (marker stripped) — the
    /// consistency contract: the artifact must honor what was said.
    pub spoken_reply: String,
    /// The brief carried by the marker.
    pub brief: String,
    /// When true, an AI illustration has been generated for this turn; the model
    /// must embed it via `<img src="__ILLUSTRATION__">` (the backend inlines it)
    /// and build labels/interaction around it instead of redrawing the subject.
    pub illustration: bool,
}

/// Replace the [`ILLUSTRATION_PLACEHOLDER`] `src` in `html` with an inlined
/// `data:image/png;base64,…` URI built from `png`. No-op if the placeholder is
/// absent. Keeps the artifact a single self-contained HTML file (works inside
/// the frontend's sandboxed `srcDoc` iframe, which has no base URL).
pub fn inline_illustration(html: &str, png: &[u8]) -> String {
    if !html.contains(ILLUSTRATION_PLACEHOLDER) {
        return html.to_string();
    }
    let data_uri = format!("data:image/png;base64,{}", STANDARD.encode(png));
    html.replace(ILLUSTRATION_PLACEHOLDER, &data_uri)
}

const RICH_HTML_SYSTEM: &str = "你是可视化生成器。根据用户请求与已给出的口头回复，产出一个【单文件、自包含】的 HTML 文档：内联 CSS/JS，不依赖任何外部资源或 CDN，可使用 <script>/SVG/<canvas> 实现交互。只输出 HTML 本身（可放在 ```html 围栏内），不要任何解释。";

/// Extract HTML from the model reply. Prefers a fenced `html` code block;
/// otherwise, if the whole reply looks like an HTML document (contains `<html`
/// or `<!doctype`), returns it as-is. Returns `None` if neither matches.
pub fn extract_html(raw: &str) -> Option<String> {
    if let Some(after) = raw.split("```html").nth(1) {
        if let Some(end) = after.find("```") {
            let body = after[..end].trim();
            if !body.is_empty() {
                return Some(body.to_string());
            }
        }
    }
    let lower = raw.to_lowercase();
    if lower.contains("<html") || lower.contains("<!doctype") {
        return Some(raw.trim().to_string());
    }
    None
}

/// Single-shot, tool-less focused call: produce self-contained HTML from the
/// context. On failure / no HTML in the output → `Err` (the caller falls back
/// to a plain spoken reply and delivers no artifact).
pub async fn author_html(llm: &dyn LlmProvider, ctx: &RichHtmlContext) -> eyre::Result<String> {
    let mut user = format!(
        "用户的语音请求：\n{}\n\n你刚才已口头回复：\n{}\n\n现在把这个可视化出来：\n{}",
        ctx.transcript, ctx.spoken_reply, ctx.brief
    );
    if ctx.illustration {
        user.push_str(&format!(
            "\n\n已为你生成一张写实插图。请在页面合适位置用 <img src=\"{ILLUSTRATION_PLACEHOLDER}\" ...> 内嵌它\
             （务必保留这个确切的 src 占位符原样，系统会替换为真实图片），并围绕它做标注、交互或讲解；\
             不要用 SVG/canvas 重画该实物本身。"
        ));
    }
    let messages = vec![Message::system(RICH_HTML_SYSTEM), Message::user(user)];
    // A whole HTML document needs a large output budget; on thinking models
    // (e.g. Gemini-3) thoughts also draw from it, so we cap thinking to `Low`
    // (a bounded budget) — otherwise the model can spend the entire budget
    // reasoning and return a turn with no text part (empty content).
    let config = ChatConfig {
        max_tokens: Some(32768),
        temperature: Some(0.2),
        tool_choice: ToolChoice::None,
        reasoning_effort: Some(ReasoningEffort::Low),
        ..Default::default()
    };
    let resp = llm.chat(&messages, &[], &config).await?;
    // #1477 P2: this focused authoring call runs OUTSIDE the turn's token
    // accounting (the turn already emitted `done`), so surface its spend here
    // for observability of the otherwise-invisible background cost.
    tracing::info!(
        input_tokens = resp.usage.input_tokens,
        output_tokens = resp.usage.output_tokens,
        illustration = ctx.illustration,
        "rich_output: author_html token usage"
    );
    let raw = resp
        .content
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("rich_output: model returned empty content"))?;
    extract_html(&raw).ok_or_else(|| eyre::eyre!("rich_output: no HTML in model output"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octos_llm::{ChatResponse, ChatStream, StopReason, TokenUsage, ToolSpec};
    use std::sync::Arc;

    struct StubProvider {
        reply: String,
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some(self.reply.clone()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatStream> {
            unimplemented!("stub does not stream")
        }

        fn model_id(&self) -> &str {
            "stub-model"
        }

        fn provider_name(&self) -> &str {
            "stub"
        }
    }

    #[test]
    fn extract_html_takes_fenced_block() {
        let raw = "这是演示：\n```html\n<!doctype html><html><body>hi</body></html>\n```\n完成。";
        let got = extract_html(raw).unwrap();
        assert!(got.starts_with("<!doctype html>"));
        assert!(!got.contains("```"));
        assert!(!got.contains("这是演示"));
    }

    #[test]
    fn extract_html_falls_back_to_bare_document() {
        let raw = "<!DOCTYPE html><html><body>x</body></html>";
        assert!(extract_html(raw).unwrap().contains("<body>x</body>"));
    }

    #[test]
    fn extract_html_none_for_prose() {
        assert!(extract_html("就是一段普通文字，没有 HTML。").is_none());
    }

    #[test]
    fn inline_illustration_swaps_placeholder_for_data_uri() {
        let html = "<body><img src=\"__ILLUSTRATION__\" alt=\"x\"></body>";
        let out = inline_illustration(html, b"PNGBYTES");
        assert!(out.contains("data:image/png;base64,"));
        assert!(!out.contains("__ILLUSTRATION__"));
        // single-quoted src form is handled too
        let html2 = "<img src='__ILLUSTRATION__'>";
        assert!(inline_illustration(html2, b"x").contains("data:image/png;base64,"));
    }

    #[test]
    fn inline_illustration_noop_without_placeholder() {
        let html = "<div>no image here</div>";
        assert_eq!(inline_illustration(html, b"bytes"), html);
    }

    #[tokio::test]
    async fn author_html_returns_extracted_document() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: "好的：\n```html\n<!doctype html><html><body>NF</body></html>\n```".into(),
        });
        let ctx = RichHtmlContext {
            transcript: "我想直观看到负反馈电路如何负反馈".into(),
            spoken_reply: "我给你画一个可调增益的负反馈电路。".into(),
            brief: "负反馈电路交互演示".into(),
            illustration: false,
        };
        let html = author_html(provider.as_ref(), &ctx).await.expect("html");
        assert!(html.contains("<body>NF</body>"));
    }

    #[tokio::test]
    async fn author_html_errors_when_model_returns_no_html() {
        let provider: Arc<dyn LlmProvider> = Arc::new(StubProvider {
            reply: "抱歉做不了".into(),
        });
        let ctx = RichHtmlContext {
            transcript: "x".into(),
            spoken_reply: "y".into(),
            brief: "z".into(),
            illustration: false,
        };
        assert!(author_html(provider.as_ref(), &ctx).await.is_err());
    }
}
