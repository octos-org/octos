import contextlib
import hashlib
import http.server
import io
import json
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import checked_jobs as tools


def write_pptx(path, text="candidate", pages=1):
    # Minimal package fixture: production validation below uses real python-pptx.
    with zipfile.ZipFile(path, "w") as package:
        slides = "".join(f'<p:sldId id="{256 + i}"/>' for i in range(pages))
        package.writestr("ppt/presentation.xml", '<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:sldIdLst>' + slides + '</p:sldIdLst></p:presentation>')
        package.writestr("fixture.txt", text)


def image_header(dimensions):
    return b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + struct.pack(">II", *dimensions)


class FakeService:
    def __init__(self, root, status="completed"):
        self.root = root
        self.state = status
        self.jobs = {}
        self.requests = []
        self.after_result = None

    def __call__(self, path, data=None):
        if path == "/jobs":
            self.requests.append(data.copy())
            if data["id"] not in self.jobs:
                receipt = {"id": data["id"], "status": self.state,
                           "octos_external_job": {"job_id": data["id"], "status": self.state}}
                if self.state == "completed":
                    out = self.root / data["out"]
                    pages = tools.pptx_page_count(self.root / data["pptx"])
                    dimensions = list(map(int, data["size"].split("x")))
                    images = []
                    for page in range(1, pages + 1):
                        image = out / f"png/page-{page}.png"
                        image.parent.mkdir(parents=True, exist_ok=True)
                        image.write_bytes(image_header(dimensions) + (self.root / data["pptx"]).read_bytes())
                        images.append({"page": page, "path": f"png/page-{page}.png", "sha256": tools.sha256(image)})
                    receipt["result"] = {
                        "out": data["out"], "snapshot_sha256": data["input_sha256"],
                        "report": {"input_sha256": data["input_sha256"], "page_images": images,
                                   "pages": pages, "rendered_pages": list(range(1, pages + 1)),
                                   "export_pixels": dimensions},
                    }
                elif self.state == "failed":
                    receipt["error"] = "synthetic render failure"
                self.jobs[data["id"]] = receipt
            receipt = self.jobs[data["id"]]
        else:
            receipt = self.jobs[path.rsplit("/", 1)[-1]]
        if self.after_result:
            self.after_result(receipt)
        return receipt


class CheckedJobsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "candidate.pptx"
        write_pptx(self.source, "candidate one")
        self.service = FakeService(self.root)
        self.client = tools.JobClient("http://unused", self.service)

    def render(self, attempt="revision", seconds=0):
        return tools.checked_render(self.client, self.root, "candidate.pptx", "proofs", attempt, seconds=seconds)

    def test_two_revisions_get_distinct_outputs_even_with_same_attempt_label(self):
        first = self.render()
        original_image = self.root / first["result"]["out"] / "png/page-1.png"
        first_bytes = original_image.read_bytes()
        write_pptx(self.source, "candidate two")
        second = self.render()
        self.assertNotEqual(first["id"], second["id"])
        self.assertNotEqual(first["result"]["out"], second["result"]["out"])
        self.assertEqual(original_image.read_bytes(), first_bytes)
        self.assertEqual(second["verified_input_sha256"], tools.sha256(self.source))

    def test_same_candidate_resumes_same_job_without_duplicate_execution(self):
        first, second = self.render(), self.render()
        self.assertEqual(first["id"], second["id"])
        self.assertEqual(len(self.service.jobs), 1)

    def test_failed_render_stops_before_any_old_artifact_is_consumed(self):
        self.render()
        write_pptx(self.source, "changed candidate")
        self.service.state = "failed"
        with self.assertRaisesRegex(tools.JobError, "synthetic render failure"):
            self.render()

    def test_pending_is_not_a_completion_proof(self):
        self.service.state = "running"
        with self.assertRaises(tools.JobPending):
            self.render()
        self.assertFalse((self.root / "proofs").exists())

    def test_source_change_during_render_rejects_completed_receipt(self):
        self.service.after_result = lambda _: self.source.write_bytes(b"changed while running")
        with self.assertRaisesRegex(tools.JobError, "current source"):
            self.render()

    def test_changed_output_is_not_reused_as_verified_evidence(self):
        receipt = self.render()
        (self.root / receipt["result"]["out"] / "png/page-1.png").write_bytes(b"changed")
        with self.assertRaisesRegex(tools.JobError, "changed or outside"):
            self.render()

    def test_mismatched_snapshot_is_rejected(self):
        def wrong_hash(receipt):
            receipt["result"]["snapshot_sha256"] = "0" * 64
        self.service.after_result = wrong_hash
        with self.assertRaisesRegex(tools.JobError, "current source"):
            self.render()

    def test_invalid_attempt_and_escape_fail_before_submission(self):
        with self.assertRaises(tools.JobError):
            self.render(attempt="../escape")
        with self.assertRaises(tools.JobError):
            tools.checked_render(self.client, self.root, "candidate.pptx", "../escape", "a")
        self.assertEqual(self.service.requests, [])

    def test_unknown_status_never_succeeds(self):
        for status in [None, "almost_done", "success"]:
            with self.assertRaises(tools.JobError):
                tools.require_completed({"id": "j", "status": status})

    def test_cli_local_validation_error_does_not_print_top_level_completed(self):
        self.service.after_result = lambda _: self.source.write_bytes(b"changed")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = tools.main(["render-checked", "candidate.pptx", "--output-base", "proofs", "--attempt", "a"],
                              self.root, self.client)
        receipt, _ = json.JSONDecoder().raw_decode(output.getvalue())
        self.assertEqual(code, 1)
        self.assertEqual(receipt["status"], "client_error")
        self.assertIn("octos_external_job", output.getvalue())

    def test_submit_acceptance_does_not_claim_completion(self):
        self.service.state = "queued"
        receipt = self.client.submit({"id": "j", "op": "ocr", "image": "a.png"})
        self.assertEqual(receipt["status"], "queued")
        with self.assertRaises(tools.JobPending):
            self.client.wait_completed("j", 0)

    def test_full_render_checks_the_entire_ordered_presentation(self):
        write_pptx(self.source, pages=3)
        receipt = self.render()
        self.assertEqual(len(receipt["result"]["report"]["page_images"]), 3)

    def test_partial_page_images_cannot_claim_full_render(self):
        write_pptx(self.source, pages=2)
        self.service.after_result = lambda r: r["result"]["report"]["page_images"].pop()
        with self.assertRaisesRegex(tools.JobError, "every source page"):
            self.render()

    def test_incorrect_report_page_count_is_rejected(self):
        self.service.after_result = lambda r: r["result"]["report"].update(pages=2)
        with self.assertRaisesRegex(tools.JobError, "every source page"):
            self.render()

    def test_partial_rendered_page_ids_are_rejected(self):
        write_pptx(self.source, pages=2)
        self.service.after_result = lambda r: r["result"]["report"].update(rendered_pages=[1])
        with self.assertRaisesRegex(tools.JobError, "every source page"):
            self.render()

    def test_duplicate_page_ids_are_rejected(self):
        write_pptx(self.source, pages=2)
        self.service.after_result = lambda r: r["result"]["report"]["page_images"][1].update(page=1)
        with self.assertRaisesRegex(tools.JobError, "duplicate or invalid page IDs"):
            self.render()

    def test_same_image_path_cannot_cover_multiple_pages(self):
        write_pptx(self.source, pages=2)
        def duplicate_path(r):
            images = r["result"]["report"]["page_images"]
            images[1].update(path=images[0]["path"], sha256=images[0]["sha256"])
        self.service.after_result = duplicate_path
        with self.assertRaisesRegex(tools.JobError, "same rendered image path"):
            self.render()

    def test_wrong_report_dimensions_are_rejected(self):
        self.service.after_result = lambda r: r["result"]["report"].update(export_pixels=[1920, 1080])
        with self.assertRaisesRegex(tools.JobError, "report dimensions"):
            self.render()

    def test_wrong_actual_dimensions_are_rejected_even_with_matching_hash(self):
        def wrong_size(r):
            item = r["result"]["report"]["page_images"][0]
            path = self.root / r["result"]["out"] / item["path"]
            path.write_bytes(image_header([1920, 1080]) + b"synthetic render")
            item["sha256"] = tools.sha256(path)
        self.service.after_result = wrong_size
        with self.assertRaisesRegex(tools.JobError, "Actual rendered image dimensions"):
            self.render()

    def test_empty_source_is_rejected_before_submission(self):
        write_pptx(self.source, pages=0)
        with self.assertRaisesRegex(tools.JobError, "no ordered slides"):
            self.render()
        self.assertEqual(self.service.requests, [])

    def test_invalid_size_is_rejected_before_submission(self):
        with self.assertRaisesRegex(tools.JobError, "positive WIDTHxHEIGHT"):
            tools.checked_render(self.client, self.root, "candidate.pptx", "proofs", "a", size="0x10")
        self.assertEqual(self.service.requests, [])


