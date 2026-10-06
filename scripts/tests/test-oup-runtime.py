#!/usr/bin/env python3
"""Exercise real chat, ACP and OUP processes against an offline HTTP provider.

Build octos-cli with its default features, then run:
  OCTOS_BIN=target/debug/octos python3 scripts/tests/test-oup-runtime.py
Evidence is retained under target/oup-functional (override OUP_TEST_OUTPUT_DIR).
Only fixture workspaces, profiles and localhost HTTP are used.
"""

import contextlib
import datetime
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get("OCTOS_BIN", ROOT / "target/debug/octos")).resolve()
OUTPUT = Path(os.environ.get(
    "OUP_TEST_OUTPUT_DIR",
    ROOT / "target/oup-functional" / datetime.datetime.now(
        datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ"),
)).resolve()
PARTIAL = 'ACTUAL_PARTIAL 中文\n"quoted" final fragment'
FINAL = "ACTUAL_COMPLETE_FINAL"
PREAMBLE = "PRE_TOOL_COMMENTARY"
FILE_CONTENT = "FIXTURE_FILE_EVIDENCE_82a7"
USAGE = {"prompt_tokens": 17, "completion_tokens": 8,
         "prompt_tokens_details": {"cached_tokens": 7},
         "completion_tokens_details": {"reasoning_tokens": 6}}
# A fully built workspace supplies debug skill binaries which serve verifies
# during startup and lazy profile bootstrap. Hashing that bundle can exceed a
# normal RPC deadline; keep the turn/ordinary RPC deadline independent.
BOOTSTRAP_TIMEOUT = 120


def reply(text, finish="stop", tool=False):
    message = {"role": "assistant", "content": text}
    if tool:
        message["tool_calls"] = [{"id": "fixture-read", "type": "function",
                                  "function": {"name": "read_file", "arguments":
                                               json.dumps({"path": "evidence.txt"})}}]
    return {"message": message, "finish_reason": finish}


class FixtureProvider:
    def __init__(self, replies):
        self.replies = queue.Queue()
        for value in replies:
            self.replies.put(value)
        self.requests = []
        self.errors = []
        self.received = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    if self.path != "/v1/chat/completions":
                        raise ValueError(f"unexpected provider path: {self.path}")
                    request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    owner.requests.append(request)
                    response = owner.replies.get_nowait()
                    owner.received.set()
                    release = response.pop("_wait_for", None)
                    if release is not None and not release.wait(timeout=20):
                        raise TimeoutError("test did not release its blocked provider response")
                    choice = {"index": 0, **response}
                    if request.get("stream"):
                        delta = dict(response["message"])
                        for i, call in enumerate(delta.get("tool_calls", [])):
                            call["index"] = i
                        events = [
                            {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                            {"choices": [{"index": 0, "delta": {},
                                          "finish_reason": response["finish_reason"]}], "usage": USAGE},
                        ]
                        body = ("".join("data: " + json.dumps(event) + "\n\n" for event in events)
                                + "data: [DONE]\n\n").encode()
                        content_type = "text/event-stream"
                    else:
                        body = json.dumps({"id": "fixture", "object": "chat.completion",
                                           "choices": [choice], "usage": USAGE}).encode()
                        content_type = "application/json"
                    self.send_response(200)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # A cancelled runtime request may close the HTTP connection.
                except Exception as error:
                    owner.errors.append(repr(error))
                    self.send_error(400, "fixture rejected an unexpected request")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/v1"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


class RpcProcess:
    def __init__(self, argv, env, directory, label):
        self.frames = []
        self.inbox = queue.Queue()
        self.pending = []
        self.errors = []
        self.stderr = (directory / f"{label}-stderr.log").open("w")
        self.trace = (directory / f"{label}-wire.jsonl").open("w")
        self.timings = (directory / f"{label}-rpc-timings.jsonl").open("w")
        self.process = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr,
                                        text=True, encoding="utf-8", bufsize=1)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        try:
            for line in self.process.stdout:
                self.trace.write(line)
                self.trace.flush()
                frame = json.loads(line)  # Any non-JSON stdout is a protocol failure.
                self.frames.append(frame)
                self.inbox.put(frame)
        except Exception as error:
            self.errors.append(repr(error))
        finally:
            self.inbox.put(None)

    def send(self, method, params, request_id=None):
        frame = {"jsonrpc": "2.0", "method": method, "params": params}
        if request_id is not None:
            frame["id"] = request_id
        self.process.stdin.write(json.dumps(frame) + "\n")
        self.process.stdin.flush()

    def wait(self, predicate, timeout=45):
        for index, frame in enumerate(self.pending):
            if predicate(frame):
                return self.pending.pop(index)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                frame = self.inbox.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty:
                break
            if frame is None:
                raise AssertionError(f"RPC process exited before response: {self.errors}")
            if predicate(frame):
                return frame
            self.pending.append(frame)
        raise AssertionError("timed out waiting for RPC response; inspect retained wire/stderr")

    def rpc(self, method, params=None, timeout=45):
        request_id = str(uuid.uuid4())
        started = time.monotonic()
        received_response = False
        self.send(method, params or {}, request_id)
        try:
            response = self.wait(lambda frame: frame.get("id") == request_id, timeout=timeout)
            received_response = True
            return response
        except AssertionError as error:
            raise AssertionError(f"{method} (deadline {timeout}s): {error}") from error
        finally:
            self.timings.write(json.dumps({"id": request_id, "method": method,
                                           "seconds": round(time.monotonic() - started, 3),
                                           "timeout_seconds": timeout,
                                           "received_response": received_response}) + "\n")
            self.timings.flush()

    def close(self):
        self.process.stdin.close()
        try:
            code = self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            raise AssertionError("runtime did not shut down after stdin EOF")
        finally:
            self.reader.join(timeout=5)
            self.process.stdout.close()
            self.trace.close()
            self.timings.close()
            self.stderr.close()
        if code != 0 or self.errors:
            raise AssertionError(f"RPC process shutdown failed: exit={code}, errors={self.errors}")


class RuntimeContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError(f"Build octos-cli first; missing binary: {BINARY}")
        OUTPUT.mkdir(parents=True, exist_ok=True)
        (OUTPUT / "binary.json").write_text(json.dumps({
            "path": str(BINARY), "sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
        }, indent=2) + "\n")
        print(f"Runtime evidence: {OUTPUT}", flush=True)

    @contextlib.contextmanager
    def fixture(self, name, replies):
        directory = OUTPUT / name
        directory.mkdir()
        workspace = directory / "workspace"
        workspace.mkdir()
        (workspace / "evidence.txt").write_text(FILE_CONTENT)
        temporary = directory / "temporary"
        temporary.mkdir()
        # The goal-control Unix socket must fit sun_path even when the evidence
        # root is a deeply nested checkout. Keep its private runtime dir short.
        socket_dir = tempfile.TemporaryDirectory(
            prefix="oup-", dir="/tmp" if os.name == "posix" else None)
        provider = FixtureProvider(replies)
        config = directory / "config.json"
        config.write_text(json.dumps({
            "provider": "custom", "model": "oup-fixture", "api_type": "openai",
            "base_url": provider.url, "api_key_env": "OUP_FIXTURE_KEY",
            "env_vars": {"OUP_FIXTURE_KEY": "fake-local-fixture-only"},
            "gateway": {"max_iterations": 4},
        }))
        env = {key: os.environ[key] for key in
               ("PATH", "SystemRoot", "WINDIR", "LANG", "LC_ALL") if key in os.environ}
        env.update({"OCTOS_HOME": str(directory / "data"),
                    "XDG_CONFIG_HOME": str(directory / "config-home"),
                    "XDG_STATE_HOME": str(directory / "state-home"),
                    "XDG_RUNTIME_DIR": socket_dir.name,
                    "TMPDIR": str(temporary), "TMP": str(temporary), "TEMP": str(temporary),
                    "OUP_FIXTURE_KEY": "fake-local-fixture-only", "NO_COLOR": "1",
                    "NO_PROXY": "*", "RUST_LOG": "warn"})
        args = ["--data-dir", str(directory / "data"), "--cwd", str(workspace),
                "--config", str(config)]
        try:
            yield directory, provider, env, args
            self.assertEqual(provider.errors, [])
            self.assertTrue(provider.replies.empty(), "expected model calls were never made")
        finally:
            provider.close()
            socket_dir.cleanup()
            (directory / "provider-requests.json").write_text(
                json.dumps(provider.requests, ensure_ascii=False, indent=2) + "\n")
            (directory / "provider-errors.json").write_text(json.dumps(provider.errors) + "\n")

    def assert_usage(self, usage, calls):
        expected = {"input_tokens": 10 * calls, "output_tokens": 8 * calls,
                    "reasoning_tokens": 6 * calls, "cache_read_tokens": 7 * calls,
                    "cache_write_tokens": 0}
        self.assertEqual({key: usage.get(key, 0) for key in expected}, expected)

    def test_chat_partial_json_and_text(self):
        for ephemeral, json_output in [(False, True), (True, True), (True, False)]:
            with self.subTest(ephemeral=ephemeral, json=json_output):
                name = f"chat-partial-{ephemeral}-{json_output}"
                with self.fixture(name, [reply(PREAMBLE, "tool_calls", True),
                                         reply(PARTIAL, "length")]) as (directory, provider, env, args):
                    command = [str(BINARY), "chat", *args, "--no-retry", "--max-iterations", "4",
                               "--ask-for-approval", "never", "--message", "Read evidence.txt and answer."]
                    if ephemeral:
                        command.append("--no-session-persistence")
                    if json_output:
                        command.append("--json")
                    result = subprocess.run(command, env=env, capture_output=True, text=True,
                                            encoding="utf-8", timeout=60)
                    (directory / "stdout.txt").write_text(result.stdout)
                    (directory / "stderr.txt").write_text(result.stderr)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(len(provider.requests), 2, result.stderr)
                    tool_rows = [m for m in provider.requests[1]["messages"] if m["role"] == "tool"]
                    self.assertEqual(len(tool_rows), 1)
                    self.assertIn(FILE_CONTENT, tool_rows[0]["content"])
                    if json_output:
                        value = json.loads(result.stdout)  # Reject duplicate JSON objects/UI chatter.
                        self.assertEqual(value["code"], "output_truncated")
                        self.assertEqual(value["partial"]["text"], PARTIAL)
                        self.assertNotIn("text", value)
                        self.assert_usage(value["usage"], 2)
                    else:
                        self.assertEqual(result.stdout.count(PARTIAL), 1)
                    self.assertNotIn("Session Summary", result.stdout)
                    self.assertEqual(list((directory / "temporary").glob("octos-chat-oup-*")), [])
                    transcripts = list((directory / "data").rglob("sessions/*.jsonl"))
                    if ephemeral:
                        self.assertEqual(transcripts, [])
                    else:
                        self.assertTrue(transcripts, "persistent chat must retain its canonical transcript")

    def test_chat_success_json(self):
        with self.fixture("chat-success", [reply(FINAL)]) as (directory, provider, env, args):
            result = subprocess.run([str(BINARY), "chat", *args, "--no-retry", "--json",
                                     "--message", "Return the fixture answer."],
                                    env=env, capture_output=True, text=True, timeout=60)
            (directory / "stdout.txt").write_text(result.stdout)
            (directory / "stderr.txt").write_text(result.stderr)
            self.assertEqual(result.returncode, 0, result.stderr)
            value = json.loads(result.stdout)
            self.assertEqual(value["text"], FINAL)
            self.assertNotIn("error", value)
            self.assertEqual(len(provider.requests), 1)

    def test_chat_truncated_tool_has_no_fabricated_partial(self):
        with self.fixture("chat-no-final", [reply(PREAMBLE, "tool_calls", True),
                                            reply(None, "length", True)]) as (directory, provider, env, args):
            result = subprocess.run([str(BINARY), "chat", *args, "--no-retry", "--json",
                                     "--ask-for-approval", "never", "--message", "Read evidence.txt."],
                                    env=env, capture_output=True, text=True, timeout=60)
            (directory / "stdout.txt").write_text(result.stdout)
            (directory / "stderr.txt").write_text(result.stderr)
            self.assertNotEqual(result.returncode, 0)
            value = json.loads(result.stdout)
            self.assertEqual(value["code"], "output_truncated")
            self.assertNotIn("partial", value, "pre-tool commentary is not a final partial answer")
            self.assert_usage(value["usage"], 2)
            self.assertEqual(len(provider.requests), 2)

    def test_chat_bootstrap_failure_keeps_generic_json_contract(self):
        with self.fixture("chat-bootstrap-error", []) as (directory, provider, env, args):
            (directory / "config.json").write_text("{}")
            result = subprocess.run([str(BINARY), "chat", *args, "--json", "--message", "Hello"],
                                    env=env, capture_output=True, text=True, timeout=60)
            (directory / "stdout.txt").write_text(result.stdout)
            (directory / "stderr.txt").write_text(result.stderr)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(set(json.loads(result.stdout)), {"error"})
            self.assertEqual(provider.requests, [])

    def test_acp_multiturn_and_truncation(self):
        with self.fixture("acp", [reply(FINAL), reply(PARTIAL, "length")]) as (
                directory, provider, env, args):
            client = RpcProcess([str(BINARY), "acp", *args], env, directory, "acp")
            try:
                initialized = client.rpc("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
                self.assertIn("result", initialized)
                opened = client.rpc("session/new", {"cwd": str(directory / "workspace"), "mcpServers": []})
                session = opened["result"]["sessionId"]
                first = client.rpc("session/prompt", {"sessionId": session,
                                                     "prompt": [{"type": "text", "text": "First turn."}]})
                self.assertEqual(first["result"]["stopReason"], "end_turn")
                second = client.rpc("session/prompt", {"sessionId": session,
                                                      "prompt": [{"type": "text", "text": "Second turn."}]})
                self.assertIn("error", second, "truncation must not return successful ACP end_turn")
                self.assertIn(FINAL, json.dumps(provider.requests[1]["messages"]))
                chunks = [frame["params"]["update"].get("content", {}).get("text", "")
                          for frame in client.frames if frame.get("method") == "session/update"
                          and frame["params"]["update"].get("sessionUpdate") == "agent_message_chunk"]
                self.assertEqual("".join(chunks).count(FINAL), 1)
                self.assertEqual("".join(chunks).count(PARTIAL), 1)
            finally:
                client.close()

    def oup_connect(self, directory, env, args, label):
        client = RpcProcess([str(BINARY), "serve", "--stdio", "--solo", *args], env, directory, label)
        try:
            hello = client.rpc("client_hello", {"client": "oup-runtime-ci", "supported_features": [
                "projection.envelope.v2", "state.session_hydrate.v1", "session.workspace_cwd.v1",
                "context.lifecycle.v1", "context.semantic_cache.v1", "auxiliary.rest_to_ws.v1",
            ]}, timeout=BOOTSTRAP_TIMEOUT)
            self.assertIn("result", hello, hello)
            return client
        except Exception:
            client.close()
            raise

    def oup_profile(self, client, provider):
        capabilities = client.rpc("config/capabilities/list")["result"]["capabilities"]
        for method in ("session/open", "turn/start", "session/hydrate", "session/compact"):
            self.assertIn(method, capabilities["supported_methods"])
        created = client.rpc("profile/local/create", {
            "name": "OUP runtime fixture", "username": "oup-ci", "email": "oup-ci@example.test",
        })
        self.assertIn("result", created, created)
        selected = client.rpc("profile/llm/upsert", {
            "profile_id": "oup-ci", "set_primary": True,
            "selection": {"family_id": "openai", "model_id": "gpt-4o-mini",
                          "route": {"route_id": "fixture", "api_type": "openai",
                                    "base_url": provider.url, "api_key_env": "OUP_FIXTURE_KEY"}},
            "api_key": "fake-local-fixture-only",
        })
        self.assertIn("result", selected, selected)

    def oup_open(self, client, directory, session, after=None):
        params = {"session_id": session, "profile_id": "oup-ci", "cwd": str(directory / "workspace")}
        if after is not None:
            params["after"] = after
        opened = client.rpc("session/open", params, timeout=BOOTSTRAP_TIMEOUT)
        self.assertIn("result", opened, opened)
        self.assertEqual(opened["result"]["opened"]["session_id"], session)
        return opened["result"]

    def oup_turn(self, client, session, prompt):
        turn = str(uuid.uuid4())
        accepted = client.rpc("turn/start", {"session_id": session, "turn_id": turn,
                                            "input": [{"kind": "text", "text": prompt}]})
        self.assertEqual(accepted.get("result"), {"accepted": True}, accepted)
        terminal = client.wait(lambda frame: frame.get("method") == "projection/envelope"
                               and frame["params"].get("turn_id") == turn
                               and frame["params"]["payload"]["type"] == "turn_terminal")
        return terminal["params"]

    def test_oup_partial_identity_usage_and_cold_replay(self):
        with self.fixture("oup-replay", [reply(FINAL), reply(PREAMBLE, "tool_calls", True),
                                         reply(PARTIAL, "length")]) as (directory, provider, env, args):
            session = "oup-ci:local:runtime-replay"
            client = self.oup_connect(directory, env, args, "before")
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, session)
                first = self.oup_turn(client, session, "Return the first answer.")
                self.assertEqual(first["payload"]["data"]["outcome"], "completed")
                failed = self.oup_turn(client, session, "Read evidence.txt and answer the next turn.")
                data = failed["payload"]["data"]
                self.assertEqual(data["outcome"], "errored")
                self.assertEqual(data["error"]["code"], "output_truncated")
                self.assert_usage(data["token_usage"], 2)
                identity = data["error"]["data"]["partial_result"]["session_result"]
                hydrated = client.rpc("session/hydrate", {
                    "session_id": session, "include": ["messages", "turns", "context"],
                })["result"]
                matches = [row for row in hydrated["messages"] if row.get("message_id") == identity["message_id"]]
                self.assertEqual(len(matches), 1)
                self.assertEqual(matches[0]["content"], PARTIAL)
                self.assertEqual(matches[0]["seq"], identity["committed_seq"])
                self.assertNotEqual(identity["message_id"], next(
                    row["message_id"] for row in hydrated["messages"] if row["content"] == FINAL))
                context = hydrated["context_state"]
                self.assertTrue(context["cache_epoch_id"])
                tool_results = [row for row in provider.requests[2]["messages"] if row["role"] == "tool"]
                self.assertEqual(len(tool_results), 1)
                self.assertIn(FILE_CONTENT, tool_results[0]["content"])
            finally:
                client.close()
            restarted = self.oup_connect(directory, env, args, "after")
            try:
                self.oup_open(restarted, directory, session, first["cursor"])
                cold = restarted.rpc("session/hydrate", {
                    "session_id": session, "include": ["messages", "turns", "context"],
                })["result"]
                self.assertEqual(cold["messages"], hydrated["messages"])
                for key in ("transcript_hash", "cache_epoch_id", "last_compaction_id"):
                    self.assertEqual(cold["context_state"].get(key), context.get(key), key)
                terminals = [frame["params"] for frame in restarted.frames
                             if frame.get("method") == "projection/envelope"
                             and frame["params"].get("turn_id") == failed["turn_id"]
                             and frame["params"]["payload"]["type"] == "turn_terminal"]
                self.assertEqual(len(terminals), 1)
                self.assertEqual(terminals[0]["payload"], failed["payload"])
                self.assertEqual(len(provider.requests), 3, "reopening must not start a model turn")
            finally:
                restarted.close()

    def test_oup_compaction_epoch_and_restart(self):
        responses = [reply(f"FINAL_{index}: evidence checked, turn complete.") for index in range(5)]
        with self.fixture("oup-compaction", responses) as (directory, provider, env, args):
            env.update({"OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS": "1000000",
                        "OCTOS_CONTEXT_COMPACT_TARGET_TOKENS": "1000",
                        "OCTOS_CONTEXT_COMPACT_KEEP_ITEMS": "2",
                        "OCTOS_PROMPT_CACHE_MANIFEST_JSONL": str(directory / "cache.jsonl"),
                        "RUST_LOG": "warn,octos.prompt_cache=trace"})
            session = "oup-ci:local:runtime-compaction"
            client = self.oup_connect(directory, env, args, "before")
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, session)
                mode = client.rpc("session/compact/mode/set", {"session_id": session, "mode": "heuristic"})
                self.assertEqual(mode["result"]["mode"], "heuristic")
                for index in range(4):
                    terminal = self.oup_turn(client, session, f"PROMPT_{index} " + "alpha beta gamma " * 150)
                    self.assertEqual(terminal["payload"]["data"]["outcome"], "completed")
                before = client.rpc("session/hydrate", {
                    "session_id": session, "include": ["messages", "context"],
                })["result"]
                compacted = client.rpc("session/compact", {"session_id": session})
                self.assertIn("result", compacted, compacted)
                after = client.rpc("session/hydrate", {
                    "session_id": session, "include": ["messages", "context"],
                })["result"]
                state = after["context_state"]
                self.assertTrue(state["last_compaction_id"])
                self.assertNotEqual(state["cache_epoch_id"], before["context_state"]["cache_epoch_id"])
                self.assertEqual(after["messages"], before["messages"], "compaction must preserve durable history")
                self.assertLess(state["token_estimate"], before["context_state"]["token_estimate"])
                self.assertLessEqual(state["token_estimate"], 1000)
                self.assertTrue(any(frame.get("method") == "context/compaction_completed"
                                    for frame in client.frames))
                # Compare bytes of real provider inputs, independently of the cache observer.
                self.assertEqual(provider.requests[0]["messages"][0], provider.requests[1]["messages"][0])
                self.assertEqual(provider.requests[0]["tools"], provider.requests[1]["tools"])
            finally:
                client.close()
            restarted = self.oup_connect(directory, env, args, "after")
            try:
                self.oup_open(restarted, directory, session)
                reopened = restarted.rpc("session/hydrate", {
                    "session_id": session, "include": ["messages", "context"],
                })["result"]
                self.assertEqual(reopened["messages"], before["messages"])
                for key in ("transcript_hash", "last_compaction_id", "cache_epoch_id"):
                    self.assertEqual(reopened["context_state"][key], state[key], key)
                self.assertEqual(reopened["context_state"]["recovery_state"], "exact")
                terminal = self.oup_turn(restarted, session, "Continue after compaction and restart.")
                self.assertEqual(terminal["payload"]["data"]["outcome"], "completed")
                self.assertEqual(len(provider.requests), 5)
                self.assertLess(len(json.dumps(provider.requests[-1]["messages"])),
                                len(json.dumps(provider.requests[-2]["messages"])))
            finally:
                restarted.close()
            cache = (directory / "cache.jsonl").read_text()
            self.assertTrue(cache)
            observations = [json.loads(line) for line in cache.splitlines()]
            manifests = [row for row in observations if row["event_kind"] == "manifest"]
            usages = [row for row in observations if row["event_kind"] == "usage"]
            self.assertEqual(len(manifests), 5)
            self.assertEqual(len(usages), 5)
            keys = [row["request_key_hash"] for row in manifests]
            self.assertEqual(len(set(keys)), 5)
            self.assertEqual(set(keys), {row["request_key_hash"] for row in usages})
            self.assertEqual(manifests[1]["relation"], "append_only")
            self.assertTrue(manifests[1]["comparison"]["stable_prefix_matches"])
            self.assertNotEqual(manifests[0]["epoch_id"], manifests[-1]["epoch_id"])
            for raw_text in ("PROMPT_0", "alpha beta gamma", "retained evidence", str(directory)):
                self.assertNotIn(raw_text, cache, "cache diagnostics must not persist prompt bodies")

    def test_oup_automatic_compaction_preserves_history(self):
        with self.fixture("oup-auto-compaction", [reply(f"AUTO_FINAL_{i}") for i in range(5)]) as (
                directory, provider, env, args):
            env.update({"OCTOS_CONTEXT_COMPACT_THRESHOLD_TOKENS": "1800",
                        "OCTOS_CONTEXT_COMPACT_TARGET_TOKENS": "1000",
                        "OCTOS_CONTEXT_COMPACT_KEEP_ITEMS": "2"})
            session = "oup-ci:local:runtime-auto-compaction"
            client = self.oup_connect(directory, env, args, "automatic")
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, session)
                self.assertIn("result", client.rpc("session/compact/mode/set", {
                    "session_id": session, "mode": "heuristic",
                }))
                for index in range(5):
                    terminal = self.oup_turn(client, session, f"AUTO_PROMPT_{index} " + "alpha beta gamma " * 150)
                    self.assertEqual(terminal["payload"]["data"]["outcome"], "completed")
                hydrated = client.rpc("session/hydrate", {
                    "session_id": session, "include": ["messages", "context"],
                })["result"]
                finals = [row["content"] for row in hydrated["messages"] if row["role"] == "assistant"]
                self.assertEqual(finals, [f"AUTO_FINAL_{i}" for i in range(5)])
                installed = [frame["params"]["compaction"] for frame in client.frames
                             if frame.get("method") == "context/compaction_completed"
                             and frame["params"]["compaction"]["status"] == "installed"]
                self.assertTrue(installed, "automatic compaction must actually install a generation")
                self.assertTrue(all(row["trigger"] != "appui_manual_compact" for row in installed))
                self.assertTrue(all(row["dropped_count"] > 0 for row in installed))
                self.assertEqual(hydrated["context_state"]["last_compaction_id"], installed[-1]["compaction_id"])
                self.assertEqual(len(provider.requests), 5)
            finally:
                client.close()

    def test_oup_interrupt_then_reuse_session(self):
        release = threading.Event()
        delayed = reply("This cancelled response must not complete the turn.")
        delayed["_wait_for"] = release
        with self.fixture("oup-interrupt", [delayed, reply(FINAL)]) as (directory, provider, env, args):
            session = "oup-ci:local:runtime-interrupt"
            client = self.oup_connect(directory, env, args, "interrupt")
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, session)
                turn = str(uuid.uuid4())
                accepted = client.rpc("turn/start", {"session_id": session, "turn_id": turn,
                                                    "input": [{"kind": "text", "text": "Wait for cancellation."}]})
                self.assertEqual(accepted.get("result"), {"accepted": True})
                self.assertTrue(provider.received.wait(timeout=10), "the real model request must be in flight")
                interrupted = client.rpc("turn/interrupt", {"session_id": session, "turn_id": turn})
                self.assertTrue(interrupted["result"]["interrupted"], interrupted)
                terminal = client.wait(lambda frame: frame.get("method") == "projection/envelope"
                                       and frame["params"].get("turn_id") == turn
                                       and frame["params"]["payload"]["type"] == "turn_terminal")
                self.assertEqual(terminal["params"]["payload"]["data"]["outcome"], "interrupted")
                release.set()
                next_turn = self.oup_turn(client, session, "Now complete a fresh turn.")
                self.assertEqual(next_turn["payload"]["data"]["outcome"], "completed")
                self.assertEqual(len(provider.requests), 2)
                terminals = [frame for frame in client.frames
                             if frame.get("method") == "projection/envelope"
                             and frame["params"].get("turn_id") == turn
                             and frame["params"]["payload"]["type"] == "turn_terminal"]
                self.assertEqual(len(terminals), 1)
            finally:
                release.set()
                client.close()

    def test_oup_peer_result_survives_restart_and_reuse(self):
        with self.fixture("oup-peer", [reply("PEER_FINAL"), reply("PEER_REUSED_FINAL")]) as (
                directory, provider, env, args):
            master = "oup-ci:local:runtime-peer"
            client = self.oup_connect(directory, env, args, "before")
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, master)
                prepared = client.rpc("peer/prepare", {
                    # Controller-owned peer: no originator to trigger an
                    # automatic parent synthesis turn when this peer finishes.
                    "profile_id": "oup-ci", "title": "CI peer",
                    "brief": "Review the fixture workspace.", "worktree": False,
                    "cwd": str(directory / "workspace"),
                })
                self.assertIn("result", prepared, prepared)
                staged = prepared["result"]
                self.assertEqual(Path(staged["brief_path"]).read_text(), "Review the fixture workspace.")
                self.assertEqual(provider.requests, [], "peer preparation must not run a model")
                peer = master + "#" + staged["topic"]
                self.oup_open(client, directory, peer)
                terminal = self.oup_turn(client, peer, "Review the fixture workspace.")
                self.assertEqual(terminal["payload"]["data"]["outcome"], "completed")
                gathered = client.rpc("peer/gather", {"session_id": master, "slugs": [staged["slug"]]})
                row = gathered["result"]["peers"][0]
                self.assertTrue(row["result"].endswith("PEER_FINAL\n"), row)
                self.assertEqual(len(row["turn_history"]), 1)
            finally:
                client.close()
            restarted = self.oup_connect(directory, env, args, "after")
            try:
                self.oup_open(restarted, directory, master)
                self.oup_open(restarted, directory, peer)
                restored = restarted.rpc("peer/gather", {"session_id": master, "slugs": [staged["slug"]]})
                self.assertEqual(restored["result"]["peers"][0]["result"], row["result"])
                self.assertEqual(len(provider.requests), 1, "reopening a peer must not rerun its brief")
                terminal = self.oup_turn(restarted, peer, "Review the next fixture revision.")
                self.assertEqual(terminal["payload"]["data"]["outcome"], "completed")
                reused = restarted.rpc("peer/gather", {"session_id": master, "slugs": [staged["slug"]]})
                row = reused["result"]["peers"][0]
                self.assertTrue(row["result"].endswith("PEER_REUSED_FINAL\n"), row)
                self.assertEqual(len(row["turn_history"]), 2)
                self.assertEqual(len(provider.requests), 2)
                master_history = restarted.rpc("session/hydrate", {
                    "session_id": master, "include": ["messages"],
                })["result"]["messages"]
                self.assertEqual(master_history, [], "peer history belongs to its own session")
            finally:
                restarted.close()

    def test_oup_dynamic_peer_wakes_parent_once(self):
        gather = reply("Collecting the completed peer result.", finish="tool_calls", tool=True)
        gather["message"]["tool_calls"][0]["function"] = {"name": "peer_gather", "arguments": "{}"}
        with self.fixture("oup-peer-synthesis", [reply("PEER_EVIDENCE"), gather, reply("PARENT_SYNTHESIS")]) as (
                directory, provider, env, args):
            master = "oup-ci:local:runtime-peer-synthesis"
            client = self.oup_connect(directory, env, args, "synthesis")
            try:
                self.oup_profile(client, provider)
                self.oup_open(client, directory, master)
                prepared = client.rpc("peer/prepare", {
                    "session_id": master, "profile_id": "oup-ci", "title": "Owned CI peer",
                    "brief": "Review the fixture workspace.", "worktree": False,
                    "cwd": str(directory / "workspace"),
                })
                self.assertIn("result", prepared, prepared)
                peer = master + "#" + prepared["result"]["topic"]
                self.oup_open(client, directory, peer)
                terminal = self.oup_turn(client, peer, "Review the fixture workspace.")
                self.assertEqual(terminal["payload"]["data"]["outcome"], "completed")
                parent = client.wait(lambda frame: frame.get("method") == "projection/envelope"
                                     and frame["params"].get("session_id") == master
                                     and frame["params"]["payload"]["type"] == "turn_terminal")
                self.assertEqual(parent["params"]["payload"]["data"]["outcome"], "completed", parent)
                hydrated = client.rpc("session/hydrate", {
                    "session_id": master, "include": ["messages"],
                })["result"]["messages"]
                self.assertEqual([row["content"] for row in hydrated if row["role"] == "assistant"],
                                 ["Collecting the completed peer result.", "PARENT_SYNTHESIS"])
                self.assertEqual(len(provider.requests), 3)
                self.assertIn("PEER_EVIDENCE", json.dumps(provider.requests[-1]["messages"]))
            finally:
                client.close()


