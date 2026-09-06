#!/usr/bin/env python3
"""Offline C + generated Python ABI smoke, using only a localhost fixture.

Run after building both libraries and regenerating Python bindings:
  python3 crates/octos-uniffi/tests/incomplete_bindings.py --library-dir target/debug
No real credentials, environment mutation, external provider, or workspace tools.
"""
import argparse
import ctypes
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


PARTIAL = "MODEL_PAYLOAD 中文\n  \0" + "".join(
    f"数据 {index:04}: {index * 7919:08x};\n" for index in range(100)
) + "END"
USAGE = {"prompt_tokens": 17, "completion_tokens": 8,
         "prompt_tokens_details": {"cached_tokens": 7},
         "completion_tokens_details": {"reasoning_tokens": 6}}
EXPECTED = {"input": 10, "output": 8, "reasoning": 6, "cache_read": 7, "cache_write": 0}


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass  # Never log authorization headers or request bodies.

    def do_POST(self):
        assert self.headers.get("Authorization") == "Bearer fixture-fake-only"
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert self.path.endswith("/chat/completions")
        content = request["messages"][-1]["content"]
        complete = "fixture-success" in str(content)
        text = "GENUINE_FINAL" if complete else PARTIAL
        reason = "stop" if complete else "length"
        usage = dict(USAGE)
        if request.get("stream"):
            events = [
                {"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": usage},
            ]
            body = ("".join("data: " + json.dumps(event) + "\n\n" for event in events)
                    + "data: [DONE]\n\n").encode()
            content_type = "text/event-stream"
        else:
            body = json.dumps({"id": "fixture", "object": "chat.completion",
                               "choices": [{"index": 0, "message": {"role": "assistant", "content": text},
                                            "finish_reason": reason}], "usage": usage}).encode()
            content_type = "application/json"
        self.server.requests += 1
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def library_name(name):
    if sys.platform == "darwin":
        return f"lib{name}.dylib"
    if sys.platform.startswith("win"):
        return f"{name}.dll"
    return f"lib{name}.so"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library-dir", type=Path, required=True)
    args = parser.parse_args()
    library_dir = args.library_dir.resolve()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    server.requests = 0
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        with tempfile.TemporaryDirectory(prefix="octos-ffi-incomplete-smoke-") as cwd:
            config = {"provider": "custom", "model": "ffi-fixture", "api_key": "fixture-fake-only",
                      "base_url": f"http://127.0.0.1:{server.server_port}/v1", "cwd": cwd,
                      "max_iterations": 3}
            lib = ctypes.CDLL(str(library_dir / library_name("octos_ffi")))
            lib.octos_runtime_new.argtypes = [ctypes.c_char_p]
            lib.octos_runtime_new.restype = ctypes.c_void_p
            lib.octos_runtime_free.argtypes = [ctypes.c_void_p]
            lib.octos_run_task.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
            lib.octos_run_task.restype = ctypes.c_void_p
            lib.octos_take_last_partial_result.argtypes = []
            lib.octos_take_last_partial_result.restype = ctypes.c_void_p
            lib.octos_last_error.argtypes = []
            lib.octos_last_error.restype = ctypes.c_char_p
            lib.octos_string_free.argtypes = [ctypes.c_void_p]

            def read_owned(pointer):
                assert pointer
                try:
                    return json.loads(ctypes.string_at(pointer))
                finally:
                    lib.octos_string_free(pointer)

            runtime = lib.octos_runtime_new(json.dumps(config).encode())
            assert runtime, "fixture runtime must build"
            try:
                assert not lib.octos_run_task(runtime, b'{"prompt":"fixture-incomplete"}')
                diagnostic = lib.octos_last_error()
                assert b"incomplete" in diagnostic and len(diagnostic) < 650
                assert b"MODEL_PAYLOAD" not in diagnostic
                partial = read_owned(lib.octos_take_last_partial_result())
                assert partial == {"output": PARTIAL, "iterations": 1, "tokens": EXPECTED}, {
                    "output_equal": partial.get("output") == PARTIAL,
                    "iterations": partial.get("iterations"), "tokens": partial.get("tokens"),
                }
                assert not lib.octos_take_last_partial_result()
                assert lib.octos_last_error() == diagnostic
                final = read_owned(lib.octos_run_task(runtime, b'{"prompt":"fixture-success"}'))
                # Successful iteration-count semantics predate this fix and
                # count assistant history rows (the final may be separate).
                # The new partial contract above uses producer final_iteration.
                assert final["output"] == "GENUINE_FINAL" and final["tokens"] == EXPECTED, final
                assert not lib.octos_take_last_partial_result() and not lib.octos_last_error()
            finally:
                lib.octos_runtime_free(runtime)

            binding = Path(__file__).resolve().parents[1] / "bindings/python/octos.py"
            spec = importlib.util.spec_from_file_location("octos_fixture_binding", binding)
            module = importlib.util.module_from_spec(spec)
            # Generated loader locates its binary next to __file__. Keep the
            # generated source unmodified; direct only its library lookup.
            module.__file__ = str(library_dir / "octos.py")
            spec.loader.exec_module(module)
            runtime = module.Runtime(module.Config(**config))
            try:
                runtime.run_task(module.Brief(prompt="fixture-incomplete"))
            except module.OctosError.Incomplete as error:
                assert error.partial.output == PARTIAL
                assert error.partial.iterations == 1
                assert {key: getattr(error.partial.tokens, key) for key in EXPECTED} == EXPECTED
                assert "MODEL_PAYLOAD" not in str(error)
            else:
                raise AssertionError("generated binding turned truncation into success")
            final = runtime.run_task(module.Brief(prompt="fixture-success"))
            assert final.output == "GENUINE_FINAL"
            assert {key: getattr(final.tokens, key) for key in EXPECTED} == EXPECTED
            del runtime
            assert server.requests == 4, "no retries/extra fixture calls expected"
            print("PASS: C NULL + consume-once partial; generated Python structured failure; exact usage; success controls (4 localhost requests)")
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)


if __name__ == "__main__":
    main()
