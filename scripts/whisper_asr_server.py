#!/usr/bin/env python3
"""Local, batch-only Whisper ASR service for Octos Voice Lab experiments.

This service intentionally runs outside Octos and binds to loopback by default.
It accepts the same JSON/base64 audio shape used by the local OminiX client and
adds explicit model-probability-based rejection metadata for Whisper.

Example (using an isolated environment containing openai-whisper):

  python3 scripts/whisper_asr_server.py \
    --python-path /path/to/whisper-packages \
    --download-root /path/to/whisper-models
"""

from __future__ import annotations

import argparse
import base64
import binascii
import http.server
import json
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_MAX_AUDIO_BYTES = 25_000_000
DEFAULT_TEMPERATURES = (0.0, 0.2, 0.4, 0.6, 0.8, 1.0)


class RequestValidationError(ValueError):
    """Raised for a malformed local transcription request."""


@dataclass(frozen=True)
class TranscriptionRequest:
    audio_bytes: bytes
    language: str | None
    verbose: bool


def parse_transcription_request(raw_body: bytes, *, max_audio_bytes: int) -> TranscriptionRequest:
    """Validate the JSON/base64 request shared by OminiX and this lab service."""
    try:
        body = json.loads(raw_body)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise RequestValidationError("request body must be valid JSON") from error

    if not isinstance(body, dict):
        raise RequestValidationError("request body must be a JSON object")

    encoded_file = body.get("file")
    if not isinstance(encoded_file, str) or not encoded_file:
        raise RequestValidationError("'file' must be a base64-encoded audio string")

    if encoded_file.startswith("data:"):
        _, separator, encoded_file = encoded_file.partition(",")
        if not separator:
            raise RequestValidationError("data URL audio is missing its base64 payload")

    try:
        audio_bytes = base64.b64decode(encoded_file, validate=True)
    except (binascii.Error, ValueError) as error:
        raise RequestValidationError("'file' is not valid base64 audio") from error

    if not audio_bytes:
        raise RequestValidationError("audio file is empty")
    if len(audio_bytes) > max_audio_bytes:
        raise RequestValidationError(
            f"audio file is too large ({len(audio_bytes)} bytes; limit {max_audio_bytes})"
        )

    language = body.get("language")
    if language is not None and (not isinstance(language, str) or not language.strip()):
        raise RequestValidationError("'language' must be a non-empty string when provided")

    response_format = body.get("response_format", "json")
    if response_format not in {"json", "verbose_json"}:
        raise RequestValidationError("'response_format' must be 'json' or 'verbose_json'")

    return TranscriptionRequest(
        audio_bytes=audio_bytes,
        language=language.strip() if isinstance(language, str) else None,
        verbose=response_format == "verbose_json",
    )


def format_transcription_response(
    *,
    text: str,
    language: str,
    processing_time: float,
    audio_duration: float,
    segments: list[dict[str, Any]],
    rejection_signal: dict[str, float] | None,
    model_info: dict[str, str],
    verbose: bool,
) -> dict[str, Any]:
    """Make rejection a successful ASR outcome rather than an error."""
    text = text.strip()
    rejected = not text
    response: dict[str, Any] = {"text": text, "rejected": rejected}

    if rejected:
        response["reject_reason"] = "no_speech"
        if rejection_signal is not None:
            response["rejection_signal"] = rejection_signal

    if verbose:
        response.update(
            {
                "language": language,
                "duration": audio_duration,
                "processing_time": processing_time,
                "realtime_factor": audio_duration / processing_time if processing_time else None,
                "segments": segments,
                "model": model_info,
            }
        )

    return response


def normalize_language(language: str | None) -> str:
    """Normalize the labels used by Octos and the Whisper tokenizer."""
    if language is None:
        return "zh"
    normalized = language.strip().lower()
    if normalized in {"chinese", "mandarin", "zh-cn", "zh_hans"}:
        return "zh"
    return normalized


