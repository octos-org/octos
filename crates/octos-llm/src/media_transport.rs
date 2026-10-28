//! Provider-scoped image transport. This layer changes transport, never image
//! bytes, prompt order, transcript history, or the model's tools.
//!
//! File references are cached in memory across provider instances and workers.
//! Namespace = endpoint + credential fingerprint + adapter; key = MIME + bytes.
//! No credential, image bytes or provider file IDs are persisted to disk here.

use base64::Engine;
use eyre::{Result, WrapErr, eyre};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex as AsyncMutex;

const MAX_SCOPES: usize = 128;
const MAX_IMAGES: usize = 4096;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const UNKNOWN_EXPIRY_TTL: Duration = Duration::from_secs(3600);
const UNSUPPORTED_TTL: Duration = Duration::from_secs(600);

/// Route policy, independent of the selected model name. Unknown endpoints
/// stay inline in Auto. Explicit adapters are for operator-configured proxies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaTransportMode {
    #[default]
    Auto,
    Inline,
    MoonshotFiles,
    #[serde(rename = "deepseek_files")]
    DeepSeekFiles,
}

impl MediaTransportMode {
    /// Exact parsed origin/path matching: a provider-looking model name or a
    /// substring in an untrusted hostname never enables a file protocol.
    pub fn resolve(self, base_url: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
            return Self::Inline;
        };
        if url.scheme() != "https"
            || url.port_or_known_default() != Some(443)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Self::Inline;
        }
        match (
            url.host_str().unwrap_or(""),
            url.path().trim_end_matches('/'),
        ) {
            ("api.kimi.com" | "api.kimi.ai", "/coding/v1")
            | ("api.moonshot.cn" | "api.moonshot.ai", "/v1") => Self::MoonshotFiles,
            ("api.deepseek.com", "" | "/v1") => Self::DeepSeekFiles,
            _ => Self::Inline,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ScopeKey {
    endpoint: String,
    credential: [u8; 32],
    adapter: MediaTransportMode,
}

#[derive(Default)]
struct Scope {
    images: Mutex<HashMap<String, Arc<AsyncMutex<Option<Reference>>>>>,
    unsupported_until: Mutex<Option<Instant>>,
    // A proxy can accept references for one model but not another. Never
    // disable all models merely because one model rejects a content type.
    rejected_models: Mutex<HashMap<String, Instant>>,
}

#[derive(Clone)]
struct Reference {
    id: String,
    usable_until: Instant,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn scope(endpoint: &str, key: &str, adapter: MediaTransportMode) -> Option<Arc<Scope>> {
    static SCOPES: OnceLock<Mutex<HashMap<ScopeKey, Arc<Scope>>>> = OnceLock::new();
    let mut scopes = lock(SCOPES.get_or_init(Mutex::default));
    let k = ScopeKey {
        endpoint: endpoint.into(),
        credential: Sha256::digest(key.as_bytes()).into(),
        adapter,
    };
    if let Some(s) = scopes.get(&k) {
        return Some(s.clone());
    }
    if scopes.len() >= MAX_SCOPES {
        let idle = scopes
            .iter()
            .find(|(_, s)| Arc::strong_count(s) == 1)
            .map(|(k, _)| k.clone())?;
        scopes.remove(&idle);
    }
    let s = Arc::new(Scope::default());
    scopes.insert(k, s.clone());
    Some(s)
}

/// One request's transport context. The provider passes the actual resolved
/// endpoint and credential, so custom routes and failover cannot borrow another
/// account's file IDs. Model capabilities can opt out without changing adapters.
pub(crate) struct ImageTransport<'a> {
    endpoint: String,
    api_key: &'a str,
    mode: MediaTransportMode,
    client: &'a Client,
    scope: Option<Arc<Scope>>,
}

struct UsedReference {
    key: String,
    id: String,
}

impl<'a> ImageTransport<'a> {
    pub fn new(base: &str, key: &'a str, mode: MediaTransportMode, client: &'a Client) -> Self {
        let endpoint = reqwest::Url::parse(base.trim())
            .map(|u| u.to_string())
            .unwrap_or_else(|_| base.trim().into())
            .trim_end_matches('/')
            .to_string();
        let mode = mode.resolve(&endpoint);
        let scope = matches!(
            mode,
            MediaTransportMode::MoonshotFiles | MediaTransportMode::DeepSeekFiles
        )
        .then(|| scope(&endpoint, key, mode))
        .flatten();
        Self {
            endpoint,
            api_key: key,
            mode,
            client,
            scope,
        }
    }

    fn enabled(&self, model: &str) -> bool {
        self.scope.as_ref().is_some_and(|s| {
            !lock(&s.unsupported_until).is_some_and(|t| t > Instant::now())
                && !lock(&s.rejected_models)
                    .get(model)
                    .is_some_and(|t| *t > Instant::now())
        })
    }

    async fn file(&self, mime: &str, raw: Vec<u8>) -> Result<Option<UsedReference>> {
        let s = self.scope.as_ref().expect("file transport enabled");
        let digest = format!("{:x}", Sha256::digest(&raw));
        let key = format!("{mime}:{digest}");
        let cell = {
            let mut entries = lock(&s.images);
            if entries.len() >= MAX_IMAGES && !entries.contains_key(&key) {
                if let Some(idle) = entries
                    .iter()
                    .find(|(_, c)| Arc::strong_count(c) == 1)
                    .map(|(k, _)| k.clone())
                {
                    entries.remove(&idle);
                } else {
                    return Ok(None);
                }
            }
            entries.entry(key.clone()).or_default().clone()
        };
        // Cancellation drops this guard; it cannot leave a stuck "uploading"
        // promise. Different images can upload concurrently.
        let mut cached = cell.lock().await;
        if let Some(r) = cached.as_ref().filter(|r| r.usable_until > Instant::now()) {
            tracing::debug!(target: "octos::media_transport", adapter=?self.mode, event="reuse");
            return Ok(Some(UsedReference {
                key,
                id: r.id.clone(),
            }));
        }
        if lock(&s.unsupported_until).is_some_and(|t| t > Instant::now()) {
            return Ok(None);
        }
        let raw_size = raw.len();
        let suffix = mime.strip_prefix("image/").unwrap_or("bin");
        let part = reqwest::multipart::Part::bytes(raw)
            .file_name(format!("{digest}.{suffix}"))
            .mime_str(mime)?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        let upload_base = match self.mode {
            MediaTransportMode::MoonshotFiles => {
                form = form.text("purpose", "image");
                self.endpoint.as_str()
            }
            MediaTransportMode::DeepSeekFiles => {
                form = form
                    .text("purpose", "user_data")
                    .text("expires_after[anchor]", "created_at")
                    .text("expires_after[seconds]", "86400");
                self.endpoint.strip_suffix("/v1").unwrap_or(&self.endpoint)
            }
            MediaTransportMode::Auto | MediaTransportMode::Inline => {
                return Err(eyre!(
                    "image upload requires a resolved file transport mode"
                ));
            }
        };
        let response = self
            .client
            .post(format!("{upload_base}/files"))
            .bearer_auth(self.api_key)
            .multipart(form)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .wrap_err("image upload transport failed")?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            // Read a bounded error body. Never mistake auth, rate limiting or
            // quota errors for absence of the Files capability.
            let (_, _, bytes) = buffer_error(response).await?;
            let text = String::from_utf8_lossy(&bytes);
            if upload_unsupported(status, &text)
                && crate::current_llm_call_policy() != crate::LlmCallPolicy::FailFast
            {
                *lock(&s.unsupported_until) = Some(Instant::now() + UNSUPPORTED_TTL);
                tracing::warn!(target:"octos::media_transport", adapter=?self.mode, status, "Files unavailable; preserving image with inline transport");
                return Ok(None);
            }
            return Err(crate::error::LlmError::from_status_with_label(
                status,
                &crate::provider::truncate_error_body(&text),
                "media upload",
            )
            .with_api_style(crate::provider::ApiStyle::OpenAiChatCompletions)
            .into());
        }
        let data: Value = response
            .json()
            .await
            .wrap_err("invalid image upload response")?;
        let id = data["id"]
            .as_str()
            .filter(|v| {
                !v.is_empty()
                    && v.len() <= 256
                    && v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or_else(|| eyre!("image upload returned invalid file ID"))?
            .to_string();
        for field in ["bytes", "size_bytes"] {
            if let Some(size) = data.get(field).and_then(Value::as_u64) {
                eyre::ensure!(size == raw_size as u64, "uploaded image size mismatch");
            }
        }
        let ttl = expiry_ttl(&data)?;
        *cached = Some(Reference {
            id: id.clone(),
            usable_until: Instant::now() + ttl,
        });
        tracing::debug!(target:"octos::media_transport", adapter=?self.mode, bytes=raw_size, event="upload");
        Ok(Some(UsedReference { key, id }))
    }

    async fn prepare(&self, body: &mut Value) -> Result<Vec<UsedReference>> {
        let model = body["model"].as_str().unwrap_or("");
        if !self.enabled(model) {
            return Ok(vec![]);
        }
        let mut used = vec![];
        let Some(messages) = body["messages"].as_array_mut() else {
            return Ok(used);
        };
        for message in messages {
            // User and tool-produced media are already normalized by the
            // serializer. Do not interpret strings, tool arguments or URLs.
            if message["role"] != "user" {
                continue;
            }
            let Some(parts) = message["content"].as_array_mut() else {
                continue;
            };
            for part in parts {
                if part["type"] != "image_url" || part["image_url"]["detail"] == "low" {
                    continue;
                }
                let Some(uri) = part["image_url"]["url"].as_str() else {
                    continue;
                };
                let Some((mime, encoded)) = uri
                    .strip_prefix("data:")
                    .and_then(|s| s.split_once(";base64,"))
                else {
                    continue;
                };
                if !matches!(
                    mime,
                    "image/png"
                        | "image/jpeg"
                        | "image/gif"
                        | "image/webp"
                        | "image/bmp"
                        | "image/heic"
                        | "image/heif"
                ) {
                    continue;
                }
                if encoded.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
                    continue;
                }
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .wrap_err("invalid image base64")?;
                if raw.len() > MAX_IMAGE_BYTES {
                    continue;
                }
                // DeepSeek's file part cannot carry arbitrary image options.
                // Preserve any non-default options by leaving that part inline.
                if self.mode == MediaTransportMode::DeepSeekFiles
                    && part["image_url"]
                        .as_object()
                        .is_some_and(|o| o.keys().any(|k| k != "url"))
                {
                    continue;
                }
                if let Some(reference) = self.file(mime, raw).await? {
                    match self.mode {
                        MediaTransportMode::MoonshotFiles => {
                            part["image_url"]["url"] = format!("ms://{}", reference.id).into()
                        }
                        MediaTransportMode::DeepSeekFiles => {
                            *part = serde_json::json!({"type":"file","file_id":reference.id})
                        }
                        MediaTransportMode::Auto | MediaTransportMode::Inline => {
                            return Err(eyre!(
                                "image reference requires a resolved file transport mode"
                            ));
                        }
                    }
                    used.push(reference);
                }
            }
        }
        Ok(used)
    }

    async fn invalidate(&self, refs: &[UsedReference]) {
        let Some(s) = &self.scope else {
            return;
        };
        for r in refs {
            let cell = lock(&s.images).get(&r.key).cloned();
            if let Some(cell) = cell {
                let mut cached = cell.lock().await;
                // A concurrent request may already have refreshed this entry.
                if cached.as_ref().is_some_and(|c| c.id == r.id) {
                    *cached = None;
                }
            }
        }
    }

    /// One bounded recovery for a rejected file reference. A generic 404,
    /// auth/quota/server error or partial successful stream is never replayed.
    pub async fn send(
        &self,
        chat_client: &Client,
        original: Value,
        allow_references: bool,
    ) -> Result<Response> {
        // Text-only calls and unsupported/disabled routes keep their normal
        // fast path: do not clone large histories just to discover no images.
        let has_images = original["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|m| {
                m["role"] == "user"
                    && m["content"].as_array().is_some_and(|parts| {
                        parts.iter().any(|p| {
                            p["type"] == "image_url"
                                && p["image_url"]["url"]
                                    .as_str()
                                    .is_some_and(|u| u.starts_with("data:"))
                        })
                    })
            })
        });
        if !allow_references
            || !has_images
            || !self.enabled(original["model"].as_str().unwrap_or(""))
        {
            return self.post(chat_client, &original).await;
        }
        let mut body = original.clone();
        let used = if allow_references {
            self.prepare(&mut body).await?
        } else {
            vec![]
        };
        let response = self.post(chat_client, &body).await?;
        let status = response.status().as_u16();
        if used.is_empty()
            || !matches!(status, 400 | 404 | 410 | 422)
            || crate::current_llm_call_policy() == crate::LlmCallPolicy::FailFast
        {
            return Ok(response);
        }
        let (status, headers, bytes) = buffer_error(response).await?;
        let text = String::from_utf8_lossy(&bytes).to_lowercase();
        if expired_reference(&text, &used) {
            self.invalidate(&used).await;
            body = original;
            self.prepare(&mut body).await?;
            tracing::debug!(target:"octos::media_transport", event="refresh");
            return self.post(chat_client, &body).await;
        }
        if reference_unsupported(&text) {
            if let Some(s) = &self.scope {
                let mut rejected = lock(&s.rejected_models);
                rejected.retain(|_, t| *t > Instant::now());
                if rejected.len() < MAX_IMAGES {
                    rejected.insert(
                        original["model"].as_str().unwrap_or("").into(),
                        Instant::now() + UNSUPPORTED_TTL,
                    );
                }
            }
            tracing::warn!(target:"octos::media_transport", "model rejected file references; preserving inline images");
            return self.post(chat_client, &original).await;
        }
        let mut buffered = http::Response::builder().status(status).body(bytes)?;
        *buffered.headers_mut() = headers;
        Ok(Response::from(buffered))
    }

    async fn post(&self, client: &Client, body: &Value) -> Result<Response> {
        client
            .post(format!("{}/chat/completions", self.endpoint))
            .bearer_auth(self.api_key)
            .json(body)
            .send()
            .await
            .inspect_err(|error| {
                crate::provider::log_android_transport_failure(error, body["stream"] == true)
            })
            .wrap_err("chat request transport failed")
    }
}

