//! Voice platform skill binary (ASR/TTS/model management) via ominix-api.
//!
//! Protocol: `./main <tool_name>` with JSON on stdin, JSON on stdout.
//! Requires OMINIX_API_URL environment variable (default: http://localhost:8080).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

// ── Input types ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TranscribeInput {
    audio_path: String,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Deserialize)]
struct SynthesizeInput {
    text: String,
    #[serde(default)]
    output_path: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    speaker: Option<String>,
}

#[derive(Deserialize)]
struct VoiceCloneInput {
    /// Path to reference audio (3-10s WAV/OGG/MP3/FLAC/M4A)
    reference_audio: String,
    /// Text to synthesize in the cloned voice
    text: String,
    #[serde(default)]
    output_path: Option<String>,
    #[serde(default)]
    language: Option<String>,
    /// If set, save the reference audio as a named voice profile for future use
    #[serde(default)]
    save_as: Option<String>,
}

#[derive(Deserialize)]
struct SaveProfileInput {
    /// Name for this voice profile (e.g. "alice", "boss")
    name: String,
    /// Path to the reference audio file to save
    audio_path: String,
}

// ── Helpers ──────────────────────────────────────────────────────────

fn api_base_url() -> String {
    // Priority: env var > discovery file > default
    if let Ok(url) = std::env::var("OMINIX_API_URL") {
        return url.trim_end_matches('/').to_string();
    }
    // Read URL written by ominix-api on startup
    if let Some(home) = std::env::var_os("HOME") {
        let discovery = Path::new(&home).join(".ominix").join("api_url");
        if let Ok(url) = std::fs::read_to_string(&discovery) {
            let url = url.trim();
            if !url.is_empty() {
                return url.trim_end_matches('/').to_string();
            }
        }
    }
    "http://localhost:8080".to_string()
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        // No global timeout — streaming PCM chunks keep the connection alive.
        // Per-request timeouts are set where needed.
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build HTTP client")
}

/// Wrap raw PCM bytes (16-bit signed LE, mono) in a WAV header.
fn pcm_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let file_len = 36 + data_len; // 44-byte header minus 8 for RIFF+size
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

