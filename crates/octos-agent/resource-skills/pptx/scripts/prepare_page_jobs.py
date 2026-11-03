#!/usr/bin/env python3
"""Prepare immutable page assignments and skill references without generating layouts."""
import argparse
import json
import os
import re
from pathlib import Path

from validate_page_handoff import load, require, sha256


def reference(path, base, role=None):
    path = path.resolve(strict=True)
    require(path.is_file(), f"Not a regular input file: {path}")
    item = {"path": os.path.relpath(path, base), "sha256": sha256(path)}
    if role:
        item["role"] = role
    return item


def prepare(plan_path, output, skill_path, tools_path, workspace=None):
    plan_path = Path(plan_path).resolve(strict=True)
    base = plan_path.parent
    plan = load(plan_path)
    contract = (base / plan["shared_contract"]).resolve(strict=True)
    order = load(contract)["page_order"]
    require(isinstance(order, list) and bool(order), "Empty page order")
    require(all(isinstance(v, str) and re.fullmatch(r"[A-Za-z0-9_-]+", v) for v in order),
            "Page IDs must be safe directory names")
    require(len(order) == len(set(order)), "Duplicate page IDs")
    pages = plan["pages"]
    require(isinstance(pages, list), "pages must be a list")
    ids = [p["page_id"] for p in pages]
    require(len(ids) == len(set(ids)) and set(ids) == set(order), "Plan differs from contract page order")
    skill = Path(skill_path).resolve(strict=True)
    independent = skill.parent / "references/page-worker.md"
    if not independent.is_file():
        independent = skill.parent / "references/independent-pages.md"
    tools = Path(tools_path).resolve(strict=True)
    instructions = [skill, independent, tools]
    method_names = (['native-builder.md', 'fit-and-revise.md'] if independent.name == 'page-worker.md'
                    else ['page-method.md', 'native-builder.md'])
    for name in method_names:
        method = skill.parent / 'references' / name
        if method.is_file():
            instructions.append(method)
    for path in instructions:
        require(path.is_file(), f"Missing worker instruction: {path}")
    # Preflight every input before creating any packet.
    for page in pages:
        require(isinstance(page["inputs"], list), "inputs must be a list")
        roles = {v["role"] for v in page["inputs"]}
        require({"canonical-copy", "design", "blank-art"} <= roles, "Missing page copy/design/blank input")
        for item in page["inputs"]:
            require((base / item["path"]).resolve(strict=True).is_file(), "Input is not a file")
    output = Path(output).resolve()
    workspace = Path(workspace or base).resolve(strict=True)
    require(workspace.is_dir() and output.is_relative_to(workspace) and output != workspace,
            "Page outputs must be inside the spawn workspace; pass --workspace for the parent agent's actual workspace")
    require(not output.exists(), "Refusing to replace existing page assignments")
    output.mkdir(parents=True)
    by_id = {p["page_id"]: p for p in pages}
    jobs = []
    for page_id in order:
        page = by_id[page_id]
        folder = output / page_id
        folder.mkdir()
        (folder / "out").mkdir()
        job = {"schema_version": 1, "page_id": page_id,
               "attempt_id": page.get("attempt_id", "initial"),
               "shared_contract": reference(contract, folder),
               "inputs": [reference(base / item["path"], folder, item["role"]) for item in page["inputs"]],
               "output_dir": "out"}
        job["inputs"] += [reference(path, folder, "worker-instruction") for path in instructions]
        job_path = folder / "job.json"
        job_path.write_text(json.dumps(job, ensure_ascii=False, indent=2) + "\n")
        task = f'''# Independent page assignment: {page_id}

Read `{job_path}`; its paths resolve relative to that file. Read the supplied
worker instructions, especially `{independent.name}` and the tool guide. Use the
canonical copy, design pixels and actual blank pixels to determine this page's
content, contrast, labels and ownership.

Write only under `{folder / 'out'}`. Shared helpers/inputs, the contract, siblings
and root exports are read-only. Reuse the supplied builder, font proposals and
inspection tools. Do not spawn nested workers or recreate the production framework.
Additional inputs may include machine-derived candidates: verify them against the authoritative copy,
design and blank pixels; their role does not establish correctness.

Batch related object edits. Resume pending renders; report reproducible helper
failures rather than inventing successful evidence. No iteration/render quota or
extra typography target is imposed. Respect the actual user requirements and budget.

Use native_cycle.py handoff to publish `{folder / 'out/result.json'}` after your
own source/full/blank visual inspection. For later revisions use native_cycle.py
revise, inspect the returned comparisons, and publish the new job/result paths.
Accepted predecessors stay unchanged. Report unresolved defects and actual evidence;
a completion message alone is not a deliverable.
'''
        (folder / "worker.md").write_text(task)
        request = {"label": page_id, "task": f"Read {folder / 'worker.md'}. Apply the supplied shared reconstruction method to this page's data, inspect and repair actual defects, and publish its verified handoff.",
                   "mode": "background", "isolation": "shared", "max_iterations": 0,
                   "owned_paths": [str((folder / "out").relative_to(workspace))],
                   "allowed_tools": ["read_file", "write_file", "edit_file", "bash"],
                   "skill_paths": [str(p) for p in instructions]}
        jobs.append({"page_id": page_id, "job": str(job_path),
                     "input_roles": [item["role"] for item in job["inputs"]],
                     "result": str(folder / "out/result.json"), "spawn_request": request})
    receipt = {"page_order": order, "jobs": jobs, "helper_generates_layouts": False,
               "spawn_workspace": str(workspace),
               "iteration_limit": 0, "visual_quality_verified": False}
    (output / "prepared-jobs.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
    return receipt


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--plan", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--skill", required=True)
    p.add_argument("--tools", required=True)
    p.add_argument("--workspace", help="Parent native-spawn workspace; defaults to the plan directory")
    a = p.parse_args()
    try:
        r = prepare(a.plan, a.out, a.skill, a.tools, workspace=a.workspace)
    except (ValueError, KeyError, TypeError, OSError) as e:
        print(json.dumps({"prepared": False, "error": str(e)})); return 1
    print(json.dumps({"prepared": True, "pages": len(r["jobs"]),
                      "receipt": str(Path(a.out).resolve() / "prepared-jobs.json")})); return 0


if __name__ == "__main__":
    raise SystemExit(main())
