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

fn api_base_url() -> String {
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
    "http://localhost:8080".to_string()
}

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(5))
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

    let base_url = api_base_url();
    let client = http_client();
    if let Err(e) = check_health(&client, &base_url) {
        fail(&e);
    }

    let language = input.language.unwrap_or_else(|| "Chinese".to_string());

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

    let (endpoint, body) = if let Some(ref ref_audio) = input.reference_audio {
        // Voice cloning mode: use /v1/audio/speech/clone (routes to Base model with ECAPA-TDNN)
        let ref_path = Path::new(ref_audio);
        if !ref_path.exists() {
            fail(&format!("Reference audio not found: {ref_audio}"));
        }
        if !ref_path.is_file() {
            fail(&format!("Not a file: {ref_audio}"));
        }
        (
            format!("{base_url}/v1/audio/speech/clone"),
            json!({
                "input": input.text,
                "reference_audio": ref_audio,
                "language": language
            }),
        )
    } else {
        // Preset voice mode: use /v1/audio/speech (routes to CustomVoice model)
        let speaker = input.speaker.unwrap_or_else(|| "vivian".to_string());
        (
            format!("{base_url}/v1/audio/speech"),
            json!({
                "input": input.text,
                "voice": speaker,
                "language": language
            }),
        )
    };

    let resp = match client
        .post(&endpoint)
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => fail(&format!("TTS request failed: {e}")),
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
