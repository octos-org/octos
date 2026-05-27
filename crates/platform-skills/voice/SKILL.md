---
name: voice
description: OminiX ASR (speech-to-text), preset-voice TTS with emotion/speed control, and model management via Qwen3 models on Apple Silicon. For voice cloning and custom voice profiles, use mofa-fm. Triggers: voice, transcribe audio, text to speech, speak this, read aloud, model management, download model, 语音识别, 语音合成, 模型管理.
version: 1.1.2
author: octos
always: true
---

# OminiX ASR / TTS / Model Management

On-device speech-to-text and preset-voice text-to-speech with emotion control
plus model lifecycle management, backed by Qwen3 ASR/TTS models running on the
local ominix-api server (Apple Silicon).

## Boundary — when NOT to use this skill

- **Voice cloning / custom voice profiles** → use **mofa-fm**
  (`fm_tts`, `fm_voice_save`, `fm_voice_list`, `fm_voice_delete`). This skill
  is **preset-voice only**.
- **Emotion prompts in fallback mode** → not supported. When ominix-api is
  unreachable, `voice_synthesize` falls through to the macOS built-in `say`
  command, which auto-picks a system voice from the text language and ignores
  the `prompt` parameter.

## Tools

| Tool               | Purpose                                                    |
|--------------------|------------------------------------------------------------|
| `voice_transcribe` | ASR — WAV/OGG/MP3/FLAC/M4A → text                          |
| `voice_synthesize` | Preset-voice TTS with optional emotion + speed             |
| `list_models`      | List loaded + catalog models on the local ominix-api       |
| `download_model`   | Pull a catalog model to local disk                         |
| `load_model`       | Load a downloaded model into GPU memory                    |
| `unload_model`     | Free a loaded model from GPU memory                        |

## Quick recipes

### Transcribe a voice message

```json
{"audio_path": "voice.ogg", "language": "Chinese"}
```

### Synthesize plain speech

```json
{"text": "Hello world", "language": "english", "speaker": "ryan"}
```

### Synthesize with emotion

```json
{"text": "我太开心了！", "speaker": "vivian", "prompt": "用兴奋激动的语气说话，充满热情和活力"}
```

After `voice_synthesize` returns a file path, deliver the audio with
`send_file`.

## Further reading

- Emotion / style prompts (Chinese + English) → `docs/emotion-prompts.md`
- Server discovery, endpoints, preset speakers, full parameter cheat-sheet →
  `docs/api-reference.md`

## Anti-patterns

- Calling `voice_synthesize` with a `prompt` while ominix-api is down — the
  fallback (`say`) silently drops the emotion. Use `list_models` to confirm
  Qwen3-TTS is loaded before relying on emotion control.
- Passing a non-preset speaker name (e.g. a cloned voice id) — this skill
  only handles preset voices; route the call to **mofa-fm** instead.
- Skipping `download_model` + `load_model` after a fresh install — the
  catalog model is not loaded until you load it explicitly.
