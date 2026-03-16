//! Async HTTP client for ominix-api (ASR/TTS) and platform model allowlist.
//!
//! Model metadata lives in ominix-api (`~/.OminiX/local_models_config.json`
//! and `/v1/models/catalog`).  octos only maintains a small allowlist at
//! `~/.octos/platform-models.json` that specifies which ominix-api models the
//! platform skills are permitted to use.

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::debug;

// ---------------------------------------------------------------------------
// Platform model allowlist — ~/.octos/platform-models.json
// ---------------------------------------------------------------------------

/// An entry in the platform allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformModel {
    /// Model ID as known by ominix-api (e.g. "qwen3-asr-1.7b").
    pub id: String,
    /// Role this model fills for octos platform skills: "asr" or "tts".
    pub role: String,
}

/// The allowlist file: `~/.octos/platform-models.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformModels {
    pub platform_models: Vec<PlatformModel>,
}

impl PlatformModels {
    /// Default allowlist — the two core ASR/TTS models.
    pub fn defaults() -> Self {
        Self {
            platform_models: vec![
                PlatformModel {
                    id: "qwen3-asr-1.7b".into(),
                    role: "asr".into(),
                },
                PlatformModel {
                    id: "qwen3-tts".into(),
                    role: "tts".into(),
                },
            ],
        }
    }

