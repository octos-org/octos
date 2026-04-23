---
name: harness-starter-audio
description: Harnessed audio-artifact starter. Synthesizes a minimal WAV file under audio/ and relies on the workspace contract to deliver it.
version: 1.0.0
author: octos
always: false
---

# Harness Starter: Audio

An audio workflow starter. Synthesizes a tiny 440 Hz WAV clip as a standin
for a real TTS engine. Copy this crate when you want to build an app that
ships an audio deliverable (TTS, podcast render, sound effect generation,
etc.).

## What this starter demonstrates

- Multi-validator contract: `file_exists:$primary_audio` and
  `file_size_min:$primary_audio:4096`.
- Named artifact (`primary_audio`) plus role alias (`primary`).
- Glob-based resolution (`audio/*.wav`).
- `on_failure: ["notify_user:..."]` for structured failure reporting.

See `docs/OCTOS_HARNESS_DEVELOPER_GUIDE.md` for the full contract.

## Tools

### synthesize_clip

Synthesize a short WAV clip for a label.

```json
{"label": "weekly digest", "duration_ms": 1000}
```

**Parameters:**
- `label` (required): used to derive the filename (`audio/weekly-digest.wav`).
- `duration_ms` (optional): clip duration in ms, clamped to [100, 5000].
  Default 500.

**Artifact:**
- writes `audio/<slug>.wav`
- policy `primary_audio = "audio/*.wav"` resolves to this path.
- `file_size_min:$primary_audio:4096` enforces non-empty output.

## Replace this stub

The synthesizer produces a pure sine tone; it is not a real TTS. When
adopting the starter, replace `render_sine_wav` with your render engine
(piper, eSpeak, ElevenLabs, Coqui, etc.) and keep the rest of the contract
unchanged.
