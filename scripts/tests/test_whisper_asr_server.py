#!/usr/bin/env python3
"""Focused contract tests for the local Whisper ASR lab service."""

from __future__ import annotations

import base64
import importlib.util
import json
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
TARGET = ROOT / "scripts" / "whisper_asr_server.py"


def load_server_module():
    spec = importlib.util.spec_from_file_location("whisper_asr_server", TARGET)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class WhisperAsrServerContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = load_server_module()

    def test_should_decode_base64_request_when_payload_is_valid(self):
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
        self.assertEqual(request.language, "Chinese")
        self.assertTrue(request.verbose)

    def test_should_reject_non_utf8_json_as_a_validation_error(self):
        with self.assertRaises(self.server.RequestValidationError):
            self.server.parse_transcription_request(b"\xff", max_audio_bytes=1024)

    def test_should_mark_empty_text_as_rejected_when_formatting_response(self):
        response = self.server.format_transcription_response(
            text="",
            language="Chinese",
            processing_time=0.42,
            audio_duration=2.0,
            segments=[],
            rejection_signal={"no_speech_prob": 0.94, "avg_logprob": -0.57},
            model_info={"name": "small", "device": "cpu"},
            verbose=True,
        )

        self.assertTrue(response["rejected"])
        self.assertEqual(response["reject_reason"], "no_speech")
        self.assertEqual(response["text"], "")
        self.assertEqual(response["rejection_signal"]["no_speech_prob"], 0.94)

    def test_should_not_mark_nonempty_text_as_rejected_when_formatting_response(self):
        response = self.server.format_transcription_response(
            text="你好",
            language="Chinese",
            processing_time=0.42,
            audio_duration=2.0,
            segments=[],
            rejection_signal=None,
            model_info={"name": "small", "device": "cpu"},
            verbose=False,
        )

        self.assertFalse(response["rejected"])
        self.assertNotIn("rejection_signal", response)
        self.assertEqual(response, {"text": "你好", "rejected": False})


if __name__ == "__main__":
    unittest.main()
