#!/usr/bin/env python3
"""Validate the M15 TaskSupervisor mirrored-agent live tmux soak."""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
REQUIRED_FILES = (
    "summary.env",
    "launch-command.txt",
    "input-replay.log",
    "terminal-size.json",
    "appui-transcript.jsonl",
    "tui-capture-task-mirror.txt",
    "task-output-modal.txt",
    "menu-capture-agents.txt",
    "tui-capture.txt",
)


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def read_text(path: Path) -> str:
    if not path.exists():
        return ""
    return ANSI_RE.sub("", path.read_text(encoding="utf-8", errors="replace")).replace("\r", "")


def load_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    if not path.exists():
        return rows
    for raw_line in read_text(path).splitlines():
        line = raw_line.strip()
        if not line:
            continue
        candidates = [line]
        if "}{" in line:
            candidates = line.replace("}{", "}\n{").splitlines()
        for candidate in candidates:
            try:
                value = json.loads(candidate)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                rows.append(value)
    return rows


def frame_from_row(row: dict[str, Any]) -> dict[str, Any] | None:
    frame = row.get("frame")
    return frame if isinstance(frame, dict) else None


def artifact_info(out_dir: Path, name: str) -> dict[str, Any]:
    path = out_dir / name
    return {
        "path": str(path),
        "exists": path.exists(),
        "bytes": path.stat().st_size if path.exists() else 0,
    }


