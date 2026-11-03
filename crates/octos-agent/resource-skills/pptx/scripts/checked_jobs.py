#!/usr/bin/env python3
"""Job receipts are not completion proofs. No credentials or layout code here."""
from pathlib import Path
import argparse
import hashlib
import json
import re
import struct
import time
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
import zipfile

PENDING = {"queued", "running", "pending"}
FAILED = {"failed", "cancelled", "canceled", "interrupted"}


class JobError(RuntimeError):
    exit_code = 1

    def __init__(self, message, receipt=None):
        super().__init__(message)
        self.receipt = receipt


class JobPending(JobError):
    exit_code = 2


def validate_receipt(receipt, expected_id=None):
    if not isinstance(receipt, dict) or not isinstance(receipt.get("id"), str):
        raise JobError("Invalid job receipt: missing id")
    if expected_id is not None and receipt["id"] != expected_id:
        raise JobError("Job receipt id does not match the requested job", receipt)
    if receipt.get("status") not in PENDING | FAILED | {"completed"}:
        raise JobError("Invalid or unknown job status", receipt)
    return receipt


def require_completed(receipt):
    validate_receipt(receipt)
    if receipt["status"] in FAILED:
        raise JobError(f"Job {receipt['id']} {receipt['status']}: {receipt.get('error', '')}", receipt)
    if receipt["status"] in PENDING:
        raise JobPending(f"Job {receipt['id']} is still pending; inspect or wait for this same id", receipt)
    return receipt


