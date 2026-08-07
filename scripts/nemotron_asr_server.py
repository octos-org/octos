#!/usr/bin/env python3
"""Serve Nemotron 3.5 locally with the OMiniX JSON/base64 ASR contract.

The service supervises an official NeMo-Speech.cpp HTTP backend and translates
OMiniX-style JSON requests into its OpenAI-compatible multipart endpoint.
It binds to loopback by default and deliberately treats an empty transcript as
a successful no-speech rejection.

Example:

  python3 scripts/nemotron_asr_server.py \
    --nemo-speech-bin ~/.local/bin/nemo-speech \
    --model ~/.OminiX/models/nemotron-3.5-asr-streaming-0.6b/nemotron-3.5-asr-streaming-0.6b.q8_0.gguf
"""

from __future__ import annotations

import argparse
import base64
import binascii
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional


MODEL_ID = "nvidia/nemotron-3.5-asr-streaming-0.6b"
DEFAULT_MAX_AUDIO_BYTES = 100_000_000
DEFAULT_MODEL_PATH = (
    Path.home()
    / ".OminiX"
    / "models"
    / "nemotron-3.5-asr-streaming-0.6b"
    / "nemotron-3.5-asr-streaming-0.6b.q8_0.gguf"
)
DEFAULT_NEMO_SPEECH_BIN = Path.home() / ".local" / "bin" / "nemo-speech"


class RequestValidationError(ValueError):
    """Raised for malformed OMiniX requests."""


class BackendError(RuntimeError):
    """Raised when NeMo-Speech.cpp cannot serve a transcription."""


@dataclass(frozen=True)
class TranscriptionRequest:
    audio_bytes: bytes
    language: str
    verbose: bool


def normalize_language(language: str | None) -> str:
    """Map common OMiniX/OpenAI labels to Nemotron 3.5 locale prompts."""
    if language is None:
        return "zh-CN"
    value = language.strip()
    normalized = value.lower().replace("_", "-")
    aliases = {
        "zh": "zh-CN",
        "zh-cn": "zh-CN",
        "zh-hans": "zh-CN",
        "chinese": "zh-CN",
        "mandarin": "zh-CN",
        "en": "en-US",
        "en-us": "en-US",
        "english": "en-US",
        "auto": "auto",
    }
    return aliases.get(normalized, value)


def parse_transcription_request(raw_body: bytes, *, max_audio_bytes: int) -> TranscriptionRequest:
    """Parse the JSON/base64 request used by OMiniX and Octos."""
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
        metadata, separator, encoded_file = encoded_file.partition(",")
        if not separator or ";base64" not in metadata.lower():
            raise RequestValidationError("data URL audio must contain a base64 payload")

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
        language=normalize_language(language),
        verbose=response_format == "verbose_json",
    )


def build_multipart_request(
    *,
    audio_bytes: bytes,
    language: str,
    response_format: str,
) -> tuple[str, bytes]:
    """Encode an OpenAI-compatible multipart transcription request."""
    boundary = f"----octos-nemotron-{uuid.uuid4().hex}"
    boundary_bytes = boundary.encode("ascii")
    parts: list[bytes] = []

    def add_field(name: str, value: str) -> None:
        parts.extend(
            [
                b"--" + boundary_bytes,
                f'Content-Disposition: form-data; name="{name}"'.encode("ascii"),
                b"",
                value.encode("utf-8"),
            ]
        )

    parts.extend(
        [
            b"--" + boundary_bytes,
            b'Content-Disposition: form-data; name="file"; filename="audio.wav"',
            b"Content-Type: audio/wav",
            b"",
            audio_bytes,
        ]
    )
    add_field("language", language)
    add_field("response_format", response_format)
    parts.extend([b"--" + boundary_bytes + b"--", b""])
    return f"multipart/form-data; boundary={boundary}", b"\r\n".join(parts)


def format_transcription_response(
    *,
    backend_response: dict[str, Any],
    language: str,
    processing_time: float,
    verbose: bool,
    model_info: dict[str, Any],
) -> dict[str, Any]:
    """Return OMiniX-compatible text while making no-speech explicit."""
    text = str(backend_response.get("text", "")).strip()
    response: dict[str, Any] = {"text": text, "rejected": not text}
    if not text:
        response["reject_reason"] = "no_speech"

    if verbose:
        for key in ("duration", "segments", "words"):
            if key in backend_response:
                response[key] = backend_response[key]
        response.update(
            {
                "language": backend_response.get("language", language),
                "processing_time": processing_time,
                "model": model_info,
            }
        )
    return response


def wav_duration(audio_bytes: bytes) -> float | None:
    """Read duration from the normalized in-memory WAV when possible."""
    import io

    try:
        with wave.open(io.BytesIO(audio_bytes), "rb") as wav_file:
            rate = wav_file.getframerate()
            return wav_file.getnframes() / rate if rate else None
    except (EOFError, wave.Error):
        return None


