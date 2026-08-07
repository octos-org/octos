#!/usr/bin/env python3
"""Contract tests for the local Nemotron/OminiX-compatible ASR service."""

from __future__ import annotations

import base64
import importlib.util
import json
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
TARGET = ROOT / "scripts" / "nemotron_asr_server.py"


def load_server_module():
    spec = importlib.util.spec_from_file_location("nemotron_asr_server", TARGET)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class NemotronAsrServerContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = load_server_module()

    def test_should_decode_ominix_json_request_when_payload_is_valid(self):
        audio = b"RIFFfake-wav"
        request = self.server.parse_transcription_request(
            json.dumps(
                {
                    "file": base64.b64encode(audio).decode("ascii"),
                    "language": "Chinese",
                    "response_format": "verbose_json",
                }
            ).encode("utf-8"),
            max_audio_bytes=1024,
        )

        self.assertEqual(request.audio_bytes, audio)
        self.assertEqual(request.language, "zh-CN")
        self.assertTrue(request.verbose)

    def test_should_reject_non_utf8_json_as_a_validation_error(self):
        with self.assertRaises(self.server.RequestValidationError):
            self.server.parse_transcription_request(b"\xff", max_audio_bytes=1024)

    def test_should_accept_data_url_audio_and_default_to_mandarin(self):
        audio = b"RIFFdata-url"
        encoded = base64.b64encode(audio).decode("ascii")
        request = self.server.parse_transcription_request(
            json.dumps({"file": f"data:audio/wav;base64,{encoded}"}).encode("utf-8"),
            max_audio_bytes=1024,
        )

        self.assertEqual(request.audio_bytes, audio)
        self.assertEqual(request.language, "zh-CN")
        self.assertFalse(request.verbose)

    def test_should_map_supported_ominix_language_labels_to_nemotron_locales(self):
        self.assertEqual(self.server.normalize_language("Chinese"), "zh-CN")
        self.assertEqual(self.server.normalize_language("zh"), "zh-CN")
        self.assertEqual(self.server.normalize_language("Mandarin"), "zh-CN")
        self.assertEqual(self.server.normalize_language("English"), "en-US")
        self.assertEqual(self.server.normalize_language("auto"), "auto")

    def test_should_default_to_ports_that_do_not_replace_existing_whisper_service(self):
        args = self.server.parse_args([])

        self.assertEqual(args.port, 8093)
        self.assertEqual(args.backend_port, 8092)

    def test_should_build_openai_multipart_request_for_nemo_speech_backend(self):
        content_type, body = self.server.build_multipart_request(
            audio_bytes=b"RIFFmultipart",
            language="zh-CN",
            response_format="verbose_json",
        )

        self.assertTrue(content_type.startswith("multipart/form-data; boundary="))
        self.assertIn(b'name="file"; filename="audio.wav"', body)
        self.assertIn(b'name="language"', body)
        self.assertIn(b"zh-CN", body)
        self.assertIn(b'name="response_format"', body)
        self.assertIn(b"verbose_json", body)
        self.assertIn(b"RIFFmultipart", body)

    def test_should_preserve_empty_transcript_as_successful_no_speech_rejection(self):
        response = self.server.format_transcription_response(
            backend_response={"text": ""},
            language="zh-CN",
            processing_time=0.25,
            verbose=True,
            model_info={"id": "nvidia/nemotron-3.5-asr-streaming-0.6b"},
        )

        self.assertEqual(response["text"], "")
        self.assertTrue(response["rejected"])
        self.assertEqual(response["reject_reason"], "no_speech")
        self.assertEqual(response["language"], "zh-CN")

    def test_should_keep_backend_transcript_and_metadata(self):
        response = self.server.format_transcription_response(
            backend_response={"text": "你好。", "duration": 1.5, "segments": []},
            language="zh-CN",
            processing_time=0.25,
            verbose=True,
            model_info={"id": "nvidia/nemotron-3.5-asr-streaming-0.6b"},
        )

        self.assertEqual(response["text"], "你好。")
        self.assertFalse(response["rejected"])
        self.assertEqual(response["duration"], 1.5)
        self.assertEqual(response["segments"], [])


if __name__ == "__main__":
    unittest.main()
