#!/usr/bin/env python3
"""Checks one run of the action against the scripted mock model.

The workflow runs the action with `dry_run`, `access: write` and the script in
`e2e_script.json`, then calls this with the mock's request log. Step outputs
and the job's token arrive through the environment.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys


def main(log_path):
    with open(log_path, encoding="utf-8") as handle:
        blob = handle.read()
    requests = [json.loads(line)["request"] for line in blob.splitlines() if line.strip()]
    assert len(requests) >= 5, "the agent made %d model requests, expected 5" % len(requests)
    results = [m["content"] for m in requests[-1]["messages"] if m.get("role") == "tool"]
    results = [r if isinstance(r, str) else json.dumps(r) for r in results]
    shell, probes, fetch = results[0], results[1:-1], results[-1]
    print("shell result:\n" + shell)
    # Printed for the reader of the job log, not checked: they differ by sandbox.
    for probe in probes:
        print("probe result:\n" + probe[:300])
    print("web_fetch result:\n" + fetch[:300])

    # What the agent's shell could see and reach.
    for fact in ("model_key=unset", "github_token=unset", "outside=blocked", "network=blocked"):
        assert fact in shell, "sandbox check failed, expected `%s`" % fact
    # The web tool is not on the allow list, so no page came back.
    assert "Example Domain" not in fetch, "web_fetch reached the network"
    # Neither the job's token nor the model key was sent to the model.
    token = os.environ.get("JOB_TOKEN") or ""
    assert token, "JOB_TOKEN is not set"
    assert token not in blob, "the GitHub token reached the model"
    assert "mock-key-0123456789" not in blob, "the model key reached the model"

    def git(*args):
        return subprocess.run(("git",) + args, check=True, stdout=subprocess.PIPE, text=True).stdout

    # The action committed the agent's file, and only that.
    shown = git("show", "--name-only", "--format=%an|%s", "HEAD")
    print("commit:\n" + shown)
    assert shown.startswith("github-actions[bot]|chore: Octos changes"), shown
    files = [line for line in shown.splitlines()[1:] if line.strip()]
    assert files == ["E2E_FIXED.txt"], files
    assert git("rev-parse", "HEAD").strip() == os.environ.get("COMMIT_SHA"), "commit_sha output"
    assert not os.path.exists(".github/workflows/e2e-sneak.yml"), "the workflow file was kept"
    temp = os.environ["RUNNER_TEMP"]
    assert not os.path.exists(os.path.join(temp, "hook-ran")), "a hook the agent wrote ran"
    assert not os.path.exists(os.path.join(temp, "escape")), "the agent wrote outside the checkout"

    expected = {
        "SHOULD_RUN": "true",
        "KIND": "prompt",
        "CONCLUSION": "success",
        "RESULT": "E2E answer: wrote E2E_FIXED.txt. cc @octocat",
        "PUSHED": "false",  # dry run
    }
    for name, value in expected.items():
        assert os.environ.get(name) == value, "%s is %r, expected %r" % (name, os.environ.get(name), value)
    print("e2e ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
