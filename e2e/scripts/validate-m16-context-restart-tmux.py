#!/usr/bin/env python3
"""Validate visual TUI evidence for M16 ContextManager restart/reconnect."""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def read_text(path: Path) -> str:
    if not path.exists():
        return ""
    return ANSI_RE.sub("", path.read_text(encoding="utf-8", errors="replace")).replace("\r", "")


def read_env(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in read_text(path).splitlines():
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key] = value
    return values


def load_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def load_jsonl_loose(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for raw in read_text(path).splitlines():
        line = raw.strip()
        if not line:
            continue
        candidates = line.replace("}{", "}\n{").splitlines() if "}{" in line else [line]
        for candidate in candidates:
            try:
                value = json.loads(candidate)
            except json.JSONDecodeError:
                continue
            if isinstance(value, dict):
                rows.append(value)
    return rows


def frame_from_row(row: dict[str, Any]) -> dict[str, Any]:
    frame = row.get("frame")
    return frame if isinstance(frame, dict) else {}


class Validator:
    def __init__(self, out_dir: Path) -> None:
        self.out_dir = out_dir
        self.expected = read_env(out_dir / "context-reconnect-expected.env")
        self.bootstrap = load_json(Path(self.expected.get("bootstrap_summary", "")))
        self.frames = [frame_from_row(row) for row in load_jsonl_loose(out_dir / "appui-transcript.jsonl")]
        capture_names = [
            "tui-capture-reconnected-session.txt",
            "tui-capture-after-status-refresh.txt",
            "menu-capture-status.txt",
            "tui-capture-status-context-menu.txt",
            "tui-capture.txt",
        ]
        self.capture_text = "\n".join(read_text(out_dir / name) for name in capture_names)
        self.checks: list[dict[str, Any]] = []

    def add(self, check_id: str, passed: bool, detail: str, evidence: list[str]) -> None:
        self.checks.append(
            {
                "id": check_id,
                "status": "passed" if passed else "failed",
                "detail": detail,
                "evidence": evidence,
            }
        )

    def require_artifacts(self) -> None:
        required = [
            "context-reconnect-expected.env",
            "bootstrap-stdio/m16-context-restart-stdio-summary.json",
            "launch-command.txt",
            "input-replay.log",
            "appui-transcript.jsonl",
            "menu-capture-status.txt",
            "tui-capture-status-context-menu.txt",
            "tui-capture.txt",
        ]
        missing = [
            name
            for name in required
            if not (self.out_dir / name).exists() or (self.out_dir / name).stat().st_size == 0
        ]
        self.add(
            "required_artifacts",
            not missing,
            "all visual restart/reconnect artifacts exist"
            if not missing
            else f"missing or empty artifacts: {missing}",
            required,
        )

    def check_bootstrap_restart_proof(self) -> None:
        first = self.bootstrap.get("firstContext") or {}
        initial = self.bootstrap.get("secondInitialContext") or self.bootstrap.get("secondContext") or {}
        final = self.bootstrap.get("secondContext") or initial
        same_hash = first.get("transcript_hash") == initial.get("transcript_hash")
        same_compaction = first.get("last_compaction_id") == initial.get("last_compaction_id")
        exact = initial.get("recovery_state") == "exact"
        post_turns = int(self.bootstrap.get("postRestartTurns") or 0)
        progressed = (
            post_turns == 0
            or int(final.get("generation") or 0) > int(initial.get("generation") or 0)
        )
        post_exact = final.get("recovery_state") == "exact"
        self.add(
            "bootstrap_stdio_restart_exact",
            bool(self.bootstrap.get("ok")) and same_hash and same_compaction and exact and progressed and post_exact,
            "direct stdio bootstrap proved exact reload before visual TUI phase"
            if bool(self.bootstrap.get("ok")) and same_hash and same_compaction and exact and progressed and post_exact
            else "bootstrap restart proof did not preserve exact initial reload or post-reconnect progress",
            ["bootstrap-stdio/m16-context-restart-stdio-summary.json"],
        )

    def check_real_backend_launch(self) -> None:
        launch = read_text(self.out_dir / "launch-command.txt").replace("\\ ", " ")
        passed = "serve --stdio" in launch and "m15-fixture-appui-backend.py" not in launch
        self.add(
            "real_restarted_octos_backend",
            passed,
            "TUI launched against restarted octos serve --stdio"
            if passed
            else "launch command did not prove real octos serve --stdio backend",
            ["launch-command.txt"],
        )

    def check_visual_context_menu(self) -> None:
        generation = self.expected.get("expected_generation", "")
        terms = ["Context", f"gen {generation}", "exact"]
        missing = [term for term in terms if term and term not in self.capture_text]
        self.add(
            "visual_context_status_exact",
            not missing,
            "TUI status context menu renders exact reloaded context generation"
            if not missing
            else f"TUI context capture missing terms: {missing}",
            ["menu-capture-status.txt", "tui-capture-status-context-menu.txt", "tui-capture.txt"],
        )

    def check_appui_status_read(self) -> None:
        expected_hash = self.expected.get("expected_transcript_hash")
        expected_compaction = self.expected.get("expected_compaction_id")
        sent_status_read = any(frame.get("method") == "session/status/read" for frame in self.frames)
        result_text = json.dumps(self.frames, ensure_ascii=False)
        has_hash = bool(expected_hash and expected_hash in result_text)
        has_compaction = bool(expected_compaction and expected_compaction in result_text)
        has_exact = '"recovery_state":"exact"' in result_text.replace(" ", "")
        self.add(
            "appui_status_read_exact_context",
            sent_status_read and has_hash and has_compaction and has_exact,
            "AppUI transcript includes session/status/read with exact compacted context state"
            if sent_status_read and has_hash and has_compaction and has_exact
            else "AppUI transcript missing status/read exact context evidence",
            ["appui-transcript.jsonl"],
        )

    def run(self) -> dict[str, Any]:
        self.require_artifacts()
        self.check_bootstrap_restart_proof()
        self.check_real_backend_launch()
        self.check_visual_context_menu()
        self.check_appui_status_read()
        failures = [check for check in self.checks if check["status"] == "failed"]
        result = {
            "schema": "octos.m16.context_restart_tmux_soak.v1",
            "generated_at": utc_now(),
            "status": "failed" if failures else "passed",
            "output_dir": str(self.out_dir),
            "expected": self.expected,
            "checks": self.checks,
            "failures": failures,
        }
        (self.out_dir / "m16-context-restart-tmux-validation.json").write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()
    result = Validator(args.out_dir.resolve()).run()
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
