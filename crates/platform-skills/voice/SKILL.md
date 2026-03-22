---
name: voice
description: OminiX ASR (speech-to-text), TTS (text-to-speech), voice cloning with saved profiles, and model management via Qwen3 models on Apple Silicon. Triggers: voice, transcribe audio, text to speech, voice clone, clone voice, save voice, my voice, model management, download model, 语音识别, 语音合成, 语音克隆, 保存声音, speak this, read aloud, 模型管理.
version: 1.0.0
author: octos
always: true
---

# OminiX ASR / TTS / Voice Cloning / Model Management

On-device speech-to-text, text-to-speech, voice cloning, and model management using OminiX Qwen3 ASR/TTS models on Apple Silicon.

## Voice Cloning Workflow

When a user sends a voice sample and wants to clone their voice:

1. **Ask for a name**: Always ask the user what they want to name this voice profile (e.g. "my_voice", "alice")
2. **Clone + save**: Use `voice_clone` with `save_as` parameter to both generate sample speech AND save the profile
3. **Future use**: The saved profile name can be used as `speaker` in `voice_synthesize` for all future TTS

Voice profiles are **private per user/profile** — stored in the profile's own data directory, not shared globally.

## Configuration

The skill auto-discovers the ominix-api server URL via (in priority order):
1. `OMINIX_API_URL` environment variable
2. Discovery file `~/.ominix/api_url` (written by ominix-api on startup)
3. Default: `http://localhost:9090`

## Checking Available Models

Use `list_models` to see what's installed. The response includes an `endpoints` array for each model, telling you which URL to use:

```json
{"data": [
  {"id": "qwen3-asr", "type": "asr", "endpoints": ["/v1/audio/asr/qwen3"]},
  {"id": "Qwen3-TTS-CustomVoice-8bit", "type": "qwen3_tts", "endpoints": ["/v1/audio/tts/qwen3"]},
  {"id": "Qwen3-TTS-Base-8bit", "type": "qwen3_tts", "endpoints": ["/v1/audio/tts/clone"]}
]}
```

If a model you need is missing, use `download_model` then `load_model` to install it.

## API Endpoints

| Function | Endpoint | Model |
|---|---|---|
| Preset TTS | `POST /v1/audio/tts/qwen3` | Qwen3-TTS CustomVoice |
| Voice Clone | `POST /v1/audio/tts/clone` | Qwen3-TTS Base (ECAPA-TDNN x-vector) |
| GPT-SoVITS | `POST /v1/audio/tts/sovits` | GPT-SoVITS (legacy) |
| Qwen3-ASR | `POST /v1/audio/asr/qwen3` | Qwen3-ASR encoder-decoder |
| Paraformer | `POST /v1/audio/asr/paraformer` | Paraformer CTC-based |

TTS and ASR run on separate threads — they do not block each other.

## Tools

### voice_transcribe

Transcribe an audio file to text via Qwen3-ASR. Supports WAV, OGG, MP3, FLAC, M4A.

```json
{"audio_path": "/tmp/voice.ogg", "language": "Chinese"}
```

**Parameters:**
- `audio_path` (required): Absolute path to the audio file
- `language` (optional, default "Chinese"): "Chinese", "English", "Japanese", "Korean", "Cantonese", etc.

### voice_synthesize

Generate speech audio from text. Produces a WAV file. The `speaker` parameter accepts both preset names and saved voice profile names.

```json
{"text": "Hello world", "language": "english", "speaker": "vivian"}
```

**Parameters:**
- `text` (required): Text to synthesize
- `output_path` (optional): Where to save WAV. Default: `/tmp/octos_tts_<timestamp>.wav`
- `language` (optional, default "chinese"): "chinese", "english", "japanese", "korean"
- `speaker` (optional, default "vivian"): Preset name OR saved voice profile name

**Preset speakers:** vivian, serena, ryan, aiden, eric, dylan (English/Chinese), uncle_fu (Chinese), ono_anna (Japanese), sohee (Korean)

### voice_clone

Clone any voice from a short reference audio (3-10 seconds) and synthesize new speech in that voice. Uses ECAPA-TDNN x-vector speaker embedding. Requires the **Base** TTS model variant (not CustomVoice).

```json
{"reference_audio": "/tmp/my_voice.ogg", "text": "Hello!", "save_as": "alice"}
```

**Parameters:**
- `reference_audio` (required): Absolute path to reference audio (3-10s)
- `text` (required): Text to synthesize in the cloned voice
- `save_as` (optional): Save this voice as a named profile for future use
- `output_path` (optional): Where to save WAV. Default: `/tmp/octos_clone_<timestamp>.wav`
- `language` (optional, default "chinese"): "chinese", "english", "japanese", "korean"

### voice_save_profile

Save an audio sample as a named voice profile without generating speech. Useful when you just want to save the voice for later.

```json
{"name": "boss", "audio_path": "/tmp/boss_voice.ogg"}
```

**Parameters:**
- `name` (required): Name for this voice profile
- `audio_path` (required): Absolute path to the reference audio (3-10s)

### voice_list_profiles

List all saved voice profiles for this user.

```json
{}
```

### list_models

List all loaded models and available models in the catalog.

### download_model

Download a model from the catalog.

**Parameters:**
- `model_id` (required): Model ID from the catalog

### load_model

Load a downloaded model into memory for inference.

**Parameters:**
- `model` (required): Model name or path
- `model_type` (optional, default "llm"): "llm", "asr", "tts"

### unload_model

Unload a model from memory.

**Parameters:**
- `model_type` (required): Type of model to unload — "llm", "asr", "tts"
