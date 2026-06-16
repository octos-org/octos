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

/// Map an on-device TTS engine name to its ominix-api endpoint path.
///
/// Unknown values fall back to GPT-SoVITS — the lighter, default on-device
/// engine — so a typo in config degrades gracefully instead of erroring.
fn tts_endpoint(engine: &str) -> &'static str {
    match engine {
        "qwen3" => "/v1/audio/speech",
        _ => "/v1/audio/tts/sovits",
    }
}

// ---------------------------------------------------------------------------
// Voice registry — ~/.OminiX/models/voices.json
// ---------------------------------------------------------------------------

/// A single registered voice in ominix-api's `voices.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceEntry {
    /// Reference audio path, relative to [`VoicesRegistry::models_base_path`].
    #[serde(default)]
    pub ref_audio: String,
    /// Verbatim transcription of `ref_audio` (drives few-shot synthesis).
    #[serde(default)]
    pub ref_text: String,
    /// Alternative names that also resolve to this voice.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// ominix-api's voice registry (`~/.OminiX/models/voices.json`). A `BTreeMap`
/// keeps the listing order deterministic (sorted by id) regardless of file
/// ordering.
#[derive(Debug, Clone, Deserialize)]
pub struct VoicesRegistry {
    #[serde(default)]
    pub default_voice: String,
    #[serde(default)]
    pub models_base_path: String,
    #[serde(default)]
    pub voices: std::collections::BTreeMap<String, VoiceEntry>,
    /// Directory the registry was loaded from (the `voices.json` parent). Used
    /// to resolve a relative `ref_audio` when `models_base_path` is empty or
    /// itself relative. Not part of the JSON; set by [`VoicesRegistry::load`].
    #[serde(skip)]
    registry_dir: Option<PathBuf>,
}

/// Expand a leading `~` / `~/` against `$HOME`. Other forms are returned as-is.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// A voice exposed to clients: the canonical id plus its aliases.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VoiceInfo {
    pub id: String,
    pub aliases: Vec<String>,
}

