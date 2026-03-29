---
name: voice
description: OminiX ASR (speech-to-text), preset-voice TTS with emotion/speed control, and model management via Qwen3 models on Apple Silicon. For voice cloning and custom voice profiles, use mofa-fm. Triggers: voice, transcribe audio, text to speech, speak this, read aloud, model management, download model, 语音识别, 语音合成, 模型管理.
version: 1.1.0
author: octos
always: true
---

# OminiX ASR / TTS / Model Management

On-device speech-to-text, preset-voice text-to-speech with emotion control, and model management using OminiX Qwen3 ASR/TTS models on Apple Silicon.

> **Voice cloning and custom voice profiles** are handled by **mofa-fm** (fm_tts, fm_voice_save, fm_voice_list, fm_voice_delete). This skill only supports preset voices.

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
  {"id": "Qwen3-TTS-CustomVoice-8bit", "type": "qwen3_tts", "endpoints": ["/v1/audio/tts/qwen3"]}
]}
```

If a model you need is missing, use `download_model` then `load_model` to install it.

## API Endpoints

| Function | Endpoint | Model |
|---|---|---|
| Preset TTS | `POST /v1/audio/tts/qwen3` | Qwen3-TTS CustomVoice |
| Qwen3-ASR | `POST /v1/audio/asr/qwen3` | Qwen3-ASR encoder-decoder |
| Paraformer | `POST /v1/audio/asr/paraformer` | Paraformer CTC-based |

TTS and ASR run on separate threads — they do not block each other.

## Tools

### voice_transcribe

Transcribe an audio file to text via Qwen3-ASR. Supports WAV, OGG, MP3, FLAC, M4A.

```json
{"audio_path": "voice.ogg", "language": "Chinese"}
```

**Parameters:**
- `audio_path` (required): Absolute path to the audio file
- `language` (optional, default "Chinese"): "Chinese", "English", "Japanese", "Korean", "Cantonese", etc.

### voice_synthesize

Generate speech audio from text using a preset voice. Uses Qwen3-TTS when ominix-api is running (high quality, emotion/style control). Falls back to macOS built-in `say` command when unavailable.

macOS Say auto-detects language from text and picks the appropriate built-in voice. Emotion prompts (`prompt`) are not supported in fallback mode.

```json
{"text": "Hello world", "language": "english", "speaker": "ryan"}
```

With emotion:
```json
{"text": "我太开心了！", "speaker": "vivian", "prompt": "用兴奋激动的语气说话，充满热情和活力"}
```

**Parameters:**
- `text` (required): Text to synthesize
- `output_path` (optional): Where to save audio. Default: auto-generated in OCTOS_WORK_DIR
- `language` (optional, default "chinese"): "chinese", "english", "japanese", "korean"
- `speaker` (optional, default "vivian"): Preset name only — vivian, serena, ryan, aiden, eric, dylan, uncle_fu, ono_anna, sohee
- `prompt` (optional): Style/emotion instruction (see tables below)
- `speed` (optional, default 1.0): Speed factor 0.5-2.0

**Preset speakers:** vivian, serena, ryan, aiden, eric, dylan (English/Chinese), uncle_fu (Chinese), ono_anna (Japanese), sohee (Korean)

**Verified Chinese emotion prompts** (best with vivian, serena, dylan, uncle_fu):

| Style | Prompt |
|-------|--------|
| Excited | `用兴奋激动的语气说话，充满热情和活力` |
| Sad | `用悲伤失望的语气说话，声音低沉，语速缓慢` |
| Cheerful | `用开朗愉快的语气说话，声音明亮上扬，节奏轻快` |
| Shout | `用大声喊叫的方式说话，声音高亢有力，语速快` |
| Sarcastic | `用讽刺嘲讽的语气说话，语调阴阳怪气，拖长尾音` |
| Soft | `用温柔轻柔的语气说话` |
| Panic | `用惊慌恐惧的语气说话，声音颤抖，语速急促` |

**English emotion prompts** (best with ryan, aiden):

| Style | Prompt |
|-------|--------|
| Excited | `Speak with excitement and enthusiasm, full of energy` |
| Sad | `Speak in a sad, disappointed tone, voice low and slow` |
| Cheerful | `Speak cheerfully with a bright, upbeat voice` |
| Shout | `Shout loudly with a powerful, high-pitched voice` |
| Sarcastic | `Speak sarcastically with a mocking, drawn-out tone` |
| Soft | `Speak gently and softly` |
| Panic | `Speak in a panicked, trembling voice, fast and breathless` |

Custom free-form prompts are also supported — include emotion + timbre + pace descriptors for strongest control.

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