class BackendManager:
    """Own an optional NeMo-Speech.cpp child and its HTTP connection."""

    def __init__(
        self,
        *,
        backend_url: str,
        binary_path: Path | None,
        model_path: Path | None,
        backend_host: str,
        backend_port: int,
        startup_timeout: float,
    ) -> None:
        self.backend_url = backend_url.rstrip("/")
        self.binary_path = binary_path
        self.model_path = model_path
        self.backend_host = backend_host
        self.backend_port = backend_port
        self.startup_timeout = startup_timeout
        self.process: subprocess.Popen[bytes] | None = None

    def start(self) -> None:
        if self.binary_path is not None:
            if not self.binary_path.is_file():
                raise RuntimeError(f"nemo-speech binary not found: {self.binary_path}")
            if self.model_path is None or not self.model_path.is_file():
                raise RuntimeError(f"Nemotron GGUF model not found: {self.model_path}")
            command = [
                str(self.binary_path),
                "serve",
                "--asr-model",
                str(self.model_path),
                "--host",
                self.backend_host,
                "--port",
                str(self.backend_port),
                "--threads",
                "2",
                "--max-upload-mb",
                "128",
                "--asr.streaming.rnnt_right_context",
                "3",
            ]
            self.process = subprocess.Popen(command)

        deadline = time.monotonic() + self.startup_timeout
        last_error = "backend did not answer"
        while time.monotonic() < deadline:
            if self.process is not None and self.process.poll() is not None:
                raise RuntimeError(
                    f"nemo-speech exited during startup with code {self.process.returncode}"
                )
            try:
                with urllib.request.urlopen(f"{self.backend_url}/ready", timeout=2) as response:
                    if response.status == 200:
                        return
                    last_error = f"backend readiness returned HTTP {response.status}"
            except (OSError, urllib.error.URLError) as error:
                last_error = str(error)
            time.sleep(0.25)
        self.stop()
        raise RuntimeError(f"Nemotron backend was not ready: {last_error}")

    def stop(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)

    def ready(self) -> tuple[bool, dict[str, Any] | None]:
        try:
            with urllib.request.urlopen(f"{self.backend_url}/ready", timeout=2) as response:
                body = json.loads(response.read())
                return response.status == 200, body if isinstance(body, dict) else None
        except (OSError, ValueError, urllib.error.URLError):
            return False, None

    def transcribe(self, audio_bytes: bytes, language: str, *, verbose: bool) -> dict[str, Any]:
        response_format = "verbose_json" if verbose else "json"
        content_type, body = build_multipart_request(
            audio_bytes=audio_bytes,
            language=language,
            response_format=response_format,
        )
        request = urllib.request.Request(
            f"{self.backend_url}/v1/audio/transcriptions",
            data=body,
            headers={"Content-Type": content_type, "Accept": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                payload = json.loads(response.read())
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise BackendError(f"Nemotron backend returned HTTP {error.code}: {detail}") from error
        except (OSError, json.JSONDecodeError, urllib.error.URLError) as error:
            raise BackendError(f"Nemotron backend request failed: {error}") from error
        if not isinstance(payload, dict) or not isinstance(payload.get("text"), str):
            raise BackendError("Nemotron backend response is missing a text field")
        return payload


class NemotronTranscriber:
    """Normalize arbitrary OMiniX audio and call the resident model backend."""

    def __init__(self, backend: BackendManager, *, ffmpeg_bin: str) -> None:
        self.backend = backend
        self.ffmpeg_bin = ffmpeg_bin
        self.lock = threading.Lock()

    @property
    def model_info(self) -> dict[str, Any]:
        return {
            "id": MODEL_ID,
            "name": "nemotron-3.5-asr-streaming-0.6b",
            "object": "model",
            "role": "asr",
            "runtime": "NeMo-Speech.cpp",
            "quantization": "Q8_0",
            "device": "Metal",
        }

    def _normalize_audio(self, audio_bytes: bytes) -> bytes:
        with tempfile.TemporaryDirectory(prefix="octos-nemotron-") as temp_dir:
            source = Path(temp_dir) / "input.audio"
            target = Path(temp_dir) / "normalized.wav"
            source.write_bytes(audio_bytes)
            command = [
                self.ffmpeg_bin,
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                str(source),
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
                str(target),
            ]
            try:
                completed = subprocess.run(
                    command,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    timeout=120,
                    check=False,
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                raise RequestValidationError(f"audio conversion failed: {error}") from error
            if completed.returncode != 0 or not target.is_file():
                detail = completed.stderr.decode("utf-8", errors="replace").strip()
                raise RequestValidationError(f"unsupported or invalid audio: {detail}")
            return target.read_bytes()

    def transcribe(self, request: TranscriptionRequest) -> dict[str, Any]:
        normalized_audio = self._normalize_audio(request.audio_bytes)
        started = time.perf_counter()
        # The current Metal backend is most predictable with one in-flight
        # decoder. The outer HTTP server remains concurrent for health checks.
        with self.lock:
            backend_response = self.backend.transcribe(
                normalized_audio,
                request.language,
                verbose=request.verbose,
            )
        processing_time = time.perf_counter() - started
        if request.verbose and "duration" not in backend_response:
            duration = wav_duration(normalized_audio)
            if duration is not None:
                backend_response["duration"] = duration
        return format_transcription_response(
            backend_response=backend_response,
            language=request.language,
            processing_time=processing_time,
            verbose=request.verbose,
            model_info=self.model_info,
        )


class NemotronAsrHttpServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(
        self,
        server_address: tuple[str, int],
        handler_class: type[http.server.BaseHTTPRequestHandler],
        *,
        transcriber: NemotronTranscriber,
        max_audio_bytes: int,
        allow_origin: str,
    ) -> None:
        super().__init__(server_address, handler_class)
        self.transcriber = transcriber
        self.max_audio_bytes = max_audio_bytes
        self.allow_origin = allow_origin


class Handler(http.server.BaseHTTPRequestHandler):
    server: NemotronAsrHttpServer

    def do_OPTIONS(self) -> None:
        self.send_response(204)
        self._send_cors_headers()
        self.send_header("Access-Control-Allow-Methods", "POST, GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, Authorization")
        self.end_headers()

    def do_GET(self) -> None:
        if self.path in {"/health", "/ready"}:
            ready, backend_status = self.server.transcriber.backend.ready()
            status = 200 if ready else 503
            self._send_json(
                status,
                {
                    "status": "ok" if ready else "unavailable",
                    "model": self.server.transcriber.model_info,
                    "backend": backend_status,
                },
            )
            return
        if self.path == "/v1/models":
            self._send_json(
                200,
                {"object": "list", "data": [self.server.transcriber.model_info]},
            )
            return
        self._send_error(404, "not found", "invalid_request_error")

    def do_POST(self) -> None:
        if self.path not in {"/v1/audio/transcriptions", "/v1/audio/asr/qwen3"}:
            self._send_error(404, "not found", "invalid_request_error")
            return

        try:
            content_length = int(self.headers.get("Content-Length", ""))
        except ValueError:
            self._send_error(400, "missing or invalid Content-Length", "invalid_request_error")
            return

        max_request_bytes = (self.server.max_audio_bytes * 4 // 3) + 1_000_000
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
        except BackendError as error:
            self.log_error("transcription failed: %s", error)
            self._send_error(502, str(error), "backend_error")
            return
        except Exception as error:  # noqa: BLE001 - local service boundary.
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
        sys.stderr.write("[nemotron-asr] " + fmt % args + "\n")


def parse_args(argv: Optional[list[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8093)
    parser.add_argument("--backend-host", default="127.0.0.1")
    parser.add_argument("--backend-port", type=int, default=8092)
    parser.add_argument(
        "--backend-url",
        default=None,
        help="Use an already-running NeMo-Speech.cpp backend instead of starting one.",
    )
    parser.add_argument("--nemo-speech-bin", type=Path, default=DEFAULT_NEMO_SPEECH_BIN)
    parser.add_argument("--model", type=Path, default=DEFAULT_MODEL_PATH)
    parser.add_argument("--ffmpeg-bin", default="ffmpeg")
    parser.add_argument("--startup-timeout", type=float, default=180)
    parser.add_argument("--max-audio-bytes", type=int, default=DEFAULT_MAX_AUDIO_BYTES)
    parser.add_argument("--allow-origin", default="*")
    return parser.parse_args(argv)


def main() -> None:
    args = parse_args()
    if args.max_audio_bytes < 1:
        raise SystemExit("--max-audio-bytes must be positive")
    backend_url = args.backend_url or f"http://{args.backend_host}:{args.backend_port}"
    manager = BackendManager(
        backend_url=backend_url,
        binary_path=None if args.backend_url else args.nemo_speech_bin.expanduser(),
        model_path=None if args.backend_url else args.model.expanduser(),
        backend_host=args.backend_host,
        backend_port=args.backend_port,
        startup_timeout=args.startup_timeout,
    )
    manager.start()
    transcriber = NemotronTranscriber(manager, ffmpeg_bin=args.ffmpeg_bin)
    server = NemotronAsrHttpServer(
        (args.host, args.port),
        Handler,
        transcriber=transcriber,
        max_audio_bytes=args.max_audio_bytes,
        allow_origin=args.allow_origin,
    )
    print(
        f"Nemotron 3.5 ASR ready at http://{args.host}:{args.port} "
        f"(backend={backend_url}, model={args.model})",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nNemotron ASR stopping", flush=True)
    finally:
        server.server_close()
        manager.stop()


if __name__ == "__main__":
    main()