class ReceiptHandler(http.server.BaseHTTPRequestHandler):
    state = "failed"

    def log_message(self, *_):
        pass

    def do_GET(self):
        receipt = {"id": "job", "status": type(self).state,
                   "octos_external_job": {"job_id": "job", "status": type(self).state}}
        raw = json.dumps(receipt).encode()
        self.send_response(200); self.end_headers(); self.wfile.write(raw)


def exercise_cli(script):
    """Real subprocess exit codes against a synthetic localhost service."""
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        (root / "tools").mkdir()
        shutil.copyfile(script, root / "tools/pptx_tools.py")
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ReceiptHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        (root / "tools/service.json").write_text(json.dumps({"url": f"http://127.0.0.1:{server.server_port}"}))
        results = {}
        try:
            for state in ("failed", "running", "completed"):
                ReceiptHandler.state = state
                run = subprocess.run([sys.executable, str(root / "tools/pptx_tools.py"), "wait", "job", "--seconds", "0"],
                                     capture_output=True, text=True, timeout=10)
                receipt, _ = json.JSONDecoder().raw_decode(run.stdout)
                results[state] = {"exit_code": run.returncode, "status": receipt["status"],
                                  "wake_marker_preserved": '"octos_external_job"' in run.stdout}
        finally:
            server.shutdown(); server.server_close(); thread.join(timeout=2)
        return results


class CliTests(unittest.TestCase):
    def test_real_cli_failed_pending_completed_exit_codes(self):
        results = exercise_cli(Path(tools.__file__))
        self.assertEqual({state: result["exit_code"] for state, result in results.items()},
                         {"failed": 1, "running": 2, "completed": 0})
        self.assertTrue(all(result["wake_marker_preserved"] for result in results.values()))

    def test_workspace_option_runs_script_from_unrelated_skill_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            workspace = root / "workspace"
            (workspace / "tools").mkdir(parents=True)
            script = root / "skill/scripts/checked_jobs.py"
            script.parent.mkdir(parents=True)
            shutil.copyfile(tools.__file__, script)
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), ReceiptHandler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                ReceiptHandler.state = "completed"
                (workspace / "tools/service.json").write_text(json.dumps({"url": f"http://127.0.0.1:{server.server_port}"}))
                run = subprocess.run([sys.executable, str(script), "--workspace", str(workspace), "wait", "job", "--seconds", "0"],
                                     capture_output=True, text=True, timeout=10, cwd=root)
                self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
                self.assertIn('"status": "completed"', run.stdout)
            finally:
                server.shutdown(); server.server_close(); thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
