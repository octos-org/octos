"""Whole-collection checks with independent synthetic slide packages."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(Path(__file__).parents[1]))
from prepare_page_jobs import prepare
from validate_page_handoff import sha256
from validate_deck_assembly import validate
from pptx import Presentation
from pptx.util import Inches, Pt


class DeckAssemblyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.order = ["alpha", "beta", "gamma"]
        self.skill = self.root / "skill/SKILL.md"
        self.skill.parent.mkdir()
        self.skill.write_text("Canonical workflow")
        (self.skill.parent / "references").mkdir()
        (self.skill.parent / "references/independent-pages.md").write_text("Worker protocol")
        tools = self.root / "TOOLS.md"
        tools.write_text("Tools")
        self.write(self.root / "contract.json", {"schema_version": 1, "page_order": self.order})
        for name in ["copy.md", "design.png", "blank.png"]:
            (self.root / name).write_text("Synthetic input " + name)
        plan = {"shared_contract": "contract.json", "pages": [
            {"page_id": page, "inputs": [{"role": role, "path": path} for role, path in
             [("canonical-copy", "copy.md"), ("design", "design.png"), ("blank-art", "blank.png")]]}
            for page in self.order]}
        self.write(self.root / "plan.json", plan)
        self.receipt = prepare(self.root / "plan.json", self.root / "jobs", self.skill, tools)
        self.jobs_path = self.root / "jobs/prepared-jobs.json"
        for entry in self.receipt["jobs"]:
            self.complete(entry)
        self.deck = self.root / "assembled.pptx"
        self.build(self.deck, self.order)

    @staticmethod
    def write(path, value):
        path.write_text(json.dumps(value), encoding="utf-8")

    def build(self, path, pages, *, bold=False, left=1):
        prs = Presentation()
        prs.slide_width = Inches(12)
        prs.slide_height = Inches(7)
        for page in pages:
            slide = prs.slides.add_slide(prs.slide_layouts[6])
            shape = slide.shapes.add_textbox(Inches(left), Inches(1), Inches(5), Inches(1))
            run = shape.text_frame.paragraphs[0].add_run()
            run.text = "Synthetic " + page
            run.font.name = "Arial"
            run.font.size = Pt(18)
            run.font.bold = bold
        prs.save(path)

    def complete(self, entry):
        out = Path(entry["result"]).parent
        self.build(out / "proof.pptx", [entry["page_id"]])
        self.build(out / "blank.pptx", [""])
        paths = {"source": "page.py", "editable_pptx": "proof.pptx", "blank_pptx": "blank.pptx",
                 "render": "render.png", "blank_render": "blank.png"}
        for key in ["source", "render", "blank_render"]:
            (out / paths[key]).write_text("Synthetic fixture " + key)
        artifacts = {key: {"path": name, "sha256": sha256(out / name)} for key, name in paths.items()}
        qa = {"inspection": {"source": True, "editable_render": True, "blank_render": True}, "open_findings": []}
        for field, key in [("pptx_sha256", "editable_pptx"), ("blank_pptx_sha256", "blank_pptx"),
                           ("render_sha256", "render"), ("blank_render_sha256", "blank_render")]:
            qa[field] = artifacts[key]["sha256"]
        self.write(out / "qa.json", qa)
        artifacts["qa"] = {"path": "qa.json", "sha256": sha256(out / "qa.json")}
        self.write(Path(entry["result"]), {"schema_version": 1, "page_id": entry["page_id"],
                   "attempt_id": "initial", "job_sha256": sha256(Path(entry["job"])),
                   "status": "complete", "artifacts": artifacts})

    def check(self):
        return validate(self.jobs_path, self.deck)

    def assert_rejected(self):
        report = self.check()
        self.assertFalse(report["assembly_consistent"], report)
        self.assertFalse(report["visual_quality_verified"])
        return report

    def test_every_job_is_checked_in_contract_order_without_visual_claim(self):
        before = {p: sha256(p) for p in self.root.rglob("*") if p.is_file()}
        report = self.check()
        self.assertTrue(report["assembly_consistent"], report)
        self.assertEqual(report["page_order"], self.order)
        self.assertEqual([p["page_id"] for p in report["pages"]], self.order)
        self.assertEqual(report["slide_count"], len(self.order))
        self.assertFalse(report["visual_quality_verified"])
        self.assertEqual(before, {p: sha256(p) for p in before})

    def test_receipt_paths_may_be_relative_to_receipt(self):
        for entry in self.receipt["jobs"]:
            for key in ["job", "result"]:
                entry[key] = os.path.relpath(entry[key], self.jobs_path.parent)
        self.write(self.jobs_path, self.receipt)
        self.assertTrue(self.check()["assembly_consistent"])

    def test_omitted_or_duplicate_receipt_job_is_rejected(self):
        original = json.loads(self.jobs_path.read_text())
        for jobs in [original["jobs"][:-1], original["jobs"] + [original["jobs"][0]]]:
            with self.subTest(jobs=len(jobs)):
                self.write(self.jobs_path, {**original, "jobs": jobs})
                self.assert_rejected()

    def test_shrinking_receipt_and_final_deck_cannot_hide_a_contract_page(self):
        self.receipt["jobs"].pop()
        self.receipt["page_order"] = self.order[:-1]
        self.write(self.jobs_path, self.receipt)
        self.build(self.deck, self.order[:-1])
        self.assert_rejected()

    def test_wrong_receipt_order_even_when_job_list_agrees_is_rejected(self):
        self.receipt["page_order"] = list(reversed(self.order))
        self.receipt["jobs"].reverse()
        self.write(self.jobs_path, self.receipt)
        self.assert_rejected()

    def test_missing_job_or_result_is_rejected(self):
        entry = self.receipt["jobs"][1]
        for key in ["job", "result"]:
            path = Path(entry[key]); data = path.read_bytes()
            path.unlink()
            with self.subTest(key=key): self.assert_rejected()
            path.write_bytes(data)

    def test_stale_source_after_handoff_is_rejected(self):
        source = Path(self.receipt["jobs"][1]["result"]).parent / "page.py"
        source.write_text("Coordinator rewrote accepted worker source")
        self.assert_rejected()

    def test_blocked_handoff_and_wrong_receipt_identity_are_rejected(self):
        path = Path(self.receipt["jobs"][1]["result"])
        result = json.loads(path.read_text())
        self.write(path, {**result, "status": "blocked"})
        self.assert_rejected()
        self.write(path, result)
        self.receipt["jobs"][1]["job"] = self.receipt["jobs"][0]["job"]
        self.write(self.jobs_path, self.receipt)
        self.assert_rejected()

    def test_geometry_and_bold_drift_are_rejected_for_every_page(self):
        for mutation in [{"left": 2}, {"bold": True}]:
            with self.subTest(mutation=mutation):
                self.build(self.deck, self.order, **mutation)
                report = self.assert_rejected()
                self.assertEqual(len(report["pages"]), len(self.order))
                self.assertTrue(all(not p["native_assembly_unchanged"] for p in report["pages"]))

    def test_theme_drift_is_rejected(self):
        with zipfile.ZipFile(self.deck) as z:
            parts = {n: z.read(n) for n in z.namelist()}
        parts["ppt/theme/theme1.xml"] = parts["ppt/theme/theme1.xml"].replace(b"4F81BD", b"FF0000")
        with zipfile.ZipFile(self.deck, "w") as z:
            for name, data in parts.items(): z.writestr(name, data)
        self.assert_rejected()

    def test_extra_missing_reordered_and_duplicated_final_pages_are_rejected(self):
        for pages in [self.order + ["extra"], self.order[:-1], list(reversed(self.order)), ["alpha", "alpha", "gamma"]]:
            with self.subTest(pages=pages):
                self.build(self.deck, pages)
                self.assert_rejected()

    def test_cli_returns_failure_and_report_without_skip_options(self):
        script = Path(__file__).parents[1] / "validate_deck_assembly.py"
        report = self.root / "report.json"
        self.build(self.deck, self.order, bold=True)
        command = [sys.executable, str(script), "--jobs", str(self.jobs_path), "--deck", str(self.deck), "--out", str(report)]
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(json.loads(report.read_text())["assembly_consistent"])
        result = subprocess.run(command + ["--skip", "beta"], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)

    def test_cli_never_overwrites_inputs_even_when_an_earlier_result_is_missing(self):
        Path(self.receipt["jobs"][0]["result"]).unlink()
        source = Path(self.receipt["jobs"][2]["result"]).parent / "page.py"
        script = Path(__file__).parents[1] / "validate_deck_assembly.py"
        for output in [self.jobs_path, self.deck, self.root / "copy.md", source]:
            with self.subTest(output=output.name):
                before = output.read_bytes()
                result = subprocess.run([sys.executable, str(script), "--jobs", str(self.jobs_path),
                                         "--deck", str(self.deck), "--out", str(output)],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(output.read_bytes(), before)
                self.assertIn("must not overwrite", result.stdout)


if __name__ == "__main__": unittest.main()
