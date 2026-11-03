"""Native-PPTX behavior checks; no PDF or project-specific slide fixtures."""
import importlib.util
import tempfile
import unittest
from pathlib import Path

from pptx import Presentation
from pptx.enum.text import MSO_AUTO_SIZE, PP_ALIGN
from pptx.util import Inches, Pt

spec = importlib.util.spec_from_file_location('inspect_slide_layers', Path(__file__).parents[1] / 'inspect_slide_layers.py')
audit = importlib.util.module_from_spec(spec); spec.loader.exec_module(audit)


class NativeInventoryTests(unittest.TestCase):
    def build(self, root, labels):
        prs = Presentation()
        for label in labels:
            slide = prs.slides.add_slide(prs.slide_layouts[6])
            sh = slide.shapes.add_textbox(Inches(.5), Inches(.5), Inches(3), Inches(.6))
            sh.name = 'TEXT__title'; sh.text = label
            sh.text_frame.auto_size = MSO_AUTO_SIZE.NONE
            p = sh.text_frame.paragraphs[0]; p.alignment = PP_ALIGN.LEFT
            p.runs[0].font.name = 'Arial'; p.runs[0].font.size = Pt(26)
        path = Path(root) / 'candidate.pptx'; prs.save(path)
        return path

    def test_all_slides_without_pdf_and_no_fixed_deck_length(self):
        with tempfile.TemporaryDirectory() as td:
            labels = ['Overview', 'Queue', 'Storage', 'Evidence', 'Next']
            path = self.build(td, labels)
            report = audit.inventory(path)
            self.assertEqual(report['slide_count'], len(labels))
            self.assertEqual(report['inspected_slide_count'], len(labels))
            self.assertEqual([p['native_text'][0]['text'] for p in report['pages']], labels)
            self.assertIsNone(report['visual_pass'])
            self.assertEqual(list(Path(td).glob('*.pdf')), [])

    def test_notes_distinguish_missing_empty_and_actual_reference_content(self):
        with tempfile.TemporaryDirectory() as td:
            path = self.build(td, ['Absent', 'Empty', 'Documented'])
            prs = Presentation(path)
            prs.slides[1].notes_slide.notes_text_frame.text = ''
            prs.slides[2].notes_slide.notes_text_frame.text = '来源：实验记录\nhttps://example.org/study'
            prs.save(path)
            notes = [p['speaker_notes'] for p in audit.inventory(path)['pages']]
            self.assertFalse(notes[0]['present'])
            self.assertEqual(notes[0]['text'], '')
            self.assertTrue(notes[1]['present'])
            self.assertEqual(notes[1]['text'], '')  # automatic slide-number is not a citation
            self.assertEqual(notes[2]['text'], '来源：实验记录\nhttps://example.org/study')
            self.assertTrue(notes[2]['parts'][0].startswith('ppt/notesSlides/'))
            self.assertNotIn('实验记录', audit.inventory(path)['pages'][2]['native_text'][0]['text'])

    def test_reordering_tracks_presentation_order_and_stable_slide_identity(self):
        with tempfile.TemporaryDirectory() as td:
            path = self.build(td, ['Alpha', 'Beta', 'Gamma'])
            before = audit.inventory(path)
            prs = Presentation(path)
            slides = prs.slides._sldIdLst
            last = slides[-1]; slides.remove(last); slides.insert(0, last)
            prs.save(path)
            after = audit.inventory(path)
            self.assertEqual([p['native_text'][0]['text'] for p in after['pages']], ['Gamma', 'Alpha', 'Beta'])
            self.assertEqual(after['pages'][0]['slide_id'], before['pages'][2]['slide_id'])
            self.assertEqual(after['pages'][0]['slide_part'], before['pages'][2]['slide_part'])

    def test_native_font_alignment_and_autofit_are_available(self):
        with tempfile.TemporaryDirectory() as td:
            report = audit.inventory(self.build(td, ['Unseen heading']))
            item = report['pages'][0]['native_text'][0]
            para = item['paragraphs'][0]
            self.assertEqual(para['properties']['attributes']['algn'], 'l')
            self.assertEqual(para['runs'][0]['properties']['attributes']['sz'], '2600')
            self.assertIn({'tag': 'latin', 'attributes': {'typeface': 'Arial'}}, para['runs'][0]['properties']['children'])
            self.assertIn('noAutofit', [e['tag'] for e in item['body_properties'][0]['children']])
            self.assertTrue(item['transform_local_emu'])

    def test_single_page_interface_retained_and_bad_index_rejected(self):
        with tempfile.TemporaryDirectory() as td:
            path = self.build(td, ['One', 'Two'])
            report = audit.inspect(path, 2)
            self.assertEqual(report['native_text'][0]['text'], 'Two')
            with self.assertRaisesRegex(ValueError, 'presentation order'):
                audit.inventory(path, 3)


if __name__ == '__main__':
    unittest.main()