async fn buffer_error(
    mut response: Response,
) -> Result<(u16, reqwest::header::HeaderMap, Vec<u8>)> {
    let status = response.status().as_u16();
    let mut headers = response.headers().clone();
    let mut bytes = Vec::new();
    // The bounded buffer may be shorter than the original/decompressed body.
    headers.remove(reqwest::header::CONTENT_LENGTH);
    headers.remove(reqwest::header::CONTENT_ENCODING);
    headers.remove(reqwest::header::TRANSFER_ENCODING);
    while let Some(chunk) = response.chunk().await? {
        let left = (64 * 1024usize).saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(left)]);
        if bytes.len() >= 64 * 1024 {
            break;
        }
    }
    Ok((status, headers, bytes))
}

fn upload_unsupported(status: u16, text: &str) -> bool {
    let t = text.to_lowercase();
    matches!(status, 404 | 405 | 501)
        || (matches!(status, 400 | 422)
            && t.contains("purpose")
            && (t.contains("unsupported") || t.contains("not supported")))
}

fn reference_unsupported(text: &str) -> bool {
    text.contains("unknown variant `file`")
        || text.contains("unsupported_file_reference")
        || ((text.contains("ms://") || text.contains("file reference"))
            && (text.contains("unsupported") || text.contains("not supported")))
}

