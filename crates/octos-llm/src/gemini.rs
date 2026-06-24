//! Google Gemini provider implementation.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::Message;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;

use crate::vertex_auth::{ServiceAccount, TokenSource, VertexTokenProvider};
use crate::vision;

use crate::config::ChatConfig;
use crate::provider::{LlmProvider, endpoint_label_from_base_url};
use crate::types::{
    ChatResponse, ChatStream, ProviderMetadata, StopReason, StreamEvent, TokenUsage, ToolSpec,
};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global monotonic counter for synthesizing Gemini tool-call ids.
///
/// Gemini's API returns `functionCall` parts WITHOUT a call id — it matches
/// `functionResponse` parts back by function NAME, not by id (see the
/// `MessageRole::Tool` arm of `to_gemini_contents`, which resolves the name
/// from a `tool_call_id → name` map). The synthesized id is therefore purely
/// octos-internal correlation, but it MUST be process-unique: it becomes
/// `BackgroundTask::tool_call_id`, and several long-lived per-session
/// structures key on it — `TaskSupervisor`'s synth-ack set (commit 9e972d8a),
/// the `mark_descendants_failed` pipeline cascade, and the orphan-sweep
/// liveness gate's tool_call_id-family exemption (fix/orphan-sweep-liveness-
/// gate). A POSITIONAL `call_{index}` resets every response, so two unrelated
/// tool calls in different turns both get `call_0`, which (a) could match a
/// stale synth-ack and fire an unwarranted recovery turn, and (b) lets a live
/// task's tcid falsely exempt a genuinely-dead task from orphan reaping. A
/// process-global monotonic counter never repeats within the process.
///
/// octos-agent's empty-id fallback (`streaming.rs`) uses the same scheme with
/// a distinct `call_synth_` prefix; the `call_gemini_` prefix here keeps the
/// two synthesized id spaces disjoint even if one session ever switched
/// providers mid-conversation.
static GEMINI_TOOL_CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Mint the next process-unique synthesized Gemini tool-call id.
fn next_gemini_tool_call_id() -> String {
    format!(
        "call_gemini_{}",
        GEMINI_TOOL_CALL_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Default AI Studio base URL (the `generativelanguage.googleapis.com` host).
const STUDIO_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// How a [`GeminiProvider`] authenticates.
///
/// `ApiKey` is AI Studio (`x-goog-api-key` against
/// `generativelanguage.googleapis.com`). `Vertex` is Vertex AI: an OAuth2
/// `Authorization: Bearer` token (minted from a service account) against the
/// `aiplatform.googleapis.com` `projects/.../locations/global` endpoint.
enum GeminiAuth {
    ApiKey(SecretString),
    Vertex {
        project: String,
        token: Arc<dyn TokenSource>,
    },
}

/// Google Gemini provider.
pub struct GeminiProvider {
    client: Client,
    auth: GeminiAuth,
    model: String,
    base_url: String,
}

impl GeminiProvider {
    /// Create a new Gemini provider authenticating with an AI Studio API key.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            auth: GeminiAuth::ApiKey(SecretString::from(api_key.into())),
            model: model.into(),
            base_url: STUDIO_BASE_URL.to_string(),
        }
    }

    /// Create a Gemini provider that calls **Vertex AI** with an OAuth2 token
    /// source. `project` is the GCP project id; the region is fixed to `global`.
    pub fn vertex(
        project: impl Into<String>,
        model: impl Into<String>,
        token: Arc<dyn TokenSource>,
    ) -> Self {
        Self {
            client: crate::provider::build_http_client(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
                crate::provider::DEFAULT_LLM_CONNECT_TIMEOUT_SECS,
            ),
            auth: GeminiAuth::Vertex {
                project: project.into(),
                token,
            },
            model: model.into(),
            // Unused in Vertex mode (endpoint is computed), kept for metadata.
            base_url: STUDIO_BASE_URL.to_string(),
        }
    }

    /// Create a Vertex-mode provider from a service account. The GCP project is
    /// taken from the service account's `project_id`.
    pub fn vertex_from_service_account(sa: ServiceAccount, model: impl Into<String>) -> Self {
        Self::vertex_from_service_account_with_timeout(sa, model, None)
    }

    /// Like [`vertex_from_service_account`], but threads the provider's HTTP
    /// timeout into the OAuth token-exchange client as well. Without this the
    /// token fetcher uses an unbounded `reqwest::Client`, so a stalled token
    /// endpoint would block a Vertex chat past the configured LLM timeout before
    /// `generateContent` is ever reached.
    pub fn vertex_from_service_account_with_timeout(
        sa: ServiceAccount,
        model: impl Into<String>,
        http_timeout: Option<(u64, u64)>,
    ) -> Self {
        let project = sa.project_id.clone();
        let token: Arc<dyn TokenSource> =
            if let Some((timeout_secs, connect_timeout_secs)) = http_timeout {
                Arc::new(VertexTokenProvider::from_service_account_with_timeout(
                    sa,
                    timeout_secs,
                    connect_timeout_secs,
                ))
            } else {
                Arc::new(VertexTokenProvider::from_service_account(sa))
            };
        Self::vertex(project, model, token)
    }

    /// Create a provider using the GEMINI_API_KEY environment variable.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_API_KEY"))
            .wrap_err("GEMINI_API_KEY or GOOGLE_API_KEY environment variable not set")?;
        Ok(Self::new(api_key, "gemini-2.5-flash"))
    }

    /// Set a custom base URL (AI Studio mode only; ignored for Vertex).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Replace the HTTP client with one using custom timeouts (in seconds).
    pub fn with_http_timeout(mut self, timeout_secs: u64, connect_timeout_secs: u64) -> Self {
        self.client = crate::provider::build_http_client(timeout_secs, connect_timeout_secs);
        self
    }

    /// Build the generateContent endpoint URL for the active auth mode.
    fn build_url(&self, streaming: bool) -> String {
        let action = if streaming {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        match &self.auth {
            GeminiAuth::ApiKey(_) => {
                format!("{}/models/{}:{}", self.base_url, self.model, action)
            }
            GeminiAuth::Vertex { project, .. } => format!(
                "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/publishers/google/models/{}:{}",
                self.model, action
            ),
        }
    }

    /// Attach the auth header for the active mode (resolving a fresh Vertex
    /// token when needed).
    async fn apply_auth(&self, req: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match &self.auth {
            GeminiAuth::ApiKey(key) => req.header("x-goog-api-key", key.expose_secret()),
            GeminiAuth::Vertex { token, .. } => {
                let t = token.token().await?;
                req.header("Authorization", format!("Bearer {t}"))
            }
        })
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let (contents, system_instruction) = build_gemini_contents(messages);

        // Build tools array
        let gemini_tools: Option<Vec<GeminiTool>> = if tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: tools
                    .iter()
                    .map(|t| {
                        let mut params = t.input_schema.clone();
                        sanitize_schema_for_gemini(&mut params);
                        GeminiFunctionDeclaration {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: params,
                        }
                    })
                    .collect(),
            }])
        };

        let request = GeminiRequest {
            contents,
            system_instruction: system_instruction.map(|text| GeminiSystemInstruction {
                parts: vec![GeminiPart::Text {
                    text,
                    thought: None,
                }],
            }),
            tools: gemini_tools,
            generation_config: Some(build_gemini_generation_config(config)),
            cached_content: None,
        };

        let url = self.build_url(false);

        let req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(
                crate::provider::DEFAULT_LLM_TIMEOUT_SECS,
            ))
            .json(&request);
        let response = self
            .apply_auth(req)
            .await?
            .send()
            .await
            .wrap_err("failed to send request to Gemini")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&body);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("gemini/{}", self.model),
            )
            .into());
        }

        let response_text = response
            .text()
            .await
            .wrap_err("failed to read Gemini response body")?;
        let api_response: GeminiResponse =
            serde_json::from_str(&response_text).wrap_err("failed to parse Gemini response")?;

        gemini_response_to_chat_response(api_response)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        let (contents, system_instruction) = build_gemini_contents(messages);

        let gemini_tools: Option<Vec<GeminiTool>> = if tools.is_empty() {
            None
        } else {
            Some(vec![GeminiTool {
                function_declarations: tools
                    .iter()
                    .map(|t| {
                        let mut params = t.input_schema.clone();
                        sanitize_schema_for_gemini(&mut params);
                        GeminiFunctionDeclaration {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: params,
                        }
                    })
                    .collect(),
            }])
        };

        let request = GeminiRequest {
            contents,
            system_instruction: system_instruction.map(|text| GeminiSystemInstruction {
                parts: vec![GeminiPart::Text {
                    text,
                    thought: None,
                }],
            }),
            tools: gemini_tools,
            generation_config: Some(build_gemini_generation_config(config)),
            cached_content: None,
        };

        let url = self.build_url(true);

        let req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request);
        let response = self
            .apply_auth(req)
            .await?
            .send()
            .await
            .wrap_err("failed to send streaming request to Gemini")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let body = crate::provider::truncate_error_body(&text);
            return Err(crate::error::LlmError::from_status_with_label(
                status.as_u16(),
                &body,
                format!("gemini/{}", self.model),
            )
            .into());
        }

        let sse_stream = crate::sse::parse_sse_response(response);
        let state = GeminiStreamState::default();
        let event_stream = sse_stream
            .scan(state, |state, event| {
                let events = map_gemini_sse(state, &event);
                futures::future::ready(Some(events))
            })
            .flat_map(futures::stream::iter);

        Ok(Box::pin(event_stream))
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn provider_name(&self) -> &str {
        // Vertex AI and AI Studio Gemini must be distinguishable for routing,
        // adaptive-lane matching and metrics — they're different backends even
        // though both use `GeminiProvider`.
        match self.auth {
            GeminiAuth::Vertex { .. } => "vertex",
            GeminiAuth::ApiKey(_) => "gemini",
        }
    }

    fn provider_metadata(&self) -> ProviderMetadata {
        let endpoint = if self.base_url != "https://generativelanguage.googleapis.com/v1beta" {
            endpoint_label_from_base_url(&self.base_url)
        } else {
            None
        };
        // Derive the metadata name from the auth mode (same as `provider_name`)
        // so Vertex calls aren't mislabelled as AI Studio gemini in provenance.
        ProviderMetadata::new(self.provider_name(), self.model.clone(), endpoint)
    }
}

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(rename = "cachedContent", skip_serializing_if = "Option::is_none")]
    cached_content: Option<String>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought: Option<bool>,
    },
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: GeminiInlineData,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
        /// Gemini thinking models require this signature to be echoed back.
        /// This is at the part level, NOT inside the functionCall object.
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct GeminiInlineData {
    #[serde(rename = "mimeType")]
    mime_type: String,
    data: String,
}

