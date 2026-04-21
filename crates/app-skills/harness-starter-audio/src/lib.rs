//! Harness starter: audio-artifact custom app.
//!
//! Declares `primary_audio = "audio/*.wav"` and a `synthesize_clip`
//! spawn-only tool. The tool writes a valid pure-Rust WAV file under
//! `<workspace_root>/audio/`.
//!
//! The WAV synthesis is deliberately minimal — this starter focuses on the
//! harness contract shape, not on audio quality. Swap in a real TTS or
//! render engine when adapting the starter.
//!
//! See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md`.

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SynthesizeClipInput {
    /// Label used for the filename and internal chunk header.
    pub label: String,
    /// Duration in milliseconds. Clamped to [100, 5000] to keep smoke-tests
    /// fast. The default is 500 ms.
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizeClipOutput {
    pub artifact_path: PathBuf,
    pub byte_len: usize,
}

const SAMPLE_RATE: u32 = 8000; // 8 kHz is enough for a smoke-test sine tone.
const BITS_PER_SAMPLE: u16 = 16;
const CHANNELS: u16 = 1;
const TONE_HZ: f32 = 440.0;
const MIN_MS: u32 = 100;
const MAX_MS: u32 = 5000;

/// Synthesize a tiny 440 Hz sine-wave WAV clip.
///
/// Writes to `<workspace_root>/audio/<slug>.wav`. Returns the workspace-
/// relative path and the total byte length (useful when paired with the
/// `file_size_min` validator).
pub fn synthesize_clip(
    workspace_root: &Path,
    input: &SynthesizeClipInput,
) -> Result<SynthesizeClipOutput> {
    let dir = workspace_root.join("audio");
    std::fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("create audio dir failed: {}", dir.display()))?;

    let slug = slugify(&input.label);
    let relative = Path::new("audio").join(format!("{slug}.wav"));
    let full = workspace_root.join(&relative);

    let duration_ms = input.duration_ms.unwrap_or(500).clamp(MIN_MS, MAX_MS);
    let bytes = render_sine_wav(duration_ms);
    std::fs::write(&full, &bytes)
        .wrap_err_with(|| format!("write wav failed: {}", full.display()))?;

    Ok(SynthesizeClipOutput {
        artifact_path: relative,
        byte_len: bytes.len(),
    })
}

/// Build an in-memory WAV (RIFF/WAVE, PCM16, mono) with a 440 Hz tone.
pub fn render_sine_wav(duration_ms: u32) -> Vec<u8> {
    let sample_count = (SAMPLE_RATE as u64 * duration_ms as u64 / 1000) as u32;
    let byte_rate = SAMPLE_RATE * (CHANNELS as u32) * (BITS_PER_SAMPLE as u32) / 8;
    let block_align = CHANNELS * BITS_PER_SAMPLE / 8;
    let data_size = sample_count * (block_align as u32);
    let riff_size = 36 + data_size;

    let mut out = Vec::with_capacity(44 + data_size as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");

    // fmt  subchunk
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // subchunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());

    // data subchunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());

    let step = std::f32::consts::TAU * TONE_HZ / (SAMPLE_RATE as f32);
    let amplitude = 0.25_f32;
    for i in 0..sample_count {
        let sample = (amplitude * (step * (i as f32)).sin() * i16::MAX as f32) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

pub fn slugify(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut last_dash = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "clip".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_emit_valid_riff_wave_header() {
        let bytes = render_sine_wav(200);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert!(bytes.len() > 44, "wav body should follow the header");
    }

    #[test]
    fn should_clamp_duration_to_safe_bounds() {
        let tmp = tempfile::tempdir().unwrap();
        let huge = SynthesizeClipInput {
            label: "abuse".into(),
            duration_ms: Some(60_000),
        };
        let out = synthesize_clip(tmp.path(), &huge).unwrap();
        // 5000 ms cap * 8000 Hz * 2 bytes = 80000 data bytes + 44 header.
        assert!(out.byte_len <= 80_100);
    }

    #[test]
    fn should_produce_artifact_under_audio_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let input = SynthesizeClipInput {
            label: "Hello World".into(),
            duration_ms: Some(300),
        };
        let out = synthesize_clip(tmp.path(), &input).unwrap();
        assert_eq!(out.artifact_path, Path::new("audio/hello-world.wav"));
        let full = tmp.path().join(&out.artifact_path);
        assert!(full.exists());
        let bytes = std::fs::read(&full).unwrap();
        assert!(bytes.len() >= 4096, "must satisfy file_size_min:4096");
    }
}
