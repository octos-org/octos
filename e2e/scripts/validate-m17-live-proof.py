#!/usr/bin/env python3
"""Validate #1023 M17 live proof evidence.

Aggregates the live DeepSeek review/start soak, TUI tmux soak, loop/goal
budget soaks, direct spawn evidence, child-context ledgers, artifact indexes,
and secret hygiene into one strict closure report. This reads captured evidence
only; it does not need provider credentials.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA = "octos.m17.live_proof.validation.v1"
INDEX_SCHEMA = "octos.m17.live_proof.artifact_index.v1"
EXPECTED_TUI_CAPTURE_FILES = (
    "tui-capture-child-start.txt",
    "tui-capture-child-progress.txt",
    "tui-capture-one-child-finished.txt",
    "tui-capture-code-review-summary.txt",
    "tui-capture-live-final.txt",
    "tui-capture.txt",
)
M17_REQUIRED_TMUX_FILES = (
    "appui-transcript.jsonl",
    "server.log",
    "runtime-policy-stamp.json",
    "agent-ledger.jsonl",
    "task-ledger.jsonl",
    "artifact-index.json",
    "m16-ux-soak-validation.json",
    "secret-scan-report.txt",
    *EXPECTED_TUI_CAPTURE_FILES,
)
TEXT_SUFFIXES = {
    ".json", ".jsonl", ".log", ".txt", ".md", ".env", ".sh",
    ".mjs", ".js", ".toml", ".yaml", ".yml",
}
SECRET_PATTERNS = (
    re.compile(r"sk-(?:proj-|ant-|svcacct-|admin-|or-v1-)?[A-Za-z0-9._-]{20,}"),
    re.compile(r"sk-ant-oat01-[A-Za-z0-9._-]{20,}"),
    re.compile(r"AIza[0-9A-Za-z_-]{30,}"),
    re.compile(r"Bearer [A-Za-z0-9._-]{32,}"),
    re.compile(r"AC[0-9a-fA-F]{32}"),
)
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def read_text(path: Path) -> str:
    if not path.exists():
        return ""
    return ANSI_RE.sub("", path.read_text(encoding="utf-8", errors="replace")).replace("\r", "")


def load_json(path: Path) -> Any:
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return None


def load_jsonl(path: Path) -> tuple[list[dict[str, Any]], int]:
    rows: list[dict[str, Any]] = []
    invalid = 0
    for raw in read_text(path).splitlines():
        line = raw.strip()
        if not line:
            continue
        candidates = line.replace("}{", "}\n{").splitlines() if "}{" in line else [line]
        for candidate in candidates:
            try:
                value = json.loads(candidate)
            except json.JSONDecodeError:
                invalid += 1
                continue
            if isinstance(value, dict):
                rows.append(value)
    return rows, invalid


def text_file(path: Path) -> bool:
    return path.suffix.lower() in TEXT_SUFFIXES or path.name in {
        "launch-command.txt", "launch.sh", "summary.env",
    }


def scan_secret_leaks(roots: list[Path]) -> list[str]:
    leaks: list[str] = []
    seen: set[Path] = set()
    for root in roots:
        if not root.exists():
            continue
        paths = [root] if root.is_file() else root.rglob("*")
        for path in paths:
            if not path.is_file() or not text_file(path):
                continue
            resolved = path.resolve()
            if resolved in seen:
                continue
            seen.add(resolved)
            text = path.read_text(encoding="utf-8", errors="replace")
            if any(pattern.search(text) for pattern in SECRET_PATTERNS):
                leaks.append(str(path))
    return sorted(leaks)


def frame_text(frame: dict[str, Any]) -> str:
    params = frame.get("params")
    if isinstance(params, dict) and isinstance(params.get("text"), str):
        return params["text"]
    result = frame.get("result")
    if isinstance(result, dict) and isinstance(result.get("text"), str):
        return result["text"]
    return ""


def artifact_record(category: str, path: Path) -> dict[str, Any]:
    exists = path.exists()
    return {
        "id": f"{category}:{path.name}",
        "category": category,
        "path": str(path),
        "exists": exists,
        "bytes": path.stat().st_size if exists and path.is_file() else 0,
    }


class Validator:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.out_dir = args.out_dir.resolve()
        self.checks: list[dict[str, Any]] = []
        self.artifacts: list[dict[str, Any]] = []
        self.inputs = {
            "m15_native_dir": str(args.m15_native_dir.resolve()) if args.m15_native_dir else None,
            "m16_tmux_dir": str(args.m16_tmux_dir.resolve()) if args.m16_tmux_dir else None,
            "loop_dir": str(args.loop_dir.resolve()) if args.loop_dir else None,
            "goal_dir": str(args.goal_dir.resolve()) if args.goal_dir else None,
            "spawn_dir": str(args.spawn_dir.resolve()) if args.spawn_dir else None,
            "budget_grace_dir": str(args.budget_grace_dir.resolve()) if args.budget_grace_dir else None,
        }

    def add(self, check_id: str, status: str, detail: str, evidence: list[Path | str], category: str) -> None:
        assert status in {"passed", "failed", "warning"}
        self.checks.append({
            "id": check_id,
            "category": category,
            "status": status,
            "detail": detail,
            "evidence": [str(item) for item in evidence],
        })

    def add_artifacts(self, category: str, root: Path | None, names: tuple[str, ...]) -> None:
        if root is None:
            return
        self.artifacts.extend(artifact_record(category, root / name) for name in names)

    def require_input_dir(self, label: str, path: Path | None) -> bool:
        if path and path.exists():
            self.add(f"{label}_input_present", "passed", f"{label} evidence directory exists", [path], label)
            return True
        self.add(f"{label}_input_present", "failed", f"{label} evidence directory was not supplied or does not exist", [path or "<missing>"], label)
        return False

    def validate_m15_native(self) -> None:
        root = self.args.m15_native_dir
        if not self.require_input_dir("m15_native", root):
            return
        assert root is not None
        summary_path = root / "m15-native-review-start-summary.json"
        transcript_path = root / "client-observed-appui-transcript.jsonl"
        server_path = root / "server-stderr.log"
        self.add_artifacts("m15_native", root, (summary_path.name, transcript_path.name, server_path.name))
        summary = load_json(summary_path)
        queried = summary.get("queriedAgents", []) if isinstance(summary, dict) else []
        completed = [agent for agent in queried if agent.get("status") == "completed"]
        native_completed = [agent for agent in completed if agent.get("backendKind") == "native" or agent.get("backend_kind") == "native"]
        deepseek = isinstance(summary, dict) and summary.get("providerFamily") == "deepseek" and "deepseek" in str(summary.get("modelId", ""))
        ok = bool(isinstance(summary, dict) and summary.get("ok") is True and deepseek and len(completed) >= 5 and len(native_completed) >= 3)
        self.add(
            "deepseek_review_start_swarm_passed",
            "passed" if ok else "failed",
            "DeepSeek review/start soak passed with at least 5 completed specialists and 3 native children" if ok else "DeepSeek review/start summary is missing, failed, or lacks 3 completed native specialists",
            [summary_path],
            "m15_native",
        )
        rows, invalid = load_jsonl(transcript_path)
        frames = [row.get("frame") for row in rows if isinstance(row.get("frame"), dict)]
        transcript_text = "\n".join(json.dumps(frame, ensure_ascii=False) for frame in frames)
        joined_text = "\n".join(frame_text(frame) for frame in frames)
        review_start_seen = "review/start" in transcript_text
        fixture_absent = "m15-fixture-appui-backend" not in transcript_text
        final_join_seen = bool(re.search(r"Code Review|Findings|review", joined_text, re.I))
        self.add(
            "deepseek_review_start_transcript",
            "passed" if review_start_seen and fixture_absent and final_join_seen else "failed",
            "AppUI transcript shows review/start, no fixture backend marker, and a joined review answer" if review_start_seen and fixture_absent and final_join_seen else "AppUI transcript does not prove review/start, no-fixture execution, and final joined review text",
            [transcript_path, f"invalid_jsonl_lines={invalid}"],
            "m15_native",
        )
        self.add(
            "deepseek_review_start_server_log",
            "passed" if server_path.exists() and server_path.stat().st_size > 0 else "warning",
            "server stderr log exists" if server_path.exists() and server_path.stat().st_size > 0 else "server stderr log is absent or empty; stdio transcript may still contain enough protocol evidence",
            [server_path],
            "m15_native",
        )

    def validate_m16_tmux(self) -> None:
        root = self.args.m16_tmux_dir
        if not self.require_input_dir("m16_tmux", root):
            return
        assert root is not None
        self.add_artifacts("m16_tmux", root, M17_REQUIRED_TMUX_FILES)
        missing = [name for name in M17_REQUIRED_TMUX_FILES if not (root / name).exists() or (root / name).stat().st_size == 0]
        self.add(
            "m16_required_artifacts",
            "passed" if not missing else "failed",
            "TUI/AppUI evidence includes captures, transcript, logs, ledgers, artifact index, validation, and secret scan report" if not missing else f"missing or empty M17 tmux artifacts: {', '.join(missing)}",
            [root / name for name in M17_REQUIRED_TMUX_FILES],
            "m16_tmux",
        )
        m16_report = load_json(root / "m16-ux-soak-validation.json")
        self.add(
            "m16_validator_passed",
            "passed" if isinstance(m16_report, dict) and m16_report.get("status") == "passed" else "failed",
            "M16 tmux validator passed" if isinstance(m16_report, dict) and m16_report.get("status") == "passed" else "M16 tmux validator report is absent or not passed",
            [root / "m16-ux-soak-validation.json"],
            "m16_tmux",
        )
        capture_text = "\n".join(read_text(root / name) for name in EXPECTED_TUI_CAPTURE_FILES)
        transcript = read_text(root / "appui-transcript.jsonl")
        visible_final = "M16_CODE_REVIEW_FINAL_LINE" in capture_text + transcript
        visible_children = bool(re.search(r"Ada Lovelace|Hypatia|Socrates|Grace Hopper|Marie Curie|reviewer-api", capture_text + transcript, re.I))
        self.add(
            "m16_tui_visible_joined_answer",
            "passed" if visible_final and visible_children else "failed",
            "TUI captures/transcript show child specialists and final joined answer marker" if visible_final and visible_children else "TUI captures/transcript do not show both child specialists and the final joined answer marker",
            [root / "tui-capture-live-final.txt", root / "appui-transcript.jsonl"],
            "m16_tmux",
        )

    def validate_loop_goal_budget(self) -> None:
        loop_root = self.args.loop_dir
        goal_root = self.args.goal_dir
        if self.require_input_dir("loop_budget", loop_root):
            assert loop_root is not None
            self.add_artifacts("loop_budget", loop_root, ("m15-loop-runtime-stdio-soak-summary.json", "client-observed-appui-transcript.jsonl", "server-stderr.log"))
            summary = load_json(loop_root / "m15-loop-runtime-stdio-soak-summary.json")
            ok = bool(isinstance(summary, dict) and summary.get("ok") is True and int(summary.get("scheduledFires", 0)) >= 3 and int(summary.get("fireNowFires", 0)) >= 1 and summary.get("secretScanClean") is True)
            self.add("loop_runtime_live_budget_passed", "passed" if ok else "failed", "Loop runtime soak passed with scheduled fires, fire_now, and clean secret scan" if ok else "Loop runtime soak summary is absent, failed, or lacks scheduled/fire_now evidence", [loop_root / "m15-loop-runtime-stdio-soak-summary.json"], "loop_budget")
        if self.require_input_dir("goal_budget", goal_root):
            assert goal_root is not None
            self.add_artifacts("goal_budget", goal_root, ("m15-goal-runtime-stdio-soak-summary.json", "client-observed-appui-transcript.jsonl", "server-stderr.log"))
            summary = load_json(goal_root / "m15-goal-runtime-stdio-soak-summary.json")
            scenarios = summary.get("goalScenarios", []) if isinstance(summary, dict) else []
            budget = next((s for s in scenarios if s.get("name") == "budget_exhaust"), {})
            sentinel = next((s for s in scenarios if s.get("name") == "sentinel_complete"), {})
            explicit_only = any("organic_token_exhaustion_not_wire_observable" in str(gap) for gap in summary.get("knownGaps", [])) if isinstance(summary, dict) else False
            ok = bool(isinstance(summary, dict) and summary.get("ok") is True and budget.get("transitioned_to") == "budget_limited" and sentinel.get("transitioned_to") == "complete" and summary.get("secretScanClean") is True)
            detail = "Goal runtime soak passed with budget_limited and sentinel_complete transitions" if ok else "Goal runtime soak summary is absent, failed, or lacks budget/sentinel transitions"
            if explicit_only:
                detail += "; organic token exhaustion remains a known production gap"
            self.add("goal_runtime_budget_passed", "passed" if ok else "failed", detail, [goal_root / "m15-goal-runtime-stdio-soak-summary.json"], "goal_budget")
        budget_grace_root = self.args.budget_grace_dir or goal_root or loop_root
        grace_texts: list[str] = []
        evidence: list[Path] = []
        if budget_grace_root and budget_grace_root.exists():
            for path in budget_grace_root.rglob("*"):
                if path.is_file() and text_file(path):
                    evidence.append(path)
                    grace_texts.append(read_text(path))
        grace_blob = "\n".join(grace_texts)
        grace_visible = bool(re.search(r"budget exhausted; granting one grace call|budget-grace|LoopRetryState|observe_budget_exhaustion", grace_blob, re.I))
        bounded_stop = bool(re.search(r"budget_limited|Token budget exceeded|max_iterations|budget exhausted", grace_blob, re.I))
        self.add("budget_stop_grace_visible", "passed" if grace_visible and bounded_stop else "failed", "Budget stop/grace behavior is visible and bounded in captured logs" if grace_visible and bounded_stop else "No captured live evidence shows the LoopRetryState budget grace-call path; #1023 should remain open until this is run", evidence[:20] or [budget_grace_root or "<missing>"], "budget_grace")

    def validate_child_context_and_spawn(self) -> None:
        roots = [root for root in (self.args.m15_native_dir, self.args.m16_tmux_dir, self.args.loop_dir, self.args.goal_dir, self.args.spawn_dir) if root and root.exists()]
        ledger_paths: list[Path] = []
        for root in roots:
            ledger_paths.extend(root.rglob("child-context-ledger.jsonl"))
            ledger_paths.extend(root.rglob("context-ledger.jsonl"))
        child_ids: set[str] = set()
        unmanaged: set[str] = set()
        for path in ledger_paths:
            rows, _ = load_jsonl(path)
            for row in rows:
                agent_value = row.get("agent")
                nested_agent_id = agent_value.get("agent_id") if isinstance(agent_value, dict) else None
                agent_id = row.get("agent_id") or row.get("child_agent_id") or row.get("child_id") or nested_agent_id
                if isinstance(agent_id, str):
                    child_ids.add(agent_id)
                mode = str(row.get("context_mode") or row.get("mode") or row.get("event") or "")
                if "external_context_unmanaged" in mode:
                    unmanaged.add(agent_id or str(row.get("backend_kind") or "unknown"))
        self.add(
            "child_context_ledgers",
            "passed" if len(child_ids) >= 3 else "failed",
            "Child context ledgers include at least three managed child records" if len(child_ids) >= 3 else f"Child context ledgers are missing or do not cover three managed children; unmanaged markers={sorted(unmanaged) or 'none'}",
            ledger_paths or [root / "child-context-ledger.jsonl" for root in roots],
            "child_context",
        )
        spawn_texts: list[str] = []
        spawn_evidence: list[Path] = []
        for root in roots:
            for path in root.rglob("*"):
                if path.is_file() and text_file(path):
                    spawn_evidence.append(path)
                    spawn_texts.append(read_text(path))
        spawn_blob = "\n".join(spawn_texts)
        direct_spawn = bool(re.search(r"\bspawn_agent\b|\"tool_name\"\s*:\s*\"spawn_agent\"", spawn_blob))
        native_agent_mentions = len(set(re.findall(r"(?:native-[0-9a-f-]+|reviewer-(?:api|tests|policy)-[0-9a-f]+)", spawn_blob)))
        self.add("direct_spawn_agent_coverage", "passed" if direct_spawn and native_agent_mentions >= 3 else "failed", "Direct spawn_agent evidence shows at least three native children" if direct_spawn and native_agent_mentions >= 3 else "Direct spawn_agent evidence for three native children is absent; review/start specialists alone do not close the explicit spawn gap", spawn_evidence[:20] or ["<missing>"], "direct_spawn")

    def validate_secret_hygiene(self) -> None:
        roots = [root for root in (self.args.m15_native_dir, self.args.m16_tmux_dir, self.args.loop_dir, self.args.goal_dir, self.args.spawn_dir, self.args.budget_grace_dir) if root and root.exists()]
        leaks = scan_secret_leaks(roots)
        self.add("secret_hygiene", "passed" if not leaks else "failed", "No raw provider keys were found in supplied evidence roots" if not leaks else f"Raw provider-key shaped values found in {len(leaks)} file(s); values are not printed", leaks or roots, "secret_hygiene")

    def write_outputs(self) -> dict[str, Any]:
        self.out_dir.mkdir(parents=True, exist_ok=True)
        artifact_index = {"schema": INDEX_SCHEMA, "generated_at": utc_now(), "artifacts": self.artifacts}
        artifact_index_path = self.out_dir / "m17-live-proof-artifact-index.json"
        artifact_index_path.write_text(json.dumps(artifact_index, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        failures = [check for check in self.checks if check["status"] == "failed"]
        warnings = [check for check in self.checks if check["status"] == "warning"]
        result = {
            "schema": SCHEMA,
            "generated_at": utc_now(),
            "status": "failed" if failures else "passed",
            "close_issue_1023": not failures,
            "inputs": self.inputs,
            "artifact_index": str(artifact_index_path),
            "checks": self.checks,
            "failures": failures,
            "warnings": warnings,
            "notes": [
                "#1023 can close only when this report passes after live execution with real credentials and external services.",
                "Missing direct spawn_agent, child context ledger, or budget grace-call evidence is a closure blocker even if component soaks pass.",
            ],
        }
        report_path = self.out_dir / "m17-live-proof-validation.json"
        report_path.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps(result, indent=2, sort_keys=True))
        return result

    def run(self) -> dict[str, Any]:
        self.validate_m15_native()
        self.validate_m16_tmux()
        self.validate_loop_goal_budget()
        self.validate_child_context_and_spawn()
        self.validate_secret_hygiene()
        return self.write_outputs()


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(row, sort_keys=True) + "\n" for row in rows), encoding="utf-8")


def create_self_test_fixture(root: Path, include_secret: bool) -> argparse.Namespace:
    m15 = root / "m15-native"
    m16 = root / "m16-tmux"
    loop = root / "loop"
    goal = root / "goal"
    spawn = root / "spawn"
    out = root / ("out-secret" if include_secret else "out-clean")
    for path in (m15, m16, loop, goal, spawn, out):
        path.mkdir(parents=True, exist_ok=True)
    queried = [
        {"agentId": "reviewer-api-abc", "backendKind": "native", "status": "completed"},
        {"agentId": "reviewer-tests-abc", "backendKind": "native", "status": "completed"},
        {"agentId": "reviewer-policy-abc", "backendKind": "native", "status": "completed"},
        {"agentId": "reviewer-cli-abc", "backendKind": "cli_process", "status": "completed"},
        {"agentId": "reviewer-mcp-abc", "backendKind": "mcp_agent", "status": "completed"},
    ]
    write_json(m15 / "m15-native-review-start-summary.json", {"ok": True, "providerFamily": "deepseek", "modelId": "deepseek-chat", "queriedAgents": queried})
    write_jsonl(m15 / "client-observed-appui-transcript.jsonl", [{"frame": {"method": "review/start", "params": {"prompt": "review"}}}, {"frame": {"method": "message/delta", "params": {"text": "Code Review Findings"}}}])
    (m15 / "server-stderr.log").write_text("server started\n", encoding="utf-8")
    for name in M17_REQUIRED_TMUX_FILES:
        target = m16 / name
        if name == "m16-ux-soak-validation.json":
            write_json(target, {"status": "passed"})
        elif name.endswith(".json"):
            write_json(target, {"ok": True, "artifacts": queried})
        elif name.endswith(".jsonl"):
            write_jsonl(target, [{"event": "agent_completed", "agent_id": "reviewer-api-abc"}])
        else:
            target.write_text("Ada Lovelace M16_CODE_REVIEW_FINAL_LINE\n", encoding="utf-8")
    write_jsonl(m16 / "child-context-ledger.jsonl", [{"agent_id": "reviewer-api-abc", "context_mode": "managed"}, {"agent_id": "reviewer-tests-abc", "context_mode": "managed"}, {"agent_id": "reviewer-policy-abc", "context_mode": "managed"}])
    write_json(loop / "m15-loop-runtime-stdio-soak-summary.json", {"ok": True, "scheduledFires": 3, "fireNowFires": 1, "secretScanClean": True})
    write_jsonl(loop / "client-observed-appui-transcript.jsonl", [{"frame": {"method": "turn/completed"}}])
    (loop / "server-stderr.log").write_text("budget exhausted; granting one grace call via LoopRetryState\n", encoding="utf-8")
    write_json(goal / "m15-goal-runtime-stdio-soak-summary.json", {"ok": True, "secretScanClean": True, "goalScenarios": [{"name": "budget_exhaust", "transitioned_to": "budget_limited"}, {"name": "sentinel_complete", "transitioned_to": "complete"}], "knownGaps": []})
    write_jsonl(goal / "client-observed-appui-transcript.jsonl", [{"frame": {"method": "session/goal/set"}}])
    (goal / "server-stderr.log").write_text("budget_limited\n", encoding="utf-8")
    write_jsonl(spawn / "task-ledger.jsonl", [{"tool_name": "spawn_agent", "agent_id": "native-11111111-1111-7111-8111-111111111111"}, {"tool_name": "spawn_agent", "agent_id": "native-22222222-2222-7222-8222-222222222222"}, {"tool_name": "spawn_agent", "agent_id": "native-33333333-3333-7333-8333-333333333333"}])
    if include_secret:
        (spawn / "server.log").write_text("synthetic leak sk-test-ABCDEFGHIJKLMNOPQRSTUVWX\n", encoding="utf-8")
    return argparse.Namespace(out_dir=out, m15_native_dir=m15, m16_tmux_dir=m16, loop_dir=loop, goal_dir=goal, spawn_dir=spawn, budget_grace_dir=loop)


def self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="m17-live-proof-validator-") as tmp:
        root = Path(tmp)
        leaked = Validator(create_self_test_fixture(root / "leaked", include_secret=True)).run()
        if leaked["status"] != "failed" or not any(check["id"] == "secret_hygiene" for check in leaked["failures"]):
            print("self-test failed: synthetic secret leak was not detected", file=sys.stderr)
            return 1
        clean = Validator(create_self_test_fixture(root / "clean", include_secret=False)).run()
        if clean["status"] != "passed":
            print("self-test failed: clean synthetic fixture did not pass", file=sys.stderr)
            return 1
    print("M17 live proof validator self-test passed")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, default=Path("e2e/test-results-m17-live-proof/manual"))
    parser.add_argument("--m15-native-dir", type=Path)
    parser.add_argument("--m16-tmux-dir", type=Path)
    parser.add_argument("--loop-dir", type=Path)
    parser.add_argument("--goal-dir", type=Path)
    parser.add_argument("--spawn-dir", type=Path)
    parser.add_argument("--budget-grace-dir", type=Path)
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        return self_test()
    result = Validator(args).run()
    return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