/// Build the Gemini generation config from ChatConfig.
fn build_gemini_generation_config(config: &ChatConfig) -> GeminiGenerationConfig {
    use crate::config::{ReasoningEffort, ResponseFormat};

    let thinking_config = config.reasoning_effort.map(|effort| {
        let budget = match effort {
            ReasoningEffort::Low => Some(1024),
            ReasoningEffort::Medium => Some(8192),
            // High and Max both let the model decide (unbounded thinking budget).
            ReasoningEffort::High | ReasoningEffort::Max => None,
        };
        GeminiThinkingConfig {
            thinking_budget: budget,
        }
    });

    let (response_mime_type, response_schema) = match &config.response_format {
        Some(ResponseFormat::JsonObject) => (Some("application/json".into()), None),
        Some(ResponseFormat::JsonSchema { schema, .. }) => {
            let mut s = schema.clone();
            sanitize_schema_for_gemini(&mut s);
            (Some("application/json".into()), Some(s))
        }
        _ => (None, None),
    };

    GeminiGenerationConfig {
        max_output_tokens: config.max_tokens,
        temperature: config.temperature,
        thinking_config,
        response_mime_type,
        response_schema,
    }
}

/// Build the Gemini `contents` array and optional system instruction from messages.
///
/// Gemini requires:
/// - Assistant messages with tool calls → `model` role with `functionCall` parts
/// - Tool result messages → `user` role with `functionResponse` parts
/// - Consecutive same-role messages are merged (Gemini rejects adjacent same-role turns)
fn build_gemini_contents(messages: &[Message]) -> (Vec<GeminiContent>, Option<String>) {
    let mut contents: Vec<GeminiContent> = Vec::new();
    let mut system_instruction: Option<String> = None;

    // Map tool_call_id → function name so tool results can reference the right name.
    let mut call_id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for msg in messages {
        match msg.role {
            octos_core::MessageRole::System => match &mut system_instruction {
                Some(existing) => {
                    existing.push_str("\n\n");
                    existing.push_str(&msg.content);
                }
                None => {
                    system_instruction = Some(msg.content.clone());
                }
            },
            octos_core::MessageRole::User => {
                let parts = build_user_parts(msg);
                push_or_merge(&mut contents, "user", parts);
            }
            octos_core::MessageRole::Assistant => {
                let mut parts = Vec::new();
                // Include text content if non-empty.
                if !msg.content.is_empty() {
                    parts.push(GeminiPart::Text {
                        text: msg.content.clone(),
                        thought: None,
                    });
                }
                // Include functionCall parts for any tool calls the model made.
                if let Some(ref tcs) = msg.tool_calls {
                    for tc in tcs {
                        call_id_to_name.insert(tc.id.clone(), tc.name.clone());
                        // Restore thought_signature from metadata if present.
                        let thought_signature = tc
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("thought_signature"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        parts.push(GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: tc.name.clone(),
                                args: tc.arguments.clone(),
                            },
                            thought_signature,
                        });
                    }
                }
                // Gemini requires at least one part; add empty text if everything was empty.
                if parts.is_empty() {
                    parts.push(GeminiPart::Text {
                        text: String::new(),
                        thought: None,
                    });
                }
                push_or_merge(&mut contents, "model", parts);
            }
            octos_core::MessageRole::Tool => {
                // Resolve function name from the matching tool call.
                let name = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| call_id_to_name.get(id))
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());

                let part = GeminiPart::FunctionResponse {
                    function_response: GeminiFunctionResponse {
                        name,
                        response: serde_json::json!({ "content": msg.content }),
                    },
                };
                push_or_merge(&mut contents, "user", vec![part]);
            }
        }
    }
    (contents, system_instruction)
}

