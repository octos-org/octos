//! Persona service: periodically generates a communication style guide via LLM
//! based on recent chat history. The generated persona is written to `persona.md`
//! in the data directory and injected into the agent's system prompt.
//!
//! Also generates a pool of creative status words (`status_words.json`) used for
//! dynamic "thinking" indicators in chat channels.
//!
//! This is a system-internal service — not exposed to users via cron or any tool.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use octos_core::{Message, MessageRole};
use octos_llm::{ChatConfig, LlmProvider, ToolChoice};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Default persona refresh interval: 6 hours.
pub const DEFAULT_INTERVAL_SECS: u64 = 6 * 3600;

/// Initial delay before first persona generation (let sessions accumulate).
const INITIAL_DELAY_SECS: u64 = 60;

/// Maximum messages to sample per session.
const MAX_MSGS_PER_SESSION: usize = 20;

/// Maximum total messages to feed to the LLM.
const MAX_TOTAL_MSGS: usize = 100;

const META_PROMPT: &str = "\
Based on the recent conversations below, generate a concise communication style guide \
for an AI assistant. Analyze how users communicate (casual/formal, language preference, \
emoji usage, humor level, topics of interest) and create guidelines that match their style.

Output ONLY the style guide as bullet points (3-8 items), no preamble or explanation. Example format:
- Be casual and use humor freely
- Respond in Chinese when the user writes in Chinese
- Keep answers short and punchy
- Use emoji occasionally";

const STATUS_WORDS_META_PROMPT: &str = "\
Generate a list of 50 creative, single-word status verbs (in present participle / -ing form) \
that an AI assistant might display while thinking. Include a mix of intellectual, technical, \
playful, and poetic words. Also include 15-20 Chinese equivalents using trendy internet slang \
(网络用语), NOT boring formal words. Examples of good Chinese ones: 正在炼丹, 疯狂输出中, \
施法中, 渡劫中, DNA动了, 拿捏中, 在线发癫中, buff叠满了, 整活中, 遥遥领先中. \
Output ONLY a JSON array of strings, nothing else. Example: \
[\"Considering\", \"Synthesizing\", \"Weaving\", \"正在炼丹\", \"施法中\"]";

/// Default status words used when no LLM-generated pool is available.
pub const DEFAULT_STATUS_WORDS: &[&str] = &[
    "Considering",
    "Synthesizing",
    "Pondering",
    "Reflecting",
    "Analyzing",
    "Processing",
    "Formulating",
    "Contemplating",
    "Deliberating",
    "Evaluating",
    "Composing",
    "Reasoning",
    "Assembling",
    "Connecting",
    "Weaving",
    "Distilling",
];

/// Default Chinese status words — trendy internet slang.
pub const DEFAULT_STATUS_WORDS_ZH: &[&str] = &[
    "正在炼丹",
    "疯狂输出中",
    "施法中",
    "渡劫中",
    "DNA动了",
    "拿捏中",
    "在线发癫中",
    "格局打开中",
    "遥遥领先中",
    "上强度了",
    "起飞中",
    "充能中",
    "buff叠满了",
    "原地升天中",
    "开大中",
    "整活中",
    "在卷了",
    "燃起来了",
    "超级加倍中",
    "觉醒中",
];

