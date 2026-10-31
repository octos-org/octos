"""Service-side receipt metadata and bounded operation evidence for Octos.

Call at the tool boundary. No agent-authored helper plan is required. The
reports describe execution only, never visual/content acceptance.
"""
import hashlib
import json
from pathlib import Path
import re
import tempfile


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            result.update(chunk)
    return result.hexdigest()


def confined(workspace, relative):
    p = Path(relative)
    if p.is_absolute() or ".." in p.parts:
        raise ValueError("Expected a workspace-relative path")
    current = workspace
    for part in p.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("Symlink outside operation evidence contract")
    return current


def attach_job_metadata(job, receipt_path):
    job["octos_external_job"] = {
        "schema_version": 1, "receipt_path": receipt_path,
        "job_id": job["id"], "request_sha256": job["request_sha256"],
        "status": job["status"],
    }
    return job


def publish_operation(workspace, job, receipt_path):
    """Publish one latest execution observation per operation type.

    Receipts retain individual jobs/failures; these bounded activity reports
    only allow the kernel to avoid an unnecessary periodic reflection.
    Repeating an operation on the same inputs does not advance activity.
    """
    workspace = Path(workspace)
    op = job["request"]["op"]
    if not re.fullmatch(r"[a-z][a-z0-9_-]{0,40}", op):
        raise ValueError("Invalid operation name")
    result = job.get("result") or {}
    inputs = []
    if op == "render" and result.get("report", {}).get("input_sha256"):
        inputs.append((job["request"]["pptx"], result["report"]["input_sha256"]))
    elif op in ("image", "ocr") and result.get("image"):
        inputs.append((result["image"]["path"], result["image"]["sha256"]))
    elif op == "vision":
        inputs.extend((image["path"], image["sha256"]) for image in result.get("images", []))
    # An unsuccessful operation must remain observable even with no result.
    # Its durable receipt is not mistaken for a verified content check.
    if not inputs:
        inputs.append((receipt_path, digest(confined(workspace, receipt_path))))
    artifacts = [{"path": path, "sha256": sha, "role": "input"} for path, sha in inputs]
    if receipt_path not in {path for path, _ in inputs}:
        artifacts.append({"path": receipt_path, "sha256": digest(confined(workspace, receipt_path)),
                          "role": "output"})
    status = "pass" if job["status"] == "completed" else "fail"
    report = {
        "schema_version": 1, "check_id": "tool-operation/" + op, "kind": "operation",
        "status": status, "artifacts": artifacts, "findings": [],
        "scope": "Latest tool execution only; not content, visual quality or delivery acceptance",
        "job_id": job["id"], "request_sha256": job["request_sha256"],
    }
    folder = confined(workspace, ".octos/tool-progress")
    folder.mkdir(parents=True, exist_ok=True)
    target = folder / (op + ".json")
    with tempfile.NamedTemporaryFile(mode="w", dir=folder, suffix=".tmp", delete=False) as stream:
        json.dump(report, stream, ensure_ascii=False)
        temporary = Path(stream.name)
    temporary.replace(target)
    return report