/// Merge parts into the last content entry if roles match (Gemini rejects adjacent same-role).
///
/// However, Gemini also silently fails when `functionResponse` parts are mixed with `text`
/// parts in the same turn. To avoid this, we only merge parts of compatible types:
/// functionResponse parts merge with other functionResponse parts, and text/inlineData
/// parts merge with other text/inlineData parts.
fn push_or_merge(contents: &mut Vec<GeminiContent>, role: &str, parts: Vec<GeminiPart>) {
    if let Some(last) = contents.last_mut() {
        if last.role == role && parts_compatible(&last.parts, &parts) {
            last.parts.extend(parts);
            return;
        }
    }
    contents.push(GeminiContent {
        role: role.to_string(),
        parts,
    });
}

/// Check if two sets of parts can be merged without mixing incompatible types.
fn parts_compatible(existing: &[GeminiPart], new: &[GeminiPart]) -> bool {
    let existing_has_func_response = existing
        .iter()
        .any(|p| matches!(p, GeminiPart::FunctionResponse { .. }));
    let new_has_func_response = new
        .iter()
        .any(|p| matches!(p, GeminiPart::FunctionResponse { .. }));
    let existing_has_text = existing
        .iter()
        .any(|p| matches!(p, GeminiPart::Text { .. } | GeminiPart::InlineData { .. }));
    let new_has_text = new
        .iter()
        .any(|p| matches!(p, GeminiPart::Text { .. } | GeminiPart::InlineData { .. }));

    // Don't merge if one side has functionResponse and the other has text
    !((existing_has_func_response && new_has_text) || (existing_has_text && new_has_func_response))
}

