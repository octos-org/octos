"""Behavioral tests using new synthetic decks; no project-specific answers."""
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).parents[1]))
from prepare_page_jobs import prepare
from validate_page_handoff import bound_file
from compare_slide_proof import compare
from pptx import Presentation
from pptx.util import Inches, Pt


class PacketTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.skill = self.root / "skill/SKILL.md"
        self.skill.parent.mkdir(); self.skill.write_text("canonical skill")
        (self.skill.parent / "references").mkdir()
        (self.skill.parent / "references/independent-pages.md").write_text("canonical worker protocol")
        self.tools = self.root / "TOOLS.md"; self.tools.write_text("tool instructions")
        (self.root / "contract.json").write_text(json.dumps({"schema_version": 1, "page_order": ["second", "first"]}))
        for name in ("copy.md", "design.png", "blank.png"):
            (self.root / name).write_text("input fixture " + name)
        self.plan = self.root / "plan.json"
        self.value = {"shared_contract": "contract.json", "pages": [
            {"page_id": name, "inputs": [{"role": role, "path": path} for role, path in
             (("canonical-copy", "copy.md"), ("design", "design.png"), ("blank-art", "blank.png"))]}
            for name in ("first", "second")]}
        self.plan.write_text(json.dumps(self.value))

    def run_prepare(self):
        return prepare(self.plan, self.root / "jobs", self.skill, self.tools)

    def test_packets_resolve_inputs_and_keep_canonical_instructions(self):
        result = self.run_prepare()
        self.assertEqual([r["page_id"] for r in result["jobs"]], ["second", "first"])
        for entry in result["jobs"]:
            path = Path(entry["job"]); job = json.loads(path.read_text())
            for item in [job["shared_contract"], *job["inputs"]]:
                self.assertTrue(bound_file(path.parent.resolve(), item).is_file())
            self.assertEqual(entry["spawn_request"]["max_iterations"], 0)
            self.assertEqual(entry["spawn_request"]["owned_paths"],
                             [str((path.parent / "out").relative_to(self.root.resolve()))])
            self.assertEqual(len(entry["spawn_request"]["skill_paths"]), 3)
            self.assertFalse(result["helper_generates_layouts"])
            self.assertEqual(entry["input_roles"], [item["role"] for item in job["inputs"]])
        self.assertNotEqual(result["jobs"][0]["result"], result["jobs"][1]["result"])

    def test_workspace_claims_resolve_from_explicit_parent_and_refuse_outside_outputs(self):
        workspace = self.root / "workspace"
        workspace.mkdir()
        result = prepare(self.plan, workspace / "jobs", self.skill, self.tools, workspace=workspace)
        self.assertEqual(result["spawn_workspace"], str(workspace.resolve()))
        self.assertEqual(result["jobs"][0]["spawn_request"]["owned_paths"], ["jobs/second/out"])
        with self.assertRaisesRegex(ValueError, "inside the spawn workspace"):
            prepare(self.plan, self.root / "outside", self.skill, self.tools, workspace=workspace)
        self.assertFalse((self.root / "outside").exists())

    def test_additional_candidate_inputs_are_disclosed_and_remain_hash_bound(self):
        candidate = self.root / "candidate.json"
        candidate.write_text('{"machine-derived": true}')
        self.value["pages"][0]["inputs"].append({"role": "layout-candidate", "path": candidate.name})
        self.plan.write_text(json.dumps(self.value))
        receipt = self.run_prepare()
        entry = next(item for item in receipt["jobs"] if item["page_id"] == "first")
        self.assertIn("layout-candidate", entry["input_roles"])
        self.assertNotIn("contains_page_layouts", receipt)
        job_path = Path(entry["job"])
        job = json.loads(job_path.read_text())
        ref = next(item for item in job["inputs"] if item["role"] == "layout-candidate")
        self.assertEqual(bound_file(job_path.parent, ref), candidate.resolve())
        task = (job_path.parent / "worker.md").read_text()
        self.assertIn("verify them against the authoritative copy", task)
        self.assertNotIn("no coordinator OCR draft", task)
        candidate.write_text("Changed candidate")
        with self.assertRaisesRegex(ValueError, "Hash mismatch"):
            bound_file(job_path.parent, ref)

    def test_missing_dependency_does_not_leave_partial_packets(self):
        (self.root / "blank.png").unlink()
        with self.assertRaises(OSError): self.run_prepare()
        self.assertFalse((self.root / "jobs").exists())

    def test_supplied_shared_method_is_delegated_and_bound(self):
        method = self.skill.parent / 'references/page-method.md'
        method.write_text('Shared conversion and completion method')
        result = self.run_prepare()
        entry = result['jobs'][0]
        self.assertIn(str(method.resolve()), entry['spawn_request']['skill_paths'])
        job_path = Path(entry['job'])
        job = json.loads(job_path.read_text())
        reference = next(ref for ref in job['inputs']
                         if (job_path.parent / ref['path']).resolve() == method.resolve())
        self.assertEqual(bound_file(job_path.parent, reference), method.resolve())
        method.write_text('Changed method after dispatch')
        with self.assertRaisesRegex(ValueError, 'Hash mismatch'):
            bound_file(job_path.parent, reference)

    def test_compact_worker_method_replaces_coordinator_context(self):
        folder = self.skill.parent/'references'
        (folder/'page-worker.md').write_text('Compact worker method')
        (folder/'fit-and-revise.md').write_text('Reusable font/revision operations')
        (folder/'page-method.md').write_text('Long legacy method')
        result = self.run_prepare()
        paths = result['jobs'][0]['spawn_request']['skill_paths']
        self.assertIn(str((folder/'page-worker.md').resolve()), paths)
        self.assertIn(str((folder/'fit-and-revise.md').resolve()), paths)
        self.assertNotIn(str((folder/'independent-pages.md').resolve()), paths)
        self.assertNotIn(str((folder/'page-method.md').resolve()), paths)
    def test_existing_assignment_is_immutable(self):
        self.run_prepare(); before = (self.root / "jobs/first/job.json").read_bytes()
        with self.assertRaisesRegex(ValueError, "Refusing"): self.run_prepare()
        self.assertEqual((self.root / "jobs/first/job.json").read_bytes(), before)

    def test_duplicate_or_missing_page_is_rejected(self):
        self.value["pages"][1]["page_id"] = "first"; self.plan.write_text(json.dumps(self.value))
        with self.assertRaisesRegex(ValueError, "Plan differs"): self.run_prepare()

    def test_changed_skill_invalidates_input_binding(self):
        result = self.run_prepare(); path = Path(result["jobs"][0]["job"])
        job = json.loads(path.read_text()); self.skill.write_text("changed skill")
        refs = [i for i in job["inputs"] if i["role"] == "worker-instruction"]
        with self.assertRaisesRegex(ValueError, "Hash mismatch"):
            bound_file(path.parent.resolve(), refs[0])


class AssemblyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def build(self, name, text="Synthetic label", bold=False, left=1, size=18, prefix=False):
        prs = Presentation(); prs.slide_width = Inches(12); prs.slide_height = Inches(7)
        if prefix:
            slide = prs.slides.add_slide(prs.slide_layouts[6])
            slide.shapes.add_textbox(Inches(2), Inches(2), Inches(3), Inches(1)).text = "Unrelated preceding page"
        slide = prs.slides.add_slide(prs.slide_layouts[6])
        shape = slide.shapes.add_textbox(Inches(left), Inches(1), Inches(4), Inches(1))
        run = shape.text_frame.paragraphs[0].add_run(); run.text = text
        run.font.name = "Arial"; run.font.size = Pt(size); run.font.bold = bold
        path = self.root / name; prs.save(path); return path

    def test_same_builder_survives_presentation_reordering(self):
        a = self.build("proof.pptx"); b = self.build("deck.pptx", prefix=True)
        report = compare(a, b, 2)
        self.assertTrue(report["native_assembly_unchanged"])
        self.assertFalse(report["visual_quality_verified"])

    def test_format_copy_and_geometry_regressions_are_detected(self):
        proof = self.build("proof.pptx")
        for mutation in ({"bold": True}, {"text": "Different meaning"}, {"left": 2}, {"size": 19}):
            with self.subTest(mutation=mutation):
                deck = self.build("changed.pptx", **mutation)
                self.assertFalse(compare(proof, deck, 1)["native_assembly_unchanged"])

    def test_inherited_theme_change_is_detected(self):
        a = self.build("proof.pptx"); b = self.build("deck.pptx")
        with zipfile.ZipFile(b) as z: parts = {n: z.read(n) for n in z.namelist()}
        theme = parts["ppt/theme/theme1.xml"]
        self.assertIn(b"4F81BD", theme)
        parts["ppt/theme/theme1.xml"] = theme.replace(b"4F81BD", b"FF0000")
        with zipfile.ZipFile(b, "w") as z:
            for n, data in parts.items(): z.writestr(n, data)
        self.assertFalse(compare(a, b, 1)["native_assembly_unchanged"])

    def test_multiple_page_proof_and_out_of_range_target_are_rejected(self):
        proof = self.build("proof.pptx"); deck = self.build("deck.pptx", prefix=True)
        with self.assertRaisesRegex(ValueError, "exactly one"):
            compare(deck, proof, 1)
        with self.assertRaisesRegex(ValueError, "outside"):
            compare(proof, deck, 3)


if __name__ == "__main__": unittest.main()
