//! Voice platform skill binary (ASR, preset-voice TTS, model management) via ominix-api.
//!
//! Protocol: `./main <tool_name>` with JSON on stdin, JSON on stdout.
//! Auto-discovers ominix-api via OMINIX_API_URL, ~/.ominix/api_url, or default http://localhost:9090.
//!
//! NOTE: Voice cloning and custom voice profiles are handled by mofa-fm.
//! This skill only supports preset voices for TTS.

use std::io::Read;
use std::path::Path;
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
    /// Style/emotion prompt (e.g. "用兴奋激动的语气说话，充满热情和活力")
    #[serde(default)]
    prompt: Option<String>,
    /// Speed factor: >1.0 = faster, <1.0 = slower (0.5-2.0)
    #[serde(default)]
    speed: Option<f32>,
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
    "http://localhost:9090".to_string()
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        // Generous connect timeout — ominix may be busy processing another request.
        .connect_timeout(Duration::from_secs(30))
        // No request timeout — TTS streaming can take minutes for long text.
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

/// Call a TTS endpoint, collect all bytes, return WAV data.
/// Auto-detects PCM vs WAV response and wraps PCM in a WAV header.
fn fetch_tts_wav(
    client: &reqwest::blocking::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<Vec<u8>, String> {
    let resp = client
        .post(url)
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

    let is_streaming_pcm = content_type.contains("audio/pcm")
        || resp
            .headers()
            .get("transfer-encoding")
            .and_then(|v| v.to_str().ok())
            .map_or(false, |v| v.contains("chunked"));

    // Read response with progress — for streaming PCM, report per-chunk progress
    eprintln!("Receiving TTS audio data...");
    let mut buf = Vec::new();
    if is_streaming_pcm {
        // Pseudo-streaming: ominix-api sends PCM chunks (one per text segment).
        // Read in small increments so we can report progress as segments arrive.
        use std::io::Read;
        let mut reader = resp;
        let mut chunk_buf = [0u8; 32768]; // 32KB read buffer (~0.34s of 24kHz 16-bit mono)
        let mut segments = 0u32;
        let mut last_report = buf.len();
        loop {
            match reader.read(&mut chunk_buf) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk_buf[..n]);
                    // Report progress roughly every 48000 bytes (~1s of audio)
                    if buf.len() - last_report >= 48000 {
                        segments += 1;
                        let duration = buf.len() as f64 / 48000.0;
                        eprintln!(
                            "Received {:.1}s of audio ({} bytes)...",
                            duration,
                            buf.len()
                        );
                        last_report = buf.len();
                    }
                }
                Err(e) => return Err(format!("Failed to read TTS response: {e}")),
            }
        }
        if segments > 0 {
            let duration = buf.len() as f64 / 48000.0;
            eprintln!(
                "Audio stream complete: {:.1}s ({} bytes)",
                duration,
                buf.len()
            );
        }
    } else {
        use std::io::Read;
        let mut reader = resp;
        reader
            .read_to_end(&mut buf)
            .map_err(|e| format!("Failed to read TTS response: {e}"))?;
    }
    let bytes = buf;
    eprintln!("Received {} bytes total", bytes.len());

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
    // Generous timeout: ominix-api is single-threaded (MLX), so /health may block
    // while a TTS/ASR synthesis is in progress.
    match client
        .get(format!("{base_url}/health"))
        .timeout(Duration::from_secs(60))
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
    eprintln!("Checking ominix-api health...");
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let language = input.language.unwrap_or_else(|| "Chinese".to_string());

    // Read audio file and base64-encode it (ominix-api expects JSON with base64 `file` field)
    let file_bytes = match std::fs::read(&input.audio_path) {
        Ok(b) => b,
        Err(e) => fail(&format!(
            "failed to read audio file '{}': {e}",
            input.audio_path
        )),
    };
    use base64::Engine;
    let file_b64 = base64::engine::general_purpose::STANDARD.encode(&file_bytes);

    eprintln!("Transcribing audio ({} bytes)...", file_bytes.len());
    let body = serde_json::json!({
        "file": file_b64,
        "language": language,
        "response_format": "verbose_json"
    });

    // Use model-specific ASR endpoint (Qwen3-ASR)
    let resp = match client
        .post(format!("{base_url}/v1/audio/asr/qwen3"))
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

// ── macOS Say fallback ──────────────────────────────────────────────