class WhisperTranscriber:
    """A single loaded model protected from concurrent decoder access."""

    def __init__(
        self,
        *,
        model_name: str,
        device: str,
        download_root: str,
        no_speech_threshold: float,
        logprob_threshold: float,
    ) -> None:
        try:
            import whisper
        except ImportError as error:
            raise RuntimeError(
                "openai-whisper is unavailable. Install it in an isolated environment "
                "or pass --python-path to its package directory."
            ) from error

        self.whisper = whisper
        self.model_name = model_name
        self.device = device
        self.no_speech_threshold = no_speech_threshold
        self.logprob_threshold = logprob_threshold
        self.model = whisper.load_model(model_name, device=device, download_root=download_root)
        self.lock = threading.Lock()

    @property
    def model_info(self) -> dict[str, str]:
        return {
            "name": self.model_name,
            "runtime": "openai-whisper",
            "version": self.whisper.__version__,
            "device": self.device,
        }

    def transcribe(self, request: TranscriptionRequest) -> dict[str, Any]:
        suffix = ".wav"
        with tempfile.NamedTemporaryFile(suffix=suffix, delete=False) as audio_file:
            audio_file.write(request.audio_bytes)
            audio_path = Path(audio_file.name)

        try:
            with self.lock:
                audio = self.whisper.load_audio(str(audio_path))
                audio_duration = len(audio) / 16000
                started = time.perf_counter()
                result = self.model.transcribe(
                    audio,
                    language=normalize_language(request.language),
                    temperature=DEFAULT_TEMPERATURES,
                    compression_ratio_threshold=2.4,
                    logprob_threshold=self.logprob_threshold,
                    no_speech_threshold=self.no_speech_threshold,
                    condition_on_previous_text=True,
                    fp16=False,
                    verbose=False,
                )
                processing_time = time.perf_counter() - started
                text = str(result.get("text", ""))
                segments = [
                    {
                        key: segment[key]
                        for key in (
                            "id",
                            "start",
                            "end",
                            "text",
                            "temperature",
                            "avg_logprob",
                            "compression_ratio",
                            "no_speech_prob",
                        )
                        if key in segment
                    }
                    for segment in result.get("segments", [])
                ]
                rejection_signal = self._rejection_signal(audio) if not text.strip() else None
        finally:
            audio_path.unlink(missing_ok=True)

        return format_transcription_response(
            text=text,
            language=normalize_language(request.language),
            processing_time=processing_time,
            audio_duration=audio_duration,
            segments=segments,
            rejection_signal=rejection_signal,
            model_info=self.model_info,
            verbose=request.verbose,
        )

    def _rejection_signal(self, audio: Any) -> dict[str, float]:
        """Expose the first-window signal that `transcribe()` omits when it skips."""
        from whisper.audio import N_FRAMES, N_SAMPLES

        mel = self.whisper.log_mel_spectrogram(
            audio,
            n_mels=self.model.dims.n_mels,
            padding=N_SAMPLES,
        )
        mel = self.whisper.pad_or_trim(mel, N_FRAMES).to(self.model.device)
        decoded = self.whisper.decode(
            self.model,
            mel,
            self.whisper.DecodingOptions(
                language="zh",
                task="transcribe",
                fp16=False,
            ),
        )
        return {
            "no_speech_prob": float(decoded.no_speech_prob),
            "avg_logprob": float(decoded.avg_logprob),
        }


class WhisperAsrHttpServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(
        self,
        server_address: tuple[str, int],
        handler_class: type[http.server.BaseHTTPRequestHandler],
        *,
        transcriber: WhisperTranscriber,
        max_audio_bytes: int,
        allow_origin: str,
    ) -> None:
        super().__init__(server_address, handler_class)
        self.transcriber = transcriber
        self.max_audio_bytes = max_audio_bytes
        self.allow_origin = allow_origin


