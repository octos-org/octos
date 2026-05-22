#!/usr/bin/env python3
"""Validate M16 live TUI tmux orchestration evidence.

This validator checks the production review/start path:

- octos-tui sends review/start to octos serve --stdio.
- octos emits AppUI agent lifecycle/output/artifact events.
- native, CLI, and MCP specialists all participate.
- tmux captures show user-visible orchestration traces.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


EXPECTED_AGENT_PREFIXES = (
    "reviewer-api",
    "reviewer-tests",
    "reviewer-policy",
    "reviewer-cli",
    "reviewer-mcp",
)
CAPTURE_FILES = (
    "tui-capture-start.txt",
    "tui-capture-child-start.txt",
    "tui-capture-child-progress.txt",
    "tui-capture-one-child-finished.txt",
    "tui-capture-code-review-summary.txt",
    "tui-capture-live-final.txt",
    "tui-capture.txt",
    "menu-capture-agents.txt",
)
EVIDENCE_FILES = (
    "appui-transcript.jsonl",
    "input-replay.log",
    "launch-command.txt",
    "m16-secret-cleanup.json",
    "summary.env",
    "terminal-size.json",
)
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
SECRET_RE = re.compile(r"(?:sk-[A-Za-z0-9_-]{16,}|AIza[0-9A-Za-z_-]{20,})")


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def read_text(path: Path) -> str:
    if not path.exists():
        return ""
    return ANSI_RE.sub("", path.read_text(encoding="utf-8", errors="replace")).replace("\r", "")


def read_summary_env(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in read_text(path).splitlines():
        if "=" not in line or line.startswith("#"):
            continue
        key, value = line.split("=", 1)
        values[key] = value
    return values


def load_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        value = json.loads(read_text(path))
    except json.JSONDecodeError:
        return {}
    return value if isinstance(value, dict) else {}


def likely_text_file(path: Path) -> bool:
    if path.suffix.lower() in {
        ".json",
        ".jsonl",
        ".log",
        ".txt",
        ".md",
        ".env",
        ".sh",
        ".mjs",
        ".js",
        ".toml",
        ".yaml",
        ".yml",
    }:
        return True
    return path.name in {"launch-command.txt", "launch.sh"}


def secret_leaks_under(roots: list[Path]) -> list[str]:
    leaks: list[str] = []
    seen: set[Path] = set()
    for root in roots:
        if not root.exists():
            continue
        paths = [root] if root.is_file() else root.rglob("*")
        for path in paths:
            if not path.is_file() or not likely_text_file(path):
                continue
            resolved = path.resolve()
            if resolved in seen:
                continue
            seen.add(resolved)
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            if SECRET_RE.search(text):
                leaks.append(str(path))
    return sorted(leaks)


def load_jsonl_loose(path: Path) -> tuple[list[dict[str, Any]], int]:
    text = read_text(path)
    rows: list[dict[str, Any]] = []
    invalid = 0
    for raw_line in text.splitlines():
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
                invalid += 1
                continue
            if isinstance(value, dict):
                rows.append(value)
    return rows, invalid


def frame_text(frame: dict[str, Any]) -> str:
    params = frame.get("params")
    if isinstance(params, dict):
        text = params.get("text")
        if isinstance(text, str):
            return text
    result = frame.get("result")
    if isinstance(result, dict):
        text = result.get("text")
        if isinstance(text, str):
            return text
    return ""


def agent_prefix(agent_id: Any) -> str | None:
    if not isinstance(agent_id, str):
        return None
    for prefix in EXPECTED_AGENT_PREFIXES:
        if agent_id.startswith(prefix):
            return prefix
    return None


class Validator:
    def __init__(self, out_dir: Path) -> None:
        self.out_dir = out_dir
        self.checks: list[dict[str, Any]] = []
        self.captures = {name: read_text(out_dir / name) for name in CAPTURE_FILES}
        self.capture_text = "\n".join(self.captures.values())
        self.transcript_rows, self.transcript_invalid = load_jsonl_loose(out_dir / "appui-transcript.jsonl")
        self.frames = [
            row.get("frame")
            for row in self.transcript_rows
            if isinstance(row.get("frame"), dict)
        ]
        self.summary = read_summary_env(out_dir / "summary.env")
        self.cleanup_report = load_json(out_dir / "m16-secret-cleanup.json")
        scan_roots = [out_dir]
        if runtime_root := self.summary.get("runtime_root"):
            scan_roots.append(Path(runtime_root))
        self.secret_scan_roots = scan_roots
        self.secret_leaks = secret_leaks_under(scan_roots)
        self.secret_scan = {
            "scanned_roots": [str(root) for root in scan_roots if root.exists()],
            "cleanup_scanned_roots": self.cleanup_report.get("scanned_roots", []),
            "cleanup_scanned_file_count": self.cleanup_report.get("scanned_file_count", 0),
            "redacted_file_count": self.cleanup_report.get("redacted_file_count", 0),
            "redactions_total": self.cleanup_report.get("redactions_total", 0),
            "removed_live_provider_config": self.cleanup_report.get(
                "removed_live_provider_config", False
            ),
            "leak_paths": self.secret_leaks,
        }

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
        required = (*CAPTURE_FILES, *EVIDENCE_FILES)
        missing = [
            name
            for name in required
            if not (self.out_dir / name).exists() or (self.out_dir / name).stat().st_size == 0
        ]
        self.add(
            "required_artifacts",
            not missing,
            "all M16 tmux and AppUI evidence artifacts exist"
            if not missing
            else f"missing or empty artifacts: {', '.join(missing)}",
            list(required),
        )

    def check_real_backend(self) -> None:
        launch = read_text(self.out_dir / "launch-command.txt")
        launch_normalized = launch.replace("\\ ", " ")
        transcript = read_text(self.out_dir / "appui-transcript.jsonl")
        launch_has_stdio_serve = (
            "serve --stdio" in launch_normalized
            and "m15-fixture-appui-backend.py" not in launch
            and "--swarm-backend stdio" in launch_normalized
        )
        capabilities_advertise_review = "review.start.v1" in transcript and "review/start" in transcript
        no_secret_literal = not self.secret_leaks
        ok = launch_has_stdio_serve and capabilities_advertise_review and no_secret_literal
        self.add(
            "real_octos_serve_stdio_backend",
            ok,
            "TUI launched against octos serve --stdio with review/start and stdio MCP swarm backend"
            if ok
            else f"backend evidence does not prove octos serve --stdio review/start without leaked secrets; secret leaks={self.secret_leaks or 'none'}",
            ["launch-command.txt", "appui-transcript.jsonl", "summary.env"],
        )

    def check_secret_cleanup_and_scan(self) -> None:
        cleanup_roots = [str(root) for root in self.cleanup_report.get("scanned_roots", [])]
        expected_roots = [str(root) for root in self.secret_scan_roots if root.exists()]
        missing_roots = [
            root
            for root in expected_roots
            if root not in cleanup_roots and str(Path(root).resolve()) not in cleanup_roots
        ]
        report_ok = self.cleanup_report.get("schema") == "octos.m16.secret_cleanup.v1"
        removed_config = bool(self.cleanup_report.get("removed_live_provider_config"))
        scanned_files = int(self.cleanup_report.get("scanned_file_count") or 0)
        no_leaks = not self.secret_leaks
        ok = report_ok and removed_config and scanned_files > 0 and not missing_roots and no_leaks
        self.add(
            "full_tree_secret_cleanup_and_scan",
            ok,
            "cleanup removed live provider config and validator scanned evidence/runtime trees without provider-key leaks"
            if ok
            else (
                "secret cleanup/scan incomplete: "
                f"report_ok={report_ok} removed_config={removed_config} "
                f"scanned_files={scanned_files} missing_roots={missing_roots or 'none'} "
                f"leak_paths={self.secret_leaks or 'none'}"
            ),
            ["m16-secret-cleanup.json", "summary.env"],
        )

    def check_prompt(self) -> None:
        replay = read_text(self.out_dir / "input-replay.log")
        review_frames = [
            frame
            for frame in self.frames
            if frame.get("method") == "review/start"
        ]
        prompt_text = replay + "\n" + "\n".join(json.dumps(frame, ensure_ascii=False) for frame in review_frames)
        missing = [
            term
            for term in (
                "M16 code review UX soak",
                "review/start",
                "reviewer-api",
                "reviewer-tests",
                "reviewer-policy",
                "reviewer-cli",
                "reviewer-mcp",
                "M16_CODE_REVIEW_FINAL_LINE",
            )
            if term not in prompt_text
        ]
        self.add(
            "explicit_code_review_prompt",
            not missing and bool(review_frames),
            "replay submitted explicit review/start prompt with all specialist agent terms and final marker"
            if not missing and review_frames
            else f"submitted prompt is missing review/start evidence or terms: {missing}",
            ["input-replay.log", "appui-transcript.jsonl"],
        )

    def check_protocol_orchestration(self) -> None:
        methods = [frame.get("method") for frame in self.frames]
        missing_methods = sorted(
            method
            for method in (
                "agent/updated",
                "agent/output/delta",
                "agent/artifact/updated",
                "task/updated",
                "message/delta",
            )
            if method not in methods
        )

        started: set[str] = set()
        completed: set[str] = set()
        output_agents: set[str] = set()
        artifact_agents: set[str] = set()
        backend_kinds: dict[str, str] = {}
        for frame in self.frames:
            method = frame.get("method")
            params = frame.get("params")
            if not isinstance(params, dict):
                continue
            if method == "agent/updated":
                agent = params.get("agent")
                if not isinstance(agent, dict):
                    continue
                prefix = agent_prefix(agent.get("agent_id"))
                if prefix is None:
                    continue
                started.add(prefix)
                if isinstance(agent.get("backend_kind"), str):
                    backend_kinds[prefix] = agent["backend_kind"]
                if agent.get("status") == "completed":
                    completed.add(prefix)
            elif method == "agent/output/delta":
                prefix = agent_prefix(params.get("agent_id"))
                if prefix is not None:
                    output_agents.add(prefix)
            elif method == "agent/artifact/updated":
                prefix = agent_prefix(params.get("agent_id"))
                if prefix is not None:
                    artifact_agents.add(prefix)

        missing = {
            "notifications": missing_methods,
            "started": sorted(set(EXPECTED_AGENT_PREFIXES) - started),
            "completed": sorted(set(EXPECTED_AGENT_PREFIXES) - completed),
            "output": sorted(set(EXPECTED_AGENT_PREFIXES) - output_agents),
            "artifacts": sorted(set(EXPECTED_AGENT_PREFIXES) - artifact_agents),
        }
        expected_kinds = {
            "reviewer-api": "native",
            "reviewer-tests": "native",
            "reviewer-policy": "native",
            "reviewer-cli": "cli_process",
            "reviewer-mcp": "mcp_agent",
        }
        missing["backend_kind"] = sorted(
            prefix
            for prefix, expected in expected_kinds.items()
            if backend_kinds.get(prefix) != expected
        )
        failures = {key: value for key, value in missing.items() if value}
        self.add(
            "appui_orchestration_events",
            not failures,
            "AppUI transcript includes child start, output, artifacts, completion, and backend kind for every reviewer"
            if not failures
            else f"orchestration event gaps: {failures}",
            ["appui-transcript.jsonl"],
        )

    def check_visible_traces(self) -> None:
        message_text = "\n".join(frame_text(frame) for frame in self.frames if frame.get("method") == "message/delta")
        all_text = "\n".join([self.capture_text, message_text, read_text(self.out_dir / "input-replay.log")])
        checks = [
            (
                "visible_child_start",
                bool(re.search(r"Ada Lovelace|Hypatia|Socrates|Grace Hopper|Marie Curie|reviewer-api", all_text, re.I)),
                "child specialist names are visible in transcript or tmux captures",
                "child specialist names missing from transcript and tmux captures",
                ["appui-transcript.jsonl", "tui-capture-child-start.txt"],
            ),
            (
                "visible_one_child_finished_summary",
                "Subagent done:" in message_text and any(prefix in message_text for prefix in EXPECTED_AGENT_PREFIXES),
                "one-child-finished summary appeared in AppUI message stream",
                "one-child-finished summary missing from AppUI message stream",
                ["appui-transcript.jsonl"],
            ),
            (
                "visible_final_joined_answer",
                bool(
                    re.search(r"Code Review|Findings|review", all_text, re.I)
                    and "M16_CODE_REVIEW_FINAL_LINE" in all_text
                ),
                "final joined code-review answer is visible in transcript or tmux captures",
                "final joined code-review answer missing from transcript and tmux captures",
                ["appui-transcript.jsonl", "tui-capture-code-review-summary.txt", "tui-capture-live-final.txt"],
            ),
        ]
        for check_id, passed, ok_detail, fail_detail, evidence in checks:
            self.add(check_id, passed, ok_detail if passed else fail_detail, evidence)

    def check_artifacts(self) -> None:
        artifact_agents: set[str] = set()
        artifact_count = 0
        for frame in self.frames:
            if frame.get("method") != "agent/artifact/updated":
                continue
            params = frame.get("params")
            if not isinstance(params, dict):
                continue
            prefix = agent_prefix(params.get("agent_id"))
            if prefix is not None:
                artifact_agents.add(prefix)
                artifact_count += 1
        missing = sorted(set(EXPECTED_AGENT_PREFIXES) - artifact_agents)
        self.add(
            "review_artifacts_collected",
            not missing and artifact_count >= len(EXPECTED_AGENT_PREFIXES),
            "AppUI artifact notifications include per-agent review artifacts"
            if not missing and artifact_count >= len(EXPECTED_AGENT_PREFIXES)
            else f"artifact notifications missing for prefixes={missing or 'none'} count={artifact_count}",
            ["appui-transcript.jsonl"],
        )

    def run(self) -> dict[str, Any]:
        self.require_files()
        self.check_real_backend()
        self.check_secret_cleanup_and_scan()
        self.check_prompt()
        self.check_protocol_orchestration()
        self.check_visible_traces()
        self.check_artifacts()
        failures = [check for check in self.checks if check["status"] == "failed"]
        result = {
            "schema": "octos.m16.tui_tmux_orchestration_soak.v1",
            "generated_at": utc_now(),
            "status": "failed" if failures else "passed",
            "output_dir": str(self.out_dir),
            "jsonl_parse_warnings": {
                "appui_transcript_invalid_lines": self.transcript_invalid,
            },
            "secret_scan": self.secret_scan,
            "checks": self.checks,
            "failures": failures,
            "evidence": {name: str(self.out_dir / name) for name in (*CAPTURE_FILES, *EVIDENCE_FILES)},
        }
        (self.out_dir / "m16-ux-soak-validation.json").write_text(
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
