import json
from pathlib import Path
import tempfile
import unittest
from receipt_adapter import attach_job_metadata, digest, publish_operation


class AdapterTests(unittest.TestCase):
    def test_publishes_real_input_binding_without_an_agent_plan(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "source.pptx").write_bytes(b"candidate")
            job = {"id": "render-1", "request_sha256": "request",
                   "status": "completed", "request": {"op": "render", "pptx": "source.pptx"},
                   "result": {"report": {"input_sha256": digest(root / "source.pptx")}}}
            attach_job_metadata(job, "job.json")
            (root / "job.json").write_text(json.dumps(job))
            report = publish_operation(root, job, "job.json")
            self.assertEqual(report["kind"], "operation")
            self.assertEqual(report["artifacts"][0]["role"], "input")
            self.assertEqual(report["artifacts"][1]["role"], "output")
            self.assertTrue((root / ".octos/tool-progress/render.json").is_file())
            self.assertFalse((root / ".octos/progress.json").exists())

    def test_failure_is_not_published_as_success(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            job = {"id": "render-1", "request_sha256": "request", "status": "failed",
                   "request": {"op": "render"}, "error": "renderer failed"}
            (root / "job.json").write_text(json.dumps(job))
            report = publish_operation(root, job, "job.json")
            self.assertEqual(report["status"], "fail")
            self.assertEqual(len(report["artifacts"]), 1)

    def test_rejects_symlinked_report_root(self):
        with tempfile.TemporaryDirectory() as folder, tempfile.TemporaryDirectory() as other:
            root = Path(folder)
            (root / ".octos").symlink_to(other, target_is_directory=True)
            job = {"id": "x", "request_sha256": "request", "status": "failed",
                   "request": {"op": "render"}}
            (root / "job.json").write_text(json.dumps(job))
            with self.assertRaises(ValueError):
                publish_operation(root, job, "job.json")


if __name__ == "__main__":
    unittest.main()