class Handler(http.server.BaseHTTPRequestHandler):
    server: WhisperAsrHttpServer

    def do_OPTIONS(self) -> None:
        self.send_response(204)
        self._send_cors_headers()
        self.send_header("Access-Control-Allow-Methods", "POST, GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.end_headers()

    def do_GET(self) -> None:
        if self.path == "/health":
            self._send_json(200, {"status": "ok", "model": self.server.transcriber.model_info})
            return
        if self.path == "/v1/models":
            self._send_json(200, {"data": [self.server.transcriber.model_info]})
            return
        self._send_error(404, "not found", "invalid_request_error")

    def do_POST(self) -> None:
        if self.path != "/v1/audio/transcriptions":
            self._send_error(404, "not found", "invalid_request_error")
            return

        try:
            content_length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            self._send_error(400, "missing or invalid Content-Length", "invalid_request_error")
            return

        max_request_bytes = self.server.max_audio_bytes * 2
        if content_length < 1 or content_length > max_request_bytes:
            self._send_error(413, "request body exceeds the local service limit", "invalid_request_error")
            return

        try:
            request = parse_transcription_request(
                self.rfile.read(content_length),
                max_audio_bytes=self.server.max_audio_bytes,
            )
            response = self.server.transcriber.transcribe(request)
        except RequestValidationError as error:
            self._send_error(400, str(error), "invalid_request_error")
            return
        except Exception as error:  # noqa: BLE001 - convert local inference failures to JSON.
            self.log_error("transcription failed: %s", error)
            self._send_error(500, str(error), "server_error")
            return

        self._send_json(200, response)

    def _send_error(self, status: int, message: str, error_type: str) -> None:
        self._send_json(status, {"error": {"message": message, "type": error_type}})

    def _send_json(self, status: int, body: dict[str, Any]) -> None:
        encoded = json.dumps(body, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self._send_cors_headers()
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def _send_cors_headers(self) -> None:
        self.send_header("Access-Control-Allow-Origin", self.server.allow_origin)
        self.send_header("Vary", "Origin")

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write("[whisper-asr] " + fmt % args + "\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8091)
    parser.add_argument("--model", default="small")
    parser.add_argument(
        "--device",
        default="cpu",
        choices=("cpu", "mps"),
        help="CPU is the verified runtime in this environment; MPS remains experimental.",
    )
    parser.add_argument(
        "--download-root",
        required=True,
        help="Directory containing the official Whisper checkpoint; the service never downloads models itself.",
    )
    parser.add_argument(
        "--python-path",
        default=None,
        help="Optional directory containing isolated openai-whisper and tiktoken packages.",
    )
    parser.add_argument("--no-speech-threshold", type=float, default=0.6)
    parser.add_argument("--logprob-threshold", type=float, default=-0.5)
    parser.add_argument("--max-audio-bytes", type=int, default=DEFAULT_MAX_AUDIO_BYTES)
    parser.add_argument(
        "--allow-origin",
        default="*",
        help="CORS origin for the loopback-only Voice Lab service.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.python_path:
        sys.path.insert(0, args.python_path)
    if args.max_audio_bytes < 1:
        raise SystemExit("--max-audio-bytes must be positive")

    started = time.perf_counter()
    transcriber = WhisperTranscriber(
        model_name=args.model,
        device=args.device,
        download_root=args.download_root,
        no_speech_threshold=args.no_speech_threshold,
        logprob_threshold=args.logprob_threshold,
    )
    loaded_in = time.perf_counter() - started
    server = WhisperAsrHttpServer(
        (args.host, args.port),
        Handler,
        transcriber=transcriber,
        max_audio_bytes=args.max_audio_bytes,
        allow_origin=args.allow_origin,
    )
    print(
        f"Whisper ASR ready at http://{args.host}:{args.port} "
        f"(model={args.model}, device={args.device}, loaded_in={loaded_in:.2f}s)",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nWhisper ASR stopping", flush=True)
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