fn build_user_parts(msg: &Message) -> Vec<GeminiPart> {
    let images: Vec<_> = msg.media.iter().filter(|p| vision::is_image(p)).collect();

    if images.is_empty() {
        return vec![GeminiPart::Text {
            text: msg.content.clone(),
            thought: None,
        }];
    }

    let mut parts = Vec::new();
    for path in images {
        if let Ok((mime, data)) = vision::encode_image(path) {
            parts.push(GeminiPart::InlineData {
                inline_data: GeminiInlineData {
                    mime_type: mime,
                    data,
                },
            });
        }
    }
    if !msg.content.is_empty() {
        parts.push(GeminiPart::Text {
            text: msg.content.clone(),
            thought: None,
        });
    }
    parts
}

#[derive(Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Serialize)]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// Maximum recursion depth for schema sanitization (matches MCP limit).
const MAX_SCHEMA_DEPTH: usize = 64;

/// Sanitize a JSON Schema for Gemini's restricted schema support.
///
/// Gemini only supports a subset of JSON Schema. This recursively removes
/// unsupported fields that cause 400 errors or silent empty responses:
/// - `additionalProperties`
/// - Empty `items` schemas (`"items": {}`)
/// - `$schema`, `$ref`, `$id`
fn sanitize_schema_for_gemini(value: &mut serde_json::Value) {
    sanitize_schema_recursive(value, 0);
}

fn sanitize_schema_recursive(value: &mut serde_json::Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }

    if let Some(obj) = value.as_object_mut() {
        obj.remove("additionalProperties");
        obj.remove("$schema");
        obj.remove("$ref");
        obj.remove("$id");

        // Gemini's tool API rejects unknown field names with HTTP 400
        // ("Unknown name <field>"), even for the conventional `x-*` JSON
        // Schema vendor-extension namespace. Strip them before the schema
        // reaches the wire so host-only metadata (e.g. octos's
        // `x-octos-host-config-keys` on the deep-search manifest) doesn't
        // crash plan_and_search workers when routing lands on Gemini.
        obj.retain(|k, _| !k.starts_with("x-"));

        // Gemini requires `items` to have a type when present.
        // Replace empty `"items": {}` with `"items": {"type": "string"}`.
        if let Some(items) = obj.get("items") {
            if items.as_object().is_some_and(|o| o.is_empty()) {
                obj.insert("items".to_string(), serde_json::json!({"type": "string"}));
            }
        }

        // Recurse into nested objects
        let keys: Vec<String> = obj.keys().cloned().collect();
        for key in keys {
            if let Some(v) = obj.get_mut(&key) {
                sanitize_schema_recursive(v, depth + 1);
            }
        }
    } else if let Some(arr) = value.as_array_mut() {
        for item in arr {
            sanitize_schema_recursive(item, depth + 1);
        }
    }
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
    #[serde(rename = "responseMimeType", skip_serializing_if = "Option::is_none")]
    response_mime_type: Option<String>,
    #[serde(rename = "responseSchema", skip_serializing_if = "Option::is_none")]
    response_schema: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget", skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u32>,
}

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
    #[serde(rename = "thoughtsTokenCount", default)]
    thoughts_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    cached_content_token_count: u32,
}