class Validator:
    def __init__(self, out_dir: Path) -> None:
        self.out_dir = out_dir
        self.checks: list[dict[str, Any]] = []
        self.frames = [
            frame
            for row in load_jsonl(out_dir / "appui-transcript.jsonl")
            if (frame := frame_from_row(row)) is not None
        ]
        self.capture_text = "\n".join(
            read_text(out_dir / name)
            for name in (
                "tui-capture-start.txt",
                "tui-capture-task-mirror.txt",
                "task-output-modal.txt",
                "menu-capture-agents.txt",
                "tui-capture-live-final.txt",
                "tui-capture.txt",
            )
        )

    def add(self, check_id: str, passed: bool, detail: str, evidence: list[str]) -> None:
        self.checks.append(
            {
                "id": check_id,
                "status": "passed" if passed else "failed",
                "detail": detail,
                "evidence": evidence,
            }
        )

    def require_files(self) -> None:
        missing = [
            name
            for name in REQUIRED_FILES
            if not (self.out_dir / name).exists() or (self.out_dir / name).stat().st_size == 0
        ]
        self.add(
            "required_artifacts",
            not missing,
            "all tmux and AppUI evidence artifacts exist"
            if not missing
            else f"missing or empty artifacts: {', '.join(missing)}",
            list(REQUIRED_FILES),
        )

    def check_real_backend(self) -> None:
        launch = read_text(self.out_dir / "launch-command.txt").replace("\\ ", " ")
        ok = (
            "serve --stdio" in launch
            and "OCTOS_M9_PROTOCOL_FIXTURES=1" in launch
            and "m15-fixture-appui-backend.py" not in launch
        )
        self.add(
            "real_octos_serve_stdio_backend",
            ok,
            "octos-tui launched against real octos serve --stdio with M9 protocol fixture enabled"
            if ok
            else "launch command does not prove real octos serve --stdio plus M9 fixture",
            ["launch-command.txt"],
        )

    def check_prompt(self) -> None:
        replay = read_text(self.out_dir / "input-replay.log")
        turn_frames = [
            frame
            for frame in self.frames
            if frame.get("method") == "turn/start"
        ]
        text = replay + "\n" + "\n".join(json.dumps(frame, ensure_ascii=False) for frame in turn_frames)
        required = (
            "M9 task output fixture",
            "mirror it into agent supervision",
        )
        missing = [term for term in required if term not in text]
        self.add(
            "task_supervisor_fixture_prompt",
            not missing and bool(turn_frames),
            "replay submitted the deterministic TaskSupervisor mirror prompt through TUI"
            if not missing and turn_frames
            else f"missing prompt evidence: {missing}",
            ["input-replay.log", "appui-transcript.jsonl"],
        )

    def check_protocol_events(self) -> None:
        methods = [frame.get("method") for frame in self.frames]
        agent_updates: list[dict[str, Any]] = []
        task_updates: list[dict[str, Any]] = []
        task_deltas: list[dict[str, Any]] = []
        agent_list_responses: list[dict[str, Any]] = []
        for frame in self.frames:
            method = frame.get("method")
            params = frame.get("params")
            if method == "agent/updated" and isinstance(params, dict):
                agent = params.get("agent")
                if isinstance(agent, dict):
                    agent_updates.append(agent)
            elif method == "task/updated" and isinstance(params, dict):
                task_updates.append(params)
            elif method == "task/output/delta" and isinstance(params, dict):
                task_deltas.append(params)
            elif isinstance(frame.get("result"), dict):
                result = frame["result"]
                if isinstance(result.get("agents"), list):
                    agent_list_responses.append(result)

        mirrored = [
            agent
            for agent in agent_updates
            if str(agent.get("backend_kind", "")) == "spawn_child_session"
            or str(agent.get("backend_kind", "")).startswith("task_supervisor:")
        ]
        terminal = [
            agent
            for agent in mirrored
            if agent.get("status") in {"completed", "failed", "interrupted"}
        ]
        listed_ids = {
            agent.get("agent_id")
            for result in agent_list_responses
            for agent in result.get("agents", [])
            if isinstance(agent, dict)
        }
        terminal_id = terminal[-1].get("agent_id") if terminal else None
        delta_text = "\n".join(str(delta.get("text", "")) for delta in task_deltas)
        missing_methods = [
            method
            for method in ("turn/started", "agent/updated", "task/updated", "task/output/delta", "turn/completed")
            if method not in methods
        ]
        ok = (
            not missing_methods
            and bool(mirrored)
            and bool(terminal)
            and terminal_id in listed_ids
            and "fixture output line one" in delta_text
        )
        self.add(
            "task_supervisor_mirrored_agent_protocol",
            ok,
            "TaskSupervisor task was mirrored into agent lifecycle, listed through agent/list, and streamed through task/output/delta"
            if ok
            else (
                "missing protocol proof: "
                f"methods={missing_methods}, mirrored={len(mirrored)}, terminal={len(terminal)}, "
                f"terminal_listed={terminal_id in listed_ids}, delta_has_output={'fixture output line one' in delta_text}, "
            ),
            ["appui-transcript.jsonl"],
        )

    def check_visible_tui(self) -> None:
        required_terms = (
            "Agent task",
            "shell m9_fixture",
            "fixture output line one",
        )
        missing = [term for term in required_terms if term not in self.capture_text]
        self.add(
            "visible_tui_task_supervisor_trace",
            not missing,
            "tmux captures show the mirrored agent task and fixture output in the terminal UI"
            if not missing
            else f"tmux captures missing visible terms: {missing}",
            ["tui-capture-task-mirror.txt", "task-output-modal.txt", "menu-capture-agents.txt", "tui-capture.txt"],
        )

    def run(self) -> dict[str, Any]:
        self.require_files()
        self.check_real_backend()
        self.check_prompt()
        self.check_protocol_events()
        self.check_visible_tui()
        failures = [check for check in self.checks if check["status"] == "failed"]
        result = {
            "schema": "octos.m15.task-supervisor-mirror.tmux-validation.v1",
            "generated_at": utc_now(),
            "status": "failed" if failures else "passed",
            "output_dir": str(self.out_dir),
            "artifacts": {name: artifact_info(self.out_dir, name) for name in REQUIRED_FILES},
            "checks": self.checks,
            "failures": failures,
        }
        (self.out_dir / "m15-task-supervisor-mirror-tmux-validation.json").write_text(
            json.dumps(result, indent=2, ensure_ascii=False, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()
    result = Validator(args.out_dir).run()
    print(json.dumps(result, indent=2, ensure_ascii=False, sort_keys=True))
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