fn expired_reference(text: &str, refs: &[UsedReference]) -> bool {
    let missing =
        text.contains("expired") || text.contains("not found") || text.contains("does not exist");
    (missing && refs.iter().any(|r| text.contains(&r.id.to_lowercase())))
        || ["file_not_found", "file_expired", "expired_file"]
            .iter()
            .any(|s| text.contains(s))
}

fn expiry_ttl(data: &Value) -> Result<Duration> {
    let Some(expiry) = data.get("expires_at").filter(|v| !v.is_null()) else {
        return Ok(UNKNOWN_EXPIRY_TTL);
    };
    let epoch = expiry
        .as_u64()
        .or_else(|| {
            expiry
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp().max(0) as u64)
        })
        .ok_or_else(|| eyre!("file expiry has unsupported format"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    eyre::ensure!(
        epoch > now + 60,
        "uploaded file is already expired or expiring"
    );
    Ok(Duration::from_secs((epoch - now - 60).min(86400)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn body() -> Value {
        json!({"model":"unnamed-future-model","messages":[
            {"role":"system","content":"system"},
            {"role":"assistant","content":null,"reasoning_content":"fixture state",
             "tool_calls":[{"id":"call-1","type":"function","function":{"name":"read","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call-1","content":"read image"},
            {"role":"user","content":[{"type":"text","text":"before"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,aW1hZ2U="}},
                {"type":"text","text":"after"}]}
        ],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object"}}}]})
    }

    async fn uploads(server: &MockServer, status: u16, data: Value, count: u64) {
        Mock::given(method("POST"))
            .and(path("/files"))
            .respond_with(ResponseTemplate::new(status).set_body_json(data))
            .expect(count)
            .mount(server)
            .await;
    }

    async fn chats(server: &MockServer, status: u16, data: Value, count: u64) {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_json(data))
            .expect(count)
            .mount(server)
            .await;
    }

    #[test]
    fn auto_is_an_endpoint_contract_not_a_model_name_heuristic() {
        for endpoint in [
            "https://api.kimi.com/coding/v1",
            "https://api.kimi.ai/coding/v1/",
            "https://api.moonshot.cn/v1",
        ] {
            assert_eq!(
                MediaTransportMode::Auto.resolve(endpoint),
                MediaTransportMode::MoonshotFiles
            );
        }
        for endpoint in ["https://api.deepseek.com", "https://api.deepseek.com/v1/"] {
            assert_eq!(
                MediaTransportMode::Auto.resolve(endpoint),
                MediaTransportMode::DeepSeekFiles
            );
        }
        for endpoint in [
            "http://api.kimi.com/coding/v1",
            "https://api.kimi.com.evil.test/coding/v1",
            "https://api.kimi.com:8443/coding/v1",
            "https://api.kimi.com/unverified/v1",
            "https://api.kimi.com/coding/v1?x=1",
            "https://user@api.kimi.com/coding/v1",
            "https://proxy.test/k3.1-pro",
            "not a url",
        ] {
            assert_eq!(
                MediaTransportMode::Auto.resolve(endpoint),
                MediaTransportMode::Inline,
                "{endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn deepseek_uses_root_files_and_preserves_transcript_and_options() {
        let server = MockServer::start().await;
        uploads(&server, 200, json!({"id":"file-deepseek", "bytes":5}), 1).await;
        let client = Client::new();
        let transport = ImageTransport::new(
            &format!("{}/v1", server.uri()),
            "deepseek-test",
            MediaTransportMode::DeepSeekFiles,
            &client,
        );
        let original = body();
        let mut actual = original.clone();
        transport.prepare(&mut actual).await.unwrap();
        let mut expected = original.clone();
        expected["messages"][3]["content"][1] = json!({"type":"file","file_id":"file-deepseek"});
        assert_eq!(actual, expected);
        let requests = server.received_requests().await.unwrap();
        let multipart = String::from_utf8_lossy(&requests[0].body);
        assert!(multipart.contains("user_data"));
        assert!(multipart.contains("expires_after[seconds]"));
        for detail in ["high", "low"] {
            let mut specific = original.clone();
            specific["messages"][3]["content"][1]["image_url"]["detail"] = detail.into();
            let before = specific.clone();
            assert!(transport.prepare(&mut specific).await.unwrap().is_empty());
            assert_eq!(specific, before);
        }
    }

    #[tokio::test]
    async fn unsupported_upload_falls_back_once_without_losing_pixels() {
        let server = MockServer::start().await;
        uploads(&server, 404, json!({"error":"route missing"}), 1).await;
        chats(&server, 200, json!({"ok":true}), 2).await;
        let client = Client::new();
        let transport = ImageTransport::new(
            &server.uri(),
            "unsupported-upload",
            MediaTransportMode::MoonshotFiles,
            &client,
        );
        for _ in 0..2 {
            assert!(
                transport
                    .send(&client, body(), true)
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
        }
        for r in server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/chat/completions")
        {
            assert_eq!(r.body_json::<Value>().unwrap(), body());
        }
    }

    #[tokio::test]
    async fn auth_quota_and_server_upload_errors_do_not_replay_as_inline() {
        for status in [401, 403, 429, 500] {
            let server = MockServer::start().await;
            uploads(&server, status, json!({"error":"file not found"}), 1).await;
            let client = Client::new();
            let key = format!("upload-error-{status}");
            let transport = ImageTransport::new(
                &server.uri(),
                &key,
                MediaTransportMode::MoonshotFiles,
                &client,
            );
            assert!(transport.send(&client, body(), true).await.is_err());
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn expired_reference_gets_only_one_refresh_even_if_retry_fails() {
        for fail_fast in [false, true] {
            let server = MockServer::start().await;
            let attempts = if fail_fast { 1 } else { 2 };
            uploads(&server, 200, json!({"id":"expired-id"}), attempts).await;
            chats(
                &server,
                404,
                json!({"error":{"code":"file_expired"}}),
                attempts,
            )
            .await;
            let client = Client::new();
            let key = format!("expired-{fail_fast}");
            let transport = ImageTransport::new(
                &server.uri(),
                &key,
                MediaTransportMode::MoonshotFiles,
                &client,
            );
            let policy = if fail_fast {
                crate::LlmCallPolicy::FailFast
            } else {
                crate::LlmCallPolicy::Normal
            };
            let response =
                crate::with_llm_call_policy(policy, transport.send(&client, body(), true))
                    .await
                    .unwrap();
            assert_eq!(response.status(), 404);
        }
    }

    #[tokio::test]
    async fn unrelated_chat_errors_are_returned_intact_without_reupload() {
        for status in [400, 401, 404, 429, 500] {
            let server = MockServer::start().await;
            uploads(&server, 200, json!({"id":"valid-image"}), 1).await;
            let error = json!({"error":{"message":"model not found", "code":"model_not_found"}});
            chats(&server, status, error.clone(), 1).await;
            let client = Client::new();
            let key = format!("chat-error-{status}");
            let transport = ImageTransport::new(
                &server.uri(),
                &key,
                MediaTransportMode::MoonshotFiles,
                &client,
            );
            let response = transport.send(&client, body(), true).await.unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.json::<Value>().await.unwrap(), error);
        }
    }

    #[tokio::test]
    async fn reference_rejection_preserves_pixels_and_only_disables_that_model() {
        let server = MockServer::start().await;
        uploads(&server, 200, json!({"id":"valid-reference"}), 1).await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(|r: &wiremock::Request| {
                let b: Value = r.body_json().unwrap();
                if b["model"] == "unnamed-future-model" && b.to_string().contains("ms://") {
                    ResponseTemplate::new(400)
                        .set_body_json(json!({"error":"unsupported_file_reference"}))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"ok":true}))
                }
            })
            .expect(4)
            .mount(&server)
            .await;
        let client = Client::new();
        let transport = ImageTransport::new(
            &server.uri(),
            "rejected-model",
            MediaTransportMode::MoonshotFiles,
            &client,
        );
        for _ in 0..2 {
            assert!(
                transport
                    .send(&client, body(), true)
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
        }
        let mut other = body();
        other["model"] = "another-future-model".into();
        assert!(
            transport
                .send(&client, other, true)
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let requests = server.received_requests().await.unwrap();
        let sent: Vec<Value> = requests
            .iter()
            .filter(|r| r.url.path() == "/chat/completions")
            .map(|r| r.body_json().unwrap())
            .collect();
        assert_eq!(sent[1], body());
        assert_eq!(sent[2], body());
        assert_eq!(
            sent[3]["messages"][3]["content"][1]["image_url"]["url"],
            "ms://valid-reference"
        );
    }

    #[tokio::test]
    async fn cancelled_upload_does_not_strand_other_workers() {
        let server = MockServer::start().await;
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = started.clone();
        Mock::given(method("POST"))
            .and(path("/files"))
            .respond_with(move |_: &wiremock::Request| {
                notify.notify_one();
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"delayed-image"}))
                    .set_delay(Duration::from_millis(50))
            })
            .expect(2)
            .mount(&server)
            .await;
        let client = Client::new();
        let transport = ImageTransport::new(
            &server.uri(),
            "cancellation-test",
            MediaTransportMode::MoonshotFiles,
            &client,
        );
        let mut original = body();
        // Cancel after the server has received the request, not after a wall
        // clock guess that can flake on a heavily loaded CI worker.
        let mut pending = Box::pin(transport.prepare(&mut original));
        tokio::select! {
            _ = started.notified() => {},
            _ = &mut pending => panic!("upload completed before cancellation"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("upload never started"),
        }
        drop(pending);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), transport.prepare(&mut original))
                .await
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn expiry_is_bounded_and_rejects_stale_or_malformed_responses() {
        assert_eq!(expiry_ttl(&json!({})).unwrap(), UNKNOWN_EXPIRY_TTL);
        assert!(expiry_ttl(&json!({"expires_at":1})).is_err());
        assert!(expiry_ttl(&json!({"expires_at":"bad"})).is_err());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(
            expiry_ttl(&json!({"expires_at":now+864000})).unwrap(),
            Duration::from_secs(86400)
        );
    }
}
