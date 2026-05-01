#!/usr/bin/env python3
"""Small DeepSeek chat compatibility proxy for Codex comparison tests.

The proxy repairs message arrays before forwarding to DeepSeek's
`/v1/chat/completions` endpoint. It is intentionally scoped to the e2e
comparison harness and should not be used as Octos production protocol code.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import sys
import threading
import urllib.error
import urllib.request

RECENT_REASONING: dict[str, str] = {}
RECENT_REASONING_BY_POSITION: list[str] = []
RECENT_REASONING_LOCK = threading.Lock()


def assistant_message_key(message: dict) -> str | None:
    tool_calls = message.get("tool_calls") or []
    tool_ids = [
        call.get("id")
        for call in tool_calls
        if isinstance(call, dict) and call.get("id")
    ]
    if tool_ids:
        return "tools:" + ",".join(tool_ids)

    content = message.get("content")
    if isinstance(content, str) and content:
        digest = hashlib.sha256(content.encode("utf-8")).hexdigest()
        return "content:" + digest

    return None


def restore_reasoning_content(messages: list[dict]) -> int:
    restored = 0
    assistant_index = 0
    with RECENT_REASONING_LOCK:
        for message in messages:
            if message.get("role") != "assistant":
                continue
            if message.get("reasoning_content"):
                assistant_index += 1
                continue
            key = assistant_message_key(message)
            if key and key in RECENT_REASONING:
                message["reasoning_content"] = RECENT_REASONING[key]
                restored += 1
            elif assistant_index < len(RECENT_REASONING_BY_POSITION):
                message["reasoning_content"] = RECENT_REASONING_BY_POSITION[assistant_index]
                restored += 1
            elif RECENT_REASONING_BY_POSITION:
                message["reasoning_content"] = RECENT_REASONING_BY_POSITION[-1]
                restored += 1
            assistant_index += 1
    return restored


def remember_reasoning_content(payload: bytes) -> int:
    try:
        body = json.loads(payload)
    except json.JSONDecodeError:
        return remember_streaming_reasoning_content(payload)

    remembered = 0
    with RECENT_REASONING_LOCK:
        for choice in body.get("choices") or []:
            if not isinstance(choice, dict):
                continue
            message = choice.get("message")
            if not isinstance(message, dict):
                continue
            reasoning_content = message.get("reasoning_content")
            if not isinstance(reasoning_content, str) or not reasoning_content:
                continue
            key = assistant_message_key(message)
            if not key:
                key = f"position:{len(RECENT_REASONING_BY_POSITION)}"
            RECENT_REASONING[key] = reasoning_content
            RECENT_REASONING_BY_POSITION.append(reasoning_content)
            remembered += 1

        while len(RECENT_REASONING) > 256:
            RECENT_REASONING.pop(next(iter(RECENT_REASONING)))
        del RECENT_REASONING_BY_POSITION[: max(0, len(RECENT_REASONING_BY_POSITION) - 256)]

    return remembered


def remember_streaming_reasoning_content(payload: bytes) -> int:
    reasoning_chunks: list[str] = []
    content_chunks: list[str] = []
    tool_call_ids: list[str] = []

    for raw_line in payload.decode("utf-8", errors="replace").splitlines():
        line = raw_line.strip()
        if not line.startswith("data:"):
            continue
        data = line.removeprefix("data:").strip()
        if not data or data == "[DONE]":
            continue
        try:
            chunk = json.loads(data)
        except json.JSONDecodeError:
            continue
        for choice in chunk.get("choices") or []:
            if not isinstance(choice, dict):
                continue
            delta = choice.get("delta") or {}
            if not isinstance(delta, dict):
                continue
            reasoning_content = delta.get("reasoning_content")
            if isinstance(reasoning_content, str) and reasoning_content:
                reasoning_chunks.append(reasoning_content)
            content = delta.get("content")
            if isinstance(content, str) and content:
                content_chunks.append(content)
            for tool_call in delta.get("tool_calls") or []:
                if not isinstance(tool_call, dict):
                    continue
                tool_call_id = tool_call.get("id")
                if isinstance(tool_call_id, str) and tool_call_id:
                    tool_call_ids.append(tool_call_id)

    reasoning_content = "".join(reasoning_chunks)
    if not reasoning_content:
        return 0

    message: dict = {
        "role": "assistant",
        "content": "".join(content_chunks),
    }
    if tool_call_ids:
        message["tool_calls"] = [{"id": tool_call_id} for tool_call_id in tool_call_ids]
    key = assistant_message_key(message) or "position:0"

    with RECENT_REASONING_LOCK:
        RECENT_REASONING[key] = reasoning_content
        RECENT_REASONING_BY_POSITION.append(reasoning_content)
        del RECENT_REASONING_BY_POSITION[: max(0, len(RECENT_REASONING_BY_POSITION) - 256)]

    return 1


def repair_tool_sequences(messages: list[dict]) -> tuple[list[dict], int, int]:
    tool_results: dict[str, dict] = {}
    for message in messages:
        if message.get("role") == "tool":
            tool_call_id = message.get("tool_call_id")
            if tool_call_id and tool_call_id not in tool_results:
                tool_results[tool_call_id] = message

    repaired: list[dict] = []
    inserted = 0
    dropped = 0
    for message in messages:
        if message.get("role") == "tool":
            dropped += 1
            continue

        repaired.append(message)
        tool_calls = message.get("tool_calls") or []
        if message.get("role") == "assistant" and tool_calls:
            expected = [
                call.get("id")
                for call in tool_calls
                if isinstance(call, dict) and call.get("id")
            ]
            for tool_call_id in expected:
                if tool_call_id in tool_results:
                    repaired.append(tool_results[tool_call_id])
                else:
                    repaired.append(
                        {
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": "[compat proxy inserted missing tool result]",
                        }
                    )
                    inserted += 1

    return repaired, inserted, dropped


class Handler(http.server.BaseHTTPRequestHandler):
    upstream = "https://api.deepseek.com"

    def do_POST(self) -> None:
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return

        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            self.send_error(400, "invalid json")
            return

        if isinstance(body.get("messages"), list):
            restored = restore_reasoning_content(body["messages"])
            body["messages"], inserted, dropped = repair_tool_sequences(body["messages"])
        else:
            restored = 0
            inserted = 0
            dropped = 0

        encoded = json.dumps(body).encode("utf-8")
        headers = {
            "Authorization": self.headers.get("Authorization", ""),
            "Content-Type": "application/json",
            "Accept": self.headers.get("Accept", "*/*"),
            "Accept-Encoding": "identity",
        }
        request = urllib.request.Request(
            f"{self.upstream}{self.path}",
            data=encoded,
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                payload = response.read()
                remembered = remember_reasoning_content(payload)
                self.send_response(response.status)
                self.send_header("Content-Type", response.headers.get("Content-Type", "application/json"))
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("X-Compat-Inserted-Tool-Results", str(inserted))
                self.send_header("X-Compat-Dropped-Orphan-Tools", str(dropped))
                self.send_header("X-Compat-Restored-Reasoning", str(restored))
                self.send_header("X-Compat-Remembered-Reasoning", str(remembered))
                self.end_headers()
                self.wfile.write(payload)
                self.log_message(
                    "forwarded inserted=%s dropped=%s restored_reasoning=%s remembered_reasoning=%s",
                    inserted,
                    dropped,
                    restored,
                    remembered,
                )
        except urllib.error.HTTPError as error:
            payload = error.read()
            self.send_response(error.code)
            self.send_header("Content-Type", error.headers.get("Content-Type", "text/plain"))
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("X-Compat-Inserted-Tool-Results", str(inserted))
            self.send_header("X-Compat-Dropped-Orphan-Tools", str(dropped))
            self.send_header("X-Compat-Restored-Reasoning", str(restored))
            self.send_header("X-Compat-Remembered-Reasoning", "0")
            self.end_headers()
            self.wfile.write(payload)
            self.log_message(
                "upstream_error=%s inserted=%s dropped=%s restored_reasoning=%s body=%s",
                error.code,
                inserted,
                dropped,
                restored,
                payload.decode("utf-8", errors="replace")[:500],
            )

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write("[deepseek-compat] " + fmt % args + "\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=18080)
    args = parser.parse_args()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"listening http://127.0.0.1:{args.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