/// Background service that generates a communication persona from chat history.
pub struct PersonaService {
    data_dir: PathBuf,
    llm: Arc<dyn LlmProvider>,
    interval_secs: u64,
    running: AtomicBool,
    timer_handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl PersonaService {
    pub fn new(data_dir: PathBuf, llm: Arc<dyn LlmProvider>, interval_secs: u64) -> Self {
        Self {
            data_dir,
            llm,
            interval_secs,
            running: AtomicBool::new(false),
            timer_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Start the persona generation loop.
    ///
    /// `on_persona` is called with the new persona text whenever it changes.
    /// `on_status_words` is called with a new pool of status words.
    pub fn start<F, G>(self: &Arc<Self>, on_persona: F, on_status_words: G)
    where
        F: Fn(String) + Send + Sync + 'static,
        G: Fn(Vec<String>) + Send + Sync + 'static,
    {
        self.running.store(true, Ordering::Relaxed);
        let this = Arc::clone(self);
        let on_persona = Arc::new(on_persona);
        let on_status_words = Arc::new(on_status_words);

        let handle = tokio::spawn(async move {
            info!(
                interval_secs = this.interval_secs,
                "persona service started"
            );

            // Initial delay — let the gateway warm up and accumulate some messages
            tokio::time::sleep(std::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;

            loop {
                if !this.running.load(Ordering::Relaxed) {
                    break;
                }

                if let Some(persona) = this.tick().await {
                    on_persona(persona);
                }

                // Generate status words (independent of persona success)
                if let Some(words) = this.generate_status_words().await {
                    on_status_words(words);
                }

                // Sleep until next interval
                tokio::time::sleep(std::time::Duration::from_secs(this.interval_secs)).await;
                if !this.running.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        let this2 = Arc::clone(self);
        tokio::spawn(async move {
            *this2.timer_handle.lock().await = Some(handle);
        });
    }

    /// Stop the persona generation loop.
    pub async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let mut handle = self.timer_handle.lock().await;
        if let Some(h) = handle.take() {
            h.abort();
        }
        info!("persona service stopped");
    }

    /// Single tick: collect conversations, call LLM, write persona.md.
    /// Returns the generated persona text if successful.
    async fn tick(&self) -> Option<String> {
        info!("persona tick: collecting conversations...");
        let conversations = self.collect_recent_conversations().await;
        if conversations.is_empty() {
            info!("persona tick: no conversations found, skipping");
            return None;
        }
        info!(
            "persona tick: got {} chars of conversation, calling LLM...",
            conversations.len()
        );

        let user_content = format!("{META_PROMPT}\n\nRecent conversations:\n\n{conversations}");

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You are a communication style analyzer. Output only bullet points."
                    .into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: Utc::now(),
            },
            Message {
                role: MessageRole::User,
                content: user_content,
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: Utc::now(),
            },
        ];

        let config = ChatConfig {
            max_tokens: Some(1024),
            temperature: Some(0.7),
            tool_choice: ToolChoice::None,
            stop_sequences: vec![],
            reasoning_effort: None,
            response_format: None,
            context_management: None,
        };

        match self.llm.chat(&messages, &[], &config).await {
            Ok(response) => {
                let persona = response.content.unwrap_or_default().trim().to_string();
                if persona.is_empty() {
                    warn!("LLM returned empty persona");
                    return None;
                }

                // Write to persona.md
                let path = self.data_dir.join("persona.md");
                if let Err(e) = tokio::fs::write(&path, &persona).await {
                    warn!("failed to write persona.md: {e}");
                    return None;
                }

                info!("persona updated ({} chars)", persona.len());
                Some(persona)
            }
            Err(e) => {
                warn!("persona generation LLM call failed: {e}");
                None
            }
        }
    }

    /// Collect recent user/assistant messages by reading session JSONL files directly.
    /// Uses spawn_blocking to avoid blocking the tokio runtime with file I/O.
    async fn collect_recent_conversations(&self) -> String {
        let sessions_dir = self.data_dir.join("sessions");

        let result = tokio::task::spawn_blocking(move || {
            Self::collect_conversations_blocking(&sessions_dir)
        })
        .await;

        match result {
            Ok(s) => s,
            Err(e) => {
                warn!("persona conversation collection failed: {e}");
                String::new()
            }
        }
    }

    /// Blocking implementation of conversation collection.
    fn collect_conversations_blocking(sessions_dir: &Path) -> String {
        let entries = match std::fs::read_dir(sessions_dir) {
            Ok(e) => e,
            Err(_) => return String::new(),
        };

        let mut out = String::new();
        let mut total = 0usize;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            // Skip oversized files
            let meta = match path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > 10 * 1024 * 1024 {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Parse last N messages from JSONL (skip header line)
            let lines: Vec<&str> = content.lines().collect();
            let start = if lines.len() > MAX_MSGS_PER_SESSION + 1 {
                lines.len() - MAX_MSGS_PER_SESSION
            } else {
                1
            };

            for line in &lines[start..] {
                let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let role_str = val.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let role_label = match role_str {
                    "user" => "User",
                    "assistant" => "Assistant",
                    _ => continue,
                };
                let msg_content = val.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if msg_content.trim().is_empty() {
                    continue;
                }

                out.push_str(role_label);
                out.push_str(": ");
                if msg_content.len() > 500 {
                    // Find a valid UTF-8 char boundary at or before byte 500
                    let mut end = 500;
                    while end > 0 && !msg_content.is_char_boundary(end) {
                        end -= 1;
                    }
                    out.push_str(&msg_content[..end]);
                    out.push_str("...");
                } else {
                    out.push_str(msg_content);
                }
                out.push_str("\n\n");

                total += 1;
                if total >= MAX_TOTAL_MSGS {
                    return out;
                }
            }
        }

        out
    }

    /// Generate a pool of creative status words via LLM.
    /// Writes to `status_words.json` and returns the word list.
    async fn generate_status_words(&self) -> Option<Vec<String>> {
        info!("generating status words...");

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You generate JSON arrays. Output only valid JSON, no explanation.".into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: Utc::now(),
            },
            Message {
                role: MessageRole::User,
                content: STATUS_WORDS_META_PROMPT.into(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                client_message_id: None,
                thread_id: None,
                timestamp: Utc::now(),
            },
        ];

        let config = ChatConfig {
            max_tokens: Some(2048),
            temperature: Some(0.9),
            tool_choice: ToolChoice::None,
            stop_sequences: vec![],
            reasoning_effort: None,
            response_format: None,
            context_management: None,
        };

        match self.llm.chat(&messages, &[], &config).await {
            Ok(response) => {
                let raw = response.content.unwrap_or_default();
                // Extract JSON array from response (may have markdown fences)
                let json_str = extract_json_array(&raw)?;
                let words: Vec<String> = match serde_json::from_str(&json_str) {
                    Ok(w) => w,
                    Err(e) => {
                        warn!("failed to parse status words JSON: {e}");
                        return None;
                    }
                };

                if words.is_empty() {
                    warn!("LLM returned empty status words");
                    return None;
                }

                // Write to status_words.json
                let path = self.data_dir.join("status_words.json");
                let json = serde_json::to_string_pretty(&words).unwrap_or_default();
                if let Err(e) = tokio::fs::write(&path, &json).await {
                    warn!("failed to write status_words.json: {e}");
                    return None;
                }

                info!("status words updated ({} words)", words.len());
                Some(words)
            }
            Err(e) => {
                warn!("status words LLM call failed: {e}");
                None
            }
        }
    }

    /// Read an existing persona.md file (for startup injection).
    pub fn read_persona(data_dir: &Path) -> Option<String> {
        let path = data_dir.join("persona.md");
        match std::fs::read_to_string(&path) {
            Ok(content) if !content.trim().is_empty() => Some(content.trim().to_string()),
            _ => None,
        }
    }

    /// Read existing status words from `status_words.json`, falling back to defaults.
    pub fn read_status_words(data_dir: &Path) -> Vec<String> {
        let path = data_dir.join("status_words.json");
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Vec<String>>(&content) {
                Ok(words) if !words.is_empty() => words,
                _ => DEFAULT_STATUS_WORDS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect(),
            },
            _ => DEFAULT_STATUS_WORDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

/// Extract a JSON array from text that may contain markdown fences or preamble.
fn extract_json_array(s: &str) -> Option<String> {
    let trimmed = s.trim();
    // Try direct parse first
    if trimmed.starts_with('[') {
        return Some(trimmed.to_string());
    }
    // Look for ```json ... ``` fences
    if let Some(start) = trimmed.find('[') {
        if let Some(end) = trimmed.rfind(']') {
            if end > start {
                return Some(trimmed[start..=end].to_string());
            }
        }
    }
    None
}