/// Call TTS API with streaming PCM, collect all bytes, return WAV data.
/// Streaming avoids timeout issues — chunks flow continuously.
fn stream_tts_to_wav(
    client: &reqwest::blocking::Client,
    base_url: &str,
    body: &serde_json::Value,
) -> Result<Vec<u8>, String> {
    let resp = client
        .post(format!("{base_url}/v1/audio/speech"))
        .json(body)
        .send()
        .map_err(|e| format!("TTS request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let resp_text = resp.text().unwrap_or_default();
        return Err(format!(
            "TTS error (HTTP {status}): {}",
            truncate(&resp_text, 200)
        ));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let bytes = resp
        .bytes()
        .map_err(|e| format!("Failed to read TTS response: {e}"))?;

    if bytes.is_empty() {
        return Err("TTS returned empty response".to_string());
    }

    // If server returned WAV already (e.g. voice clone path), pass through
    if content_type.contains("wav") || (bytes.len() >= 4 && &bytes[..4] == b"RIFF") {
        return Ok(bytes.to_vec());
    }

    // Otherwise it's raw PCM — wrap in WAV header (24kHz, 16-bit, mono)
    Ok(pcm_to_wav(&bytes, 24000))
}

fn check_health(client: &reqwest::blocking::Client, base_url: &str) -> Result<(), String> {
    match client
        .get(format!("{base_url}/health"))
        .timeout(Duration::from_secs(5))
        .send()
    {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => Err(format!(
            "ominix-api returned HTTP {} — is it running on {base_url}?",
            resp.status()
        )),
        Err(e) => Err(format!(
            "Cannot reach ominix-api at {base_url}: {e}. \
             Start it with: ominix-api --port 8080"
        )),
    }
}

fn fail(msg: &str) -> ! {
    let out = json!({"output": msg, "success": false});
    println!("{out}");
    std::process::exit(1);
}

fn succeed(msg: &str) -> ! {
    let out = json!({"output": msg, "success": true});
    println!("{out}");
    std::process::exit(0);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let end: String = s.chars().take(max).collect();
        format!("{end}...")
    }
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Per-profile voice profiles directory: `$CREW_DATA_DIR/voice_profiles/`.
/// Each saved profile is a raw audio file named `{name}.wav`.
fn voice_profiles_dir() -> Option<PathBuf> {
    std::env::var("CREW_DATA_DIR")
        .ok()
        .map(|d| Path::new(&d).join("voice_profiles"))
}

/// Resolve a speaker name: if a saved voice profile exists, return its path.
/// Otherwise return None (caller should treat it as a preset name).
fn resolve_voice_profile(speaker: &str) -> Option<PathBuf> {
    let dir = voice_profiles_dir()?;
    // Try exact name, then with .wav extension
    for ext in &["", ".wav", ".ogg", ".mp3", ".m4a", ".flac"] {
        let path = dir.join(format!("{speaker}{ext}"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Sanitize a profile name: lowercase, alphanumeric + underscore + hyphen only.
fn sanitize_profile_name(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── voice_transcribe ─────────────────────────────────────────────────

fn handle_transcribe(input_json: &str) {
    let input: TranscribeInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    let path = Path::new(&input.audio_path);
    if !path.exists() {
        fail(&format!("Audio file not found: {}", input.audio_path));
    }
    if !path.is_file() {
        fail(&format!("Not a file: {}", input.audio_path));
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() == 0 {
            fail("Audio file is empty (0 bytes)");
        }
        if meta.len() > 100_000_000 {
            fail("Audio file too large (>100MB)");
        }
    }

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let language = input.language.unwrap_or_else(|| "Chinese".to_string());

    // ominix-api accepts file paths (starting with '/') or base64 in the "file" field
    let body = json!({
        "file": input.audio_path,
        "language": language,
        "response_format": "verbose_json"
    });

    let resp = match client
        .post(format!("{base_url}/v1/audio/transcriptions"))
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => fail(&format!("ASR request failed: {e}")),
    };

    let status = resp.status();
    let resp_text = resp.text().unwrap_or_default();

    if !status.is_success() {
        fail(&format!(
            "ASR error (HTTP {status}): {}",
            truncate(&resp_text, 200)
        ));
    }

    let result: serde_json::Value = match serde_json::from_str(&resp_text) {
        Ok(v) => v,
        Err(e) => fail(&format!("Failed to parse ASR response: {e}")),
    };

    let text = result["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        fail("ASR returned empty transcription (silence or unsupported format)");
    }

    let mut output = text.to_string();
    if let Some(duration) = result["duration"].as_f64() {
        output = format!("{text}\n\n[Audio duration: {duration:.1}s]");
    }

    succeed(&output);
}

// ── voice_synthesize ─────────────────────────────────────────────────

fn synthesize_segment(
    client: &reqwest::blocking::Client,
    base_url: &str,
    text: &str,
    voice: &str,
    language: &str,
    output_path: &Path,
) -> Result<(usize, f64), String> {
    let body = json!({
        "input": text,
        "voice": voice,
        "language": language
    });

    let wav_bytes = stream_tts_to_wav(client, base_url, &body)?;

    std::fs::write(output_path, &wav_bytes)
        .map_err(|e| format!("Failed to write {}: {e}", output_path.display()))?;

    // 24kHz 16-bit mono = 48000 bytes/sec
    let duration_secs = wav_bytes.len().saturating_sub(44) as f64 / 48000.0;
    Ok((wav_bytes.len(), duration_secs))
}

fn handle_synthesize(input_json: &str) {
    let input: SynthesizeInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.text.trim().is_empty() {
        fail("'text' must not be empty");
    }

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let output_path = input
        .output_path
        .unwrap_or_else(|| format!("/tmp/crew_tts_{}.wav", timestamp()));

    if let Some(parent) = Path::new(&output_path).parent() {
        if !parent.exists() {
            fail(&format!(
                "Output directory does not exist: {}",
                parent.display()
            ));
        }
    }

    let language = input.language.unwrap_or_else(|| "chinese".to_string());
    let speaker = input.speaker.unwrap_or_else(|| "vivian".to_string());

    // Check if speaker is a saved voice profile — if so, use clone mode
    if let Some(profile_path) = resolve_voice_profile(&speaker) {
        let ref_bytes = match std::fs::read(&profile_path) {
            Ok(b) => b,
            Err(e) => fail(&format!("Failed to read voice profile '{}': {e}", speaker)),
        };
        use base64::Engine;
        let ref_b64 = base64::engine::general_purpose::STANDARD.encode(&ref_bytes);

        let body = json!({
            "input": input.text,
            "reference_audio": ref_b64,
            "language": language
        });

        // Clone path returns WAV (has reference_audio → wants_wav=true in API)
        let wav_bytes = match stream_tts_to_wav(&client, &base_url, &body) {
            Ok(b) => b,
            Err(e) => fail(&format!("TTS (clone) failed: {e}")),
        };

        if let Err(e) = std::fs::write(Path::new(&output_path), &wav_bytes) {
            fail(&format!("Failed to write {output_path}: {e}"));
        }
        let duration_secs = wav_bytes.len().saturating_sub(44) as f64 / 48000.0;
        succeed(&format!(
            "Generated audio (voice profile '{speaker}'): {output_path} ({duration_secs:.1}s, {} bytes). Use send_file to deliver it to the user.",
            wav_bytes.len()
        ));
    }

    // Preset speaker mode
    match synthesize_segment(
        &client,
        &base_url,
        &input.text,
        &speaker,
        &language,
        Path::new(&output_path),
    ) {
        Ok((size, duration_secs)) => {
            succeed(&format!(
                "Generated audio: {output_path} ({duration_secs:.1}s, {size} bytes). Use send_file to deliver it to the user."
            ));
        }
        Err(e) => fail(&e),
    }
}

// ── voice_clone ──────────────────────────────────────────────────────

fn handle_voice_clone(input_json: &str) {
    let input: VoiceCloneInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.text.trim().is_empty() {
        fail("'text' must not be empty");
    }

    let ref_path = Path::new(&input.reference_audio);
    if !ref_path.exists() {
        fail(&format!(
            "Reference audio not found: {}",
            input.reference_audio
        ));
    }
    if !ref_path.is_file() {
        fail(&format!("Not a file: {}", input.reference_audio));
    }

    let meta = match std::fs::metadata(ref_path) {
        Ok(m) => m,
        Err(e) => fail(&format!("Cannot read reference audio: {e}")),
    };
    if meta.len() == 0 {
        fail("Reference audio is empty (0 bytes)");
    }
    if meta.len() > 10_000_000 {
        fail("Reference audio too large (>10MB). Use a 3-10 second clip.");
    }

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    // Read and base64-encode the reference audio
    let ref_bytes = match std::fs::read(ref_path) {
        Ok(b) => b,
        Err(e) => fail(&format!("Failed to read reference audio: {e}")),
    };

    use base64::Engine;
    let ref_b64 = base64::engine::general_purpose::STANDARD.encode(&ref_bytes);

    let output_path = input
        .output_path
        .unwrap_or_else(|| format!("/tmp/crew_clone_{}.wav", timestamp()));

    if let Some(parent) = Path::new(&output_path).parent() {
        if !parent.exists() {
            fail(&format!(
                "Output directory does not exist: {}",
                parent.display()
            ));
        }
    }

    let language = input.language.unwrap_or_else(|| "chinese".to_string());

    let body = json!({
        "input": input.text,
        "reference_audio": ref_b64,
        "language": language
    });

    // Clone path returns WAV (reference_audio triggers wants_wav in API)
    let wav_bytes = match stream_tts_to_wav(&client, &base_url, &body) {
        Ok(b) => b,
        Err(e) => fail(&format!("Voice clone failed: {e}")),
    };

    if let Err(e) = std::fs::write(Path::new(&output_path), &wav_bytes) {
        fail(&format!("Failed to write {output_path}: {e}"));
    }

    // 24kHz 16-bit mono = 48000 bytes/sec
    let duration_secs = wav_bytes.len().saturating_sub(44) as f64 / 48000.0;

    // Optionally save the reference audio as a named voice profile
    let save_msg = if let Some(name) = &input.save_as {
        match save_voice_profile(name, ref_path) {
            Ok(saved_name) => format!(" Voice profile '{saved_name}' saved — use speaker=\"{saved_name}\" in voice_synthesize for future TTS."),
            Err(e) => format!(" Warning: failed to save voice profile: {e}"),
        }
    } else {
        String::new()
    };

    succeed(&format!(
        "Voice clone generated: {output_path} ({duration_secs:.1}s, {} bytes). \
         Use send_file to deliver it to the user.{save_msg}",
        wav_bytes.len()
    ));
}

// ── voice_save_profile ──────────────────────────────────────────────

/// Save an audio file as a named voice profile.
fn save_voice_profile(name: &str, audio_path: &Path) -> Result<String, String> {
    let dir = voice_profiles_dir()
        .ok_or_else(|| "CREW_DATA_DIR not set — cannot save voice profiles".to_string())?;

    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create voice_profiles dir: {e}"))?;

    let sanitized = sanitize_profile_name(name);
    if sanitized.is_empty() {
        return Err("Profile name is empty after sanitization".to_string());
    }

    // Preserve original extension
    let ext = audio_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav");
    let dest = dir.join(format!("{sanitized}.{ext}"));

    std::fs::copy(audio_path, &dest)
        .map_err(|e| format!("Failed to copy audio to {}: {e}", dest.display()))?;

    Ok(sanitized)
}

fn handle_save_profile(input_json: &str) {
    let input: SaveProfileInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    let audio_path = Path::new(&input.audio_path);
    if !audio_path.is_file() {
        fail(&format!("Audio file not found: {}", input.audio_path));
    }

    let meta = match std::fs::metadata(audio_path) {
        Ok(m) => m,
        Err(e) => fail(&format!("Cannot read audio file: {e}")),
    };
    if meta.len() == 0 {
        fail("Audio file is empty (0 bytes)");
    }
    if meta.len() > 10_000_000 {
        fail("Audio file too large (>10MB). Use a 3-10 second clip.");
    }

    match save_voice_profile(&input.name, audio_path) {
        Ok(saved_name) => succeed(&format!(
            "Voice profile '{saved_name}' saved. Use speaker=\"{saved_name}\" in voice_synthesize to generate speech in this voice."
        )),
        Err(e) => fail(&e),
    }
}

// ── voice_list_profiles ─────────────────────────────────────────────

fn handle_list_profiles(_input_json: &str) {
    let dir = match voice_profiles_dir() {
        Some(d) => d,
        None => fail("CREW_DATA_DIR not set — no voice profiles directory"),
    };

    if !dir.exists() {
        succeed("No saved voice profiles yet. Use voice_save_profile or voice_clone with save_as to create one.");
    }

    let mut profiles = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => fail(&format!("Failed to read voice_profiles dir: {e}")),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("?");
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        profiles.push(format!("- **{name}** ({ext}, {size} bytes)"));
    }

    if profiles.is_empty() {
        succeed("No saved voice profiles yet. Use voice_save_profile or voice_clone with save_as to create one.");
    }

    let list = profiles.join("\n");
    succeed(&format!(
        "## Saved Voice Profiles\n\n{list}\n\nUse speaker=\"<name>\" in voice_synthesize to use a saved profile."
    ));
}

// ── list_models ──────────────────────────────────────────────────────

fn handle_list_models(_input_json: &str) {
    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    // Get loaded models
    let loaded = match client.get(format!("{base_url}/v1/models")).send() {
        Ok(r) if r.status().is_success() => r.text().unwrap_or_default(),
        Ok(r) => fail(&format!("Failed to list models: HTTP {}", r.status())),
        Err(e) => fail(&format!("Failed to list models: {e}")),
    };
    let loaded: serde_json::Value = serde_json::from_str(&loaded).unwrap_or(json!({}));

    // Get catalog (available for download)
    let catalog = match client.get(format!("{base_url}/v1/models/catalog")).send() {
        Ok(r) if r.status().is_success() => r.text().unwrap_or_default(),
        _ => "{}".to_string(),
    };
    let catalog: serde_json::Value = serde_json::from_str(&catalog).unwrap_or(json!({}));

    let mut output = String::from("## Loaded Models\n\n");
    if let Some(models) = loaded.get("data").and_then(|d| d.as_array()) {
        if models.is_empty() {
            output.push_str("No models loaded.\n");
        }
        for m in models {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let mtype = m.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            output.push_str(&format!("- {id} ({mtype})\n"));
        }
    }

    output.push_str("\n## Available Models (catalog)\n\n");
    if let Some(models) = catalog.get("models").and_then(|d| d.as_array()) {
        for m in models {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let mtype = m.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            let downloaded = m
                .get("downloaded")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let status = if downloaded {
                "downloaded"
            } else {
                "not downloaded"
            };
            output.push_str(&format!("- {id} ({mtype}) [{status}]\n"));
        }
    }

    succeed(&output);
}

// ── download_model ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct DownloadModelInput {
    model_id: String,
}

fn handle_download_model(input_json: &str) {
    let input: DownloadModelInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let resp = match client
        .post(format!("{base_url}/v1/models/download"))
        .json(&json!({"model_id": input.model_id}))
        .timeout(Duration::from_secs(600))
        .send()
    {
        Ok(r) => r,
        Err(e) => fail(&format!("Download request failed: {e}")),
    };

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        fail(&format!(
            "Download failed (HTTP {status}): {}",
            truncate(&text, 200)
        ));
    }

    succeed(&format!(
        "Download started for model: {}. Use list_models to check status.",
        input.model_id
    ));
}

// ── load_model / unload_model ────────────────────────────────────────

#[derive(Deserialize)]
struct LoadModelInput {
    model: String,
    #[serde(default = "default_model_type")]
    model_type: String,
}

fn default_model_type() -> String {
    "llm".to_string()
}

fn handle_load_model(input_json: &str) {
    let input: LoadModelInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let resp = match client
        .post(format!("{base_url}/v1/models/load"))
        .json(&json!({"model": input.model, "model_type": input.model_type}))
        .timeout(Duration::from_secs(120))
        .send()
    {
        Ok(r) => r,
        Err(e) => fail(&format!("Load request failed: {e}")),
    };

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        fail(&format!(
            "Load failed (HTTP {status}): {}",
            truncate(&text, 200)
        ));
    }

    succeed(&format!(
        "Model loaded: {} (type: {})",
        input.model, input.model_type
    ));
}

#[derive(Deserialize)]
struct UnloadModelInput {
    model_type: String,
}

fn handle_unload_model(input_json: &str) {
    let input: UnloadModelInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let resp = match client
        .post(format!("{base_url}/v1/models/unload"))
        .json(&json!({"model_type": input.model_type}))
        .send()
    {
        Ok(r) => r,
        Err(e) => fail(&format!("Unload request failed: {e}")),
    };

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        fail(&format!(
            "Unload failed (HTTP {status}): {}",
            truncate(&text, 200)
        ));
    }

    succeed(&format!("Model unloaded: {}", input.model_type));
}

// ── Main ─────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tool_name = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");

    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        fail(&format!("Failed to read stdin: {e}"));
    }

    match tool_name {
        "voice_transcribe" => handle_transcribe(&buf),
        "voice_synthesize" => handle_synthesize(&buf),
        "voice_clone" => handle_voice_clone(&buf),
        "voice_save_profile" => handle_save_profile(&buf),
        "voice_list_profiles" => handle_list_profiles(&buf),
        "list_models" => handle_list_models(&buf),
        "download_model" => handle_download_model(&buf),
        "load_model" => handle_load_model(&buf),
        "unload_model" => handle_unload_model(&buf),
        _ => fail(&format!(
            "Unknown tool '{tool_name}'. Expected: voice_transcribe, voice_synthesize, voice_clone, voice_save_profile, voice_list_profiles, list_models, download_model, load_model, unload_model"
        )),
    }
}
