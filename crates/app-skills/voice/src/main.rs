//! Voice app-skill binary (ASR + TTS) via ominix-api.
//!
//! Protocol: `./voice-skill <tool_name>` with JSON on stdin, JSON on stdout.

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
    /// Path to a 3-10s WAV reference clip for x-vector voice cloning (requires Base model).
    #[serde(default)]
    reference_audio: Option<String>,
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Resolve the ominix-api base URL. Checks in order:
///   1. OMINIX_API_URL env var
///   2. ~/.ominix/api_url discovery file
///   3. Default http://localhost:9090
fn ominix_base_url() -> String {
    if let Ok(url) = std::env::var("OMINIX_API_URL") {
        return url.trim_end_matches('/').to_string();
    }
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
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Duration::from_secs(15))
        .build()
        .expect("failed to build HTTP client")
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
             Make sure ominix-api is running."
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

    let base_url = ominix_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let language = input.language.unwrap_or_else(|| "Chinese".to_string());

    // Read audio file and base64-encode it (ominix-api expects base64 in `file` field)
    let file_bytes = match std::fs::read(&input.audio_path) {
        Ok(b) => b,
        Err(e) => fail(&format!(
            "failed to read audio file '{}': {e}",
            input.audio_path
        )),
    };
    use base64::Engine;
    let file_b64 = base64::engine::general_purpose::STANDARD.encode(&file_bytes);

    let body = json!({
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

// ── voice_synthesize ─────────────────────────────────────────────────

fn handle_synthesize(input_json: &str) {
    let input: SynthesizeInput = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(e) => fail(&format!("Invalid input: {e}")),
    };

    if input.text.trim().is_empty() {
        fail("'text' must not be empty");
    }

    let base_url = ominix_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let output_path = input.output_path.unwrap_or_else(|| {
        // Prefer OCTOS_WORK_DIR (set by PluginTool for per-profile isolation),
        // fall back to OCTOS_DATA_DIR, then /tmp as last resort.
        let base = std::env::var("OCTOS_WORK_DIR")
            .or_else(|_| std::env::var("OCTOS_DATA_DIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        let dir = Path::new(&base).join("voice");
        std::fs::create_dir_all(&dir).ok();
        format!("{}/octos_tts_{}.wav", dir.display(), timestamp())
    });

    if let Some(parent) = Path::new(&output_path).parent() {
        if !parent.exists() {
            fail(&format!(
                "Output directory does not exist: {}",
                parent.display()
            ));
        }
    }

    let language = input.language.unwrap_or_else(|| "chinese".to_string());

    let resp = if let Some(ref ref_audio) = input.reference_audio {
        // Voice cloning → /v1/audio/tts/clone (Base model, x-vector, multipart)
        let ref_path = Path::new(ref_audio);
        if !ref_path.exists() {
            fail(&format!("Reference audio not found: {ref_audio}"));
        }
        if !ref_path.is_file() {
            fail(&format!("Not a file: {ref_audio}"));
        }
        let ref_bytes = match std::fs::read(ref_path) {
            Ok(b) => b,
            Err(e) => fail(&format!("Failed to read reference audio: {e}")),
        };
        use reqwest::blocking::multipart::{Form, Part};
        let form = Form::new()
            .text("input", input.text.clone())
            .text("language", language.clone())
            .part(
                "reference_audio",
                Part::bytes(ref_bytes)
                    .file_name("ref.wav")
                    .mime_str("audio/wav")
                    .unwrap(),
            );
        let endpoint = format!("{base_url}/v1/audio/tts/clone?format=wav");
        match client.post(&endpoint).multipart(form).send() {
            Ok(r) => r,
            Err(e) => fail(&format!("Clone request failed: {e}")),
        }
    } else {
        // Preset voice → /v1/audio/tts/qwen3 (CustomVoice model, JSON)
        let speaker = input.speaker.unwrap_or_else(|| "vivian".to_string());
        let endpoint = format!("{base_url}/v1/audio/tts/qwen3?format=wav");
        let body = json!({
            "input": input.text,
            "voice": speaker,
            "language": language
        });
        match client.post(&endpoint).json(&body).send() {
            Ok(r) => r,
            Err(e) => fail(&format!("TTS request failed: {e}")),
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let resp_text = resp.text().unwrap_or_default();
        fail(&format!(
            "TTS error (HTTP {status}): {}",
            truncate(&resp_text, 200)
        ));
    }

    let wav_bytes = match resp.bytes() {
        Ok(b) => b,
        Err(e) => fail(&format!("Failed to read TTS response: {e}")),
    };

    if wav_bytes.len() < 44 {
        fail("TTS returned invalid WAV data (too small)");
    }

    if let Err(e) = std::fs::write(Path::new(&output_path), &wav_bytes) {
        fail(&format!("Failed to write {output_path}: {e}"));
    }

    // 24kHz 16-bit mono = 48000 bytes/sec
    let duration_secs = wav_bytes.len().saturating_sub(44) as f64 / 48000.0;

    let mode = if input.reference_audio.is_some() {
        "cloned voice"
    } else {
        "preset voice"
    };
    succeed(&format!(
        "Generated audio: {output_path} ({duration_secs:.1}s, {mode}, {} bytes). Use send_file to deliver it to the user.",
        wav_bytes.len()
    ));
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
        _ => fail(&format!(
            "Unknown tool '{tool_name}'. Expected: voice_transcribe, voice_synthesize"
        )),
    }
}