class MinimalContract(unittest.TestCase):
    def test_frontends_require_oup_runtime(self):
        directory = OUTPUT / "minimal"
        directory.mkdir(parents=True)
        config = directory / "config.json"
        config.write_text("{}")
        for frontend in ("chat", "acp"):
            with self.subTest(frontend=frontend):
                command = [str(BINARY), frontend, "--config", str(config),
                           "--data-dir", str(directory / "data"), "--cwd", str(directory)]
                if frontend == "chat":
                    command += ["--json", "--message", "No model call should occur."]
                result = subprocess.run(command, input="", capture_output=True, text=True, timeout=20)
                (directory / f"{frontend}-stdout.txt").write_text(result.stdout)
                (directory / f"{frontend}-stderr.txt").write_text(result.stderr)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("OUP runtime", result.stdout + result.stderr)
                self.assertIn("--features api", result.stdout + result.stderr)
                if frontend == "chat":
                    self.assertEqual(set(json.loads(result.stdout)), {"error"})


if __name__ == "__main__":
    binary_before = hashlib.sha256(BINARY.read_bytes()).hexdigest() if BINARY.is_file() else None
    script_before = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    minimal = "--minimal" in sys.argv
    if minimal:
        sys.argv.remove("--minimal")
    program = unittest.main(defaultTest="MinimalContract" if minimal else "RuntimeContract",
                            verbosity=2, exit=False)
    unchanged = (BINARY.is_file() and hashlib.sha256(BINARY.read_bytes()).hexdigest() == binary_before
                 and hashlib.sha256(Path(__file__).read_bytes()).hexdigest() == script_before)
    passed = program.result.wasSuccessful() and unchanged and program.result.testsRun > 0
    OUTPUT.mkdir(parents=True, exist_ok=True)
    (OUTPUT / "results.json").write_text(json.dumps({
        "passed": passed, "tests": program.result.testsRun, "minimal": minimal,
        "failures": [str(test) for test, _ in program.result.failures],
        "errors": [str(test) for test, _ in program.result.errors],
        "binary_sha256": binary_before, "script_sha256": script_before,
        "inputs_unchanged": unchanged,
    }, indent=2) + "\n")
    if not unchanged:
        print("FAIL: binary or test source changed during validation", file=sys.stderr)
    sys.exit(0 if passed else 1)
