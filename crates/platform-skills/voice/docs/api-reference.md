# Voice Skill: API + Model Reference

## Server discovery

The skill auto-discovers the ominix-api server URL via (in priority order):

1. `OMINIX_API_URL` environment variable
2. Discovery file `~/.ominix/api_url` (written by ominix-api on startup)
3. Probes candidate ports `http://localhost:9090`, `http://localhost:8080`,
   `http://localhost:8081` — first one answering `/health` within 500 ms wins
4. Falls back to macOS `say` only when every probe fails

## Endpoints

| Function   | Endpoint                       | Model                       |
|------------|--------------------------------|-----------------------------|
| Preset TTS | `POST /v1/audio/tts/qwen3`     | Qwen3-TTS CustomVoice       |
| Qwen3-ASR  | `POST /v1/audio/asr/qwen3`     | Qwen3-ASR encoder-decoder   |
| Paraformer | `POST /v1/audio/asr/paraformer`| Paraformer CTC-based        |

TTS and ASR run on separate threads — they do not block each other.

## Checking available models

`list_models` returns an `endpoints` array per model so you can tell which URL
to call:

```json
{"data": [
  {"id": "qwen3-asr", "type": "asr", "endpoints": ["/v1/audio/asr/qwen3"]},
  {"id": "Qwen3-TTS-CustomVoice-8bit", "type": "qwen3_tts", "endpoints": ["/v1/audio/tts/qwen3"]}
]}
```

If a needed model is missing: `download_model` then `load_model`. Downloads
can take several minutes for large models.

## Preset speakers

| Speaker     | Languages         |
|-------------|-------------------|
| vivian      | English / Chinese (default) |
| serena      | English / Chinese |
| ryan        | English / Chinese |
| aiden       | English / Chinese |
| eric        | English / Chinese |
| dylan       | English / Chinese |
| uncle_fu    | Chinese           |
| ono_anna    | Japanese          |
| sohee       | Korean            |

## Tool parameter cheat-sheet

### voice_transcribe

- `audio_path` (required): absolute path to WAV/OGG/MP3/FLAC/M4A
- `language` (optional, default `"Chinese"`): `"Chinese"`, `"English"`,
  `"Japanese"`, `"Korean"`, `"Cantonese"`, …

### voice_synthesize

- `text` (required)
- `output_path` (optional): default auto-generated in `OCTOS_WORK_DIR`
- `language` (optional, default `"chinese"`): `"chinese"`, `"english"`,
  `"japanese"`, `"korean"`
- `speaker` (optional, default `"vivian"`): one of the preset names above
- `prompt` (optional): style/emotion instruction (see
  `emotion-prompts.md`)
- `speed` (optional, default 1.0): 0.5–2.0

### load_model

- `model` (required): name or path of a downloaded model
- `model_type` (optional, default `"llm"`): `"llm"`, `"asr"`, `"tts"`

### unload_model

- `model_type` (required): `"llm"`, `"asr"`, `"tts"`