fn append_nonempty(target: &mut Option<String>, text: String) {
    if text.is_empty() {
        return;
    }
    match target {
        Some(existing) => existing.push_str(&text),
        None => *target = Some(text),
    }
}

fn gemini_response_to_chat_response(api_response: GeminiResponse) -> Result<ChatResponse> {
    let GeminiResponse {
        candidates,
        usage_metadata,
    } = api_response;

    let candidate = candidates
        .into_iter()
        .next()
        .ok_or_else(|| eyre::eyre!("no candidates in Gemini response"))?;

    let mut content = None;
    let mut reasoning_content = None;
    let mut tool_calls = Vec::new();

    for part in candidate.content.parts {
        match part {
            GeminiPart::Text { text, thought } => {
                if thought.unwrap_or(false) {
                    append_nonempty(&mut reasoning_content, text);
                } else {
                    append_nonempty(&mut content, text);
                }
            }
            GeminiPart::FunctionCall {
                function_call,
                thought_signature,
            } => {
                let metadata =
                    thought_signature.map(|sig| serde_json::json!({ "thought_signature": sig }));
                tool_calls.push(octos_core::ToolCall {
                    id: next_gemini_tool_call_id(),
                    name: function_call.name,
                    arguments: function_call.args,
                    metadata,
                });
            }
            GeminiPart::InlineData { .. } | GeminiPart::FunctionResponse { .. } => {
                // InlineData and FunctionResponse are only used in requests.
            }
        }
    }

    let stop_reason = match candidate.finish_reason.as_deref() {
        Some("STOP") => StopReason::EndTurn,
        Some("MAX_TOKENS") => StopReason::MaxTokens,
        Some("SAFETY" | "RECITATION" | "OTHER" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            StopReason::ContentFiltered
        }
        Some("MALFORMED_FUNCTION_CALL") => {
            // Gemini sometimes fails to format tool calls properly.
            // Treat as empty response so the retry logic picks it up.
            tracing::warn!("Gemini returned MALFORMED_FUNCTION_CALL");
            StopReason::EndTurn
        }
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };

    let usage = usage_metadata.unwrap_or(GeminiUsageMetadata {
        prompt_token_count: 0,
        candidates_token_count: 0,
        thoughts_token_count: 0,
        cached_content_token_count: 0,
    });

    Ok(ChatResponse {
        content,
        reasoning_content,
        tool_calls,
        stop_reason,
        usage: TokenUsage {
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            reasoning_tokens: usage.thoughts_token_count,
            cache_read_tokens: usage.cached_content_token_count,
            ..Default::default()
        },
        provider_index: None,
    })
}

// --- Streaming SSE helpers ---

#[derive(Default)]
struct GeminiStreamState {
    tool_count: usize,
    has_tool_calls: bool,
}