/// Synthesize using macOS built-in `say` command.
/// `say` auto-detects language from text and picks the appropriate voice.
/// Outputs AIFF, then converts to WAV via macOS built-in `afconvert`.
fn synthesize_with_say(text: &str, speed: Option<f32>, output_path: &str) -> Result<(), String> {
    let aiff_path = format!("{output_path}.aiff");

    let mut cmd = std::process::Command::new("say");
    cmd.arg("-o").arg(&aiff_path);
    // Map speed factor (0.5-2.0) to words-per-minute (~175 WPM is normal)
    if let Some(s) = speed {
        let wpm = (175.0 * s).clamp(80.0, 400.0) as u32;
        cmd.arg("-r").arg(wpm.to_string());
    }
    cmd.arg(text);

    let status = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("`say` command failed: {e}"))?;

    if !status.success() {
        return Err(format!("`say` exited with status {status}"));
    }

    // Convert AIFF to WAV using macOS built-in afconvert
    let af_status = std::process::Command::new("afconvert")
        .args(["-f", "WAVE", "-d", "LEI16@24000", &aiff_path, output_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Clean up temp AIFF
    let _ = std::fs::remove_file(&aiff_path);

    match af_status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("afconvert failed with status {s}")),
        Err(e) => Err(format!("afconvert failed: {e}")),
    }
}

// ── voice_synthesize ─────────────────────────────────────────────────

fn handle_synthesize(input_json: &str) {
    let input: SynthesizeInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.text.trim().is_empty() {
        fail("'text' must not be empty");
    }

    // Always save to OCTOS_WORK_DIR (inside profile data_dir) so send_file
    // can access the file. Ignore LLM's output_path to avoid sandbox violations.
    let filename = input
        .output_path
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("tts_{}.wav", timestamp()));
    let output_path = if let Ok(work_dir) = std::env::var("OCTOS_WORK_DIR") {
        let dir = Path::new(&work_dir);
        let _ = std::fs::create_dir_all(dir);
        dir.join(&filename).to_string_lossy().to_string()
    } else {
        match std::env::current_dir() {
            Ok(dir) => dir.join(&filename).to_string_lossy().to_string(),
            Err(_) => format!("/tmp/{filename}"),
        }
    };

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

    // Try ominix-api first; fall back to macOS `say` if unavailable
    let base_url = api_base_url();
    let client = http_client();
    let ominix_available = check_health(&client, &base_url).is_ok();

    if ominix_available {
        // Preset speaker — build JSON body with optional prompt/speed
        let mut body = json!({
            "input": input.text,
            "voice": speaker,
            "language": language,
            "response_format": "pcm"
        });
        if let Some(ref prompt) = input.prompt {
            body["prompt"] = json!(prompt);
        }
        if let Some(speed) = input.speed {
            body["speed"] = json!(speed);
        }

        eprintln!("Synthesizing with preset voice '{speaker}'...");
        let url = format!("{base_url}/v1/audio/tts/qwen3");
        match fetch_tts_wav(&client, &url, &body) {
            Ok(wav_bytes) => {
                if let Err(e) = std::fs::write(Path::new(&output_path), &wav_bytes) {
                    fail(&format!("Failed to write {output_path}: {e}"));
                }
                let duration_secs = wav_bytes.len().saturating_sub(44) as f64 / 48000.0;
                eprintln!("Converting to MP3...");
                let final_path = try_convert_to_mp3(&output_path);
                succeed(&format!(
                    "Generated audio: {final_path} ({duration_secs:.1}s, {} bytes). Use send_file to deliver it to the user.",
                    wav_bytes.len()
                ));
            }
            Err(e) => {
                eprintln!("Qwen3-TTS failed ({e}), falling back to macOS Say...");
                // Fall through to macOS Say below
            }
        }
    } else {
        eprintln!("ominix-api not available, using macOS Say...");
    }

    // Fallback: macOS built-in `say` command
    match synthesize_with_say(&input.text, input.speed, &output_path) {
        Ok(()) => {
            let size = std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let duration_secs = size.saturating_sub(44) as f64 / 48000.0;
            eprintln!("Converting to MP3...");
            let final_path = try_convert_to_mp3(&output_path);
            succeed(&format!(
                "Generated audio (macOS Say): {final_path} ({duration_secs:.1}s, {size} bytes). \
                 Note: using built-in macOS voice — install ominix-api for higher-quality Qwen3-TTS. \
                 Use send_file to deliver it to the user."
            ));
        }
        Err(e) => fail(&format!(
            "TTS failed: ominix-api not available, macOS Say also failed: {e}"
        )),
    }
}

/// Try to convert WAV to MP3 using ffmpeg. Returns the MP3 path on success,
/// or the original WAV path if ffmpeg is not available.
fn try_convert_to_mp3(wav_path: &str) -> String {
    let mp3_path = wav_path.replace(".wav", ".mp3");
    let result = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            wav_path,
            "-codec:a",
            "libmp3lame",
            "-q:a",
            "2",
            &mp3_path,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match result {
        Ok(status) if status.success() => {
            // Remove WAV, return MP3
            let _ = std::fs::remove_file(wav_path);
            mp3_path
        }
        _ => wav_path.to_string(),
    }
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
        "list_models" => handle_list_models(&buf),
        "download_model" => handle_download_model(&buf),
        "load_model" => handle_load_model(&buf),
        "unload_model" => handle_unload_model(&buf),
        _ => fail(&format!(
            "Unknown tool '{tool_name}'. Expected: voice_transcribe, voice_synthesize, list_models, download_model, load_model, unload_model"
        )),
    }
}
