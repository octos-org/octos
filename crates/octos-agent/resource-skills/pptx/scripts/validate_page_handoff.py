#!/usr/bin/env python3
"""Read-only identity/hash checks for independent page handoffs, not visual QA."""
import argparse
import hashlib
import json
from pathlib import Path


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def load(path):
    value = json.loads(path.read_text(encoding="utf-8"))
    require(isinstance(value, dict), f"Expected a JSON object: {path}")
    return value


def bound_file(base, item, confined=False):
    relative = Path(item["path"])
    require(not relative.is_absolute(), "Artifact/input paths must be relative")
    path = (base / relative).resolve(strict=True)
    if confined:
        require(path.is_relative_to(base), f"Artifact escapes assigned output directory: {relative}")
    require(path.is_file(), f"Missing regular file: {relative}")
    require(sha256(path) == item["sha256"], f"Hash mismatch: {relative}")
    return path


def validate_pair(job_path, result_path):
    job_path = Path(job_path).resolve(strict=True)
    result_path = Path(result_path).resolve(strict=True)
    job, result = load(job_path), load(result_path)
    require(job["schema_version"] == result["schema_version"] == 1, "Unsupported schema version")
    for key in ("page_id", "attempt_id"):
        require(isinstance(job[key], str) and bool(job[key]), f"Invalid {key}")
        require(result[key] == job[key], f"Wrong {key} in handoff")
    require(result["job_sha256"] == sha256(job_path), "Handoff belongs to a different job revision")
    require(result["status"] == "complete", "Page is not complete")
    contract_path = bound_file(job_path.parent, job["shared_contract"])
    contract = load(contract_path)
    order = contract["page_order"]
    require(isinstance(order, list) and order and all(isinstance(p, str) and p for p in order), "Invalid page_order")
    require(len(order) == len(set(order)), "Duplicate IDs in page_order")
    require(job["page_id"] in order, "Page is not assigned in the shared contract")
    require(isinstance(job["inputs"], list) and job["inputs"], "No page inputs declared")
    roles = set()
    for item in job["inputs"]:
        require(isinstance(item["role"], str) and item["role"], "Input role is missing")
        roles.add(item["role"])
        bound_file(job_path.parent, item)
    require("canonical-copy" in roles, "Canonical copy input is missing")
    output_relative = Path(job["output_dir"])
    require(not output_relative.is_absolute(), "output_dir must be relative to the job")
    output = (job_path.parent / output_relative).resolve(strict=True)
    require(output.is_dir(), "output_dir must exist")
    require(result_path.is_relative_to(output), "Result is outside its assigned output directory")
    artifacts = result["artifacts"]
    required = {"source", "editable_pptx", "blank_pptx", "render", "blank_render", "qa"}
    require(isinstance(artifacts, dict) and required <= artifacts.keys(), "Incomplete artifact handoff")
    files = {key: bound_file(output, item, confined=True) for key, item in artifacts.items()}
    require(len(set(files.values())) == len(files), "Artifact roles must not alias the same file")
    qa = load(files["qa"])
    for field, key in (("pptx_sha256", "editable_pptx"), ("blank_pptx_sha256", "blank_pptx"),
                       ("render_sha256", "render"), ("blank_render_sha256", "blank_render")):
        require(qa[field] == artifacts[key]["sha256"], f"Stale QA binding: {field}")
    require(qa["open_findings"] == [], "Unresolved page findings")
    for key in ("source", "editable_render", "blank_render"):
        require(qa["inspection"][key] is True, f"Inspection not recorded: {key}")
    return {"page_id": job["page_id"], "attempt_id": job["attempt_id"],
            "result": str(result_path), "contract_sha256": job["shared_contract"]["sha256"],
            "page_order": order, "output_dir": str(output), "files": {k: str(v) for k, v in files.items()}}


def validate_many(pairs, complete_deck=False):
    pages = [validate_pair(*pair) for pair in pairs]
    require(bool(pages), "No handoffs supplied")
    ids = [page["page_id"] for page in pages]
    require(len(ids) == len(set(ids)), "Duplicate page handoffs")
    require(len({page["contract_sha256"] for page in pages}) == 1, "Mixed shared contract versions")
    roots = [Path(page["output_dir"]) for page in pages]
    for index, root in enumerate(roots):
        require(all(not (root.is_relative_to(other) or other.is_relative_to(root))
                    for other in roots[index + 1:]), "Worker output directories overlap")
    order = pages[0]["page_order"]
    if complete_deck:
        require(set(ids) == set(order), "Missing page handoffs for complete-deck collection")
    by_id = {page["page_id"]: page for page in pages}
    return {"handoff_consistent": True, "visual_quality_verified": False,
            "pages": [by_id[page_id] for page_id in order if page_id in by_id]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pair", nargs=2, action="append", required=True, metavar=("JOB", "RESULT"))
    parser.add_argument("--complete-deck", action="store_true")
    args = parser.parse_args()
    try:
        report = validate_many(args.pair, args.complete_deck)
    except (ValueError, KeyError, TypeError, OSError) as error:
        print(json.dumps({"handoff_consistent": False, "error": str(error)}))
        return 1
    print(json.dumps(report, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