class JobClient:
    def __init__(self, url, transport=None):
        self.url = url.rstrip("/")
        self.transport = transport

    def call(self, path, data=None):
        if self.transport is not None:
            return self.transport(path, data)
        request = urllib.request.Request(
            self.url + path,
            data=json.dumps(data).encode() if data is not None else None,
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise JobError(f"Service HTTP {error.code}: {error.read().decode(errors='replace')[:2000]}") from error
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
            # An ambiguous submission is NOT automatically resubmitted with
            # a fresh id; callers can inspect the original requested id.
            raise JobError(f"Service request failed: {error}") from error

    def submit(self, request):
        receipt = validate_receipt(self.call("/jobs", request), request.get("id"))
        if receipt["status"] in FAILED:
            require_completed(receipt)
        return receipt  # Successful submission may be queued, not completed.

    def status(self, job_id):
        return validate_receipt(self.call("/jobs/" + urllib.parse.quote(job_id, safe="")), job_id)

    def wait_completed(self, job_id, seconds=40):
        deadline = time.monotonic() + min(max(seconds, 0), 45)
        while True:
            receipt = self.status(job_id)
            if receipt["status"] not in PENDING or time.monotonic() >= deadline:
                return require_completed(receipt)
            time.sleep(min(2, max(0, deadline - time.monotonic())))


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def confined(root, value):
    path = (root / value).resolve()
    if not path.is_relative_to(root):
        raise JobError("Path must remain in the workspace")
    return path


def pptx_page_count(source):
    """Count ordered presentation slides, not possibly unreferenced ZIP parts."""
    try:
        with zipfile.ZipFile(source) as package:
            presentation = ET.fromstring(package.read("ppt/presentation.xml"))
        count = len(presentation.findall(
            "{http://schemas.openxmlformats.org/presentationml/2006/main}sldIdLst/"
            "{http://schemas.openxmlformats.org/presentationml/2006/main}sldId"))
    except (zipfile.BadZipFile, KeyError, ET.ParseError) as error:
        raise JobError("Cannot establish the source PPTX page count") from error
    if count < 1:
        raise JobError("Source PPTX has no ordered slides")
    return count


def png_dimensions(path):
    with path.open("rb") as stream:
        header = stream.read(24)
    if len(header) != 24 or header[:16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR":
        raise JobError("Rendered page is not a PNG with a valid dimension header")
    return list(struct.unpack(">II", header[16:24]))


def checked_render(client, workspace, pptx, output_base, attempt, size="3840x2160", seconds=40):
    """Render a candidate into its own namespace; return only verified output.

    Reusing the same attempt label with changed source bytes creates a NEW
    output directory. Retrying an unchanged candidate resumes the same job.
    Neither failed nor pending receipts authorize reading prior artifacts.
    """
    root = Path(workspace).resolve()
    source = confined(root, pptx)
    base = confined(root, output_base)
    if source.suffix.lower() != ".pptx" or not source.is_file():
        raise JobError("A readable .pptx source is required")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", attempt):
        raise JobError("Attempt must use 1–64 letters, digits, underscores or hyphens")
    if not isinstance(size, str) or not re.fullmatch(r"[1-9][0-9]*x[1-9][0-9]*", size):
        raise JobError("Size must specify positive WIDTHxHEIGHT pixels")
    dimensions = list(map(int, size.split("x")))
    source_hash = sha256(source)
    expected_pages = list(range(1, pptx_page_count(source) + 1))
    identity = dict(pptx=str(source.relative_to(root)), input_sha256=source_hash, size=size)
    fingerprint = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    output = base / f"{attempt}-{fingerprint[:24]}"
    request = dict(op="render", **identity, out=str(output.relative_to(root)))
    request["id"] = "render-" + hashlib.sha256(json.dumps(request, sort_keys=True).encode()).hexdigest()[:32]
    receipt = client.submit(request)
    if receipt["status"] != "completed":
        receipt = client.wait_completed(request["id"], seconds)
    require_completed(receipt)
    result = receipt.get("result", {})
    if not isinstance(result, dict) or not isinstance(result.get("report"), dict):
        raise JobError("Completed render has no structured report", receipt)
    report = result.get("report", {})
    if (result.get("out") != request["out"] or
            result.get("snapshot_sha256") != source_hash or
            report.get("input_sha256") != source_hash or sha256(source) != source_hash):
        raise JobError("Completed render does not bind to the current source and output directory", receipt)
    images = report.get("page_images")
    if not isinstance(images, list) or not images:
        raise JobError("Completed render has no page-image evidence", receipt)
    if (type(report.get("pages")) is not int or report["pages"] != len(expected_pages) or
            report.get("rendered_pages") != expected_pages or
            any(type(page) is not int for page in report["rendered_pages"]) or
            len(images) != len(expected_pages)):
        raise JobError("Completed render does not cover every source page", receipt)
    pixels = report.get("export_pixels")
    if pixels != dimensions or any(type(pixel) is not int for pixel in pixels):
        raise JobError("Render report dimensions do not match the requested size", receipt)
    seen_pages, seen_paths = set(), set()
    for item in images:
        if not isinstance(item, dict) or not isinstance(item.get("path"), str):
            raise JobError("Invalid page-image evidence", receipt)
        page = item.get("page")
        if type(page) is not int or page not in expected_pages or page in seen_pages:
            raise JobError("Page-image evidence has missing, duplicate or invalid page IDs", receipt)
        seen_pages.add(page)
        image = (output / item["path"]).resolve()
        if (not image.is_relative_to(output) or not image.is_file() or
                sha256(image) != item.get("sha256")):
            raise JobError("Rendered page evidence is missing, changed or outside its output directory", receipt)
        if image in seen_paths:
            raise JobError("Multiple pages reference the same rendered image path", receipt)
        seen_paths.add(image)
        if png_dimensions(image) != dimensions:
            raise JobError("Actual rendered image dimensions do not match the requested size", receipt)
    return {**receipt, "verified_input_sha256": source_hash}


def emit(result):
    print(json.dumps(result, ensure_ascii=False, indent=2))
    if result.get("octos_external_job"):
        # Preserve the runtime's wake-up marker even on pending/failure exits.
        print(json.dumps({"octos_external_job": result["octos_external_job"]}, ensure_ascii=False))


def main(argv=None, workspace=None, client=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", type=Path, help="Workspace containing tools/service.json; required when running from a skill directory")
    sub = parser.add_subparsers(dest="command", required=True)
    submit = sub.add_parser("submit"); submit.add_argument("request", type=Path)
    status = sub.add_parser("status"); status.add_argument("id")
    wait = sub.add_parser("wait"); wait.add_argument("id"); wait.add_argument("--seconds", type=float, default=40)
    sub.add_parser("health")
    render = sub.add_parser("render-checked")
    render.add_argument("pptx"); render.add_argument("--output-base", required=True)
    render.add_argument("--attempt", required=True); render.add_argument("--size", default="3840x2160")
    render.add_argument("--seconds", type=float, default=40)
    args = parser.parse_args(argv)
    try:
        root = Path(args.workspace or workspace or Path(__file__).resolve().parent.parent).resolve()
        if client is None:
            client = JobClient(json.loads((root / "tools/service.json").read_text())["url"])
        if args.command == "health":
            result = client.call("/health")
        elif args.command == "submit":
            result = client.submit(json.loads(args.request.read_text()))
        elif args.command == "status":
            result = require_completed(client.status(args.id))
        elif args.command == "wait":
            result = client.wait_completed(args.id, args.seconds)
        else:
            result = checked_render(client, root, args.pptx, args.output_base, args.attempt, args.size, args.seconds)
        emit(result)
        return 0
    except JobError as error:
        receipt = error.receipt
        if receipt and receipt.get("status") in PENDING | FAILED:
            emit({**receipt, "client_error": str(error)})
        else:
            # A server's completed status must not hide local hash/path
            # validation failure from the next stage or the producing model.
            failure = {"status": "client_error", "error": str(error)}
            if receipt:
                failure["receipt"] = receipt
                if receipt.get("octos_external_job"):
                    failure["octos_external_job"] = receipt["octos_external_job"]
            emit(failure)
        return error.exit_code
    except (OSError, ValueError, KeyError, TypeError) as error:
        emit({"status": "client_error", "error": str(error)})
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