impl VoicesRegistry {
    /// Parse a `voices.json` body.
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).wrap_err("failed to parse voices.json")
    }

    /// Load and parse `voices.json` from disk.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        let mut reg = Self::parse(&data)?;
        reg.registry_dir = path.parent().map(Path::to_path_buf);
        Ok(reg)
    }

    /// Resolve a voice entry's `ref_audio` to an absolute filesystem path.
    ///
    /// Handles the real-world registry shapes: an **absolute** `ref_audio` (the
    /// per-profile clones the fleet script writes) is used verbatim; a
    /// **relative** one is joined onto `models_base_path` (with a leading `~`
    /// expanded — the script stores a literal `~/.OminiX/models`), falling back
    /// to the `voices.json` parent dir when the base is empty or relative.
    fn resolved_ref_path(&self, ref_audio: &str) -> Option<PathBuf> {
        if ref_audio.is_empty() {
            return None;
        }
        let ra = expand_tilde(ref_audio);
        if ra.is_absolute() {
            return Some(ra);
        }
        let base = if self.models_base_path.is_empty() {
            self.registry_dir.clone()?
        } else {
            let b = expand_tilde(&self.models_base_path);
            if b.is_absolute() {
                b
            } else {
                // A relative base resolves against the registry dir when known.
                self.registry_dir.as_ref().map(|d| d.join(&b)).unwrap_or(b)
            }
        };
        Some(base.join(ra))
    }

    /// Whether a voice entry's reference audio actually exists on disk (= the
    /// engine can synthesize it).
    fn ref_exists(&self, entry: &VoiceEntry) -> bool {
        self.resolved_ref_path(&entry.ref_audio)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Voices the engine can actually synthesize (ref audio present), sorted by
    /// id for a stable client-facing list.
    pub fn synthesizable(&self) -> Vec<VoiceInfo> {
        self.synthesizable_visible(|_| true)
    }

    /// Like [`synthesizable`](Self::synthesizable), but only voices whose
    /// `ref_audio` path satisfies `is_visible`. Callers use this to scope the
    /// listing to a single tenant (shared presets + that tenant's own clones)
    /// so cloned voices never leak across profiles.
    pub fn synthesizable_visible(&self, is_visible: impl Fn(&str) -> bool) -> Vec<VoiceInfo> {
        self.voices
            .iter()
            .filter(|(_, e)| self.ref_exists(e) && is_visible(&e.ref_audio))
            .map(|(id, e)| VoiceInfo {
                id: id.clone(),
                aliases: e.aliases.clone(),
            })
            .collect()
    }

    /// Resolve a user-supplied name (canonical id or alias) to its canonical
    /// id, but only when the voice is synthesizable. `None` for unknown names
    /// or entries whose ref audio is missing.
    pub fn resolve(&self, name: &str) -> Option<String> {
        self.resolve_visible(name, |_| true)
    }

    /// Like [`resolve`](Self::resolve), but only matches voices whose
    /// `ref_audio` path satisfies `is_visible`, so a tenant can't select a
    /// voice cloned by (and owned by) another profile.
    pub fn resolve_visible(&self, name: &str, is_visible: impl Fn(&str) -> bool) -> Option<String> {
        self.voices
            .iter()
            .find(|(id, e)| {
                (id.as_str() == name || e.aliases.iter().any(|a| a == name))
                    && self.ref_exists(e)
                    && is_visible(&e.ref_audio)
            })
            .map(|(id, _)| id.clone())
    }
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
    ///
    /// `engine` selects the on-device TTS endpoint:
    /// - `"sovits"` (default): GPT-SoVITS (~1.4GB, RTF ~0.4) at
    ///   `/v1/audio/tts/sovits`.
    /// - `"qwen3"`: the Qwen3-TTS pool at `/v1/audio/speech` (~5GB).
    ///
    /// Both endpoints accept the same `{ input, voice }` body. `voice` selects
    /// the registered voice (voices.json) so the server uses its ref_audio +
    /// ref_text → few-shot synthesis (far fewer filler artifacts than the
    /// zero-shot startup ref). The voice name / its aliases must exist in
    /// voices.json, else the server errors.
    pub async fn synthesize(
        &self,
        text: &str,
        voice: &str,
        engine: &str,
        language: Option<&str>,
    ) -> Result<Vec<u8>> {
        // `language` is unused by the on-device few-shot path: the voice's
        // ref_audio/ref_text already pin the language, so the engines ignore a
        // separate language hint. Kept in the signature for cloud/future
        // engines that do consume it.
        let _ = language;
        let endpoint = tts_endpoint(engine);
        let body = serde_json::json!({
            "input": text,
            "voice": voice,
        });

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, endpoint))
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
        engine: &str,
        language: Option<&str>,
        path: &Path,
    ) -> Result<f64> {
        let wav_bytes = self.synthesize(text, voice, engine, language).await?;

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

#[cfg(test)]
mod tests {
    use super::tts_endpoint;

    #[test]
    fn sovits_is_the_default_endpoint() {
        assert_eq!(tts_endpoint("sovits"), "/v1/audio/tts/sovits");
    }

    #[test]
    fn qwen3_maps_to_speech_pool() {
        assert_eq!(tts_endpoint("qwen3"), "/v1/audio/speech");
    }

    #[test]
    fn unknown_engine_falls_back_to_sovits() {
        assert_eq!(tts_endpoint("nonsense"), "/v1/audio/tts/sovits");
    }
}

#[cfg(test)]
mod voices_tests {
    use super::{VoiceInfo, VoicesRegistry};

    /// Write a registry JSON whose `models_base_path` is `base`, plus create the
    /// listed ref files under it so existence checks pass.
    fn registry_with(base: &std::path::Path, present: &[&str]) -> VoicesRegistry {
        for f in present {
            let p = base.join("ref_audios").join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"fake").unwrap();
        }
        let json = format!(
            r#"{{
              "default_voice": "doubao",
              "models_base_path": {base:?},
              "voices": {{
                "doubao": {{ "ref_audio": "ref_audios/doubao_ref.wav", "ref_text": "x", "aliases": ["vivian"] }},
                "ghost":  {{ "ref_audio": "ref_audios/ghost_ref.wav",  "ref_text": "y", "aliases": [] }}
              }}
            }}"#,
            base = base.to_string_lossy()
        );
        VoicesRegistry::parse(&json).unwrap()
    }

    #[test]
    fn synthesizable_lists_only_entries_whose_ref_audio_exists() {
        let dir = tempfile::tempdir().unwrap();
        // Only doubao's ref file exists; ghost's is missing.
        let reg = registry_with(dir.path(), &["doubao_ref.wav"]);
        assert_eq!(
            reg.synthesizable(),
            vec![VoiceInfo {
                id: "doubao".to_string(),
                aliases: vec!["vivian".to_string()],
            }]
        );
    }

    #[test]
    fn resolve_accepts_id_and_alias_but_only_when_ref_exists() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry_with(dir.path(), &["doubao_ref.wav"]);
        assert_eq!(reg.resolve("doubao").as_deref(), Some("doubao"));
        assert_eq!(reg.resolve("vivian").as_deref(), Some("doubao")); // alias → id
        assert_eq!(reg.resolve("ghost"), None); // ref missing
        assert_eq!(reg.resolve("nope"), None); // unknown
    }

    #[test]
    fn ref_exists_resolves_relative_audio_against_voices_json_dir_when_base_empty() {
        // A registry with an empty `models_base_path` must resolve a relative
        // `ref_audio` against the voices.json parent dir, not the process CWD.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doubao_ref.wav"), b"fake").unwrap();
        let json = r#"{
          "default_voice": "doubao",
          "models_base_path": "",
          "voices": { "doubao": { "ref_audio": "doubao_ref.wav" } }
        }"#;
        let path = dir.path().join("voices.json");
        std::fs::write(&path, json).unwrap();

        let reg = super::VoicesRegistry::load(&path).unwrap();
        assert_eq!(
            reg.synthesizable()
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            vec!["doubao"],
            "relative ref_audio should resolve against the voices.json dir"
        );
    }

    #[test]
    fn synthesizable_visible_and_resolve_visible_apply_ownership_filter() {
        let dir = tempfile::tempdir().unwrap();
        // Both refs exist on disk, but the predicate hides "ghost".
        let reg = registry_with(dir.path(), &["doubao_ref.wav", "ghost_ref.wav"]);
        let visible = |ref_audio: &str| !ref_audio.contains("ghost");

        assert_eq!(
            reg.synthesizable_visible(visible)
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            vec!["doubao"]
        );
        assert_eq!(
            reg.resolve_visible("doubao", visible).as_deref(),
            Some("doubao")
        );
        // Hidden by the predicate even though its ref audio exists.
        assert_eq!(reg.resolve_visible("ghost", visible), None);
    }

    #[test]
    fn expand_tilde_expands_home_prefix_only() {
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                super::expand_tilde("~/.OminiX/models"),
                std::path::Path::new(&home).join(".OminiX/models")
            );
        }
        assert_eq!(
            super::expand_tilde("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
        assert_eq!(
            super::expand_tilde("rel/path"),
            std::path::PathBuf::from("rel/path")
        );
    }
}
