"""Behavior tests for scoped text-region detection, using synthetic geometry."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

import fitz
from PIL import Image
from pptx import Presentation
from pptx.util import Inches

spec = importlib.util.spec_from_file_location('audit_text_regions', Path(__file__).parents[1] / 'audit_text_regions.py')
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class TextRegionTests(unittest.TestCase):
    def fixture(self, directory, mode='good'):
        r = Path(directory)
        prs = Presentation(); prs.slide_width = Inches(10); prs.slide_height = Inches(5.625)
        slide = prs.slides.add_slide(prs.slide_layouts[6])
        label = 'Ready' if mode != 'copy' else 'Reedy'
        x, y = ((5, 1) if mode == 'wrong_owner' else (1, 1))
        sh = slide.shapes.add_textbox(Inches(x), Inches(y), Inches(1.5), Inches(.4))
        sh.name = 'TEXT__caption'; sh.text = label
        if mode == 'duplicate':
            dup = slide.shapes.add_textbox(Inches(5), Inches(3), Inches(1.5), Inches(.4))
            dup.name = 'TEXT__caption-duplicate'; dup.text = label
        pptx = r / 'deck.pptx'; prs.save(pptx)
        doc = fitz.open(); page = doc.new_page(width=720, height=405)
        page.insert_text((x * 72, y * 72 + 14), label, fontsize=12)
        if mode == 'duplicate':
            page.insert_text((360, 230), label, fontsize=12)
        pdf = r / 'deck.pdf'; doc.save(pdf)
        Image.new('RGB', (720, 405), 'white').save(r / 'source.png')
        binding = {'input_sha256': audit.digest(pptx), 'pdf_sha256': audit.digest(pdf)}
        (r / 'binding.json').write_text(json.dumps(binding))
        region = {'version': 1, 'pages': [{'page': 1, 'design_image': 'source.png',
                  'artwork_image': 'source.png', 'region_basis': 'Synthetic fixture: independent intended card.',
                  'objects': [{'id': 'caption', 'native_name': 'TEXT__caption', 'text': 'Ready',
                               'owner_id': 'card-a', 'owner_rect': [.09, .16, .26, .27],
                               'source_evidence': 'Fixture card A', 'max_lines': 1}],
                  'counts': [{'id': 'caption-count', 'text': label, 'expected': 1}]}]}
        (r / 'regions.json').write_text(json.dumps(region))
        return r

    def run_fixture(self, r):
        return audit.run(r / 'deck.pptx', r / 'deck.pdf', r / 'regions.json', r / 'binding.json', r / 'audit')

    def test_correct_caption_and_explicitly_scoped_result(self):
        with tempfile.TemporaryDirectory() as td:
            result = self.run_fixture(self.fixture(td))
            self.assertEqual(result['findings'], [])
            self.assertIsNone(result['whole_deck_pass'])
            self.assertTrue(Path(result['objects'][0]['evidence']['render']).exists())

    def test_text_fits_own_frame_but_is_in_wrong_card(self):
        with tempfile.TemporaryDirectory() as td:
            result = self.run_fixture(self.fixture(td, 'wrong_owner'))
            self.assertIn('TEXT_OUTSIDE_OWNER', [x['code'] for x in result['findings']])

    def test_extra_standalone_caption_detected(self):
        with tempfile.TemporaryDirectory() as td:
            result = self.run_fixture(self.fixture(td, 'duplicate'))
            self.assertIn('EXACT_NATIVE_COUNT_MISMATCH', [x['code'] for x in result['findings']])

    def test_copy_mismatch_does_not_prevent_geometry_check(self):
        with tempfile.TemporaryDirectory() as td:
            result = self.run_fixture(self.fixture(td, 'copy'))
            self.assertIn('COPY_MISMATCH', [x['code'] for x in result['findings']])
            self.assertIn('rendered_rect', result['objects'][0])

    def test_arrow_region_collision(self):
        with tempfile.TemporaryDirectory() as td:
            r = self.fixture(td)
            region = json.loads((r / 'regions.json').read_text())
            region['pages'][0]['forbidden'] = [{'id': 'connector', 'rect': [.11, .18, .15, .22]}]
            (r / 'regions.json').write_text(json.dumps(region))
            result = self.run_fixture(r)
            self.assertIn('TEXT_OVER_ARTWORK_REGION', [x['code'] for x in result['findings']])

    def test_stale_render_binding_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            r = self.fixture(td)
            d = json.loads((r / 'binding.json').read_text()); d['pdf_sha256'] = 'stale'
            (r / 'binding.json').write_text(json.dumps(d))
            with self.assertRaisesRegex(ValueError, 'binding'):
                self.run_fixture(r)

    def test_missing_native_identity_remains_unresolved(self):
        with tempfile.TemporaryDirectory() as td:
            r = self.fixture(td)
            d = json.loads((r / 'regions.json').read_text())
            d['pages'][0]['objects'][0]['native_name'] = 'nonexistent'
            (r / 'regions.json').write_text(json.dumps(d))
            result = self.run_fixture(r)
            self.assertIn('NATIVE_OBJECT_UNRESOLVED', [x['code'] for x in result['findings']])
            self.assertEqual(result['scope'][0]['uncovered_native_names'], ['TEXT__caption'])

    def test_no_overwrite_of_prior_evidence(self):
        with tempfile.TemporaryDirectory() as td:
            r = self.fixture(td); self.run_fixture(r)
            with self.assertRaises(FileExistsError):
                self.run_fixture(r)

    def test_normalization_preserves_meaningful_glyph_differences(self):
        self.assertNotEqual(audit.norm('C₁'), audit.norm('C1'))
        self.assertNotEqual(audit.norm('a ≤ b'), audit.norm('a < b'))
        self.assertEqual(audit.norm('Hello\nworld'), audit.norm('Hello world'))

    def test_reordered_or_absent_pdf_text_is_not_a_pass(self):
        actual, error = audit.locate('h₁=H(C₁)', 'h=H(C)₁₁', [], (.1, .1, .2, .2))
        self.assertIsNone(actual); self.assertIsNotNone(error)

    def test_line_count_clusters_subscript_but_not_wrap(self):
        lines = audit.rendered_lines([{'origin': (0, y), 'size': 12} for y in (10, 12, 10, 28)])
        self.assertEqual(len(lines), 2)
        self.assertEqual(len(lines[0]['characters']), 3)


if __name__ == '__main__':
    unittest.main()