// Visible for testing
fn map_gemini_sse(state: &mut GeminiStreamState, event: &crate::sse::SseEvent) -> Vec<StreamEvent> {
    let data: serde_json::Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut events = Vec::new();

    if let Some(candidates) = data["candidates"].as_array() {
        if let Some(candidate) = candidates.first() {
            if let Some(parts) = candidate["content"]["parts"].as_array() {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        if !text.is_empty() {
                            if part["thought"].as_bool().unwrap_or(false) {
                                events.push(StreamEvent::ReasoningDelta(text.to_string()));
                            } else {
                                events.push(StreamEvent::TextDelta(text.to_string()));
                            }
                        }
                    }
                    if let Some(fc) = part.get("functionCall") {
                        state.has_tool_calls = true;
                        let name = fc["name"].as_str().unwrap_or("").to_string();
                        let args = fc
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        // Capture thought_signature for Gemini thinking models.
                        // thoughtSignature is at the part level, not inside functionCall.
                        let thought_sig = part
                            .get("thoughtSignature")
                            .and_then(|v| v.as_str())
                            .map(|s| serde_json::json!({ "thought_signature": s }));
                        events.push(StreamEvent::ToolCallDelta {
                            index: state.tool_count,
                            id: Some(next_gemini_tool_call_id()),
                            name: Some(name),
                            arguments_delta: args.to_string(),
                        });
                        // Emit metadata as a separate event so the agent can store it.
                        if let Some(meta) = thought_sig {
                            events.push(StreamEvent::ToolCallMetadata {
                                index: state.tool_count,
                                metadata: meta,
                            });
                        }
                        state.tool_count += 1;
                    }
                }
            }

            if let Some(reason) = candidate["finishReason"].as_str() {
                let stop_reason = match reason {
                    "STOP" if state.has_tool_calls => StopReason::ToolUse,
                    "STOP" => StopReason::EndTurn,
                    "MAX_TOKENS" => StopReason::MaxTokens,
                    "SAFETY" | "RECITATION" | "OTHER" | "BLOCKLIST" | "PROHIBITED_CONTENT" => {
                        StopReason::ContentFiltered
                    }
                    "MALFORMED_FUNCTION_CALL" => {
                        tracing::warn!("Gemini returned MALFORMED_FUNCTION_CALL (streaming)");
                        StopReason::EndTurn
                    }
                    _ if state.has_tool_calls => StopReason::ToolUse,
                    _ => StopReason::EndTurn,
                };
                events.push(StreamEvent::Done(stop_reason));
            }
        }
    }

    if let Some(usage) = data.get("usageMetadata").filter(|u| !u.is_null()) {
        let input = usage["promptTokenCount"].as_u64().unwrap_or(0) as u32;
        let output = usage["candidatesTokenCount"].as_u64().unwrap_or(0) as u32;
        let thinking = usage["thoughtsTokenCount"].as_u64().unwrap_or(0) as u32;
        let cached = usage["cachedContentTokenCount"].as_u64().unwrap_or(0) as u32;
        if input > 0 || output > 0 {
            events.push(StreamEvent::Usage(TokenUsage {
                input_tokens: input,
                output_tokens: output,
                reasoning_tokens: thinking,
                cache_read_tokens: cached,
                ..Default::default()
            }));
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::{Message, MessageRole, ToolCall};

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    // --- sanitize_schema_for_gemini tests ---

    #[test]
    fn test_sanitize_removes_additional_properties() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "additionalProperties": false
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn test_sanitize_removes_dollar_fields() {
        let mut schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "$ref": "#/definitions/Foo",
            "$id": "my-schema",
            "type": "object"
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("$ref").is_none());
        assert!(schema.get("$id").is_none());
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn test_sanitize_strips_x_extension_keys() {
        // Pins the fix for the deep-search → Gemini 400 regression where
        // `x-octos-host-config-keys` in input_schema crashed plan_and_search
        // workers. Gemini's tool API rejects unknown field names (including
        // the conventional `x-*` extension namespace) with HTTP 400.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "synthesis_config": {"type": "object"},
                "query": {"type": "string"}
            },
            "x-octos-host-config-keys": ["synthesis_config"],
            "x-some-other-extension": {"nested": true}
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(schema.get("x-octos-host-config-keys").is_none());
        assert!(schema.get("x-some-other-extension").is_none());
        // Non-x-prefixed fields preserved.
        assert!(schema.get("type").is_some());
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_sanitize_replaces_empty_items() {
        let mut schema = serde_json::json!({
            "type": "array",
            "items": {}
        });
        sanitize_schema_for_gemini(&mut schema);
        assert_eq!(schema["items"]["type"], "string");
    }

    #[test]
    fn test_sanitize_preserves_non_empty_items() {
        let mut schema = serde_json::json!({
            "type": "array",
            "items": {"type": "integer"}
        });
        sanitize_schema_for_gemini(&mut schema);
        assert_eq!(schema["items"]["type"], "integer");
    }

    #[test]
    fn test_sanitize_recursive() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "list": {
                            "type": "array",
                            "items": {}
                        }
                    }
                }
            }
        });
        sanitize_schema_for_gemini(&mut schema);
        assert!(
            schema["properties"]["nested"]
                .get("additionalProperties")
                .is_none()
        );
        assert_eq!(
            schema["properties"]["nested"]["properties"]["list"]["items"]["type"],
            "string"
        );
    }

    // --- build_gemini_contents tests ---

    #[test]
    fn test_build_contents_system_extracted() {
        let messages = vec![
            msg(MessageRole::System, "You are helpful"),
            msg(MessageRole::User, "Hi"),
        ];
        let (contents, system) = build_gemini_contents(&messages);
        assert_eq!(system.as_deref(), Some("You are helpful"));
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn test_build_contents_assistant_mapped_to_model() {
        let messages = vec![
            msg(MessageRole::User, "Hi"),
            msg(MessageRole::Assistant, "Hello!"),
        ];
        let (contents, _) = build_gemini_contents(&messages);
        assert_eq!(contents[1].role, "model");
    }

    #[test]
    fn test_build_contents_tool_call_and_result() {
        let messages = vec![
            msg(MessageRole::User, "read file"),
            Message {
                role: MessageRole::Assistant,
                content: String::new(),
                media: vec![],
                tool_calls: Some(vec![ToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "foo.rs"}),
                    metadata: None,
                }]),
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
            Message {
                role: MessageRole::Tool,
                content: "file contents".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: Some("tc1".into()),
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        let (contents, _) = build_gemini_contents(&messages);
        // user, model (with functionCall), user (with functionResponse)
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1].role, "model");
        assert_eq!(contents[2].role, "user");
    }

    #[test]
    fn test_build_contents_merges_consecutive_same_role() {
        let messages = vec![
            msg(MessageRole::User, "first"),
            msg(MessageRole::User, "second"),
        ];
        let (contents, _) = build_gemini_contents(&messages);
        // Should merge into 1 user turn
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts.len(), 2);
    }

    #[test]
    fn test_parts_compatible_blocks_mixed_types() {
        let text = vec![GeminiPart::Text {
            text: "hi".into(),
            thought: None,
        }];
        let func_resp = vec![GeminiPart::FunctionResponse {
            function_response: GeminiFunctionResponse {
                name: "test".into(),
                response: serde_json::json!({"content": "ok"}),
            },
        }];
        assert!(!parts_compatible(&text, &func_resp));
        assert!(!parts_compatible(&func_resp, &text));
        assert!(parts_compatible(&text, &text));
        assert!(parts_compatible(&func_resp, &func_resp));
    }

    // --- SSE mapping tests ---

    #[test]
    fn test_gemini_sse_text_delta() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"text": "Hello"}]}}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "Hello"));
    }

    #[test]
    fn test_gemini_sse_thought_text_delta() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"text": "Check the invariants.", "thought": true}]}}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta(t) if t == "Check the invariants.")
        );
    }

    #[test]
    fn test_gemini_sse_function_call() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "shell", "args": {"command": "ls"}}}]}}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolCallDelta { name, .. } if name.as_deref() == Some("shell"))));
        assert!(state.has_tool_calls);
    }

    #[test]
    fn test_gemini_sse_finish_reason() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": [{"text": "done"}]}, "finishReason": "STOP"}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Done(StopReason::EndTurn)))
        );
    }

    #[test]
    fn test_gemini_sse_finish_with_tools() {
        let mut state = GeminiStreamState {
            has_tool_calls: true,
            ..Default::default()
        };
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"candidates": [{"content": {"parts": []}, "finishReason": "STOP"}]}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Done(StopReason::ToolUse)))
        );
    }

    fn first_tool_call_id(events: Vec<StreamEvent>) -> String {
        events
            .into_iter()
            .find_map(|e| match e {
                StreamEvent::ToolCallDelta { id, .. } => id,
                _ => None,
            })
            .expect("a ToolCallDelta carrying an id")
    }

    /// Regression (codex round-3 / fix/orphan-sweep-liveness-gate): Gemini
    /// synthesizes tool-call ids (its API supplies none — it matches results
    /// by function name). Those ids MUST be PROCESS-UNIQUE, not positional. A
    /// positional `call_{index}` resets every response, so two unrelated tool
    /// calls in different turns both get `call_0` — colliding in the
    /// supervisor's per-session synth-ack set and the orphan-sweep
    /// tool_call_id-family exemption (a live task's tcid would then falsely
    /// exempt a genuinely-dead task from reaping). Two independent SSE
    /// responses (each a fresh state with tool_count back at 0) must mint
    /// disjoint ids.
    #[test]
    fn synthesized_gemini_tool_call_ids_are_unique_across_responses() {
        let fc = r#"{"candidates": [{"content": {"parts": [{"functionCall": {"name": "search", "args": {}}}]}}]}"#;

        let mut resp1 = GeminiStreamState::default();
        let id1 = first_tool_call_id(map_gemini_sse(
            &mut resp1,
            &crate::sse::SseEvent {
                event: None,
                data: fc.into(),
            },
        ));

        let mut resp2 = GeminiStreamState::default();
        let id2 = first_tool_call_id(map_gemini_sse(
            &mut resp2,
            &crate::sse::SseEvent {
                event: None,
                data: fc.into(),
            },
        ));

        assert!(id1.starts_with("call_gemini_"), "got {id1}");
        assert_ne!(
            id1, id2,
            "synthesized ids must be process-unique across responses, not positional",
        );
    }

    #[test]
    fn test_gemini_sse_usage() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50}}"#
                .into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.input_tokens == 100 && u.output_tokens == 50)
        ));
    }

    #[test]
    fn test_gemini_sse_invalid_json() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: "not valid json".into(),
        };
        assert!(map_gemini_sse(&mut state, &event).is_empty());
    }

    #[test]
    fn test_gemini_response_captures_thought_parts() {
        let api_response: GeminiResponse = serde_json::from_value(serde_json::json!({
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [
                            {
                                "text": "Inspect the constraints.",
                                "thought": true
                            },
                            {
                                "text": "Return the concise answer."
                            }
                        ]
                    },
                    "finishReason": "STOP"
                }
            ],
            "usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 22,
                "thoughtsTokenCount": 7
            }
        }))
        .unwrap();

        let response = gemini_response_to_chat_response(api_response).unwrap();
        assert_eq!(
            response.reasoning_content.as_deref(),
            Some("Inspect the constraints.")
        );
        assert_eq!(
            response.content.as_deref(),
            Some("Return the concise answer.")
        );
        assert_eq!(response.usage.reasoning_tokens, 7);
    }

    // --- Provider metadata tests ---

    #[test]
    fn test_provider_name_and_model() {
        let provider = GeminiProvider::new("test-key", "gemini-2.5-flash");
        assert_eq!(provider.provider_name(), "gemini");
        assert_eq!(provider.model_id(), "gemini-2.5-flash");
    }

    #[test]
    fn test_with_base_url() {
        let provider =
            GeminiProvider::new("key", "model").with_base_url("https://custom.googleapis.com");
        assert_eq!(provider.base_url, "https://custom.googleapis.com");
    }

    // --- Vertex / Gemini auth-mode tests ---

    struct StaticToken(&'static str);

    #[async_trait::async_trait]
    impl crate::vertex_auth::TokenSource for StaticToken {
        async fn token(&self) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    #[test]
    fn should_build_global_vertex_endpoint_when_vertex_mode() {
        let provider =
            GeminiProvider::vertex("my-proj", "gemini-2.5-flash", Arc::new(StaticToken("tok")));
        assert_eq!(
            provider.build_url(false),
            "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global/publishers/google/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            provider.build_url(true),
            "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global/publishers/google/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn provider_name_distinguishes_vertex_from_studio_gemini() {
        use crate::provider::LlmProvider;
        let vertex = GeminiProvider::vertex("p", "m", Arc::new(StaticToken("tok")));
        let studio = GeminiProvider::new("key", "gemini-2.5-flash");
        assert_eq!(vertex.provider_name(), "vertex");
        assert_eq!(studio.provider_name(), "gemini");
        // provider_metadata must agree with provider_name (provenance label).
        assert_eq!(vertex.provider_metadata().provider, "vertex");
        assert_eq!(studio.provider_metadata().provider, "gemini");
    }

    #[test]
    fn should_build_studio_endpoint_when_api_key_mode() {
        let provider = GeminiProvider::new("key", "gemini-2.5-flash");
        assert_eq!(
            provider.build_url(false),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
    }

    #[tokio::test]
    async fn should_send_bearer_header_when_vertex_mode() {
        let provider = GeminiProvider::vertex("p", "m", Arc::new(StaticToken("abc123")));
        let req = provider.client.post("https://example.com");
        let built = provider.apply_auth(req).await.unwrap().build().unwrap();
        assert_eq!(
            built.headers().get("Authorization").unwrap(),
            "Bearer abc123"
        );
        assert!(
            built.headers().get("x-goog-api-key").is_none(),
            "Vertex mode must not send the AI Studio api-key header"
        );
    }

    #[tokio::test]
    async fn should_send_api_key_header_when_studio_mode() {
        let provider = GeminiProvider::new("secret-key", "m");
        let req = provider.client.post("https://example.com");
        let built = provider.apply_auth(req).await.unwrap().build().unwrap();
        assert_eq!(built.headers().get("x-goog-api-key").unwrap(), "secret-key");
        assert!(built.headers().get("Authorization").is_none());
    }

    // --- Generation config tests ---

    #[test]
    fn test_thinking_config_low_effort() {
        use crate::config::ReasoningEffort;
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::Low),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        let tc = gen_config.thinking_config.unwrap();
        assert_eq!(tc.thinking_budget, Some(1024));
    }

    #[test]
    fn test_thinking_config_high_effort() {
        use crate::config::ReasoningEffort;
        let config = ChatConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        let tc = gen_config.thinking_config.unwrap();
        assert!(tc.thinking_budget.is_none());
    }

    #[test]
    fn test_no_thinking_config_by_default() {
        let config = ChatConfig::default();
        let gen_config = build_gemini_generation_config(&config);
        assert!(gen_config.thinking_config.is_none());
    }

    #[test]
    fn test_response_format_json_object() {
        use crate::config::ResponseFormat;
        let config = ChatConfig {
            response_format: Some(ResponseFormat::JsonObject),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        assert_eq!(
            gen_config.response_mime_type.as_deref(),
            Some("application/json")
        );
        assert!(gen_config.response_schema.is_none());
    }

    #[test]
    fn test_response_format_json_schema() {
        use crate::config::ResponseFormat;
        let config = ChatConfig {
            response_format: Some(ResponseFormat::JsonSchema {
                name: "test".into(),
                schema: serde_json::json!({"type": "object", "additionalProperties": false}),
                strict: true,
            }),
            ..Default::default()
        };
        let gen_config = build_gemini_generation_config(&config);
        assert_eq!(
            gen_config.response_mime_type.as_deref(),
            Some("application/json")
        );
        // additionalProperties should be sanitized away
        let schema = gen_config.response_schema.unwrap();
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn test_gemini_sse_usage_with_thinking_tokens() {
        let mut state = GeminiStreamState::default();
        let event = crate::sse::SseEvent {
            event: None,
            data: r#"{"usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50, "thoughtsTokenCount": 20, "cachedContentTokenCount": 30}}"#.into(),
        };
        let events = map_gemini_sse(&mut state, &event);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::Usage(u) if u.reasoning_tokens == 20 && u.cache_read_tokens == 30)
        ));
    }
}
