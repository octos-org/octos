---
name: voice-skill
description: Speech-to-text (ASR) and text-to-speech (TTS) via OminiX Qwen3 models. Triggers: voice, transcribe, speech to text, text to speech, read aloud, speak this, send voice, 语音识别, 语音合成, 朗读, 发语音.
version: 1.0.0
author: octos
always: true
---

# Voice: ASR & TTS & Voice Cloning

On-device speech-to-text, text-to-speech, and voice cloning using OminiX Qwen3 models.

## Models

| Model | Purpose | Required For |
|-------|---------|-------------|
| `Qwen3-ASR-1.7B-8bit` | Speech-to-text | voice_transcribe |
| `Qwen3-TTS-12Hz-1.7B-CustomVoice-8bit` | TTS with preset voices | voice_synthesize (preset speakers) |
| `Qwen3-TTS-12Hz-1.7B-Base` | TTS with x-vector voice cloning | voice_synthesize (voice cloning with reference_audio) |

Download models via ominix-api:
```bash
curl -X POST http://localhost:8080/v1/models/download -d '{"repo_id": "Qwen/Qwen3-TTS-12Hz-1.7B-Base"}'
```

## When to Use

- **ASR (voice_transcribe)**: When you receive a voice message or audio file attachment, ALWAYS transcribe it first to understand what the user said.
- **TTS (voice_synthesize)**: When the user asks you to "speak", "read aloud", "send voice", "发语音", or wants an audio response. Generate the WAV file, then use `send_file` to deliver it.
- **Voice Cloning**: When the user wants to clone a voice, they send a 3-10s audio clip. Use that as `reference_audio` in voice_synthesize. Requires the Base model.

## Tools

### voice_transcribe

Transcribe audio to text. Supports WAV, OGG, MP3, FLAC, M4A.

```json
{"audio_path": "voice-input.ogg", "language": "Chinese"}
```

- `audio_path` (required): Absolute path to the audio file
- `language` (optional, default "Chinese"): "Chinese", "English", "Japanese", "Korean", "Cantonese"

### voice_synthesize

Generate speech audio from text. Produces a WAV file. Supports both preset voices and voice cloning via reference audio.

**Preset voice:**
```json
{"text": "你好世界", "language": "chinese", "speaker": "vivian"}
```

**Voice cloning (requires Base model):**
```json
{"text": "你好世界", "language": "chinese", "reference_audio": "/path/to/reference.wav"}
```

- `text` (required): Text to synthesize
- `output_path` (optional): Where to save WAV. Default: `<OCTOS_WORK_DIR>/voice/octos_tts_<timestamp>.wav`
- `language` (optional, default "chinese"): "chinese", "english", "japanese", "korean"
- `speaker` (optional, default "vivian"): vivian, serena, ryan, aiden, eric, dylan, uncle_fu, ono_anna, sohee
- `reference_audio` (optional): Path to a 3-10s WAV reference clip for voice cloning. When provided, the Base model's x-vector speaker encoder extracts the voice embedding and generates speech in that voice. Overrides `speaker`.

**IMPORTANT**: After generating audio, always use `send_file` to send the WAV file to the user.