    /// Load from disk, or create with defaults if missing.
    pub fn load_or_create(octos_home: &Path) -> Self {
        let path = Self::path(octos_home);
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(list) = serde_json::from_str::<PlatformModels>(&data) {
                return list;
            }
            tracing::warn!("invalid platform-models.json, using defaults");
        }
        let list = Self::defaults();
        if let Ok(json) = serde_json::to_string_pretty(&list) {
            let _ = std::fs::create_dir_all(octos_home);
            let _ = std::fs::write(&path, json);
        }
        list
    }

    /// Path to the allowlist file.
    pub fn path(octos_home: &Path) -> PathBuf {
        octos_home.join("platform-models.json")
    }

    /// Find an entry by model ID.
    pub fn find(&self, id: &str) -> Option<&PlatformModel> {
        self.platform_models.iter().find(|m| m.id == id)
    }

    /// Save the allowlist to disk.
    pub fn save(&self, octos_home: &Path) -> Result<()> {
        let path = Self::path(octos_home);
        let _ = std::fs::create_dir_all(octos_home);
        let json = serde_json::to_string_pretty(self)
            .wrap_err("failed to serialise platform-models.json")?;
        std::fs::write(&path, json)
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Get all model IDs for a given role.
    pub fn ids_for_role(&self, role: &str) -> Vec<&str> {
        self.platform_models
            .iter()
            .filter(|m| m.role == role)
            .map(|m| m.id.as_str())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// CatalogModel — ominix-api's model schema (for deserialising API responses)
// ---------------------------------------------------------------------------

/// A model from ominix-api's `/v1/models/catalog` response.
///
/// We only define the fields octos needs; unknown fields are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source: CatalogSource,
    #[serde(default)]
    pub storage: CatalogStorage,
    #[serde(default)]
    pub runtime: CatalogRuntime,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_status() -> String {
    "not_downloaded".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogSource {
    #[serde(default)]
    pub primary_url: String,
    #[serde(default)]
    pub backup_urls: Vec<String>,
    #[serde(default)]
    pub source_type: String,
    #[serde(default)]
    pub repo_id: Option<String>,
    #[serde(default)]
    pub revision: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogStorage {
    #[serde(default)]
    pub local_path: String,
    #[serde(default)]
    pub total_size_bytes: Option<u64>,
    #[serde(default)]
    pub total_size_display: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogRuntime {
    #[serde(default)]
    pub memory_required_mb: u32,
    #[serde(default)]
    pub quantization: Option<String>,
    #[serde(default)]
    pub inference_engine: Option<String>,
}

// ---------------------------------------------------------------------------
// OminixClient — async HTTP client for ominix-api
// ---------------------------------------------------------------------------

/// Async client for ominix-api ASR/TTS endpoints.
pub struct OminixClient {
    client: Client,
    base_url: String,
    language: Option<String>,
}

impl OminixClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            language: None,
        }
    }

    /// Set default ASR language hint.
    pub fn with_language(mut self, language: Option<String>) -> Self {
        self.language = language;
        self
    }

    /// Check if ominix-api is reachable.
    pub async fn health(&self) -> bool {
        match self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Fetch the full model catalog from ominix-api `/v1/models/catalog`.
    pub async fn fetch_catalog(&self) -> Result<Vec<CatalogModel>> {
        let resp = self
            .client
            .get(format!("{}/v1/models/catalog", self.base_url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .wrap_err("ominix-api unreachable")?;

        if !resp.status().is_success() {
            let status = resp.status();
            eyre::bail!("ominix-api catalog returned {status}");
        }

        resp.json()
            .await
            .wrap_err("failed to parse ominix-api catalog")
    }

    /// Fetch catalog from ominix-api, filtered to only platform-allowed models.
    pub async fn platform_catalog(&self, allowlist: &PlatformModels) -> Result<Vec<CatalogModel>> {
        let all = self.fetch_catalog().await?;
        let filtered = all
            .into_iter()
            .filter(|m| allowlist.find(&m.id).is_some())
            .collect();
        Ok(filtered)
    }

    /// Transcribe an audio file to text.
    pub async fn transcribe(&self, audio_path: &Path) -> Result<String> {
        let meta = tokio::fs::metadata(audio_path)
            .await
            .wrap_err_with(|| format!("failed to stat audio: {}", audio_path.display()))?;
        if meta.len() > 100_000_000 {
            eyre::bail!("audio file too large ({} bytes, max 100MB)", meta.len());
        }

        let bytes = tokio::fs::read(audio_path)
            .await
            .wrap_err_with(|| format!("failed to read audio: {}", audio_path.display()))?;

        use base64::Engine;
        let file_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let mut body = serde_json::json!({
            "file": file_b64,
            "response_format": "verbose_json",
        });

        if let Some(ref lang) = self.language {
            body["language"] = serde_json::Value::String(lang.clone());
        }

        let resp = self
            .client
            .post(format!("{}/v1/audio/transcriptions", self.base_url))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .wrap_err("failed to call ominix-api transcription")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("ominix-api transcription failed: {status} - {body}");
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .wrap_err("invalid transcription response")?;

        let text = json
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("no text field in transcription response"))?;

        debug!(chars = text.len(), "audio transcribed via ominix-api");
        Ok(text.to_string())
    }

    /// Synthesize text to speech, returning raw WAV bytes.
    pub async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        language: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut body = serde_json::json!({
            "input": text,
            "voice": voice,
        });
        if let Some(lang) = language {
            body["language"] = serde_json::Value::String(lang.to_string());
        }

        let resp = self
            .client
            .post(format!("{}/v1/audio/speech", self.base_url))
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .wrap_err("failed to call ominix-api TTS")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("ominix-api TTS failed: {status} - {body}");
        }

        let wav_bytes = resp.bytes().await.wrap_err("failed to read TTS response")?;

        debug!(size = wav_bytes.len(), "TTS audio generated via ominix-api");
        Ok(wav_bytes.to_vec())
    }

    /// Synthesize text to a WAV file. Returns audio duration in seconds.
    pub async fn synthesize_to_file(
        &self,
        text: &str,
        voice: &str,
        language: Option<&str>,
        path: &Path,
    ) -> Result<f64> {
        let wav_bytes = self.synthesize(text, voice, language).await?;

        if wav_bytes.len() < 44 {
            eyre::bail!("TTS returned invalid WAV data (too small)");
        }

        tokio::fs::write(path, &wav_bytes)
            .await
            .wrap_err_with(|| format!("failed to write TTS output: {}", path.display()))?;

        // 24kHz 16-bit mono = 48000 bytes/sec
        let duration_secs = wav_bytes.len().saturating_sub(44) as f64 / 48000.0;
        Ok(duration_secs)
    }
}
