#!/usr/bin/env python3
"""Dependency-light Octos harness event emitter for Python tools."""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
from typing import Any
from urllib.parse import unquote, urlparse

SCHEMA = "octos.harness.event.v1"


def build_progress_event(
    session_id: str,
    task_id: str,
    workflow: str,
    phase: str,
    message: str,
    progress: float | None = None,
) -> dict[str, Any]:
    event: dict[str, Any] = {
        "schema": SCHEMA,
        "kind": "progress",
        "session_id": session_id,
        "task_id": task_id,
        "workflow": workflow,
        "phase": phase,
        "message": message,
    }
    if progress is not None:
        event["progress"] = progress
    return event


def _sink_path(sink: str) -> tuple[str, str]:
    if "://" not in sink:
        return ("file", sink)

    parsed = urlparse(sink)
    scheme = parsed.scheme.lower()
    if scheme == "file":
        path = unquote(parsed.path)
        if parsed.netloc and parsed.netloc != "localhost":
            path = f"/{parsed.netloc}{path}"
        return ("file", path)
    if scheme == "unix":
        path = unquote(parsed.path or parsed.netloc)
        return ("unix", path)
    return (scheme, sink)


def _append_file(path: str, line: str) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(fd, line.encode("utf-8"))
    finally:
        os.close(fd)


def _send_unix_socket(path: str, line: str) -> None:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(path)
        sock.sendall(line.encode("utf-8"))
    finally:
        sock.close()


def emit_event(event: dict[str, Any], sink: str | None = None) -> bool:
    target = sink if sink is not None else os.environ.get("OCTOS_EVENT_SINK", "")
    if not target:
        return False

    line = json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n"
    transport, path = _sink_path(target)

    try:
        if transport == "unix":
            if not path:
                raise ValueError("unix sink path is empty")
            _send_unix_socket(path, line)
        else:
            _append_file(path, line)
        return True
    except Exception as exc:
        print(f"octos event sink write failed: {exc}", file=sys.stderr)
        return False


def emit_progress(
    session_id: str,
    task_id: str,
    workflow: str,
    phase: str,
    message: str,
    progress: float | None = None,
    sink: str | None = None,
) -> bool:
    return emit_event(
        build_progress_event(
            session_id=session_id,
            task_id=task_id,
            workflow=workflow,
            phase=phase,
            message=message,
            progress=progress,
        ),
        sink=sink,
    )


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Emit an Octos harness progress event")
    parser.add_argument("--session-id", required=True)
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--workflow", required=True)
    parser.add_argument("--phase", required=True)
    parser.add_argument("--message", required=True)
    parser.add_argument("--progress", type=float)
    parser.add_argument("--sink", default=os.environ.get("OCTOS_EVENT_SINK", ""))
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    wrote = emit_progress(
        session_id=args.session_id,
        task_id=args.task_id,
        workflow=args.workflow,
        phase=args.phase,
        message=args.message,
        progress=args.progress,
        sink=args.sink,
    )
    return 0 if not args.sink or wrote else 1


if __name__ == "__main__":
    raise SystemExit(main())
