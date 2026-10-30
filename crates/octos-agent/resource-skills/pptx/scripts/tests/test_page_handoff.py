"""Protocol tests only: fixture bytes are deliberately not real slide packages."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("handoff", Path(__file__).parents[1] / "validate_page_handoff.py")
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)


class PageHandoffTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.write(self.root / "contract.json", {"page_order": ["alpha", "beta"]})

    def write(self, path, value):
        path.write_text(json.dumps(value), encoding="utf-8")

    def ref(self, path, base):
        return {"path": str(path.relative_to(base)), "sha256": h.sha256(path)}

    def fixture(self, page):
        out = self.root / page
        out.mkdir()
        copy = self.root / (page + "-copy.txt")
        copy.write_text("canonical copy")
        job = {"schema_version": 1, "page_id": page, "attempt_id": "one",
               "shared_contract": self.ref(self.root / "contract.json", self.root),
               "inputs": [{"role": "canonical-copy", **self.ref(copy, self.root)}], "output_dir": page}
        job_path = self.root / (page + "-job.json")
        self.write(job_path, job)
        artifacts = {}
        for key in ["source", "editable_pptx", "blank_pptx", "render", "blank_render"]:
            path = out / key
            path.write_text(page + " fixture " + key)
            artifacts[key] = self.ref(path, out)
        qa = {"pptx_sha256": artifacts["editable_pptx"]["sha256"],
              "blank_pptx_sha256": artifacts["blank_pptx"]["sha256"],
              "render_sha256": artifacts["render"]["sha256"],
              "blank_render_sha256": artifacts["blank_render"]["sha256"],
              "inspection": {"source": True, "editable_render": True, "blank_render": True},
              "open_findings": []}
        self.write(out / "qa.json", qa)
        artifacts["qa"] = self.ref(out / "qa.json", out)
        result = {"schema_version": 1, "page_id": page, "attempt_id": "one",
                  "job_sha256": h.sha256(job_path), "status": "complete", "artifacts": artifacts}
        result_path = out / "result.json"
        self.write(result_path, result)
        return job_path, result_path

    def test_collection_orders_independent_pages_without_claiming_visual_quality(self):
        a, b = self.fixture("alpha"), self.fixture("beta")
        report = h.validate_many([b, a], True)
        self.assertEqual([p["page_id"] for p in report["pages"]], ["alpha", "beta"])
        self.assertFalse(report["visual_quality_verified"])

    def test_duplicate_and_missing_pages_are_rejected(self):
        a = self.fixture("alpha")
        with self.assertRaisesRegex(ValueError, "Duplicate page"):
            h.validate_many([a, a])
        with self.assertRaisesRegex(ValueError, "Missing page"):
            h.validate_many([a], True)

    def test_changed_input_invalidates_a_finished_handoff(self):
        pair = self.fixture("alpha")
        (self.root / "alpha-copy.txt").write_text("new copy")
        with self.assertRaisesRegex(ValueError, "Hash mismatch"):
            h.validate_pair(*pair)

    def test_changed_contract_invalidates_a_finished_handoff(self):
        pair = self.fixture("alpha")
        self.write(self.root / "contract.json", {"page_order": ["beta", "alpha"]})
        with self.assertRaisesRegex(ValueError, "Hash mismatch"):
            h.validate_pair(*pair)

    def test_wrong_page_identity_is_rejected(self):
        job, result = self.fixture("alpha")
        data = h.load(result)
        data["page_id"] = "beta"
        self.write(result, data)
        with self.assertRaisesRegex(ValueError, "Wrong page_id"):
            h.validate_pair(job, result)

    def test_stale_qa_binding_is_rejected_even_when_file_hash_is_updated(self):
        job, result = self.fixture("alpha")
        (self.root / "alpha/render").write_text("new render")
        data = h.load(result)
        data["artifacts"]["render"] = self.ref(self.root / "alpha/render", self.root / "alpha")
        self.write(result, data)
        with self.assertRaisesRegex(ValueError, "Stale QA binding"):
            h.validate_pair(job, result)

    def test_sibling_artifact_is_rejected_despite_matching_hash(self):
        job, result = self.fixture("alpha")
        self.fixture("beta")
        data = h.load(result)
        data["artifacts"]["source"] = {"path": "../beta/source", "sha256": h.sha256(self.root / "beta/source")}
        self.write(result, data)
        with self.assertRaisesRegex(ValueError, "escapes"):
            h.validate_pair(job, result)

    def test_unresolved_findings_and_blocked_status_cannot_pass(self):
        job, result = self.fixture("alpha")
        qa_path = self.root / "alpha/qa.json"
        qa = h.load(qa_path)
        qa["open_findings"] = [{"id": "unresolved", "evidence": "render"}]
        self.write(qa_path, qa)
        data = h.load(result)
        data["artifacts"]["qa"] = self.ref(qa_path, qa_path.parent)
        self.write(result, data)
        with self.assertRaisesRegex(ValueError, "Unresolved"):
            h.validate_pair(job, result)
        data["status"] = "blocked"
        self.write(result, data)
        with self.assertRaisesRegex(ValueError, "not complete"):
            h.validate_pair(job, result)


if __name__ == "__main__":
    unittest.main()
