#!/usr/bin/env python3
"""A scripted stand-in for an OpenAI-compatible chat endpoint.

Used to test the Octos GitHub Action end to end without a model key. It
answers `POST .../chat/completions` from a script: step N is the reply to
the request that already holds N assistant messages, so a retry gets the
same reply. Every request body is appended to the log as one JSON line.

    mock_llm.py --port-file PORT --log LOG --script '[
      {"tool": "shell", "args": {"command": "echo hi > note.txt"}},
      {"text": "Wrote note.txt."}
    ]'

Standard library only.
"""

from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def step_for(script, body):
    turns = sum(1 for message in body.get("messages") or [] if message.get("role") == "assistant")
    return script[turns] if turns < len(script) else {"text": "Done."}


def message_for(step, turn):
    if "tool" in step:
        call = {
            "index": 0,
            "id": "call_%d" % turn,
            "type": "function",
            "function": {"name": step["tool"], "arguments": json.dumps(step.get("args") or {})},
        }
        return {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
    return {"role": "assistant", "content": step.get("text") or ""}, "stop"


def make_handler(script, log_path, model):
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args):  # keep the job log quiet
            pass

        def _json(self, status, payload):
            raw = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def do_GET(self):
            if self.path.rstrip("/").endswith("/models"):
                self._json(200, {"object": "list", "data": [{"id": model, "object": "model"}]})
            else:
                self._json(404, {"error": {"message": "not found"}})

        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length)
            if not self.path.rstrip("/").endswith("/chat/completions"):
                self._json(404, {"error": {"message": "not found"}})
                return
            try:
                body = json.loads(raw.decode("utf-8"))
            except ValueError:
                self._json(400, {"error": {"message": "bad json"}})
                return
            with lock, open(log_path, "a", encoding="utf-8") as handle:
                handle.write(json.dumps({"path": self.path, "request": body}) + "\n")
            turn = sum(1 for m in body.get("messages") or [] if m.get("role") == "assistant")
            message, finish = message_for(step_for(script, body), turn)
            usage = {"prompt_tokens": len(raw) // 4, "completion_tokens": 12, "total_tokens": len(raw) // 4 + 12}
            base = {"id": "mock-%d" % turn, "created": 0, "model": body.get("model") or model}
            if not body.get("stream"):
                choice = {"index": 0, "message": message, "finish_reason": finish}
                self._json(200, dict(base, object="chat.completion", choices=[choice], usage=usage))
                return
            chunks = [
                dict(base, object="chat.completion.chunk", choices=[{"index": 0, "delta": message, "finish_reason": None}]),
                dict(base, object="chat.completion.chunk", choices=[{"index": 0, "delta": {}, "finish_reason": finish}]),
                dict(base, object="chat.completion.chunk", choices=[], usage=usage),
            ]
            payload = "".join("data: %s\n\n" % json.dumps(chunk) for chunk in chunks) + "data: [DONE]\n\n"
            raw_out = payload.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(raw_out)))
            self.end_headers()
            self.wfile.write(raw_out)

    return Handler


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--port", type=int, default=0, help="0 picks a free port")
    parser.add_argument("--port-file", help="the chosen port is written here once the server listens")
    parser.add_argument("--log", required=True, help="request bodies, one JSON object per line")
    parser.add_argument("--script", required=True, help="JSON list of {tool, args} or {text} steps")
    parser.add_argument("--model", default="mock-model")
    args = parser.parse_args(argv)
    script = json.loads(args.script)
    open(args.log, "a", encoding="utf-8").close()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(script, args.log, args.model))
    if args.port_file:
        with open(args.port_file, "w", encoding="utf-8") as handle:
            handle.write(str(server.server_address[1]))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
